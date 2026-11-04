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
  fidelity; `crash_consistency.rs` stops `stack`, `load` and `park` before each of
  their renames in turn and checks the store after every stop; `determinism.rs` pins deterministic tree hashes,
  byte-reproducible repacks, and the warehouse-lock refusal. A parser or
  format change should keep the fuzzer green; if you add a length-prefixed field,
  bound it with a `checked_add` (never `start + length` before the bounds check).
- Keep tests fast; see [Keeping tests fast](#keeping-tests-fast) below. Under
  `cargo nextest run`, which CI uses, `.config/nextest.toml` reports a test as SLOW at
  30 s and fails it at two minutes. CI runs twice
  as many tests as cores, so a test with a wall-clock bound needs margin for a shared
  core; `cargo nextest run --profile ci --test-threads $((2 * $(nproc 2>/dev/null || sysctl -n hw.ncpu)))`
  reproduces that load locally.
- CI (`.github/workflows/ci.yml`) splits the Windows tests across four runners,
  each running its share with nextest's `--partition count:K/N`; Linux and macOS
  run theirs in one job each. The last share of each OS also runs the doc-tests
  and Clippy; the formatting guard runs on Linux and the split-debuginfo guard on
  macOS. The `ci passed` check is green only when every share and the
  generated-docs check passed, or the run was docs-only; gate a merge on it, not
  on the individual shares. To rerun a failed share, say
  `test (windows-latest, 3/4)`, run its failing tests by name, or on Windows the
  whole share: `cargo nextest run --workspace --profile ci --partition count:3/4`.
  The share holds every fourth test of each test binary, so on another OS a test
  that exists only on some platforms shifts the rest of its binary into other
  shares, and `--partition count:3/4` there runs a different set.
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

### Keeping tests fast

CI runs the whole suite on three OSes on every push to a pull request that changes code,
and a pull request gets several pushes, so every second a test spends is paid many times
over. Much of the suite's time has gone to tests waiting out real timeouts or writing far
more files than they checked. Write new tests so that work does not have to be done again.

**The budget.** A new or changed test finishes in about a second on Linux and in at most
**10 s on the Windows CI runner**, which is where the suite's time goes. Windows is not a
fixed multiple of Linux: on main CI run 41699714241 (2026-11-03) the same test took about
four times as long at the median, eight times or more for one test in ten, and 22 times
for `walk_tree_survives_a_pathologically_deep_tree`, so a test that fits on Linux can
still blow the budget there. A test that needs more says why in a comment above it: what
it does that takes the time, and why a smaller version would not catch the same failure.
Not every existing test meets this yet (37 took over 10 s on Windows in that run); do not
take one of them as the model for a new test. The SLOW and failure limits in
`.config/nextest.toml` are a backstop against hangs, not the budget: a test can be well
inside them and still too slow.

**Check the time where it is paid.** Every test's time is in the CI log: nextest prints
`PASS [  12.368s]` before each name. Before merging a pull request that adds or changes
tests, look up those tests in the `test (windows-latest, K/4)` jobs. A local run on a fast
machine does not tell you what Windows will take.

**How to stay inside it:**

- **Never wait out a production timeout.** A test that has to watch a timeout or budget
  fire makes it a value the code takes and arms a short one: `remote_utils.rs` tests
  build their clients with `FAST` budgets in place of the real ones, and the server's
  hook tests arm a 1.5 s hook instead of 10 s. If the timeout is a constant, turn it into
  a parameter first.
- **Wait on the event, not the clock.** A `sleep` long enough to be safe on a loaded
  runner is far too long everywhere else. Poll for the condition with a short interval
  and an overall deadline, so the test ends as soon as the thing happens.
- **Age a file by setting its mtime.** A stat is proven only once the file is a couple of
  seconds older than the command that records it. A test that needs a proven stat moves
  the file's mtime into the past (`backdate` and `backdate_tree` in `cli.rs`) rather than
  sleeping until it is old enough.
- **Count the syncs of an in-process test.** Unit tests run with fsync on, so that the
  sync calls are exercised, and every real write pays for one. Seed a large starting state
  by writing it directly, and push only the few writes the assertion is about through the
  code under test. The journal cap test spent 24 s on Windows filling the journal one
  synced write at a time. A fixture whose tests check nothing about durability can skip
  its own thread's syncs with `file_utils::skip_syncs_on_this_thread()`, as the office
  fixture in `audit_utils.rs` does: its 39 tests took about 4 s each in a parallel run and
  0.2 s without the syncs. It also disarms the injected sync faults, so a test that arms
  one must not hold it. It helps only where the test's own syncs are the cost. Under a
  parallel run every write also waits on the other tests' syncs, so a test that takes
  0.1 s alone can take 14 s in the full suite while syncing almost nothing itself.
- **Make a limit something the test can set small.** A test that crosses a cap, a depth
  or a size passes a small value of it rather than building a fixture past the
  production one. `walk_tree_survives_a_pathologically_deep_tree` checks stack safety on
  a small thread stack at depth 2,000 instead of a tree 50,000 levels deep (226 s on
  Windows); a fixture of about 2,000 loose objects took 4 s on Linux and was killed at the
  two-minute limit on Windows.
- **Write only what the code under test reads.** No blob contents for a check that looks
  only at tree shape, no history beyond the parcels the assertion needs.
- **Launch through the helpers.** `forklift_command()` and `server_command()` turn fsync
  off (above); a test that builds its own `Command` pays for every sync. Each launch of a
  binary costs much more on Windows than on Linux, so a loop over launches is where a
  Windows run spends its time: launch once with many paths rather than many times with
  one. A list of names or inputs that must all be accepted goes into one fixture and one
  command, not a warehouse each. Independent fixtures that one test needs, such as the
  states of a matrix or the repeated runs of a determinism check, run on threads of their
  own with `std::thread::scope`, since each is a chain of launches that waits on nothing
  the others do.
- **Test at the cheapest level that can fail.** An integration test launches real
  binaries and pays for every launch; a unit test of the same function pays for none.
  Put the cases of a rule (every input shape, every edge) in unit tests, and keep one
  integration test per path that proves the command reaches the rule and reports it.
  Before adding an integration test, check whether an existing one already crosses the
  same wiring; if so, add an assertion to it instead of a new test.
- **Check one built state in one test.** nextest runs every test in a process of its own,
  so tests cannot share a fixture: two tests that build the same state to check different
  things about it pay for the whole build twice. Build it once and make every check in
  that test. Put the checks that only read the state first, and the ones that damage it
  last, undoing each damage before the next so that every failure has one cause.
  `a_sparse_audit_states_its_boundary_and_re_verifies_in_scope_content` in `remote.rs`
  replaced five tests that each built the same signed warehouse and sparse clone. Keep
  tests apart when a check changes the state a later one needs to see fresh: a refused
  command that has already fetched from the remote, for instance, leaves the next command
  a different starting point than a first contact would.
- **Crash at each step, not at a random time.** To test what a crash leaves behind, stop
  the command before each of its renames in turn with `FORKLIFT_DEBUG_CRASH_AT_RENAME`
  (see `crash_at_every_rename` in `crash_consistency.rs`). Killing it after a guessed
  delay costs a long corpus and many rounds, and still lands on the one state that matters
  only by luck: the crash suite's kills caught a reordered `load` publish in 4 runs out of
  14, and stopping at every rename catches it on every run, in half a second. The stops
  reach renames only: a step that writes or removes a file in place needs its own check,
  and so does the order of syncs, since a stopped process loses nothing it wrote. Check
  at each stop only what that stop alone can show. Every check is a launch per stop, and
  a check of state that the next command heals, or that the closing read covers anyway,
  buys nothing: the `stack` and `load` crash tests took about 20 s each on Windows, when
  most of the six or seven launches at each stop were checks.
- **Repetition is the cost.** A test that repeats a run to catch a race uses the fewest
  rounds that still catch the failure, and says in a comment how the count was chosen.

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
| `pult release` | `bin/release` — preflight, bump the workspace version, roll the changelog, tag `v<x.y.z>`, push (the tag triggers `.github/workflows/release.yml`) |
| `pult install` | build & install the CLI + server from this checkout |
| `pult dev-server` | `bin/serve` — a throwaway local `forklift-server` over `.dev/server` for testing lift/lower/clone |
| `pult design` | open `docs/DESIGN.html` (the source of truth) in a browser |
| `pult gen-docs` | `bin/gen-docs` — regenerate the derived docs (error codes, the command and alias list, per-command JSON schemas); `bin/gen-docs --check` only checks they are current |

Cutting a release: `pult release` picks the next patch/minor/major from the latest `v*`
tag (or run `bin/release <x.y.z> --dry-run` to preflight without touching anything). It
refuses unless the working tree is clean, `main` is in sync with `origin/main`, and
[`CHANGELOG.md`](../../CHANGELOG.md) has entries under **Unreleased**, which it then rolls into the new
version. What earns an entry is listed in [§8](#8-keeping-docs-in-sync).

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
- **A build reads exactly one warehouse format.** `WAREHOUSE_FORMAT` is checked when a warehouse is
  opened, and the open holds `format.lock` shared until the process exits. A change to anything a
  warehouse keeps on disk starts from [the format contract](../format/WAREHOUSE_FORMAT.md).
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
- A new file or folder name is a `FILE_NAME_*`, `FOLDER_NAME_*`, `FILE_SUFFIX_*` or
  `FILE_EXTENSION_*` constant with a row in the census of
  [the format contract](../format/WAREHOUSE_FORMAT.md#the-census); a test fails
  until it has one. A changed record or object version, or a renamed record kind, fails the frozen
  `versions` snapshot until `WAREHOUSE_FORMAT` is bumped, and a bump needs the new
  format's fixtures made with `freeze_the_current_format_fixtures` (and, for the
  AWS head, `freeze_the_current_aws_format_fixture` after an `AWS_FORMAT` bump).
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

Documentation is part of the change, not a follow-up. Some of it is checked: the generated docs,
the format census, the guide pages and the links fail CI when stale. The rest is checked by nobody,
and has been missed for that reason, so a pull request's description says which of the entries below
it touched, or that none applied. When a change alters:

- **anything a user of a release would notice** → add an entry under **Unreleased** in
  [`CHANGELOG.md`](../../CHANGELOG.md), in the same pull request. That means:
  - a command, flag, alias, exit code or `--json` field added, removed or changed in meaning, and any
    `--json` schema bump (update the schema number the entry names);
  - an on-disk or wire format change, above all one that makes existing warehouses or remotes
    refuse;
  - behaviour a user relies on, such as what is tracked, ignored, staged, refused or overwritten;
  - a fix to data loss, a security property or trust verification, named on its own line;
  - something removed.

  Leave out what no user can observe: refactors, tests, CI, internal speed-ups without a measured
  user-visible effect, and docs-only changes. Many small fixes of one kind share one line. Write
  for someone deciding whether to upgrade, not as a diff summary; a pull request title is not an
  entry;
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
  dated changelog row (the design's own history, which is not `CHANGELOG.md`), and flip the
  feature's status chip in its §5 when it lands;
- **the parallelism of an operation** → update its status in
  [`../PARALLELIZATION_PLAN.md`](../PARALLELIZATION_PLAN.md);
- **a performance item listed in [`../PERFORMANCE_BACKLOG.md`](../PERFORMANCE_BACKLOG.md) or a
  number in [`../BENCHMARK.md`](../BENCHMARK.md)** → update the entry, with the measurement.

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
