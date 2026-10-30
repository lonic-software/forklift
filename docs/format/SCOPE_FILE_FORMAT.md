# Scope file format
This format records a **task-scoped sparse-workspace scope** (§7.6): the subtree path
prefixes a checkout materializes and operates on. It is used by three files:

- **Bay materialization scope** — `.forklift/bays/<name>/scope` (bay-local). What a bay checks
  out, stages and stacks. Every bay has one, an unscoped bay's naming every path.
- **Main-tree materialization scope** — `.forklift/scope`. The same for the warehouse's main
  working tree; only a sparse warehouse's main tree has one.
- **Warehouse fetch scope** — `.forklift/config/fetch-scope` (shared across bays). What the
  warehouse has fetched at all. Every materialization scope is a subset (⊆) of the fetch
  scope. Only a warehouse whose [marker](STATE_RECORD_FORMAT.md#files) records it as
  `sparse` has one; see [the fetch scope and the mode](#the-fetch-scope-and-the-warehouse-mode).

Both files are **local only and never tracked**: scope is a property of *this* checkout, not
of the project, so it is never written into a parcel, pushed to a remote, or imposed on
collaborators. Which of them must exist is set out [below](#which-scope-files-must-exist).

## Structure
A [state record](STATE_RECORD_FORMAT.md) of kind `scope`: a format header line, then one
in-scope path prefix per line, escaped:
```
/forklift-scope 3[NL]
[prefix_1][NL]
[prefix_2][NL]
...
```
Where:
- `/forklift-scope 3` names the format. It opens with `/`, which no stored prefix can, so it
  cannot be mistaken for a prefix. A full scope is written as the empty file, with no header.
- `prefix_n` is a **warehouse path key**: `/`-separated, relative to the warehouse root,
  with no leading or trailing `/` and no `.`/`..` components (e.g. `src/api`). A prefix names
  a directory; everything at or under it is *in scope*. It is escaped as every state record
  entry is: `\` as `\\`, a line feed as `\n`, a carriage return as `\r`, since a directory name
  may hold any of them.
- `NL` is an ASCII newline character (decimal value `10`), written after every prefix
  (including the last).

## Semantics
- An **empty file** means **full scope** — no restriction, the whole tree is in scope, and
  every scope-aware operation behaves exactly as in an unscoped checkout. An **absent** file is
  never read as full on its own account: whether each file may be absent, or empty, is decided
  by whose scope it is and by the warehouse mode ([below](#which-scope-files-must-exist)).
- A header followed by no prefix is **refused**, and so is one with a root prefix (an empty
  string or `/`) anywhere among its prefixes, since the root takes in everything. Forklift
  never writes either: the full scope is the empty file.
- Prefixes are trimmed of surrounding `/`, de-duplicated and sorted, giving a canonical
  on-disk form. Whitespace is part of a prefix: a directory name may begin or end with a space.
- A non-empty file whose first line is not the header is the unversioned format 1, written
  before the root ignore file joined every scope, and is **refused** — read under today's
  meaning, the never-inventoried ignore file would show as removed and the next stack would
  commit the removal. A header naming any other version is refused too, format 2 included,
  which wrote prefixes raw, so a prefix holding a line break read back as two. Nothing is
  migrated.
  The way out depends on the file: for the fetch scope or the main tree's scope, clone
  the remote again, into a new directory; for a bay's scope, remove the bay with `bay remove`
  and add a new one under another name (a removed bay keeps its pallet, so its name stays
  taken; copy any uncommitted work across by hand).

## Which scope files must exist
A file is checked against these rules each time it is read, and every refusal names the way
out.

| File | Full or cache warehouse | Sparse warehouse | Way out |
|---|---|---|---|
| Fetch scope | absent | present, naming a prefix | absent where required: clone again; present where not: delete it |
| Main-tree scope | absent | present, naming a prefix, ⊆ the fetch scope | the same |
| Bay scope | present, any scope | present, naming a prefix, ⊆ the fetch scope | remove the bay, add a new one under another name |

The main tree's scope follows the mode, as the fetch scope does: only a sparse clone
scopes the main tree, and `narrow` cannot scope a checkout that holds every path. A bay's scope
is the bay's own, since a full warehouse may hold a scoped bay, so `bay add` writes it for every
bay, the empty file for an unscoped one, and a bay without one is refused in every mode. In a
sparse warehouse `bay add` without `--scope` is refused before any bay state is made.

### The fetch scope and the warehouse mode
The warehouse marker records whether the warehouse is `full`, `sparse` or a `cache`, and the
fetch-scope file must agree with it. A full warehouse holds every path and a cache fetches
content as it needs it, so neither has a fetch-scope file. A sparse warehouse
must have one, naming at least one prefix. Reading the fetch scope refuses every other
pairing:

- A sparse warehouse whose file is missing or empty is never read as full, since it would then
  claim content it never fetched. The way out is to clone the remote again, into a new
  directory.
- A full warehouse or a cache with a fetch-scope file is refused whatever the file holds. Only
  a hand edit puts one there, and the way out is to delete it: the mode, not the file, says
  what the warehouse holds.

`prepare` is the only writer of the mode. When it makes a sparse warehouse, which only
`clone --only` asks for, it writes the fetch scope and the main tree's scope before the
marker, so no warehouse is ever marked sparse without them. `expand` and `scope-prune` rewrite
the fetch scope afterwards; a fetch scope can be written only into a warehouse already marked
sparse.

Against a scope, every user-pallet content path is classified three ways (the classifier the
sparse walks branch on):

- **In scope** — the path is at or under an in-scope prefix, or is the root `.forkliftignore`,
  which every non-full scope holds so that a sparse checkout always has the rules that apply
  to it. Fully materialized: descended, staged, verified and merged exactly as a full
  workspace would. The ignore file is not a declared prefix, so a scope never requires it to
  be a directory.
- **Spine** — the path is a strict ancestor of an in-scope prefix (so the root is always
  spine in a scoped bay). Walked, but its *other* entries at that level are out of scope and
  carried forward by hash, never descended.
- **Out of scope** — neither. Sealed by the hash already committed in the parent spine tree
  object: never loaded, never materialized.

Meta pallets (`@office` and `.forklift/meta/*`) are **never** scoped — the scope files govern
user-pallet content only.
