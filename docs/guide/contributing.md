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
- Integration tests launch the binaries with `FORKLIFT_FSYNC=0`, which makes the
  suite much faster: `forklift_command()` in `crates/forklift/tests/common/mod.rs`
  (and `server_command()` in `remote.rs`) sets it. Launch through those helpers,
  not `Command::new(FORKLIFT)`. The exception is `crash_consistency.rs`, the
  durability suite, which forces fsync on. In-process tests leave fsync on, so
  they are what still runs the sync calls; a sync call that no in-process test
  reaches needs one.
- The hardening test spine is worth knowing when touching the object store,
  the parsers, or the parallel walks: `crates/forklift-core/tests/fuzz_formats.rs`
  fuzzes every parse entry point (must never panic) and checks round-trip
  fidelity; `crash_consistency.rs` SIGKILLs `stack` mid-write to prove the store
  stays consistent across power loss; `determinism.rs` pins deterministic tree hashes,
  byte-reproducible repacks, and the warehouse-lock refusal. A parser or
  format change should keep the fuzzer green; if you add a length-prefixed field,
  bound it with a `checked_add` (never `start + length` before the bounds check).
- Keep tests fast: CI runs the suite on every pull request, on three OSes. A
  test that has to watch a timeout fire arms a short one instead of waiting out
  production's (see `FAST` in `remote_utils.rs`), and a fixture writes only the
  files the code under test reads. Under `cargo nextest run`, which CI uses,
  `.config/nextest.toml` reports a test as SLOW at 30 s and fails it at two
  minutes; the crash suite's kill tests get ten. CI runs twice as many tests
  as cores, so a test with a wall-clock bound needs margin
  for a shared core; `cargo nextest run --profile ci --test-threads $((2 * $(nproc 2>/dev/null || sysctl -n hw.ncpu)))`
  reproduces that load locally (the `ci` profile gives the crash suite's kill
  tests two slots each, which only pays at that thread count).
- On macOS, `.cargo/config.toml` builds with `-Csplit-debuginfo=packed`: debuginfo
  goes into a `.dSYM` beside each binary instead of object files left in
  `target/debug/deps`. Under the default, every incremental rebuild leaves fresh
  objects there and nothing removes the old ones. rustc lists that directory to find
  crates, so builds slow down as it grows: with 2.9 million stale files, rebuilding
  the server and the tests after a core edit took over 3 min. With the setting it
  takes about 16 s on an 18-core Mac, and does not grow. Things to know:
  - It does not remove objects that are already there, so a target directory built
    before it needs one `cargo clean`.
  - Cargo takes rustflags from only one place. A `RUSTFLAGS` or
    `CARGO_ENCODED_RUSTFLAGS` environment variable replaces this setting, and the
    objects pile up again; if you need one, include `-Csplit-debuginfo=packed` in
    it. This setting in turn replaces `[build] rustflags` from
    your own `~/.cargo/config.toml`. Put personal flags under
    `[target.'cfg(target_os = "macos")'] rustflags` instead: cargo joins those
    lists across config files.
  - A debug binary copied away from `target/` leaves its `.dSYM` behind, so its
    backtraces lose file and line.
  - To check for the problem, `find target/debug/deps -name '*.rcgu.o' | head -1000 | wc -l`
    should print 0. Listing the whole directory is itself slow once it is large.
- To install a working binary (e.g. to dogfood): `cargo install --path
  crates/forklift --force` (lands in `~/.cargo/bin`).

### Formatting

Forklift is formatted by hand. **Do not run `cargo fmt` or `rustfmt`**; match the
formatting of the code around your change instead. A construct stays on one line
whenever it fits the width, and doc comments are wrapped by hand.

