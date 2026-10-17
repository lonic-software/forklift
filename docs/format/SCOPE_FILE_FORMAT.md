# Scope file format
This format records a **task-scoped sparse-workspace scope** (§7.6): the subtree path
prefixes a scoped bay materializes and operates on. It is used by two files:

- **Bay materialization scope** — `.forklift/bays/<name>/scope` (bay-local). What a scoped
  bay checks out, stages and stacks.
- **Warehouse fetch scope** — `.forklift/config/fetch-scope` (shared across bays). What the
  warehouse has fetched at all. The bay materialization scope is always a subset (⊆) of the
  fetch scope. In the materialization-only (stage 1) release the store holds everything, so
  the fetch-scope file is normally absent (= full).

Both files are **local only and never tracked**: scope is a property of *this* checkout, not
of the project, so it is never written into a parcel, pushed to a remote, or imposed on
collaborators. A plain bay (or the main tree) has no scope file at all.

## Structure
A UTF-8 text file: a format header line, then one in-scope path prefix per line:
```
/forklift-scope 2[NL]
[prefix_1][NL]
[prefix_2][NL]
...
```
Where:
- `/forklift-scope 2` names the format. It opens with `/`, which no stored prefix can, so it
  cannot be mistaken for a prefix. A full scope is written as the empty file, with no header.
- `prefix_n` is a **warehouse path key**: `/`-separated, relative to the warehouse root,
  with no leading or trailing `/` and no `.`/`..` components (e.g. `src/api`). A prefix names
  a directory; everything at or under it is *in scope*.
- `NL` is an ASCII newline character (decimal value `10`), written after every prefix
  (including the last).

## Semantics
- An **absent file, or a file with no prefixes**, means **full scope** — no restriction, the
  whole tree is in scope. This is what makes a scoped bay opt-in: without a scope file, every
  scope-aware operation behaves exactly as an unscoped workspace.
- A prefix of the empty string (the warehouse root) is equivalent to full scope.
- On read, blank lines and surrounding whitespace are ignored; prefixes are trimmed of
  surrounding `/`, de-duplicated and sorted, giving a canonical on-disk form.
- A non-empty file whose first line is not the header is the unversioned format 1, written
  before the root ignore file joined every scope, and is **refused** — read under format 2's
  meaning, the never-inventoried ignore file would show as removed and the next stack would
  commit the removal. A header naming any other version is refused too. Neither is migrated:
  franchise the remote again.

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
