// SPDX-License-Identifier: MIT

//! Domain configuration — `.nibdex-domains.toml` at the workspace root maps
//! workspace-relative subdir PATHS to IP domains, so a single workspace can be
//! indexed into separate per-domain databases (docs/SESSION_SCOPE_DESIGN.md §0).
//! Paths may be at any depth (docs/LABEL_DEPTH_DESIGN.md); matching is
//! component-wise and the deepest listed path wins.
//!
//! Each `nibdex index --domain <name>` writes ONLY that domain's labeled subdirs
//! into its db. The isolation guarantee (the invariant): a domain's db contains
//! only its labeled subdirs, PLUS the memory dir if it explicitly claims one via
//! the optional `[memory]` table. Nothing else. No file = one implicit domain (today's
//! behavior; zero cost for unpartitioned users). This module is Gear 1 — the
//! label config + the `includes` predicate the index filter uses; it does not
//! touch sessions (Gear 2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const CONFIG_FILE: &str = ".nibdex-domains.toml";

/// The `[domains]` table of `.nibdex-domains.toml`: each key is an arbitrary
/// domain name mapping to workspace-relative subdir PATHS, at any depth. A path
/// may be listed under any number of domains — it is then indexed into each of
/// those domains' dbs. That is how a shared library is expressed; there are no
/// reserved domain names.
///
/// ```toml
/// [domains]
/// personal = ["my-app", "my-lib", "oss-lib-a", "learn/python-sandbox"]
/// client-a = ["acme-api", "acme-web", "oss-lib-a"]  # oss-lib-a shared with personal
/// ```
///
/// `learn/python-sandbox` above shows the two depth rules that matter in
/// practice: a repo nested inside another directory must be listed to be
/// indexed at all (D6), and listing it makes `learn` a "mixed parent" whose
/// other child directories then need listing too (D7).
#[derive(Debug, Clone, Deserialize)]
pub struct DomainConfig {
    domains: HashMap<String, Vec<String>>,
    /// The OPTIONAL `[memory]` table: which domain owns the workspace's memory
    /// dir. See [`DomainConfig::claims_memory`]. Absent = no domain claims it,
    /// which is today's behavior (memory skipped entirely in domain mode).
    #[serde(default)]
    memory: HashMap<String, String>,
    /// The OPTIONAL `[unassigned]` table — subdir paths that are in no domain,
    /// annotated with how far the user has got in deciding, and at depth the
    /// only way to WITHDRAW a path from an enclosing label. See [`Unassigned`].
    #[serde(default)]
    unassigned: Unassigned,
}

/// The `[unassigned]` table — the config's ONE withdrawing table.
///
/// `[domains]` grants and `[unassigned]` withdraws; there is nothing else, and
/// **nothing in `[unassigned]` can ever cause content to be indexed.**
///
/// Entries are workspace-relative PATHS at any depth. At top level a listing
/// here reads as a pure annotation, because "not labeled" already means "not
/// admitted" there — that is why this table was originally documented as purely
/// an audit annotation. At DEPTH it is load-bearing: an entry **withdraws that
/// path from any enclosing label's reach** (`docs/LABEL_DEPTH_DESIGN.md` D3),
/// which is how a foreign tree under a labeled parent is expressed when the
/// domain that owns it does not exist in this config at all — the motivating
/// case, and one no positive-label-only scheme can state.
///
/// Withdrawal never confers neutrality (D4). A path named here is indexed by no
/// domain and still TAINTS sessions that touch it. Treating "not this domain's"
/// as "harmless" would hand the user a foreign tree they could touch freely
/// without withholding prose — a one-line ratchet off-switch.
///
/// Both lists withdraw identically. They differ only in what the audit does
/// with them:
///
/// - `acknowledged` — reviewed, deliberately in no domain. **Silent.** Without
///   it the audit reports the same directories forever and stops being read.
/// - `undecided` — discovered and written into the file by
///   `nibdex audit --stage-undecided`, awaiting a decision. **Reported.**
///
/// The pair exists because the config file is the only place the decision is
/// auditable, so it should also be the place the decision is MADE: staging the
/// discovered set into `undecided` turns triage into editing the real config
/// (move a string from `undecided` into a domain's list, or into
/// `acknowledged`) instead of answering prompts that write it for you. There is
/// no second format and no import step — the file is the config.
///
/// `undecided` deliberately never carries a suggested domain. Guessing which
/// domain a directory belongs to is the one inference that mis-routes IP, and
/// it would be trusted precisely because it is usually right.
/// `deny_unknown_fields` because this table is now HAND-EDITED as the normal
/// workflow. A mistyped `undecied = [...]` would otherwise parse fine and do
/// nothing, and the user would be left rereading an audit that keeps reporting
/// directories they believe they have annotated. Silence is the wrong answer in
/// a file whose whole job is being auditable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unassigned {
    #[serde(default)]
    acknowledged: Vec<String>,
    #[serde(default)]
    undecided: Vec<String>,
}

