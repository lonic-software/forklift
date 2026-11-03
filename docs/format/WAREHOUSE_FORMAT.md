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
| Older | is refused | The operator runs `forklift-aws-upgrade`. |
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

A state left by another run is refused unless the operator passes `--takeover`, after making sure that
run has stopped. Takeover puts a new owner in the state and resumes at the first step not done, so
steps must be idempotent. Steps change only the warehouse's own DynamoDB partition: the bucket's
`objects/` and `signatures/` keys are shared by every warehouse and named by their content, and
`staging/` and `responses/` are transient. An S3 layout change has no in-place path.

A format break on a deployment refuses every warehouse until each is upgraded. The AWS head's chain is
empty as well: format 1 is its first format.

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
