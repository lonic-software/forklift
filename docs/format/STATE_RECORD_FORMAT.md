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
```
Where:
- `/forklift-[kind] [version]` is the header: the kind of file and the version of its format,
  e.g. `/forklift-marker 1`. It is always the first line, and only the first line is read as a
  header, so an entry is never mistaken for one, even an absolute path beginning `/forklift-`.
- `entry_n` is one entry, escaped: `\` is written `\\`, a line feed `\n` and a carriage return
  `\r`. No other escape exists. An escaped entry holds no line break, so a line is exactly one
  entry. An entry may be empty, which is an empty line.
- `NL` is an ASCII newline character (decimal value `10`), written after every line, the last
  one included.

## Reading
A reader refuses, and never guesses at, a file that:
- does not open with the header of its kind — an older Forklift wrote it, before the format was
  versioned, or it is not this kind of file at all;
- names a version of its kind the reader does not know;
- ends without a newline, so it was cut short;
- holds a raw carriage return, or a `\` that is not one of the three escapes;
- is not UTF-8.

Nothing is migrated. Each error names the file and what is wrong with it, and, where Forklift has
one, the way out. Every writer replaces its file atomically, so a crash never leaves one cut short.

## Files
| File | Kind | Entries |
|------|------|---------|
| `consolidation` (bay-local) | `consolidation` | The head parcel hash of the pallet being consolidated in, then its name. |
| `consolidation-skeleton` (bay-local) | `consolidation-skeleton` | One out-of-scope entry of a scoped merge per line: `S <code> <hash> <path>` or `D <path>`. The whole line is the escaped entry. |
| `inventory/incomplete-load` (bay-local) | `marker` | The warehouse path key of each load root left incomplete; the root is the empty entry. |
| `inventory/metadata` (bay-local) | `inventory-metadata` | The warehouse path key of every directory with an inventory shard; the root is `./`. |
| `redirect` (in a bay's own `.forklift` folder) | `bay` | The warehouse root, then the bay's name. Its presence is what makes the folder a bay's rather than a warehouse's; its header is how reading one refuses a stray file instead of misreading it as a bay. |
| `path` (in a bay's state folder) | `bay-path` | The bay's working directory. |
| `position` (bay-local) | `position` | The current pallet's name, then the bay's base: the parcel its files stand on, empty while the pallet is unborn. See [pallet refs](PALLET_REFS.md). |
| `scope` (bay-local), `.forklift/config/fetch-scope` | `scope` | See [the scope file format](SCOPE_FILE_FORMAT.md), which adds an empty-file rule of its own. |
| `.forklift/warehouse` | `warehouse` | Exactly one: what the warehouse holds, `full`, `sparse` (franchised with `--only`) or `cache` (franchised with `--cache`). `prepare` writes it last, so its presence marks a complete warehouse. A marker with no entry, more than one, or a word this build does not know is refused, and so is a marker of kind version 1, which recorded no mode: make the warehouse again. A sparse warehouse's fetch scope must agree with it; see [the scope file format](SCOPE_FILE_FORMAT.md#the-fetch-scope-and-the-warehouse-mode). |
| `.forklift/served` | `served` | None. Its presence marks a warehouse a `forklift-server` has served; nothing removes it. A mark that exists but cannot be read is an error, never "not served". |
| `.forklift/tombstones` | `tombstones` | One `<head> <pallet>` per deleted pallet of a served warehouse: the head its ref held, then its name. |

Bay-local files live in the bay's state folder: `.forklift/` for the main tree,
`.forklift/bays/<name>/` for a bay. Paths in the `bay` and `bay-path` records are filesystem
paths, which must be UTF-8; a bay at a path that is not is refused.

Taint files (`.forklift/taint/`) escape their paths the same way under a `/forklift-taint 1`
header, followed by an `END` line, but are read differently: a taint file that cannot be fully
read is not refused but counts as torn, which makes the next recovery check everything.

## Not state records
Some other line-per-entry files keep formats of their own, because no entry of theirs can hold
a line break:
- **`parked`** (bay-local) lists object hashes.
- **Pallet refs** hold a hash; pallet and tag names are validated and cannot hold a line break.