/// The only accepted `[memory]` value today: the whole memory dir. Kept as a
/// string rather than a bool so a future per-file form (a list of globs) can be
/// added without changing the shape of the existing declaration.
const MEMORY_CLAIM_ALL: &str = "*";

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
        Self::parse_validated(&raw, &path.display().to_string()).map(Some)
    }

    /// Parse and fully validate config TEXT, naming `source` in any error.
    ///
    /// Split out of [`Self::load`] so a writer can validate a CANDIDATE document
    /// BEFORE it replaces the real file. `.nibdex-domains.toml` is the security
    /// boundary; "write it, then discover it is invalid" leaves that boundary
    /// broken on disk and makes recovery the user's problem. One validator
    /// serves both paths, so a rule can never apply to loading but not writing.
    pub fn parse_validated(raw: &str, source: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(raw).with_context(|| format!("parsing {source}"))?;
        // Validate `[memory]` at LOAD time, not at query time: a typo'd domain
        // name or an unrecognized claim must fail loudly here rather than
        // silently resolving to "no domain claims memory" and quietly dropping a
        // corpus the user believes they enabled.
        for (domain, claim) in &cfg.memory {
            if !cfg.domains.contains_key(domain) {
                anyhow::bail!(
                    "[memory] names domain {domain:?}, which is not declared in [domains] \
                     (declared: {:?}) — {}",
                    cfg.domain_names(),
                    source
                );
            }
            if claim != MEMORY_CLAIM_ALL {
                anyhow::bail!(
                    "[memory] {domain} = {claim:?} is not understood; the only supported value \
                     is \"{MEMORY_CLAIM_ALL}\" (the whole memory dir) — {}",
                    source
                );
            }
        }
        // Exactly one domain may claim memory. There is ONE workspace-global
        // memory dir, so two claims would put the same content in both dbs while
        // "claim" reads as exclusive ownership. IP_DOMAINS.md already tells
        // multi-domain users to leave it unclaimed; this makes that advice
        // mechanical instead of advisory (adversarial review, 2026-07-18).
        if cfg.memory.len() > 1 {
            let mut claimants: Vec<&str> = cfg.memory.keys().map(String::as_str).collect();
            claimants.sort_unstable();
            anyhow::bail!(
                "[memory] is claimed by {} domains ({claimants:?}), but a workspace has ONE \
                 memory dir — claiming it from several domains would put the same content in \
                 each of their databases. If this workspace really holds several domains, \
                 leave [memory] unset. — {}",
                claimants.len(),
                source
            );
        }
        // Every entry must be a usable workspace-relative path. Entries are
        // PATHS now (docs/LABEL_DEPTH_DESIGN.md D1), so this rejects the two
        // shapes that could not be audited by eye: an absolute path, and one
        // containing `..` — either can name a tree outside the subtree it
        // appears to describe. `includes` independently refuses them, so this
        // is a loud-failure convenience, not the boundary itself. A DANGLING
        // entry (one matching nothing on disk) is deliberately NOT an error —
        // it is an inert report line (D8), because under D6/D7 nothing missing
        // can ever cause admission.
        for (table, entry) in cfg
            .domains
            .iter()
            .flat_map(|(d, subs)| subs.iter().map(move |s| (d.as_str(), s)))
            .chain(
                cfg.unassigned
                    .acknowledged
                    .iter()
                    .chain(cfg.unassigned.undecided.iter())
                    .map(|s| ("unassigned", s)),
            )
        {
            if entry_components(entry).is_none() {
                anyhow::bail!(
                    "[{table}] lists {entry:?}, which is not a usable workspace-relative path \
                     (entries may not be empty, absolute, or contain \"..\") — {}",
                    source
                );
            }
        }
        // An unassigned-table subdir that is ALSO labeled is a contradiction the
        // user should resolve rather than have silently resolved for them (the
        // label wins; the annotation is dead text).
        //
        // This matters MORE for `undecided` than for `acknowledged`, because
        // `undecided` is the list people edit: the intended gesture is to MOVE a
        // string out of it into a domain's list, and a copy-instead-of-move
        // leaves the entry behind. Failing loudly turns a half-finished edit into
        // an error at the next command instead of a directory that reads as
        // "still undecided" in the file while actually being indexed.
        for (key, names) in [
            ("acknowledges", &cfg.unassigned.acknowledged),
            ("lists as undecided", &cfg.unassigned.undecided),
        ] {
            for name in names {
                if cfg.domains.values().any(|subs| subs.contains(name)) {
                    anyhow::bail!(
                        "[unassigned] {key} {name:?}, but it is also labeled in [domains] — \
                         one of the two is stale. Remove whichever no longer applies. — {}",
                        source
                    );
                }
            }
        }
        // Acknowledged AND undecided is the same class of half-finished edit:
        // the two lists mean opposite things ("decided to exclude" vs "not yet
        // decided"), so a subdir in both has no readable state.
        for name in &cfg.unassigned.undecided {
            if cfg.unassigned.acknowledged.contains(name) {
                anyhow::bail!(
                    "[unassigned] lists {name:?} as BOTH acknowledged and undecided — those \
                     mean opposite things. Keep it in `acknowledged` if the decision is made, \
                     in `undecided` if it is not. — {}",
                    source
                );
            }
        }
        Ok(cfg)
    }

    /// Does `domain` claim the workspace's memory dir (the `[memory]` table)?
    ///
    /// Memory is NOT filesystem-attributable the way a subdir is: Claude Code
    /// owns its path (`~/.claude/projects/<slug>/memory/`), so it cannot be moved
    /// into a labeled subdir, and nibdex cannot verify that a memory file really
    /// belongs to the claiming domain. This is therefore an explicit USER
    /// ASSERTION — the same trust level as a subdir label, but unverifiable, and
    /// documented as such.
    ///
    /// The dir is already per-workspace (`default_memory_dir` derives it from the
    /// WORKSPACE), so a single-domain workspace's memory holds only that domain's
    /// *files*. That is NOT the same as holding only that domain's *words*, and
    /// the difference is measured, not hypothetical: on the author's own
    /// single-domain box 8 of 25 memory files name another domain's projects —
    /// inside this project's own narrative (the work/personal split, the
    /// publication scrub, the distribution plan), not as that domain's IP.
    ///
    /// Per-file labelling does NOT clean this up, which is why it is not built:
    /// the entanglement is diffuse, and excluding every file that mentions
    /// another domain would drop core memories of THIS domain. The residual is
    /// semantic, so no labelling scheme closes it — this is the documented §6
    /// position restated (mechanical isolation, never a semantic censor).
    /// Claiming memory is therefore a deliberate trade: a whole corpus back, in
    /// exchange for cross-domain *names* appearing in one's own narrative.
    ///
    /// FAIL-NARROW: no `[memory]` table, or no entry for this domain, means the
    /// memory corpus is skipped — byte-for-byte today's domain-mode behavior.
    pub fn claims_memory(&self, domain: &str) -> bool {
        self.memory.get(domain).is_some_and(|c| c == MEMORY_CLAIM_ALL)
    }

    /// Has this subdir path been reviewed and deliberately left unlabeled?
    /// Grants NO access — see [`Unassigned`]. Exact-match on the entry as
    /// written; the depth-aware "does anything claim this path" question is
    /// [`Self::includes`].
    pub fn is_acknowledged(&self, subdir: &str) -> bool {
        self.unassigned.acknowledged.iter().any(|a| a == subdir)
    }

    /// Is this subdir path staged in `[unassigned] undecided` — recorded in
    /// the file, still awaiting a decision? Grants NO access — see
    /// [`Unassigned`].
    pub fn is_undecided(&self, subdir: &str) -> bool {
        self.unassigned.undecided.iter().any(|u| u == subdir)
    }

    /// Everything staged in `[unassigned] undecided`, in file order (the order
    /// the user sees while editing).
    pub fn undecided(&self) -> &[String] {
        &self.unassigned.undecided
    }

    /// The subdirs labeled for `domain`, sorted.
    pub fn subdirs_of(&self, domain: &str) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .domains
            .get(domain)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        v.sort_unstable();
        v
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
    /// `domain`? See `docs/LABEL_DEPTH_DESIGN.md` — this is the ONE predicate
    /// behind both admission (what enters a db) and taint (whether a session's
    /// prose stays attestable), deliberately not forked (D5), so a depth mistake
    /// cannot be made in one place and not the other.
    ///
    /// Config entries are workspace-relative PATHS at arbitrary depth, matched
    /// **component-wise, never by string prefix** — which is what keeps the
    /// sibling trap (`learn/acme` vs `learn/acme-extra`) closed at every depth.
    /// One sentence a human can execute: **the longest path listed anywhere wins;
    /// if that longest listing is in `[unassigned]`, nobody gets it.**
    ///
    /// - **D1/D2** deepest match wins; equal-depth matches from several domains
    ///   share (the existing many-to-many semantics); `[unassigned]` beats a
    ///   domain at equal depth, so a contradiction resolves narrow.
    /// - **D3/D4** `[unassigned]` is the sole withdrawal, and withdrawal never
    ///   confers neutrality: withdrawn ⇒ not admitted here AND still taints
    ///   (taint is `!visible`, so a withdrawal that admitted nothing but also
    ///   stopped tainting would be a one-line ratchet off-switch).
    ///
    /// The workspace root itself (no component below it) belongs to no domain —
    /// its top-level files are not domain-attributable. `workspace` and `path`
    /// are expected canonicalized by the caller (both production callers do), so
    /// the `strip_prefix` is exact.
    pub fn includes(&self, domain: &str, workspace: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(workspace) else {
            return false;
        };
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rel.is_empty() {
            return false; // the workspace root itself
        }

        // D1/D2 — sweep every entry in the file, not just this domain's: a
        // DEEPER entry owned by someone else is exactly what narrows this
        // domain's claim, and that is the whole point of the rule.
        let mut best = 0usize; // depth of the deepest matching entry; 0 = none
        let mut withdrawn = false; // ...and a withdrawal sits at that depth
        let mut mine = false; // ...and `domain` has an entry at that depth
        for (entry, owner) in self.entries() {
            let Some(depth) = entry_match_depth(entry, &rel) else {
                continue;
            };
            let (is_mine, is_withdrawal) = (owner == Some(domain), owner.is_none());
            if depth > best {
                best = depth;
                withdrawn = is_withdrawal;
                mine = is_mine;
            } else if depth == best {
                // Ties SHARE (many-to-many), except that a withdrawal at the
                // same depth wins — resolved narrow, per D2.
                withdrawn |= is_withdrawal;
                mine |= is_mine;
            }
        }
        // Nothing claims it (best == 0), it is withdrawn, or the deepest claim
        // belongs to another domain. Each is a "no" for this db.
        if best == 0 || withdrawn || !mine {
            return false;
        }

        // D7 — the moment ANY deeper entry exists beneath the claiming entry,
        // that entry stops auto-sweeping its other child DIRECTORIES: each must
        // be listed explicitly. It keeps admitting its own directly-contained
        // files; only child directories require listing.
        //
        // THIS IS WHAT RETIRES THE DANGLING-ENTRY PROBLEM. With no implicit
        // sweep past a carve-out, nothing MISSING can ever cause admission, so
        // routing becomes a function of the config alone, independent of disk
        // state — which is why the whole git-operations surface (branch switch,
        // stash, bisect, worktree removal, a second machine) evaporates: disk
        // changes alter COVERAGE, never POLICY.
        //
        // It activates only when a deep entry is introduced, i.e. exactly when
        // the hazard is. A config with no carve-outs has no mixed parent and is
        // completely unaffected.
        if rel.len() > best && self.has_entry_below(&rel[..best]) {
            // The child immediately below the claiming entry. Were it listed it
            // would BE the deepest match, so reaching here means it is not.
            let child_is_dir = rel.len() > best + 1 || {
                // Nothing sits under it in this path, so ask the disk. A path
                // that no longer EXISTS reads as a file and stays admitted:
                // that is the shape of an edit to a since-deleted file directly
                // inside a labeled directory, and withholding those would be a
                // false-positive regression of exactly the kind measured at
                // 89.5% on a clean single-domain box.
                let mut child = workspace.to_path_buf();
                child.extend(&rel[..=best]);
                child.is_dir()
            };
            if child_is_dir {
                return false;
            }
        }

        // D6 — an unlabeled git repo root at ANY depth below the claiming entry
        // is quarantined: indexed by nobody, and it still taints. This is the
        // SAFETY DEFAULT, the answer for nested content nobody has said anything
        // about, and it is exactly the semantics an unlabeled TOP-LEVEL directory
        // already has — one rule at all depths, zero new config semantics.
        //
        // Repo-root-ness is a legitimate signal and not an inference because it
        // decides "ask the human", never "route it there": the repo's identity is
        // evidence SHOWN by the audit, never routing input.
        //
        // The deciding argument against the alternative (keep admitting unnamed
        // nested repos and merely report them) is the ignore-path, since ignoring
        // is most users' default: ignoring this rule is safe and annoyed;
        // ignoring sweep-and-report is comfortable and leaking.
        //
        // Clearing it uses the existing verbs at a nested path —
        // `nibdex label <path> --domain X` (including the parent's own domain,
        // which restores indexing) or `--acknowledge`.
        let mut probe = workspace.to_path_buf();
        probe.extend(&rel[..best]);
        for comp in &rel[best..] {
            probe.push(comp);
            if is_repo_root(&probe) {
                return false;
            }
        }
        true
    }

    /// Every config entry paired with the domain that lists it — `None` meaning
    /// it is an `[unassigned]` withdrawal.
    ///
    /// BOTH `[unassigned]` lists withdraw. They differ only in what the audit
    /// says about them (`acknowledged` is silent, `undecided` is reported); they
    /// have never granted anything, and at depth "not yet decided" must resolve
    /// the same safe way as "decided to exclude".
    fn entries(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.domains
            .iter()
            .flat_map(|(name, subs)| subs.iter().map(move |s| (s.as_str(), Some(name.as_str()))))
            .chain(
                self.unassigned
                    .acknowledged
                    .iter()
                    .chain(self.unassigned.undecided.iter())
                    .map(|s| (s.as_str(), None)),
            )
    }

    /// Does any entry — a label from any domain, or a withdrawal — sit STRICTLY
    /// below `prefix`? That is what makes `prefix` a "mixed parent" under D7.
    fn has_entry_below(&self, prefix: &[String]) -> bool {
        self.entries().any(|(entry, _)| {
            entry_components(entry).is_some_and(|parts| {
                parts.len() > prefix.len() && parts.iter().zip(prefix).all(|(p, r)| p == r)
            })
        })
    }
}

