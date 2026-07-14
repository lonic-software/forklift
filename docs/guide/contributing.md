# Contributing to forklift

This guide is for people working on forklift itself: the architecture, how to
build and test, the conventions the codebase holds to, and how to add a command
or a format without breaking the invariants.

The **design rationale and roadmap** live in [`../DESIGN.html`](../DESIGN.html) —
read it before making a design decision; it is the source of truth and carries a
dated changelog. The **byte-level format specs** live in
[`../format/`](../format/) and are authoritative for wire/disk layout.

Contents:
1. [Architecture](#1-architecture)
2. [Building and testing](#2-building-and-testing)
3. [Conventions](#3-conventions)
4. [Where things live](#4-where-things-live)
5. [Adding a command](#5-adding-a-command)
6. [Formats and versioning](#6-formats-and-versioning)
7. [Commit and PR conventions](#7-commit-and-pr-conventions)
8. [Keeping docs in sync](#8-keeping-docs-in-sync)

---

## 1. Architecture

Forklift is one Cargo workspace: a **core** library plus thin **heads** that adapt
it to execution environments.

```
crates/
  forklift-core        all base logic — objects, inventory, refs, diff, merge,
                       trust, the remote protocol client. NEVER prints, never
                       exits, never assumes a terminal: it returns data and errors.
  forklift             the client CLI head (on clap). Owns all presentation.
  forklift-server      the self-hostable server head (axum) — serve/prepare/bundle/gc.
  forklift-aws-lambda  the serverless head (Lambda). The protocol logic (`Head<O, R>`)
                       over two traits — `ObjectStore` (S3) and `RefStore` (DynamoDB) —
                       with in-memory fakes and the full protocol suite running in CI
                       without AWS; the SDK implementations are the next milestone.
```

The one rule that keeps the split clean: **core owns logic, heads own
presentation.** Core functions return `Result<T, String>` (data and error
messages); the head decides how to render — prose, `--json`, an HTTP response, a
Lambda reply. Anywhere core needs to interact with a terminal (e.g. prompting for
a key passphrase), it exposes a *provider seam* the head fills, rather than
prompting itself (`sign_utils::set_passphrase_provider` is the pattern).

Why a workspace and not published crates: members share atomic commits, one
lockfile, and one CI run. A private-registry dependency (the retired
`rust-cli-core`) cost a publish cycle per change — the lesson that shaped this.

---

## 2. Building and testing

```sh
cargo build --workspace          # build everything (heads locate each other in target/)
cargo test  --workspace          # the whole suite (core unit + CLI/remote integration)
cargo clippy --workspace --all-targets
```

Notes:
- The remote integration tests (`crates/forklift/tests/remote.rs`) spawn a real
  `forklift-server` process next to the `forklift` binary, so run tests via a
  **workspace build** (plain `cargo test`), not a single-crate build. The crash
  (`crates/forklift/tests/crash_consistency.rs`) and determinism
  (`crates/forklift/tests/determinism.rs`) suites likewise drive the real binary.
- Integration tests isolate state with `FORKLIFT_GLOBAL_CONFIG` and
  `FORKLIFT_KEYS_DIR` env vars pointed at a scratch directory, so they never
  touch a developer's real config or keys. Use the same pattern for new tests
  (`TestWarehouse` / `TestArea` helpers already do this).
- The hardening test spine is worth knowing when touching the object store,
  the parsers, or the parallel walks: `crates/forklift-core/tests/fuzz_formats.rs`
  fuzzes every parse entry point (must never panic) and checks round-trip
  fidelity; `crash_consistency.rs` SIGKILLs `stack` mid-write to prove the store
  stays consistent across power loss; `determinism.rs` pins deterministic tree hashes,
  byte-reproducible repacks, and the warehouse-lock refusal. A parser or
  format change should keep the fuzzer green; if you add a length-prefixed field,
  bound it with a `checked_add` (never `start + length` before the bounds check).
- To install a working binary (e.g. to dogfood): `cargo install --path
  crates/forklift --force` (lands in `~/.cargo/bin`).

### Maintainer commands (pult)

The repo ships a [pult](https://github.com/lonic-software/pult) manifest (`pult.yaml`)
so the operational commands are one launcher away; the real logic lives in `bin/`. Run
`pult` for the menu, or a command directly:

| Command | What it does |
| --- | --- |
| `pult check` | `bin/check` — build + test + clippy (what CI runs; green here = green there) |
| `pult test` | just the test suite (fast loop) |
| `pult release` | `bin/release` — preflight, bump the workspace version, tag `v<x.y.z>`, push (the tag triggers `.github/workflows/release.yml`) |
| `pult install` | build & install the CLI + server from this checkout |
| `pult serve` | `bin/serve` — a throwaway local `forklift-server` over `.dev/server` for testing lift/lower/franchise |
| `pult design` | open `docs/DESIGN.html` (the source of truth) in a browser |
| `pult gen-docs` | `bin/gen-docs` — regenerate the derived docs (error codes, per-command JSON schemas) |

Cutting a release: `pult release` picks the next patch/minor/major from the latest `v*`
tag (or run `bin/release <x.y.z> --dry-run` to preflight without touching anything). It
refuses unless the working tree is clean and `main` is in sync with `origin/main`.

---

## 3. Conventions

These are load-bearing — most of them are enforced by tests and are the reason
forklift behaves predictably. From `DESIGN.html` §2:

- **Bounded memory.** The inventory (staging) is sharded per directory; whole-repo
  operations *stream* shards rather than accumulating them. No operation should
  need RAM proportional to repository size.
- **Parallelism first.** Independent work (per-directory scans, per-file hashing,
  compression) fans out over the shared `TaskExecutor` worker pool (sized to core
  count). Reuse it; never spin up a second pool.
- **One runtime: tokio.** Networking is async; the file walker shares the runtime,
  so filesystem calls that would block go through `spawn_blocking`/`tokio::fs`.
- **Versioned formats, length-prefixed strings.** Every payload leads with a VLQ
  version code (adding a version is a match arm). Every user-controlled string
  (file names, descriptions, ids) is length-prefixed, never terminator-delimited
  — file names may contain any byte. Hashes may be newline-terminated (ASCII hex
  can't contain one).
- **Canonical paths.** All user paths normalize exactly once at the command
  boundary into `WarehousePath`: repo-relative, `/`-separated on every platform,
  no `.`/`..`, root = empty key. The storage layer never sees anything else.
- **Atomicity.** Object writes and every state file (refs, config, signatures) are
  temp-file + rename. Mutating commands hold the **warehouse lock**
  (`.forklift/lock`, `create_new`, held for the whole command) so a reader can
  never see a half-updated staging area. Read-only commands don't take it. Add a
  new mutating command to the lock list in `cli.rs`
  (`requires_warehouse_lock`).
- **The remote is untrusted storage + thin verifiers.** Anything a client uploads
  is unverified until the server checks it (hash, signature, privileges). Anything
  a client downloads is verifiable offline (content addressing + signatures).
- **Timestamps never decide security.** Validity questions (trust boundaries,
  revocation) are decided by exact ancestry in the parcel graph, never by time — a
  forged or shifted clock must change nothing.

---

## 4. Where things live

Core (`crates/forklift-core/src/`):

| Area | Modules |
|------|---------|
| Object store | `util/object_utils`, `util/file_utils`, `builder/object/…`, `parser/object/…` |
| Inventory (staging) | `util/inventory_utils`, `util/stocktake_utils`, `model/inventory` |
| Refs & history | `util/pallet_utils`, `model/parcel` |
| Diff & merge | `util/diff`, `util/lcs`, `util/merge_utils` |
| Trust / office / keys | `util/office_utils`, `util/sign_utils`, `util/audit_utils` |
| Remote protocol | `util/remote_utils`, `util/bundle_utils`, `model/remote` |
| Hooks (provider seam) | `util/hook_utils`, `model/hooks` |
| Config / identity | `util/config_utils`, `model/operator` |
| Parallelism | `model/task` (the `TaskExecutor`) |

CLI (`crates/forklift/src/`): `cli.rs` (the clap command surface — the single
source of truth for commands, flags, aliases, help), `main.rs` (dispatch),
`commands/<name>.rs` (one handler per command), `output.rs` (the `--json`
envelope + error/exit-code taxonomy), `passphrase.rs` (the terminal side of key
unlocking).

Docs: `format/` (byte specs, authoritative), `DESIGN.html` (design + roadmap +
changelog), `SERVER.md` (server ops), `MACHINE_INTERFACE.md` (`--json`/MCP),
`guide/` (these user-facing guides).

---

## 5. Adding a command

1. **Define it in `cli.rs`** — a variant on the `Command` enum with its args,
   flags, `visible_alias`, and `long_about`. This is the single source of truth
   for the surface; the generated help comes from here.
2. If it needs a warehouse, add it to `requires_warehouse`; if it mutates state,
   add it to `requires_warehouse_lock` (both in `cli.rs`).
3. **Write the handler** in `commands/<name>.rs`. Keep logic in `forklift-core`;
   the handler orchestrates and presents.
4. **Present through `output`** so `--json` works for free: build a `Serialize`
   result type implementing `CommandOutput`, and `output::emit("<name>", &result)`
   — human mode calls `render_human()`, JSON mode wraps it in the envelope. For a
   one-line outcome use `output::message`; for silenced-under-`--json` progress
   lines use the `human!` macro. **Nothing else may print to stdout**, or `--json`
   stops being a single valid document.
5. **Dispatch it** in `main.rs`.
6. **Test it** in `tests/cli.rs` (or `tests/remote.rs` for remote behavior). Assert
   both the human output and, where it matters, the `--json` envelope.
7. **Document it** in [`cli.md`](cli.md) (and the reference table).

---

## 6. Formats and versioning

Object, tree, parcel, signature, pallet-ref, bundle, remote-protocol and
tracked-metadata formats each have a spec in `format/`. When you change one:

- Bump the version (a new VLQ code / a new date on the protocol) and add a
  match arm — never repurpose an existing version.
- Update the spec in `format/` so it stays byte-accurate.
- Additive protocol endpoints (old servers `404`, clients fall back) don't need a
  version bump; document the fallback.
- Pre-release, forklift does **not** carry legacy-tolerance code for its own
  older on-disk formats (there are no external users yet). This is a deliberate
  decision — see the `DESIGN.html` §8.14 note — so a format change may require
  rebuilding a warehouse. Don't add legacy shims without a reason recorded in the
  design doc.

---

## 7. Commit and PR conventions

Commit subjects follow `Area - Topic: short description`, e.g.
`Trust - Revocation: reason + distrust boundary`. The body explains the *why* and
the invariants touched, not just the *what*. When a feature lands, flip its status
chip in `DESIGN.html` §5 and add a dated changelog row in §9 — the doc is
maintained alongside the code.

---

## 8. Keeping docs in sync

Documentation is part of the change, not a follow-up. When a change alters:

- **a command's flags, name, or behavior** → update [`cli.md`](cli.md) (and its
  reference table);
- **trust / office / key behavior** → update [`trust-and-identity.md`](trust-and-identity.md);
- **server flags / ops** → update [`../SERVER.md`](../SERVER.md);
- **the `--json` envelope, error codes, or MCP tools** → update
  [`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md);
- **a byte format** → update the spec in [`../format/`](../format/);
- **a design decision** → record it in [`../DESIGN.html`](../DESIGN.html) with a
  dated changelog row.

The guides intentionally *reference* rather than duplicate `SERVER.md`,
`MACHINE_INTERFACE.md`, `DESIGN.html`, and `format/`, so there's a single place to
update for each concern. Keep it that way — duplication is what drifts.

**Generated docs never need this by hand.** `docs/generated/errors.md` (the error/exit-code
table) and `docs/generated/json-schemas.md` (every command's `--json` `data` schema) are
produced by `bin/gen-docs` from the code itself — the `ErrorCode` enum and the
`#[derive(schemars::JsonSchema)]` output structs in `crates/forklift/src/commands/`
(behind the dev-only `docgen` cargo feature, never enabled in a release build; see
`crates/forklift/src/docgen.rs`). `bin/check` regenerates them into a scratch directory
and diffs against the committed copy, so **stale generated docs fail CI** — if you add an
error code or change a command's `--json` output, run `pult gen-docs` (or `./bin/gen-docs`)
and commit the result along with your change; you don't hand-edit those two files.
