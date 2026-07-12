# Licensing

nibdex is **open source under the [MIT License](LICENSE)**. That is the whole
license for everything in this repository. You can use, modify, distribute, and
build on nibdex — including commercially — under MIT's terms.

This document explains the licensing *intent* so contributors and users know
what to expect. It is not a separate license and does not modify the MIT terms
in `LICENSE`.

## The core is MIT, and stays MIT

The nibdex core — everything in this repository — is and will remain MIT
licensed. The project is deliberately permissive:

- **Reach over restriction.** nibdex exists to help budget-constrained
  developers use AI clients effectively against their own workspace. A
  permissive license maximizes who can adopt it, which is the point.
- **Single-config-line adoption is the moat.** The value depends on every
  MCP-using developer being able to add nibdex trivially. Copyleft friction
  would work against that.
- **Local-first by design.** nibdex runs locally (loopback-only HTTP, local
  SQLite, no telemetry, no hosted service). There is no network/SaaS surface
  that a copyleft license would be meant to protect, so MIT costs nothing here.

We are not dual-licensing the core and are not planning to relicense it away
from MIT.

## How nibdex may be funded ("open-core")

Future commercial offerings, if any, will be **separate proprietary add-on
features layered on top of the MIT core** — not a relicensing of this
repository, and not a paid "commercial license" for the same code. The open
core remains fully functional and MIT on its own.

Concretely, that means:

- This repository stays MIT and self-contained.
- Any proprietary add-ons live in their own separate repositories under their
  own terms.
- Nothing in a paid add-on is required to use the open core.

## Why we ask contributors to sign a CLA

Keeping the open-core path viable requires the project to hold clear rights to
all contributed code. The [Contributor License Agreement](CLA.md) ensures that:

- the project can keep distributing your contribution under MIT, and
- the project retains the flexibility to build the separate proprietary add-ons
  described above without ambiguity about who owns what.

The CLA does **not** take your copyright — you keep ownership of your
contributions and grant the project a license to use them. See [CLA.md](CLA.md)
for the exact terms.

## Dependencies

nibdex's dependencies are permissively licensed (MIT / Apache-2.0 / BSD /
Unlicense). One note worth recording: `git2` binds **libgit2**, which is
GPL-2.0 **with a linking exception** — that exception means it imposes no
copyleft obligation on nibdex's MIT distribution.

## Questions

Open a GitHub issue on [`gnuphie-labs/nibdex`](https://github.com/gnuphie-labs/nibdex) or
contact the maintainer.

---

*Maintainer: Richard Dunn. This document describes intent and may evolve; the
binding license is always [`LICENSE`](LICENSE).*
