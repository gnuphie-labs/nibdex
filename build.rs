// SPDX-License-Identifier: MIT

//! Bake compile-time build provenance into the binary so `check()`,
//! `/healthz`, and `nibdex version` can report exactly which build is running
//! — the un-foolable "did my deploy take" signal (git sha + `-dirty` flag) the
//! config-driven `calibration_model_version` can't give on its own.
//!
//! All three values degrade to "unknown" when built outside a git checkout
//! (e.g. from a published tarball), never failing the build.

use std::process::Command;

fn main() {
    // `.git/logs/HEAD` is appended on every commit / checkout / pull / reset,
    // so watching it reliably re-runs this script after HEAD moves (watching
    // `.git/HEAD` alone misses new commits — it's a symref that only changes
    // on branch switch). No-ops gracefully when `.git` is absent.
    println!("cargo:rerun-if-changed=.git/logs/HEAD");

    emit("NIBDEX_GIT_SHA", git(&["rev-parse", "--short", "HEAD"]));
    emit(
        "NIBDEX_GIT_DESCRIBE",
        git(&["describe", "--always", "--dirty", "--tags"]),
    );
    emit("NIBDEX_COMMIT_TIME", git(&["show", "-s", "--format=%cI", "HEAD"]));
}

fn emit(key: &str, value: Option<String>) {
    println!(
        "cargo:rustc-env={key}={}",
        value.unwrap_or_else(|| "unknown".to_string())
    );
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