/// How deep `entry` claims `rel`, or `None` if it does not claim it at all.
///
/// The match is component-wise equality over the leading components of `rel` —
/// `Path::starts_with` semantics, spelled out over already-split components.
/// A string-prefix comparison here would re-open the sibling trap at every
/// depth, so this is the one place that must never see a `str::starts_with`.
///
/// `..` and absolute entries are rejected rather than resolved: an entry that
/// can walk out of its own subtree cannot be audited by eye, and the config's
/// whole job is being auditable by eye. (They are also rejected at parse time —
/// this is the belt to that suspenders, since `includes` is the security
/// boundary and must not depend on a validator having run.)
fn entry_match_depth(entry: &str, rel: &[String]) -> Option<usize> {
    let parts = entry_components(entry)?;
    if parts.len() > rel.len() {
        return None; // the entry reaches deeper than this path — not a claim on it
    }
    parts
        .iter()
        .zip(rel)
        .all(|(p, r)| p == r)
        .then_some(parts.len())
}

/// Is `dir` the root of a git repository?
///
/// Tests for a `.git` ENTRY, not a `.git` directory: a linked worktree and a
/// submodule checkout both carry `.git` as a FILE holding a `gitdir:` pointer.
/// Missing that shape would fail OPEN — the foreign tree would be swept into its
/// enclosing label — so the check is deliberately the weaker "does `.git` exist"
/// rather than anything that inspects what it is.
fn is_repo_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Split a config entry into path components, or `None` if it is unusable.
/// Accepts either separator so a Windows-authored config still reads; drops
/// empty and `.` segments (a trailing slash or a `./` prefix is a typo, not a
/// different path); rejects `..`, absolute entries, and the empty entry.
fn entry_components(entry: &str) -> Option<Vec<&str>> {
    if entry.starts_with('/') || entry.starts_with('\\') {
        return None;
    }
    let parts: Vec<&str> = entry
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    (!parts.is_empty() && !parts.contains(&"..")).then_some(parts)
}

