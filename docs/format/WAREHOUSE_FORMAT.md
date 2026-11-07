# Warehouse format

A warehouse records one format number in its marker, and a build reads exactly one format,
`WAREHOUSE_FORMAT`. The number covers everything a warehouse and its bays keep on disk. Format 1 is
the first format, so nothing before it upgrades: a warehouse made before formats existed is made
again.

## The marker

`.forklift/warehouse` is a [state record](STATE_RECORD_FORMAT.md) of kind `warehouse`, version 3:

```text
/forklift-warehouse 3
format 1
full
```

| Entry | Meaning |
|---|---|
| `format N` | The format the warehouse is in. Decimal, with no leading zero. |
| mode word | What the warehouse holds. A build judges this word only at its own format. |
| `upgrading M S` | Optional: an upgrade to format `M` is under way, with `S` steps done. |

This framing is frozen, so any future build can read any marker well enough to refuse it correctly.
So are two others. A bay's redirect is frozen because re-creating a bay would destroy its parked
parcels, staged inventory and any consolidation in progress. The franchise record of an unfinished
clone is frozen because a clone does not pass the gate: it names the format that wrote it first, and a
resume at another format discards any bundle it kept.

## The gate

Deciding that a folder is a warehouse takes any format. Judging the format is a separate step, taken
when a warehouse is opened, including through a bay, and by `prepare`, which would otherwise add
pieces of its own format under another number.

| Marker | Refusal | Exit | Way out |
|---|---|---|---|
| `upgrading` present | `warehouse_upgrade_incomplete` | 42 | Run the upgrade again, with a build that reads the target format. If this build would finish it but does not upgrade the format it started from, `warehouse_format_older` (40): make the warehouse again. |
| Older than this build | `warehouse_format_older` | 40 | Run the upgrade; for a format older than any the build upgrades, make the warehouse again. |
| Newer than this build | `warehouse_format_newer` | 41 | Update the binary. Upgrades are one-way. |

A `--warehouses` server answers `503` for such a warehouse and serves the others. No warehouse can
reach the first or third row without a hand-edited marker until a second format exists.

## The upgrade

