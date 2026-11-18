# proof-load

Proof Load is Forklift's fault-injection harness. It runs `forklift`, `forklift-server` and `git` as
programs, injects faults while they work, and grades what each one reads back afterwards. The
faults include damaged files, kill points, power loss, a full disk, a read-only warehouse,
case-folding and no-exec volumes, dead-PID locks, network faults and round trips between the two
tools. Each read is graded against an oracle built from a seed.

The harness never links Forklift's code, so its expectations cannot share Forklift's bugs.

The method is in the guide: what is faulted, the five grades and the recoverability record, seeds
and replaying a row, the lanes, the comparison with git and the data plate, and how to read the
report. See [`docs/guide/proof-load.md`](../../docs/guide/proof-load.md).

## Build

From the repository root:

```sh
cargo build -p forklift -p forklift-server -p proof-load
```

## Run

This runs one scenario against the binaries you just built:

```sh
target/debug/proof-load run --source . --out ../proof-load-out \
  --forklift target/debug/forklift --forklift-server target/debug/forklift-server \
  --git "$(command -v git)" --only history
```

- `--source` is the source tree of the version under test. It is only read.
- `--out` must be an empty or new folder. It holds the scratch areas, `report.md`, `report.json`
  and the details files.
- Without `--only`, every scenario runs.
- Leave out a binary, and the lanes that need it are reported as not run.
- `proof-load run --help` lists the lanes' flags: `--strace`, `--kill`, `--power`,
  `--capped-volume`, `--disk-full`, `--read-only`, `--folding-volume`, `--network` and
  `--history-from`.

The exit status is 0 when the report passes, 1 when it fails, and 2 when the run could not go
ahead.

To look at a report in a window, use [`tools/proof-load-desktop`](../../tools/proof-load-desktop/README.md).

## Tests

```sh
cargo test -p proof-load
```

Among them, `bindings::tests::this_trees_census_is_covered` fails when a census row has neither a
damage binding nor an exemption in `src/bindings.rs`. Tests that need a real forklift binary are
`#[ignore]`d and read its path from `PROOF_LOAD_FORKLIFT`.

[`contributing.md`](../../docs/guide/contributing.md) describes how CI runs the harness.
