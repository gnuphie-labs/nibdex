// SPDX-License-Identifier: MIT

//! Compile-time build provenance, baked in by `build.rs`.
//!
//! Lets `check()`, `/healthz`, and `nibdex version` report exactly which binary
//! is answering — the un-foolable "did my deploy take" signal. The
//! `calibration_model_version` tells you which *config* a daemon loaded, but
//! not which *binary*: a stale binary can load a fresh `calibration.toml`, and
//! a freshly-built binary running uncommitted code is invisible to the config
//! version. `git_describe`'s `-dirty` suffix surfaces exactly that.

use schemars::JsonSchema;
use serde::Serialize;

/// Build identity of the running binary. All fields are compile-time constants;
/// values are `"unknown"` when built outside a git checkout (see `build.rs`).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct BuildInfo {
    /// Crate version from `Cargo.toml` (e.g. `"0.1.0-rc.0"`).
    pub crate_version: String,
    /// Short git SHA at build time.
    pub git_sha: String,
    /// `git describe --always --dirty --tags`. A `-dirty` suffix means the
    /// build came from an uncommitted working tree.
    pub git_describe: String,
    /// RFC3339 commit timestamp of the built HEAD.
    pub commit_time: String,
}

/// Snapshot this binary's build identity.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        git_sha: env!("NIBDEX_GIT_SHA").to_string(),
        git_describe: env!("NIBDEX_GIT_DESCRIBE").to_string(),
        commit_time: env!("NIBDEX_COMMIT_TIME").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate version is always real (from Cargo); the git fields are either
    /// real or the graceful `"unknown"` sentinel — never empty.
    #[test]
    fn build_info_is_populated() {
        let b = build_info();
        assert_eq!(b.crate_version, env!("CARGO_PKG_VERSION"));
        assert!(!b.git_sha.is_empty());
        assert!(!b.git_describe.is_empty());
        assert!(!b.commit_time.is_empty());
    }

    /// Round-trips through serde as a flat object with the four named keys —
    /// the shape `check()` / `nibdex version --json` emit.
    #[test]
    fn build_info_serializes_flat() {
        let v = serde_json::to_value(build_info()).unwrap();
        for k in ["crate_version", "git_sha", "git_describe", "commit_time"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
    }
}
