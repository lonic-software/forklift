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
| `upgrading` present | `warehouse_upgrade_incomplete` | 42 | Run the upgrade again, with a build that reads the target format. |
| Older than this build | `warehouse_format_older` | 40 | Run the upgrade. |
| Newer than this build | `warehouse_format_newer` | 41 | Update the binary. Upgrades are one-way. |

A `--warehouses` server answers `503` for such a warehouse and serves the others. The upgrade command
itself is not built yet; the refusals name it ahead of it. No warehouse can reach the first or third
row without a hand-edited marker until a second format exists.

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
