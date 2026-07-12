# Directory entry type codes
Each directory entry type has a specific code. \
A directory entry is an entry inside a directory (i.e. a file or a subdirectory).

## Codes
You can find the codes of individual directory entry types in the table below.

| Code | Item type           | Description                                             |
|------|---------------------|--------------------------------------------------------|
| `1`  | `Normal`            | A normal file.                                         |
| `2`  | `Executable`        | An executable file.                                    |
| `3`  | `SymbolicLink`      | A symbolic link.                                       |
| `4`  | `Tree`              | A subtree (i.e. a directory).                          |
| `5`  | `NormalChunked`     | A normal file stored chunked (hash names a recipe).    |
| `6`  | `ExecutableChunked` | An executable file stored chunked (hash names a recipe). |

Codes `5` and `6` are new **legal values** of the existing entry type field — the tree object
byte layout is unchanged (no `TREE_OBJECT_FORMAT` version bump). A binary built before chunk
support rejects these codes when parsing a tree (its `from_code` returns an error), so it fails
loudly on any directory that directly lists a chunked file rather than mis-reading it. A symlink
is never chunked (its target is a tiny path string), so there is no chunked symlink code.