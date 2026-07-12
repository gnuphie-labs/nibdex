# Building nibdex on Windows

> **Status: code-ready, hardware-unverified.** The cross-platform code gaps are
> fixed **on `main`** and verified building + 212 tests on macOS, but nibdex has
> **not yet been built or run on real Windows**. This doc is the quick-start for
> that first build pass.

## Prerequisites

Install these *before* the first build — the one native dependency is libgit2,
and building it from source needs a C/C++ toolchain. (nibdex no longer shells
out to a `git` binary at runtime — it uses libgit2 directly — so a runtime `git`
install is **not** required; the tools below are all build-time.)

1. **Rust** via [rustup](https://rustup.rs/) — the default `x86_64-pc-windows-msvc`
   toolchain is fine.
2. **MSVC C++ build tools** — Visual Studio Build Tools with the
   **"Desktop development with C++"** workload. Provides the linker and C
   compiler `git2`/libgit2 needs.
3. **CMake** — required to build the vendored libgit2 from source (see the `git2`
   note below). `winget install Kitware.CMake` or the installer from cmake.org.
4. **SQLite** is not required as a separate install — `sqlx` is used in
   offline/bundled mode and the binary is self-contained once built.

## Build

```powershell
git fetch
cargo build --release
cargo test
```

### `git2` / libgit2 — vendored on Windows (already configured)

`Cargo.toml` links a **system** libgit2 off-Windows (unchanged macOS/Linux
behavior) and **vendors** libgit2 on the Windows target so it builds from source
— which is why CMake + MSVC are prerequisites above. This is already wired via
per-target dependency tables (mirrors the `notify` split):

```toml
[target.'cfg(not(target_os = "windows"))'.dependencies]
git2 = { version = "0.21", default-features = false }

[target.'cfg(target_os = "windows")'.dependencies]
git2 = { version = "0.21", default-features = false, features = ["vendored-libgit2"] }
```

So end users on Windows need no separate libgit2 install. (If you'd rather vendor
everywhere for build-consistency, that's a deliberate call — move `git2` back to
`[dependencies]` with `vendored-libgit2` on — not the current default.)

## Smoke test (in priority order)

`index` and `mcp` (stdio) are the must-work paths; `serve`/`watch` is least
tested.

```powershell
# 1. Index a workspace
.\target\release\nibdex.exe index C:\path\to\your\workspace

# 2. Generate MCP client config (resolves the current binary path)
.\target\release\nibdex.exe print-mcp-config --transport stdio

# 3. Health / sanity
.\target\release\nibdex.exe check
```

## Known Windows gotchas to check

These are the open `[ ]` items from the G9 checklist — expect them, verify each:

- **Memory dir resolves wrong.** `default_memory_dir()` (`src/indexer.rs`) builds
  the Claude memory path with a `/ _ .` → `-` encoding derived from Claude Code's
  *Unix* layout. On Windows, `canonicalize()` returns an extended-length
  `\\?\C:\...` path with a drive letter and backslashes the encoding does **not**
  collapse — so the computed path is probably wrong and memory rows come back
  **0** (a silent failure, not an error). Compare the computed path to where
  Claude Code actually writes `%USERPROFILE%\.claude\projects\...` on this
  machine, then implement the real Windows encoding. There's an in-code `NOTE`
  at the call site.
- **`serve` / `watch` watcher** — confirm the `notify` ReadDirectoryChangesW
  backend actually delivers events for CLAUDE.md / memory / git-ref changes.
  Only the macOS FSEvents path has been dogfooded.
- **Path display** — watch for `\\?\` extended-length prefixes or drive-relative
  paths leaking into user-facing output (`check()`, logs).
- **Case-insensitive filesystem** — path comparisons/dedup that assume
  case-sensitivity may behave differently.

## What's already handled

These are on `main` and verified building + 212 tests on macOS:

- **libgit2, not the `git` CLI.** The source-index file enumeration + provenance
  walk go through `git2` (`src/source_index.rs`): index entries for tracked
  files, a `revwalk` + per-commit tree-diff for provenance. No `git` binary is
  required at runtime. (One spike path, `src/diff_index.rs`, still shells out —
  it is a dev-only spike, not a shipping surface.)
- Home dir: `%USERPROFILE%` on Windows, `$HOME` elsewhere (`src/indexer.rs`).
- `notify` / `notify-debouncer-mini` backend features split by target — Windows
  gets the bare crate (ReadDirectoryChangesW auto-selected by `target_os`).
- `git2` vendored-libgit2 on the Windows target (above).
- Shutdown signal handler already has a `#[cfg(not(unix))]` `ctrl_c()` arm
  (`src/main.rs`).
