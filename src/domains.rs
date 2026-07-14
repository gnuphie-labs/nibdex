// SPDX-License-Identifier: MIT

//! Domain configuration — `.nibdex-domains.toml` at the workspace root maps
//! top-level subdirs to IP domains, so a single workspace can be indexed into
//! separate per-domain databases (docs/SESSION_SCOPE_DESIGN.md §0).
//!
//! Each `nibdex index --domain <name>` writes ONLY that domain's labeled subdirs
//! into its db. The isolation guarantee (the invariant): a domain's db never
//! contains a subdir not labeled for it. No file = one implicit domain (today's
//! behavior; zero cost for unpartitioned users). This module is Gear 1 — the
//! label config + the `includes` predicate the index filter uses; it does not
//! touch sessions (Gear 2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const CONFIG_FILE: &str = ".nibdex-domains.toml";

/// The `[domains]` table of `.nibdex-domains.toml`: each key is an arbitrary
/// domain name mapping to its top-level subdir names (relative to the workspace
/// root). A subdir may be listed under any number of domains — it is then indexed
/// into each of those domains' dbs. That is how a shared library is expressed;
/// there are no reserved domain names.
///
/// ```toml
/// [domains]
/// personal = ["my-app", "my-lib", "oss-lib-a"]
/// client-a = ["acme-api", "acme-web", "oss-lib-a"]  # oss-lib-a shared with personal
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DomainConfig {
    domains: HashMap<String, Vec<String>>,
}

impl DomainConfig {
    /// Load `<workspace>/.nibdex-domains.toml`. `Ok(None)` when the file is
    /// absent (unpartitioned workspace). Malformed TOML is a hard error — an
    /// unreadable domain map must fail loudly, never silently mis-route content.
    pub fn load(workspace: &Path) -> Result<Option<Self>> {
        let path = workspace.join(CONFIG_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(cfg))
    }

    /// All declared domain names, sorted.
    pub fn domain_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.domains.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Is `name` a declared domain?
    pub fn has_domain(&self, name: &str) -> bool {
        self.domains.contains_key(name)
    }

    /// Does `path` (an absolute repo/anchor path under `workspace`) belong to
    /// `domain`? True iff its FIRST component below `workspace` is a subdir
    /// listed under `domain`. The workspace root itself (no component below it)
    /// belongs to no domain and is excluded — its top-level files are not
    /// domain-attributable. `workspace` and `path` are expected canonicalized by
    /// the caller (the indexer canonicalizes both), so the `strip_prefix` is exact.
    pub fn includes(&self, domain: &str, workspace: &Path, path: &Path) -> bool {
        let Some(subdirs) = self.domains.get(domain) else {
            return false;
        };
        match top_subdir(workspace, path) {
            Some(sub) => subdirs.contains(&sub),
            None => false,
        }
    }
}

