# Contributing to nibdex

Thanks for your interest in contributing. This guide covers how to propose a
change and how the Contributor License Agreement (CLA) sign-off works. For the
broader picture — how the project is maintained and how decisions are made — see
[GOVERNANCE.md](GOVERNANCE.md); for licensing intent see
[LICENSING.md](LICENSING.md).

## Before you start: ideas vs. contributions

- **Ideas and suggestions are welcome with no CLA.** Opening an issue to suggest a
  feature, report a bug, or discuss a design is not a "contribution" under the CLA
  — a bare idea is not a copyrightable work. No sign-off is needed to talk.
- **Code, documentation, and configuration are contributions** and require CLA
  agreement (below) before they can be merged.

## The CLA sign-off (one time)

nibdex uses a simple, in-repo ledger — no third-party CLA service.

1. Read the [Contributor License Agreement](CLA.md). In short: you keep your
   copyright; you grant the project the rights it needs to distribute your work
   under MIT and to support the open-core model described in
   [LICENSING.md](LICENSING.md).
2. In the **same pull request as your first contribution**, add one line to the
   Signatures section of [CONTRIBUTORS.md](CONTRIBUTORS.md) in this exact format:

   ```
   - Full Name (@github-handle) — YYYY-MM-DD — I have read and agree to the nibdex CLA (CLA.md).
   ```

   Commit it under your own name and email — that commit is your signature.
3. An automated check (`.github/workflows/cla.yml`) confirms your `@github-handle`
   appears in `CONTRIBUTORS.md`. Once it does, the check passes for this and all
   future pull requests; you only sign once.

### If you are employed

If you are employed, the CLA's representations (CLA §5.2) require you to confirm
you are entitled to grant its licenses — i.e. that any employer with rights to IP
you create has waived them for your contribution, or that you have permission to
contribute on the employer's behalf. **Please make sure that is true before you
sign.** Note: *using* nibdex at work (even commercially) needs no CLA and gives
your employer no ownership of the project — only *contributing* does.

## Opening a pull request

1. Open an issue first for anything non-trivial, so direction can be agreed before
   you invest effort.
2. Keep changes focused; one logical change per pull request.
3. Make sure the suite passes locally: `cargo test`. Keep `cargo clippy` clean.
4. The pull-request template includes a CLA checkbox — confirm it.
5. The maintainer reviews for fit, correctness, and scope. There is no obligation
   to accept any contribution.

## Reporting security issues

Report suspected vulnerabilities privately to the maintainer rather than in a
public issue, and allow time for a fix before public disclosure.

---

*The CLA and license terms are provided as-is and are not legal advice; the
binding documents are [LICENSE](LICENSE) and [CLA.md](CLA.md).*
