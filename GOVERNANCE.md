# nibdex Governance

This document describes how nibdex is maintained, how decisions are made, and how
contributions are accepted. It complements [`LICENSING.md`](LICENSING.md) (the
licensing intent) and [`CLA.md`](CLA.md) (the contributor agreement).

## Project status and stewardship

nibdex is an **independently maintained open-source project**. It is developed on
its maintainer's own time and equipment, under the maintainer's own tooling and
accounts — not on behalf of, at the direction of, or with compensation from any
employer. The project is MIT-licensed (see [`LICENSE`](LICENSE)) and intends to
stay that way (see [`LICENSING.md`](LICENSING.md)).

**Maintainer:** Richard Dunn.

## Decision-making

Final authority over what merges, what ships, and what the roadmap is rests with
the maintainer. The reasoning behind scope decisions is documented in
[`docs/DESIGN.md`](docs/DESIGN.md) and surfaced in the project's GitHub issues and
pull requests; the conservative-versioning bar is in [`docs/VERSIONING.md`](docs/VERSIONING.md),
and what nibdex deliberately does *not* do is fenced in
[`docs/LIMITATIONS.md`](docs/LIMITATIONS.md). Feature scope expands only against
named, falsifiable bars rather than on speculation — see the tracker's §9.1
discipline.

## Using nibdex vs. contributing to nibdex

These are deliberately different, and the distinction protects everyone.

- **Using nibdex** — running it (including at work, including commercially) is
  governed solely by the [MIT License](LICENSE). Using an MIT-licensed tool, even
  one that benefits your employer, **does not give your employer any ownership of
  nibdex.** No agreement beyond MIT is required to use it.
- **Contributing to nibdex** — submitting code, documentation, or configuration
  requires agreement to the [Contributor License Agreement](CLA.md). The CLA lets
  the project keep distributing your contribution under MIT and preserves the
  open-core path described in [`LICENSING.md`](LICENSING.md), while **you retain
  copyright in your contribution.**

### Contributors who are employed

If you are employed and you contribute, the CLA's representations (CLA §5.2)
require you to confirm that you are entitled to grant the licenses in the CLA —
that is, that any employer with rights to intellectual property you create has
waived them for your contribution, or that you have permission to contribute on
that employer's behalf. **Please make sure that is true before you submit.** This
is what keeps a contribution from later becoming an ownership dispute between the
project and a contributor's employer.

## How contributions are accepted

1. Open an issue or pull request describing the change.
2. Agree to the [CLA](CLA.md) by adding your line to the
   [`CONTRIBUTORS.md`](CONTRIBUTORS.md) ledger in your first pull request — an
   in-repo sign-off enforced by an automated check
   ([`.github/workflows/cla.yml`](.github/workflows/cla.yml)), with no third-party
   CLA service. [`CONTRIBUTING.md`](CONTRIBUTING.md) has the step-by-step.
3. The maintainer reviews for fit, correctness, and scope. There is no obligation
   to accept any contribution.

**Suggestions and ideas** — an issue with no attached code or text — are welcome
and are not themselves "contributions" under the CLA: a bare idea is not a
copyrightable work of authorship. The CLA applies once a suggestion arrives as
authored material (a patch, a snippet, or written documentation).

## Security

Report suspected vulnerabilities privately rather than in a public issue, and
allow time for a fix before public disclosure. The full threat model, redaction
stance, and reporting channel are in [`SECURITY.md`](SECURITY.md).

## Questions

Open a GitHub issue on [`gnuphie-labs/nibdex`](https://github.com/gnuphie-labs/nibdex) or
contact the maintainer.

---

*This document describes project practice and may evolve. It is provided as-is
and is not legal advice; the binding terms are always [`LICENSE`](LICENSE) and
[`CLA.md`](CLA.md).*