/// Lexically resolve `.` and `..` segments WITHOUT touching the filesystem.
///
/// `canonicalize()` is the primary normalizer (it resolves symlinks and yields
/// the true path), but it FAILS on a deleted or never-created target — exactly
/// the shape a stale transcript edge can carry (`Write` to a since-removed file,
/// an edit through a `..` that no longer exists). When canonicalize fails we fall
/// back to this lexical pass so `top_subdir` reads the intended first component,
/// not a literal `..`/`.` (GEAR2_DESIGN §3 finding B3).
///
/// Returns `None` when a `..` cannot be resolved — it would pop above the path's
/// root (a residue `..`). Such a path is treated as not-domain-visible
/// (fail-narrow): a path we can't pin under the workspace must never route into
/// a domain db. An absolute input stays absolute; a relative input stays relative.
pub fn normalize_lexical(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {} // drop "."
            Component::ParentDir => match out.components().next_back() {
                // Pop a real dir; anything else (empty, root, prefix) means the
                // `..` escapes what we can resolve → residue → not-visible.
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => return None,
            },
            // RootDir / Prefix / Normal — carried through verbatim.
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// The first path component of `path` relative to `workspace`, if `path` is
/// strictly under `workspace`. `None` when `path == workspace` or is not under it.
fn top_subdir(workspace: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(workspace).ok()?;
    rel.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> DomainConfig {
        // `oss-lib-a` is listed under BOTH domains → shared, with no reserved word.
        let toml = r#"
            [domains]
            personal = ["my-app", "my-lib", "oss-lib-a"]
            client-a = ["acme-api", "acme-web", "oss-lib-a"]
        "#;
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn load_absent_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(DomainConfig::load(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn load_parses_present_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n",
        )
        .unwrap();
        let cfg = DomainConfig::load(tmp.path()).unwrap().unwrap();
        assert!(cfg.has_domain("personal"));
    }

    #[test]
    fn load_malformed_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(CONFIG_FILE), "not = valid = toml =").unwrap();
        assert!(DomainConfig::load(tmp.path()).is_err());
    }

    #[test]
    fn domain_names_lists_all_declared() {
        let cfg = sample();
        assert_eq!(cfg.domain_names(), vec!["client-a", "personal"]);
        assert!(cfg.has_domain("personal"));
        assert!(!cfg.has_domain("nonexistent"));
    }

    #[test]
    fn includes_routes_repos_by_top_subdir() {
        let cfg = sample();
        let ws = Path::new("/ws");
        // own-domain subdir
        assert!(cfg.includes("personal", ws, &PathBuf::from("/ws/my-app")));
        assert!(cfg.includes("client-a", ws, &PathBuf::from("/ws/acme-api")));
        // NOT the other domain
        assert!(!cfg.includes("personal", ws, &PathBuf::from("/ws/acme-api")));
        assert!(!cfg.includes("client-a", ws, &PathBuf::from("/ws/my-app")));
        // a subdir listed under BOTH domains is in both (many-to-many sharing)
        assert!(cfg.includes("personal", ws, &PathBuf::from("/ws/oss-lib-a")));
        assert!(cfg.includes("client-a", ws, &PathBuf::from("/ws/oss-lib-a")));
        // nested path resolves by its top-level subdir
        assert!(cfg.includes("personal", ws, &PathBuf::from("/ws/my-app/crates/core")));
        // an undeclared domain matches nothing
        assert!(!cfg.includes("nonexistent", ws, &PathBuf::from("/ws/my-app")));
        // workspace root itself belongs to no domain
        assert!(!cfg.includes("personal", ws, ws));
        // outside the workspace
        assert!(!cfg.includes("personal", ws, &PathBuf::from("/elsewhere/my-app")));
        // unlabeled subdir belongs to no domain
        assert!(!cfg.includes("personal", ws, &PathBuf::from("/ws/unlabeled")));
    }

    #[test]
    fn normalize_lexical_drops_curdir_and_noop() {
        assert_eq!(
            normalize_lexical(Path::new("/ws/a/./b.rs")),
            Some(PathBuf::from("/ws/a/b.rs"))
        );
        assert_eq!(
            normalize_lexical(Path::new("/ws/a/b.rs")),
            Some(PathBuf::from("/ws/a/b.rs"))
        );
    }

    #[test]
    fn normalize_lexical_resolves_parentdir() {
        assert_eq!(
            normalize_lexical(Path::new("/ws/a/../b/c.rs")),
            Some(PathBuf::from("/ws/b/c.rs"))
        );
        // The cross-domain `..` trap (B3): a path physically under proj-a's dir
        // but lexically pointing INTO proj-b must resolve to proj-b, so routing
        // sees the true target subdir — never the literal `proj-a` component.
        assert_eq!(
            normalize_lexical(Path::new("/ws/proj-a/../proj-b/x.rs")),
            Some(PathBuf::from("/ws/proj-b/x.rs"))
        );
    }

    #[test]
    fn normalize_lexical_unresolvable_parentdir_is_none() {
        // Escapes above the root → residue → not-domain-visible.
        assert_eq!(normalize_lexical(Path::new("/ws/../../etc/x")), None);
        assert_eq!(normalize_lexical(Path::new("/..")), None);
        // A relative `..` cannot be pinned lexically either.
        assert_eq!(normalize_lexical(Path::new("../foo")), None);
    }

    #[test]
    fn normalize_lexical_then_includes_routes_by_true_target() {
        // The compose Gear 2 core uses: normalize a `..`-bearing path, then ask
        // `includes`. The proj-a→proj-b escape must land in proj-b's domain.
        let cfg: DomainConfig = toml::from_str(
            "[domains]\nalpha = [\"proj-a\"]\nbeta = [\"proj-b\"]\n",
        )
        .unwrap();
        let ws = Path::new("/ws");
        let norm = normalize_lexical(Path::new("/ws/proj-a/../proj-b/x.rs")).unwrap();
        assert!(cfg.includes("beta", ws, &norm));
        assert!(!cfg.includes("alpha", ws, &norm));
    }
}