No stable rustfmt configuration matches this style. The closest one tried
(`use_small_heuristics = "Max"`, no import or module reordering) still disagreed
with about half the tree, and the options that would close the gap (comment
wrapping, import grouping and granularity) are nightly-only. So the root
`rustfmt.toml` sets `disable_all_formatting = true`, which makes an accidental
`cargo fmt`, or an editor's format-on-save through rustfmt, a no-op rather than a
reflow of files the change never touched. An editor that formats Rust with its own
formatter rather than rustfmt (RustRover's default) does not read `rustfmt.toml`,
so turn its reformat-on-save off for this project.

`bin/check` and CI enforce the guard with `bin/fmt-guard`. It fails if the root
`rustfmt.toml` holds anything but comments and `disable_all_formatting = true`,
or if a second `rustfmt.toml` or `.rustfmt.toml` exists anywhere in the tree
outside build output, `.git` and `.claude/` (ignored files included), since a
second config would switch rustfmt back on for some files beneath it. It also
runs `cargo fmt --all --check`, which needs the rustfmt component installed. It
never formats anything itself.

### Maintainer commands (pult)

The repo ships a [pult](https://github.com/lonic-software/pult) manifest (`pult.yaml`)
so the operational commands are one launcher away; the real logic lives in `bin/`. Run
`pult` for the menu, or a command directly:

| Command | What it does |
| --- | --- |
| `pult check` | `bin/check` — the [formatting guard](#formatting) + build + test + clippy, the same checks CI runs (CI runs the tests under cargo-nextest and skips them on docs-only pull requests), plus docs freshness. A test that reads a file under `docs/` must name it by its literal repo path (e.g. `"../../docs/guide/cli.md"`), because that is how CI tells a docs-only change from one a test depends on |
| `pult test` | just the test suite (fast loop) |
| `pult release` | `bin/release` — preflight, bump the workspace version, tag `v<x.y.z>`, push (the tag triggers `.github/workflows/release.yml`) |
| `pult install` | build & install the CLI + server from this checkout |
| `pult dev-server` | `bin/serve` — a throwaway local `forklift-server` over `.dev/server` for testing lift/lower/clone |
| `pult design` | open `docs/DESIGN.html` (the source of truth) in a browser |
| `pult gen-docs` | `bin/gen-docs` — regenerate the derived docs (error codes, the command and alias list, per-command JSON schemas); `bin/gen-docs --check` only checks they are current |

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
  temp-file + rename. Rename through `file_utils::rename` (or `rename_no_replace`),
  which waits up to a second for another process (an editor, indexer or antivirus
  scanner) to release a Windows handle that blocks the rename; `clippy.toml` bans
  a bare `std::fs::rename`. Mutating commands hold the **warehouse lock**
  (`.forklift/lock`, `create_new`, held for the whole command) so a reader can
  never see a half-updated staging area. Read-only commands don't take it. Add a
  new mutating command to the lock list in `cli.rs`
  (`requires_warehouse_lock`). Every command on that list also holds the
  object store **shared** (`StoreAccessLock`, `.forklift/store-access.lock`)
  unless it deletes objects itself (`holds_store_access`); anything that deletes
  objects on a liveness or scope decision — a full repack, `gc`, `scope prune`, `store evict` —
  takes it exclusively with a try and refuses beside a writer. Bay lock first,
  store access second.
- **The remote is untrusted storage + thin verifiers.** Anything a client uploads
  is unverified until the server checks it (hash, signature, privileges). Anything
  a client downloads is verifiable offline (content addressing + signatures).
  Nor may a peer decide how much memory we spend: read a response through
  `util::body_utils` with a cap derived from what the request can produce, stream
  anything without a fixed bound (bundles) to disk or to the importer, and build
  every zstd decoder with `object_utils::bounded_zstd_decoder` /
  `bounded_zstd_dictionary_decoder`. `clippy.toml` bans the uncapped alternatives,
  so `cargo clippy` fails on a new one; a deliberate exception carries an
  `#[allow]` with its justification.
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
   result type implementing `CommandOutput`, and `output::emit(&result)` — the
   envelope's `command` comes from the parsed command line, never the handler; human mode calls `render_human()`, JSON mode wraps it in the envelope. For a
   one-line outcome use `output::message`; for silenced-under-`--json` progress
   lines use the `human!` macro. **Nothing else may print to stdout**, or `--json`
   stops being a single valid document.
5. **Dispatch it** in `main.rs`.
6. **Test it** in `tests/cli.rs` (or `tests/remote.rs` for remote behavior). Assert
   both the human output and, where it matters, the `--json` envelope.
7. **Document it** in [`cli.md`](cli.md), and run `bin/gen-docs` to add it to the generated
   command list.
8. **Approve it in `crates/forklift/surface/`.** `spec.txt` is the reviewed list of every
   command: place the new one there and classify it against each list's definition (the daily
   loop, destructive, recorded text). A family nested inside another (`self alias`) gets its own
   `self alias: install uninstall status` line, written after the line that lists it. The
   surface tests (`cargo test -p forklift --bin forklift surface_check`) fail until it agrees
   with the binary. A new name may make a prefix people
   type ambiguous (`pi` stops meaning `pick` once `pin` exists); that is expected, because
   prefixes are only for typing. A command with subcommands takes no positional argument, and
   each of its options takes no value or exactly one, so the prefix expansion in
   `crates/forklift/src/prefix.rs` can tell its subcommand from its arguments; that module's
   tests hold it. A flag never switches a command into a different operation: an on/off flag
   declared to conflict with a positional argument (`--abort` against a URL) is a second
   operation, and the surface tests refuse it, so make it a subcommand of its own.
9. **Decide whether agents drive it.** `forklift mcp` generates its tools from the command
   definition, so a new command is an agent tool with nothing more to do: its one-line and long
   descriptions are the tool's description and each argument's help is that argument's (put
   anything an agent needs to know in the help, not elsewhere). A command only a person should
   run goes on `HUMAN_ONLY` in `crates/forklift/src/commands/mcp.rs`, with the reason, and an
   argument no agent should pass on `HUMAN_ONLY_ARGS`, which serves the tool without it. The MCP
   unit tests (`cargo test -p
   forklift --bin forklift commands::mcp`) parse every tool's arguments against the command and
   fail when one would be misread.

Any message that names a command does so as `forklift <command> …`, spelled out in full. The
same tests parse every such mention in the crates' string literals and the binary's help text
against the binary, so a hint naming a renamed command, a flag that does not exist, or a prefix
(`forklift pall`) fails CI. A top-level command named in double quotes or backticks
without `forklift ` in front fails too, because the check could not read it.

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

**Licensing and the CLA.** The legal side of contributing lives in the root
[`CONTRIBUTING.md`](../../CONTRIBUTING.md): client/core/docs contributions are
inbound = outbound (MIT/Apache, no paperwork); contributions touching the server
heads (`forklift-server`, `forklift-aws-lambda`) or the infrastructure that
deploys them (`infra/aws-serverless`) require the
[CLA](../../CLA.md), signed once via a bot comment on your pull request.

Commit subjects follow `Area - Topic: short description`, e.g.
`Trust - Revocation: reason + distrust boundary`. The body explains the *why* and
the invariants touched, not just the *what*. When a feature lands, flip its status
chip in `DESIGN.html` §5 and add a dated changelog row in §9 — the doc is
maintained alongside the code.

---

## 8. Keeping docs in sync

Documentation is part of the change, not a follow-up. When a change alters:

- **a command's flags, name, or behavior** → update [`cli.md`](cli.md), and run `bin/gen-docs`
  when its name, alias or one-line help changed;
- **anything [`tutorial.md`](tutorial.md) or [`concepts.md`](concepts.md) runs or shows** →
  update the page. The `guide_pages` test in `crates/forklift/tests/` runs each page's commands
  in order and fails when one fails or prints something other than the page shows. Any page that
  gains a `console` block must live in `docs/guide/` and be added to that test's `RUN_PAGES`,
  which it enforces;
- **a heading another page links to** → fix the links. The `doc_links` test reads every `.md` page
  in the repository and fails on a relative link to a missing file or a missing heading;
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
table), `docs/generated/commands.md` (every command and subcommand with its aliases),
`docs/generated/json-schemas.md` (every command's `--json` `data` schema) and
`docs/generated/mcp-tools.md` (every MCP tool with its arguments) are
produced by `bin/gen-docs` from the code itself — the `ErrorCode` enum, the clap command
definition in `crates/forklift/src/cli.rs`, and the
`#[derive(schemars::JsonSchema)]` output structs in `crates/forklift/src/commands/`
(behind the dev-only `docgen` cargo feature, never enabled in a release build; see
`crates/forklift/src/docgen.rs`). Each command registers its structs from a
`__docgen_schemas` fn through `docgen::OutputSchema::of`, which describes the serialized
shape: a field with `skip_serializing_if` comes out optional without needing
`#[serde(default)]`. `bin/gen-docs --check` regenerates them into a scratch directory
and diffs against the committed copy; CI's "generated docs" job and `bin/check` both run it, so
**stale generated docs fail CI** — if you add an error code, add or rename a command or alias,
change a command's one-line help, or change a command's `--json` output, run `pult gen-docs`
(or `./bin/gen-docs`) and commit the result along with your change; you don't hand-edit those
files. Other docs link to these pages rather than copying their tables.

The doc comments those pages are rendered from are read by people outside the project, so the
generator refuses a page that cites a `FORK-<n>` ticket, a `§<n>` section of the design
document, or a review-round label (`S<n>-F<n>`, "review round …", "round <n> finding"): a
reader cannot open any of them. State the reasoning
in the comment itself. Elsewhere in the source, a ticket reference is fine as provenance beside
reasoning that is already written out, but never as the only place the reasoning lives.
