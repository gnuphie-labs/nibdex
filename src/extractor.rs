// SPDX-License-Identifier: MIT

//! Per-corpus extractors.
//!
//! Spec: DESIGN §5.3. Brittleness is intentional MVP scope — `check()` surfaces
//! parse stats so regressions are visible.

pub mod design_doc;
pub mod git_commits;
pub mod memory;
pub mod session_history;
