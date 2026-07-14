# Contributing to Forklift

Thanks for wanting to contribute! This file covers the **legal side** of
contributing (licenses and the CLA). For the technical side — architecture, how to
build and test, conventions, how to add a command — see the
[contributor guide](docs/guide/contributing.md).

## Licensing of contributions

Forklift is [open-core](LICENSING.md). Which terms apply to your contribution
depends on what it touches:

### Client, core, and docs — no CLA needed

Contributions to `crates/forklift-core`, `crates/forklift`, and the documentation
are accepted under the project's dual **MIT OR Apache-2.0** terms, inbound =
outbound: unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions. Just open a
pull request.

### Server heads — CLA required

Contributions touching the server heads — `crates/forklift-server` and
`crates/forklift-aws-lambda` — require a signed
[Contributor License Agreement](CLA.md).

**Why:** the server heads are licensed under the
[Functional Source License](LICENSE-FSL) because managed hosting is the intended
business that funds the project. For that to stay possible, the maintainer needs
the right to license server-head code commercially and to relicense it (each FSL
release also converts to Apache-2.0 after two years). The CLA grants those rights
while **you keep the copyright** to your work — it is a license, not an
assignment, and it never restricts what you can do with your own code elsewhere.

**How:** it's automatic. When you open a pull request that touches the server
heads, a bot comments with a sentence to reply with; replying registers your
acceptance, and the bot won't prompt you again for the same CLA version. One
legal nuance, deliberate: the agreement works contribution by contribution —
each pull request you submit is a fresh grant for that specific contribution,
because a blanket license of your unwritten future work would be void under
Hungarian copyright law (which governs the CLA). Registering once is a
convenience; it never claims your future works in advance.

A pull request that touches both client and server code just needs the one CLA
signature.

## Before you build something big

For anything larger than a bug fix — especially on the server heads or anything
that adds a new command, format, or protocol surface — please **open an issue
first** to discuss it. Forklift has a deliberate
[design document and roadmap](docs/DESIGN.html), and features are sequenced
against it; an early conversation avoids building something that can't be merged
as-is.

## The mechanics

- Read the [contributor guide](docs/guide/contributing.md) for architecture,
  build/test instructions, and the conventions the codebase holds to (they are
  load-bearing and enforced by tests).
- Run `pult check` (build + test + clippy — what CI runs) before opening a PR.
- Commit subjects follow `Area - Topic: short description`.
- Documentation is part of the change, not a follow-up — see
  [keeping docs in sync](docs/guide/contributing.md#8-keeping-docs-in-sync).