`forklift upgrade`, run anywhere in the warehouse's own working tree, and `forklift-server upgrade
--root <root>` upgrade a warehouse in place. Each is dispatched before the gate and opens the warehouse
itself:

1. A bay is refused with the warehouse to run it in, since a bay is upgraded with its warehouse. An
   unfinished clone is refused too (`clone_incomplete`); running the clone again finishes it in the
   new build's format.
2. It takes the [lease](#the-lease) exclusively with a try. If anything holds it, the upgrade refuses at
   once (`warehouse_in_use`, exit 45): every other command, a server serving the warehouse, a bundle
   or a gc.
3. It moves one format at a time. Before the first change it records `upgrading F+1 0`, by replacing
   the marker atomically, so a crash leaves every other command refusing the warehouse.
4. It runs that format's steps in order. A step is recorded done only after it returns, which is after
   its writes are synced, so a step can be run again after a crash and must be idempotent. The steps
   are given the forklift folder and the state folder of every bay.
5. It records `format F+1` with no upgrade, and takes the next format until it reaches the one it reads.

Run again after a crash or a failed step, the upgrade resumes at the first step not recorded done. Steps
run with the warehouse as the storage root (the CLI also changes into the warehouse root), and a step
that spreads work over threads enters that root on each. The mode word is kept, unless a step rewrites
it.

What it cannot carry it refuses, changing nothing:

| Marker | Refusal | Exit | Way out |
|---|---|---|---|
| In the build's format | none | 0 | Nothing to do. |
| Older than any format the build upgrades | `warehouse_format_older` | 40 | Make the warehouse again. |
| Newer than the build | `warehouse_format_newer` | 41 | Update the binary. |
| `upgrading` to a format newer than the build | `warehouse_upgrade_incomplete` | 42 | Update the binary, and run its upgrade. |
| Steps done, but the mode left is not one the build reads | `warehouse_upgrade_incomplete` | 42 | Update the binary to one whose upgrade reads that mode; the upgrade stays on record and it resumes there. |

Format 1 is the first format, so the chain this build carries is empty, and nothing older than format 1
is ever upgraded.

## The lease

`.forklift/format.lock` is an empty file that `prepare` creates and Forklift never deletes. Every
process that opens a warehouse takes a kernel lock on it, shared, before reading the format, and
holds it until it exits. A server holds it for as long as it serves that warehouse. An upgrade takes
it exclusively with a try, and refuses if anyone holds it.

- A process that finds it held exclusively refuses at once (`warehouse_upgrade_running`, exit 43).
- It is opened read-only, so a warehouse that cannot be written can still be read.
- If it is missing it is made again. If it cannot be made, the open refuses
  (`warehouse_lease_missing`, exit 44).
- A process that dies releases it, so there is nothing to clean up.
- Deleting it while another process holds it defeats the lease. That is tampering, not a case Forklift
  defends against.

**A warehouse being upgraded must not be in use from another machine.** Whether kernel locks hold on a
network filesystem is unverified, and read-only locking has only been run on macOS.

## The AWS head

The AWS head has its own format number and its own chain, since what it keeps is DynamoDB items rather
than files. Each warehouse records its format in one item, `entity = "format"`, in its own partition.
Its `state` is the format number, or `N>M owner step` while an upgrade from `N` to `M` holds it.

Every request reads that item strongly consistent before anything else, and is refused with a `503`
unless the state is exactly the format the deployment reads:

| State | The request | Way out |
|---|---|---|
| The deployment's format | is served | Nothing to do. |
| Absent, and nothing stored in the warehouse | is served; its first write records the format | Nothing to do. |
| Absent, with data stored | is refused | Make the warehouse again under a new id. |
| Older, and the chain reaches it | is refused | The operator runs `forklift-aws-upgrade`. |
| Older than the chain reaches | is refused | Make the warehouse again under a new id. |
| Newer | is refused | Update the deployment. |
| An upgrade | is refused | Wait for the upgrade, or have the operator run it again. |
| Anything else | is refused | Nothing to guess at: the state was not written by Forklift. |

Every ref and trust write is a transaction whose first action is that check: a `ConditionCheck` on the
state the request read, which never rewrites it, or, for a new warehouse, a `Put` of the format item
that holds only while the item is absent. A write whose check fails is gated again, so a write
prepared before an upgrade began cannot land once it has.

`forklift-aws-upgrade` upgrades a warehouse in place. It runs from an operator's machine, never from a
Lambda, and takes warehouse ids or `--all`:

1. It replaces the state with `F>F+1 owner 0`, with a fresh owner token, by a compare-and-set.
2. It waits longer than the control-plane Lambda's timeout (`--wait-secs`, 60 by default), so no
   request that read the old state is still running.
3. It runs that format's steps in order, advancing the step in the state after each. Every write a
   step makes is fenced on the exact state, so an operator whose run was taken over can no longer
   write.
4. It records `F+1`, and takes the next format until it reaches the one it reads.

A state left by another run is refused unless the operator passes `--takeover` with that warehouse's id,
after making sure that run has stopped. Takeover puts a new owner in the state, waits again (the run it
takes over may have stopped before its wait was out), and resumes at the first step not done, so steps
must be idempotent. A warehouse that fails is reported and the others are still upgraded. Steps change
only the warehouse's own DynamoDB partition: a step is given no object store, and nothing fences the
bucket's keys, so an S3 layout change has no in-place path.

Since AWS format 2 every warehouse has its own keys in the bucket: `objects/{warehouse}/{hash}`,
`signatures/{warehouse}/{parcel_hash}` and `staging/{warehouse}/{session}/{hash}`. Only `responses/`,
which holds transient bundle bodies reached through a presigned URL, is shared. AWS format 1 kept one
`objects/{hash}` and one `signatures/{hash}` key per object for every warehouse in the bucket, so a
warehouse could learn from the answers whether another one held an object. No step can tell whose a
shared key was, so the chain does not reach format 1: a format 1 warehouse is refused, never read
against the new keys, and is made again under a new id.

A format break on a deployment refuses every warehouse until each is upgraded. The AWS head's chain is
empty: it reads format 2 and upgrades no older format.

## What a format break ships

A change to what a warehouse keeps on disk ships all of these together:

- a `WAREHOUSE_FORMAT` bump;
- migration steps for the local chain;
- an AWS step, if a DynamoDB item shape moved;
- a frozen fixture of the new format;
- a changelog and guide entry.

Records fall into three classes, and the class decides what a break can do:

| Class | Examples | What a break does |
|---|---|---|
| History-borne | objects, pack contents, signatures | Never rewritten, since an object's name is the hash of its bytes. Every version stays readable forever. A new written version also ships a protocol bump or a capability both sides agree on before sending it. |
| Rewritable | marker, refs, tombstones, journal, operation and consolidation records, scope, inventory metadata | Migrated by steps, in-progress operation records included. |
| Derived | graph, clock, bundles | Deleted by the step that changes them, and rebuilt. |

Home-directory state (`~/.forkliftconfig`, keys) is outside the warehouse format. It keeps its own
tolerant or versioned reader, and no upgrade touches it.

An S3 layout change has no in-place path. It blocks its release until it has its own design.

## The census

Every name Forklift gives a file or folder is classified below. Paths are relative to `.forklift/`.
`{bay}` is a bay's state folder: nothing for the main tree, `bays/<name>/` for a bay. `<…>` stands for
part of one name, and `**` for any number of folders. A path ending in `/` is a folder.

Besides the three classes above:

- **Lock**: an empty file only ever locked, never read.
- **Probe**: rebuilt whenever it cannot be read, and never trusted. A probe lives in the working
  folder's own `.forklift`, on the volume it measures, so an upgrade cannot reach a bay's probes.
- **Transient**: lives only while an operation runs, and is swept or replaced by the next one.
- **Foreign**: in another tool's format; Forklift reads it only through that tool.
- **Outside**: not in a warehouse at all.

A test holds the source and the [fixtures](#the-fixtures) to this table. Every `FILE_NAME_*`,
`FOLDER_NAME_*`, `FILE_SUFFIX_*` and `FILE_EXTENSION_*` constant in the source is named in a row, every
name in a row is a constant, and every path in every fixture matches a row.

<!-- census:start -->
| Path | Class | Names |
|---|---|---|
| `warehouse` | rewritable | `warehouse_utils::FILE_NAME_WAREHOUSE_MARKER` |
| `format.lock` | lock | `lock_utils::FILE_NAME_FORMAT_LOCK` |
| `objects/`, `objects/<xx>/`, `objects/<xx>/<hash>` | history-borne | `globals::FOLDER_NAME_OBJECTS_ROOT` |
| `objects/<xx>/<hash>.sig` | history-borne | `sign_utils::FILE_SUFFIX_SIGNATURE` |
| `objects/pack/`, `objects/pack/<name>.pack`, `objects/pack/<name>.idx` | history-borne | `pack_utils::FOLDER_NAME_PACK`, `pack_utils::FILE_EXTENSION_PACK_DATA`, `pack_utils::FILE_EXTENSION_PACK_INDEX` |
| `objects/pack/<name>.sigs` | history-borne | `signature_table::FILE_EXTENSION_SIGNATURE_TABLE` |
| `objects/pack/densify-pending` | derived | `pack_utils::FILE_NAME_DENSIFY_MARKER` |
| `objects/pack/.compact-<name>.tmp` | transient | |
| `pallets/`, `pallets/<name>` | rewritable | `pallet_utils::FOLDER_NAME_PALLETS_ROOT` |
| `meta/`, `meta/<name>` | rewritable | `pallet_utils::FOLDER_NAME_META_ROOT` |
| `trust` | rewritable | `office_utils::FILE_NAME_TRUST` |
| `served` | rewritable | `tombstone_utils::FILE_NAME_SERVED` |
| `tombstones` | rewritable | `tombstone_utils::FILE_NAME_TOMBSTONES` |
| `sealed` | rewritable | `sealed_utils::FILE_NAME_SEALED` |
| `redactions-applied` | rewritable | `applied_redactions::FILE_NAME_APPLIED` |
| `redactions-chunks-owed` | rewritable | `redaction_deleter::FILE_NAME_CHUNKS_OWED` |
| `unvouched-heads` | rewritable | `take_utils::FILE_NAME_UNVOUCHED` |
| `held-heads` | rewritable | `held_heads::FILE_NAME` |
| `fetched-heads` | rewritable | `fetch_ledger::FILE_NAME_LEDGER` |
| `origin` | rewritable | `origin_utils::FILE_NAME` |
| `origin-move` | rewritable | `origin_move::FILE_NAME` |
| `franchise` | rewritable | `franchise_utils::FILE_NAME_FRANCHISE` |
| `mirror-roots` | rewritable | `mirror_utils::FILE_NAME_MIRROR_ROOTS` |
| `mirror.git/**` | foreign | `mirror_utils::FOLDER_NAME_MIRROR_REPOSITORY` |
| `.gitignore` | foreign | `warehouse_utils::FILE_NAME_GIT_SELF_IGNORE` |
| `.gitignore`, in a bay's own folder too | foreign | `warehouse_utils::FILE_NAME_GIT_SELF_IGNORE` |
| `config/`, `config/warehouse.toml` | rewritable | `config_utils::FOLDER_NAME_CONFIG_ROOT`, `config_utils::FILE_NAME_WAREHOUSE_CONFIG`, `scope_utils::FOLDER_NAME_CONFIG` |
| `config/warehouse.toml.lock` | lock | |
| `config/fetch-scope` | rewritable | `scope_utils::FILE_NAME_FETCH_SCOPE` |
| `taint/`, `taint/<name>` | rewritable | `taint_utils::FOLDER_NAME_TAINT` |
| `graph/`, `graph/<xx>` | derived | `globals::FOLDER_NAME_GRAPH_ROOT` |
| `bundles/`, `bundles/latest`, `bundles/partial` | derived | `bundle_utils::FOLDER_NAME_BUNDLES`, `bundle_utils::FILE_NAME_LATEST`, `bundle_utils::FILE_NAME_DOWNLOAD` |
| `store.lock`, `serve.lock`, `store-access.lock`, `refs.lock`, `mirror.lock`, `origin-move.lock` | lock | `lock_utils::FILE_NAME_STORE_LOCK`, `lock_utils::FILE_NAME_SERVE_LOCK`, `lock_utils::FILE_NAME_STORE_ACCESS_LOCK`, `lock_utils::FILE_NAME_REF_LOCK`, `lock_utils::FILE_NAME_MIRROR_LOCK`, `lock_utils::FILE_NAME_MOVE_LOCK` |
| `bays/`, `bays/<name>/`, `bays/<name>/path` | rewritable | `globals::FOLDER_NAME_BAYS_ROOT`, `bay_utils::FILE_NAME_BAY_PATH` |
| `{bay}inventory/`, `{bay}inventory/metadata` | rewritable | `globals::FOLDER_NAME_INVENTORY_ROOT`, `file_utils::FILE_NAME_INVENTORY_METADATA` |
| `{bay}inventory/inv_/`, `{bay}inventory/inv_/<xx>/`, `{bay}inventory/inv_/<xx>/<hash>/`, `{bay}inventory/inv_/<xx>/<hash>/data` | rewritable | `file_utils::FOLDER_NAME_INVENTORY_STORE`, `file_utils::FILE_NAME_INVENTORY_DATA` |
| `{bay}inventory/incomplete-load` | rewritable | `load_guard_utils::FILE_NAME_INCOMPLETE_LOAD_MARKER` |
| `{bay}position` | rewritable | `pallet_utils::FILE_NAME_POSITION` |
| `{bay}scope` | rewritable | `scope_utils::FILE_NAME_BAY_SCOPE` |
| `{bay}parked` | rewritable | `park_utils::FILE_NAME_PARKED` |
| `{bay}cherry-pick` | rewritable | `cherry_pick_utils::FILE_NAME_CHERRY_PICK` |
| `{bay}consolidation` | rewritable | `merge_utils::FILE_NAME_CONSOLIDATION` |
| `{bay}consolidation-skeleton` | rewritable | `merge_utils::FILE_NAME_SKELETON` |
| `{bay}operation` | rewritable | `operation_utils::FILE_NAME_OPERATION` |
| `{bay}journal.json` | rewritable | `journal_utils::FILE_NAME_JOURNAL` |
| `{bay}lock` | lock | `lock_utils::FILE_NAME_LOCK`, `franchise_utils::FILE_NAME_LOCK` |
| `{bay}assembly/**` | transient | `shift_utils::FOLDER_NAME_ASSEMBLY` |
| `redirect`, in a bay's own `.forklift` | rewritable | `bay_utils::FILE_NAME_BAY_REDIRECT` |
| `clock`, `exec-probe-<pid>-<n>` | probe | `file_utils::FILE_NAME_CLOCK_PROBE`, `file_utils::FILE_NAME_PREFIX_EXECUTABLE_BIT_PROBE` |
| `**/<name>.tmp~<pid>-<n>`, `**/<name>.tmp~<pid>-<n>/**` | transient | `warehouse_utils::FILE_NAME_STAGING_LOCK` |
| `.forklift`, `.forklift.new`, beside a working folder | outside | `globals::FOLDER_NAME_FORKLIFT_ROOT`, `bay_utils::FOLDER_NAME_FORKLIFT_NEW` |
| `.forklift-server.lock`, in a server's folder of warehouses | outside | `lock_utils::FILE_NAME_BASE_SERVE_LOCK` |
| `~/.forkliftconfig`, `~/.forklift-keys/`, `<key-id>.key`, `owners.toml`, in the home folder | outside | `config_utils::FILE_NAME_GLOBAL_CONFIG`, `sign_utils::FOLDER_NAME_KEYS`, `sign_utils::FILE_SUFFIX_PRIVATE_KEY`, `sign_utils::FILE_NAME_KEY_OWNERS` |
<!-- census:end -->

A walk matches a path only against the rows whose path is relative to `.forklift/`; a row that names
another place says so after its paths.

The versions of what this format writes are frozen with it, in
`crates/forklift/tests/fixtures/format-<N>/versions`: each record and object version constant and its
value, each string that carries a version in its text (the bundle header, the schemas of signed meta
cells), and each record kind or format name a header opens with, since renaming one makes every file
already written read as having none. Protocol versions are not frozen here: they are the wire's, not the
warehouse's; nor is the key file's magic, which is home-folder state. A test fails when the source's set differs from the current format's in any way. A changed
version is a format break, so it ships everything above, and a new snapshot beside the old ones,
which never change.

## The fixtures

`crates/forklift/tests/fixtures/format-<N>/` holds a warehouse frozen as this format wrote it, one per
mode (`full`, `sparse`, `cache`), plus a consolidation and a pick left in conflict and a served
warehouse with a tombstone and a bundle. The `operator` fixture beside them holds the global
configuration and keys of the enrolled operator who made them. A test opens each of the current
format's with the current build, reading every record it holds: it lists the parked work, audits the
trust, reads the scope, the tags and the office, prints a packed file, and finishes the consolidation
and the pick as that operator. It walks each against the census, before and after. For every older
format, it runs the upgrade on each and then does the same. Format 1 has no older formats, so that
loop is empty for now.

The fixtures are made by an ignored test, `freeze_the_current_format_fixtures`, which refuses to
replace a format's fixtures. A format's fixtures change only by being made again at a new format, once a
release has carried it: a format no release has carried has no warehouse to upgrade, so a change
to it edits its `versions` and makes its fixtures again in place, after deleting them. Two
things are not frozen: bays, because a bay's redirect holds an absolute path, and the out-of-scope
skeleton's reader, which only a partial checkout's consolidation reaches.

The AWS head's items are frozen the same way, against `AWS_FORMAT`:
`crates/forklift-aws-lambda/tests/fixtures/aws-format-<N>/items` lists the DynamoDB items a warehouse
holding a live pallet, a deleted one, the office pallet and a trust anchor is written as, on the
memory store. The current build must write exactly those items and serve them back, and the LocalStack
suite writes the same warehouse through DynamoDB and requires the very same items there, so the fixture
holds what DynamoDB stores rather than only the memory store's mapping. An older AWS format's items must
upgrade through the chain and then be served when the chain reaches that format, and must be refused
by both the gate and the upgrade, and left as they were, when it does not.
