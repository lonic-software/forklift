# State record format
Forklift keeps a handful of small local state files that each hold a list of names: warehouse
paths, pallet labels, filesystem paths. They share one on-disk form, the **state record**
(`record_utils` in `forklift-core`). None of them is ever tracked, pushed or fetched.

A tree entry name may hold any character but `/` and NUL, line breaks included, so an entry
cannot be written raw, one per line. A state record escapes each entry instead, and stays
readable by eye.

## Structure
```
/forklift-[kind] [version][NL]
[entry_1][NL]
[entry_2][NL]
...
digest [blake3][NL]
```
Where:
- `/forklift-[kind] [version]` is the header: the kind of file and the version of its format,
  e.g. `/forklift-marker 1`. It is always the first line, and only the first line is read as a
  header, so an entry is never mistaken for one, even an absolute path beginning `/forklift-`.
- `entry_n` is one entry, escaped: `\` is written `\\`, a line feed `\n` and a carriage return
  `\r`. No other escape exists. An escaped entry holds no line break, so a line is exactly one
  entry. An entry may be empty, which is an empty line.
- `digest [blake3]` is the last line, and is not an entry: the BLAKE3 hash of every byte
  before it, in lowercase hex. Damage that still parses, such as a flipped digit in a hash or a
  cut on a line boundary, then shows. This framing is frozen across versions.
- `NL` is an ASCII newline character (decimal value `10`), written after every line, the last
  one included.

## Reading
A reader checks the digest first, and refuses, never guessing at, a file that:
- ends in a digest line that does not match its bytes: it is damaged;
- has no digest line, and opens with the header of an older version of its kind: an older
  Forklift wrote it, before the record carried a digest;
- has no digest line otherwise: it does not open with the header of its kind at all, so it is
  not this kind of file or an older Forklift wrote it before the format was versioned, or it
  does, and was cut short;
- matches its digest but names a version of its kind the reader does not know: a newer
  Forklift wrote it;
- holds a raw carriage return, or a `\` that is not one of the three escapes, or is not UTF-8.

Only a record whose digest matches is told apart as a newer version's, so damage to a header
never passes for one.

Nothing is migrated. Each error names the file and what is wrong with it, and, where Forklift has
one, the way out. Every writer replaces its file atomically, so a crash never leaves one cut short.

## The records with an older digest
The warehouse marker, `parked`, and a server's `served`, `tombstones` and `withheld` carried
the digest first. The marker names what an older marker of its own lacked (see [the warehouse
format](WAREHOUSE_FORMAT.md)).

## Files
| File | Kind | Entries |
|------|------|---------|
| `cherry-pick` (bay-local) | `cherry-pick` | The parcel being picked, the head and then the pallet it is applied to, then the description the completing `stack` defaults to, when there is one. |
| `consolidation` (bay-local) | `consolidation` | The head parcel hash of the pallet being consolidated in, then its name; the head parcel hash and the name of the pallet consolidated into; then the blake3 digest, in lowercase hex, of the whole `consolidation-skeleton` file written with it. A skeleton whose bytes do not match it is refused as damaged. |
| `consolidation-skeleton` (bay-local) | `consolidation-skeleton` | One out-of-scope entry of a scoped merge per line: `S <code> <hash> <path>` or `D <path>`. The whole line is the escaped entry. |
| `cherry-pick-copy`, `consolidation-copy`, `consolidation-skeleton-copy` (bay-local) | as the record | A copy of the record of the same name without `-copy`, written before it on every write (and removed again when the record's own write fails) and removed after it, so while the record exists its copy holds the bytes of its last write, or of a write that stopped before reaching it. Only `heal` reads a copy, to write a damaged record back; a copy with no record beside it is never read as a merge in progress. |
| `inventory/incomplete-load` (bay-local) | `marker` | The warehouse path key of each load root left incomplete; the root is the empty entry. |
| `inventory/metadata` (bay-local) | `inventory-metadata` | The warehouse path key of every directory with an inventory shard; the root is `./`. |
| `redirect` (in a bay's own `.forklift` folder) | `bay` | The warehouse root, then the bay's name. Its presence is what makes the folder a bay's rather than a warehouse's; its header is how reading one refuses a stray file instead of misreading it as a bay. |
| `parked` (bay-local) | `parked` | One parked parcel hash per entry, the most recently parked last. |
| `path` (in a bay's state folder) | `bay-path` | The bay's working directory. |
| `position` (bay-local) | `position` | The current pallet's name, then the bay's base: the parcel its files stand on, empty while the pallet is unborn. See [pallet refs](PALLET_REFS.md). |
| `scope` (bay-local), `.forklift/config/fetch-scope` | `scope` | See [the scope file format](SCOPE_FILE_FORMAT.md), which says which of these files must exist. |
| `.forklift/warehouse` | `warehouse` | `format N`, then what the warehouse holds, `full`, `sparse` (cloned with `--only`) or `cache` (cloned with `--cache`), then an optional `upgrading M S`. This framing is frozen; see [the warehouse format](WAREHOUSE_FORMAT.md). `prepare` writes it last, so its presence marks a complete warehouse. A marker missing an entry, with an extra one, or with a word its own format says this build should know and does not, is refused, and so is a marker of kind version 1 or 2, which recorded no format: make the warehouse again. A sparse warehouse's fetch scope must agree with it; see [the scope file format](SCOPE_FILE_FORMAT.md#the-fetch-scope-and-the-warehouse-mode). |
| `.forklift/served` | `served` | None. Its presence marks a warehouse a `forklift-server` has served; nothing removes it. A mark that exists but cannot be read is an error, never "not served". |
| `.forklift/tombstones` | `tombstones` | One `<head> <pallet>` per deleted pallet of a served warehouse: the head its ref held, then its name; or `<head>` alone, a head kept with no name. |
| `.forklift/withheld` | `withheld` | One `<hash> <digest> <statement>` per object a served warehouse withholds: the object, its chunk-list digest or `-`, then the takedown's statement. |
| `.forklift/redactions-applied` | `redactions-applied` | One redacted object hash per applied redaction, sorted. It only grows: an applied redaction is never taken back. An entry that is not an object hash is refused, like a header of another version, and such a record is never written over. |

Bay-local files live in the bay's state folder: `.forklift/` for the main tree,
`.forklift/bays/<name>/` for a bay. Paths in the `bay` and `bay-path` records are filesystem
paths, which must be UTF-8; a bay at a path that is not is refused.

Taint files (`.forklift/taint/`) are state records of kind `taint`, one path per entry, but are
read differently: one that does not match its digest is not refused but counts as torn, which
makes the next recovery check everything.

## Not state records
Some other line-per-entry files keep formats of their own, because no entry of theirs can hold
a line break:
- **Pallet refs** hold a hash; pallet and tag names are validated and cannot hold a line break.