/// Lexically resolve `.` and `..` segments WITHOUT touching the filesystem.
///
/// `canonicalize()` is the primary normalizer (it resolves symlinks and yields
/// the true path), but it FAILS on a deleted or never-created target — exactly
/// the shape a stale transcript edge can carry (`Write` to a since-removed file,
/// an edit through a `..` that no longer exists). When canonicalize fails we fall
/// back to this lexical pass so the components `includes` matches against are the
/// intended ones, not a literal `..`/`.` (GEAR2_DESIGN §3 finding B3).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `[memory]` is OPTIONAL and fail-narrow: absent means no domain claims the
    /// memory dir, which is byte-for-byte the prior domain-mode behavior.
    #[test]
    fn memory_table_absent_means_no_claim() {
        let cfg: DomainConfig = toml::from_str(
            "[domains]\npersonal = [\"my-app\"]\n",
        )
        .unwrap();
        assert!(!cfg.claims_memory("personal"));
        assert!(!cfg.claims_memory("nope"));
    }

    /// A declared claim is honored, and ONLY for the claiming domain — a second
    /// domain in the same file must not inherit it.
    #[test]
    fn memory_claim_is_per_domain() {
        let cfg: DomainConfig = toml::from_str(
            "[domains]\npersonal = [\"my-app\"]\nclient-a = [\"acme\"]\n\n[memory]\npersonal = \"*\"\n",
        )
        .unwrap();
        assert!(cfg.claims_memory("personal"));
        assert!(!cfg.claims_memory("client-a"));
    }

    /// A typo'd domain name in `[memory]` must fail LOUDLY at load. Silently
    /// resolving to "no claim" would drop a corpus the user believes they
    /// enabled — the same class of quiet mis-routing the module forbids.
    #[test]
    fn memory_claim_for_undeclared_domain_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n\n[memory]\nprsonal = \"*\"\n",
        )
        .unwrap();
        let err = DomainConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("prsonal"), "{err}");
        assert!(err.contains("not declared"), "{err}");
    }

    /// Two domains claiming the one workspace-global memory dir is a hard error:
    /// "claim" reads as exclusive, and both dbs would get the same content.
    #[test]
    fn memory_claimed_by_two_domains_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"a\"]\nclient-a = [\"b\"]\n\n\
             [memory]\npersonal = \"*\"\nclient-a = \"*\"\n",
        )
        .unwrap();
        let err = DomainConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("claimed by 2 domains"), "{err}");
    }

    /// An unrecognized claim value fails loudly too, rather than being treated as
    /// a claim (over-permissive) or ignored (silently dropping the corpus).
    #[test]
    fn memory_claim_with_unknown_value_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n\n[memory]\npersonal = \"yes\"\n",
        )
        .unwrap();
        let err = DomainConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not understood"), "{err}");
    }

    /// `undecided` is an annotation, nothing more: it must be readable back and
    /// must NOT make a subdir visible to any domain, or the staging step would
    /// quietly widen a domain's reach — the exact failure the audit's
    /// "evidence, never a suggestion" rule exists to prevent.
    #[test]
    fn undecided_is_recorded_and_grants_no_visibility() {
        let cfg: DomainConfig = toml::from_str(
            "[domains]\npersonal = [\"my-app\"]\n\n[unassigned]\nundecided = [\"mystery\"]\n",
        )
        .unwrap();
        assert!(cfg.is_undecided("mystery"));
        assert!(!cfg.is_undecided("my-app"));
        assert_eq!(cfg.undecided(), ["mystery".to_string()]);
        let ws = Path::new("/ws");
        assert!(
            !cfg.includes("personal", ws, &PathBuf::from("/ws/mystery/x.rs")),
            "staging as undecided must never grant domain visibility"
        );
    }

    /// Absent `undecided` is the norm (every config written before this key
    /// existed) and must default to empty, not fail to parse.
    #[test]
    fn undecided_absent_defaults_empty() {
        let cfg: DomainConfig = toml::from_str(
            "[domains]\npersonal = [\"my-app\"]\n\n[unassigned]\nacknowledged = [\"junk\"]\n",
        )
        .unwrap();
        assert!(cfg.undecided().is_empty());
        assert!(cfg.is_acknowledged("junk"));
    }

    /// The copy-instead-of-move edit: the user meant to move a name out of
    /// `undecided` into a domain and left the original behind. The file would
    /// then read as "undecided" while the directory is in fact indexed.
    #[test]
    fn undecided_and_labeled_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n\n[unassigned]\nundecided = [\"my-app\"]\n",
        )
        .unwrap();
        let err = DomainConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("undecided"), "{err}");
        assert!(err.contains("also labeled"), "{err}");
    }

    /// A mistyped key in a hand-edited boundary file must not parse to silence.
    #[test]
    fn an_unknown_key_in_the_unassigned_table_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n\n[unassigned]\nundecied = [\"typo\"]\n",
        )
        .unwrap();
        // The serde detail rides in the error CHAIN under the "parsing <path>"
        // context, which is what the CLI prints as `Caused by:`.
        let err = format!("{:?}", DomainConfig::load(dir.path()).unwrap_err());
        assert!(err.contains("undecied"), "the typo is named: {err}");
        assert!(err.contains("unknown field"), "{err}");
    }

    /// Decided-to-exclude and not-yet-decided are opposite claims; a subdir in
    /// both lists has no readable state.
    #[test]
    fn undecided_and_acknowledged_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[domains]\npersonal = [\"my-app\"]\n\n\
             [unassigned]\nacknowledged = [\"junk\"]\nundecided = [\"junk\"]\n",
        )
        .unwrap();
        let err = DomainConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("BOTH acknowledged and undecided"), "{err}");
    }

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
