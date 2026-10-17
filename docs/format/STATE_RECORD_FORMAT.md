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
  e.g. `/forklift-marker 1`. No entry of any of these files can begin with `/`, since path keys
  are relative, so the header is never mistaken for an entry.
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

Nothing is migrated. Each file's error says how to get out of the state it describes.

## Files
| File | Kind | Entries |
|------|------|---------|
| `consolidation` (bay-local) | `consolidation` | The head parcel hash of the pallet being consolidated in, then its name. |
| `consolidation-skeleton` (bay-local) | `consolidation-skeleton` | One out-of-scope entry of a scoped merge per line: `S <code> <hash> <path>` or `D <path>`. The whole line is the escaped entry. |
| `inventory/incomplete-load` (bay-local) | `marker` | The warehouse path key of each load root left incomplete; the root is the empty entry. |
| `inventory/metadata` (bay-local) | `inventory-metadata` | The warehouse path key of every directory with an inventory shard; the root is `./`. |
| A bay's `.forklift` file | `bay` | The warehouse root, then the bay's name. Its header is how discovery tells a bay from a stray file named `.forklift`. |
| `path` (in a bay's state folder) | `bay-path` | The bay's working directory. |
| `scope` (bay-local), `.forklift/config/fetch-scope` | `scope` | See [the scope file format](SCOPE_FILE_FORMAT.md), which adds an empty-file rule of its own. |

Bay-local files live in the bay's state folder: `.forklift/` for the main tree,
`.forklift/bays/<name>/` for a bay. Paths in the `bay` and `bay-path` records are filesystem
paths, which must be UTF-8; a bay at a path that is not is refused.
