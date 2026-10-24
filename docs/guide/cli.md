# The forklift CLI — user guide

Every command forklift ships, organized by what you're trying to do. If you know
git, the [terminology map](README.md#the-warehouse-model-in-one-minute) will get
you oriented fast.

Conventions in this guide:
- `forklift <command>` — most commands have a short alias, shown in the reference
  table at the end and in each command's `forklift help <command>`.
- Three global flags apply to every command: `-v`/`--verbose` (more detail),
  `--json` (machine output — see [Machine & agent output](#machine--agent-output)),
  and `--no-pager` (see [Paging](#paging) below).
- `forklift help` lists everything; `forklift help <command>` explains one command
  (subcommands work too: `forklift help office admit`).

Contents:
1. [Getting started](#1-getting-started)
2. [Tracking changes](#2-tracking-changes)
3. [Inspecting history](#3-inspecting-history)
4. [Branching and merging](#4-branching-and-merging)
5. [Parking work in progress](#5-parking-work-in-progress)
6. [Working with remotes](#6-working-with-remotes)
7. [Identity, signing, and agents](#7-identity-signing-and-agents)
8. [Machine & agent output](#8-machine--agent-output)
9. [Configuration reference](#9-configuration-reference)
10. [Command & alias reference](#10-command--alias-reference)
11. [Exit codes](#11-exit-codes)

---

## 1. Getting started

### `prepare` — create a warehouse

```sh
forklift prepare
```

Creates `.forklift/` in the current directory: the object store, the inventory,
the config folder, the pallets folder, the default pallet `main` (unborn until
your first `stack`) and a warehouse config file. It writes no `.forkliftignore`,
as `git init` writes no `.gitignore` — see [`.forkliftignore`](#forkliftignore--files-forklift-shouldnt-track).
Idempotent —
running it in an existing warehouse only creates what's missing. `-v` lists each
piece created.

The last thing `prepare` writes is `.forklift/warehouse`, a marker saying the folder
is a complete warehouse; running `prepare` in a warehouse that has no marker yet adds
one. A new `.forklift/` (or one that is there but empty) is built under the name
`.forklift.tmp~<pid>-<n>` and renamed into place when it is complete, so an interrupted
`prepare` leaves no half-built warehouse, only that folder. It is never tracked, and
the next `prepare` in the same directory removes it; one it cannot tell is abandoned
is left for you to delete.

### `import-git` — migrate a git repository

```sh
cd my-git-repo
forklift prepare
forklift import-git .
```

One-way migration of a git repo's history into the warehouse: git commits become
parcels, trees become trees, blobs become blobs, and each **local branch becomes a
pallet** (git's author/committer split maps onto forklift's author/stack). A branch's pallet
name is its name lowercased, with every character a pallet name does not allow replaced by `-`,
and any part that would then start with `-` prefixed with `branch` (`@wip` becomes `branch-wip`).
Before anything is converted, the import refuses, naming the branches, when a mapped name is
still not a valid pallet name (a branch `con`, or a part ending in `.`), when two branches map to
one name (`Feature` and `feature`), when one mapped name would be a folder of another (`a+b` and
`a-b/x`), or when the name is already a pallet here or nests with one. Git's HEAD
branch is checked out, and `.git/` is added to `.forkliftignore` (creating the file if
there is none) so a colocated repo reads clean, unless a rule there already matches `.git`, which
it then names. The rule is added before the inventory is built, so an import interrupted there and
finished by `heal` has it too. An ignore file Forklift refuses to read is left as it is, with a warning. The edit is left in the working tree,
not stacked: `load .forkliftignore` and stack it with your next parcel. Requires `git` on your PATH. The imported history is **unsigned** (it
predates trust) — import into a fresh warehouse, then `office enroll` to anchor it as the
legacy boundary (a later `audit` then tolerates the imported parcels as legacy). Submodules
(gitlinks) are skipped with a warning. A file or directory name that is not valid UTF-8, or that
is not one path component (`..`, a name containing `/`), stops the import with an error naming it
and its directory, rather than being imported under a different path; so does an entry at any
depth that some file system opens as `.forklift` (`.Forklift`, `.forklift.`). For agents, this means a project can be moved onto
forklift with one command.

Forklift stores a file's bytes as they are and converts nothing on checkout, so the history
keeps the bytes git stored. Git rewrites some files as it checks them out: LF to CRLF under
`eol=crlf`, under `core.autocrlf=true` (Git for Windows' default, which converts every text
file), or under `text` with `core.eol=crlf`; an `ident` expansion; a smudge `filter` such as Git
LFS; a `working-tree-encoding`; and, under `core.symlinks=false`, a symlink written as a plain
file. Such a file does not hold its imported content. The import checks every working file
against the imported tree, so such a file reads `modified` from the first `stocktake`, the same
way an uncommitted edit does, and the import warns, naming the files (`--json` lists them all as
`unheld`). Like any unstaged change, it must be settled before a command that needs a clean
working tree, in one of two ways: `load` and stack it to track the file as checked out (under
`core.autocrlf=true` that stacks every text file with CRLF line endings), or, for a file you did
not edit, `restore` it to write the imported content back, discarding what the file holds (a
colocated git checkout may then list it as modified; a symlink is restored only where one can be
created). The import does not refuse such a repository: git.git itself converts its `.bat`
files.

A large history lands hundreds of thousands of objects at once — the case the loose store
is slowest and largest in — so the imported objects are written **straight into native
packs** ([`compact`](#compact--pack-the-object-store)'s format, without the detour through
one loose file per object), delta-compressing successive versions of files and directory
trees on the way in. The store arrives dense and you never have to remember to `compact`
it. Pass `--no-compact` to store loose objects instead (e.g. to inspect the raw store or
benchmark the loose baseline). Because that packing only ever sees one file's own history as
it streams in, `import-git` tips a one-line suggestion (also visible from `store`) that a
one-shot `compact --all --redelta` could shrink the result further by delta-compressing
across the whole store, including similarity a per-path pass cannot see.

Refuses in a scoped (sparse) bay (see `bay add --scope` below): importing builds
every pallet's history straight from the git tree, bypassing the sparse overlay entirely,
and materializes the whole imported HEAD — not a sensible operation to scope. Run it from
a full workspace.

### `export-git` — migrate back to git

```sh
forklift export-git ../my-repo-in-git
```

The reverse of `import-git`, and the **escape hatch**: parcels become commits, trees become
trees, blobs become blobs, and each **user pallet becomes a branch** (git's HEAD is set to
the current pallet, and its working tree is checked out). The point is that trying forklift is
reversible — your code history is never trapped. Requires `git` on your PATH; the target path
must be empty or new. **Lossy in this direction:** git has no home for the signed office, the
`@manifest` (approvals/provenance) or per-parcel signatures, so those are dropped. Author
identity round-trips as the on-chain **identifier** — forklift never stores display names
on-chain (zero-PII by design), so the git author name is that identifier. The meta pallets
(`@office`, `@manifest`) are not exported.

### `config` — who you are, and settings

Identity is zero-configuration: if you never set anything, an opaque operator id
(a UUID) is minted on first use and your history is pseudonymous. To be
recognizable, set a name:

```sh
forklift config operator.name "Ada Lovelace"
```

`config` has four forms:

```sh
forklift config                       # list every known key and its value
forklift config <key>                 # print one value
forklift config <key> <value>         # set a value (warehouse scope)
forklift config --unset <key>         # remove a value
```

- Add `--global` to target your per-user config (`~/.forkliftconfig`) instead of
  the warehouse. Reads consult the warehouse first, then fall back to global.
- Unknown keys are rejected (a typo can't silently do nothing).
- The known keys are listed in [Configuration reference](#9-configuration-reference).

### `.forkliftignore` — files forklift shouldn't track

Create a `.forkliftignore` at the warehouse root when you need one; `prepare`
does not write it. Each line is a **regex** (not a glob) matched against
warehouse-relative, `/`-separated paths. Blank lines and lines starting with `#`
are skipped; a `#` anywhere else is part of the pattern, so put comments on their
own lines:

```
# a folder called target (not targets.txt)
^target(/.*)?$
# any .log file
\.log$
```

The file is **tracked content**, like `.gitignore`: `load` and `stack` it, and its
rules travel with the history — a `franchise` or `lower` brings the remote's rules
along. Only `.forklift/` and the `.forklift.tmp~<pid>-<n>` folders an interrupted
`prepare` or `franchise` leaves (at any depth) are always ignored. `import-git` adds a rule for `.git/` when
the warehouse holds a `.git` folder. The rules are read only from the file on disk at
the warehouse root. Every sparse scope holds that file, whatever its prefixes: a
`franchise --only` checkout fetches and materializes it, so its rules apply there too and
an edit to it stacks like any in-scope file.

Forklift refuses to read the file, and stops, when it is a symbolic link, when a line
is a leftover conflict marker (consolidating two edits of the file writes them), when
a pattern holds a control or invisible character (a tab, a carriage return, a
zero-width space, a soft hyphen, a bidirectional override; a joiner between two
characters of an emoji or a Persian or Indic name is fine, as is a flag emoji),
when a pattern starts or ends with whitespace, or when a pattern matches the empty
string (`.*`, `x*`, `a|`): such a pattern matches every path. Whitespace around a
pattern would be part of the regex, so `^build$ ` never matches `build`; write a
space the pattern needs at its edge as `\x20`, and a tab as `\t`. A space inside a
pattern is fine. The error names the character's code point and its escape, so an
invisible one can be found. Save the file with newline (or CRLF) line endings; a
byte order mark at its start is skipped.
`import-git` leaves a refused ignore file as it is and says why.

As in git, a rule covers only **untracked** paths. A path inside a folder a rule
covers is covered too, even where no rule matches the path itself. A tracked path
stays tracked whatever the rules say, so its edits keep showing in `stocktake`, and
`shift` and the other pallet switches refuse to overwrite them. Because the rules
travel, this matters when someone else stacks a rule over files you track. To stop
tracking a path a rule now covers, `remove` it and stack: the file stays on disk and
is ignored from then on.

Naming a file in `load` tracks it despite a rule. Loading a folder (`load .`,
`load build`) never adds content the rules cover. It still refreshes tracked files
inside that folder. Nothing under `.forklift/` can be loaded at all.

Anchor a folder rule at both ends. `^target\/?.*$` also matches `targets.txt` and
`target-notes/`, so an unanchored rule can hide new files you meant to load.

### `alias` — the `fl` short name

Forklift ships a short `fl` alias (`f` and `l` are both left-hand home row —
quicker to type than `git`). It is a **shell-agnostic symlink** next to the
`forklift` binary (a `.cmd` shim on Windows, where a symlink needs elevated
privilege), not a per-shell alias — it works from scripts, non-interactive
shells and every shell dialect alike, and one `ls`/`rm` finds or removes it.

Every installer (the curl script, `pult install`, and self-update — see
[`self-update`](#self-update--check-for-and-apply-a-newer-release)) creates it
by default, calling the same command under the hood:

```sh
forklift alias install            # create "fl" next to this binary
forklift alias install fl2        # create a differently-named alias instead
forklift alias uninstall          # remove it
forklift alias                    # (or "alias status") report whether it's installed, and where
```

`install` is idempotent (an alias that already points at this binary succeeds
without change) and refuses — deliberately, with no `--force` — to overwrite
anything it did not create: a real file already named `fl`, or a symlink
pointing somewhere else. `uninstall` is a no-op if nothing is installed, and
also refuses a foreign real file, though it does remove a symlink/shim
pointing elsewhere (deleting a symlink can never lose data). Set
`FORKLIFT_NO_ALIAS=1` before installing to skip alias creation — there is no
interactive prompt (`curl … | sh` has no TTY to answer one).

---

## 2. Tracking changes

The cycle is: change files → **load** them into the inventory → **stack** a
parcel. `stocktake` shows you where you are; `diff` shows the actual lines;
`restore` throws changes away.

### `load` — stage changes (`l`)

```sh
forklift load .                # stage everything under the current directory
forklift load src/main.rs      # stage one file
forklift load src              # stage a directory
```

`load` reconciles each directory against its inventory shard: new files become
tracked, modified files are re-hashed, deleted files are staged as removals. It
is incremental — unchanged files are recognized by their stat data and never
re-read. `load` (like every mutating command) holds the warehouse lock while it
runs, so two forklift processes never interleave.

`load` records a small marker (beside the inventory) before it starts walking, and
clears it only once that walk finishes cleanly — so a load that fails partway (or is
interrupted outright, e.g. a crash) leaves it behind. Several independent failed loads
over different paths are all recorded at once — healing one never silently clears
another. `stack` and `park` both check for these markers and refuse while any is
present (see below), rather than silently commit whatever happened to make it into the
inventory. Two remedies clear an affected marker: re-running `load` over the same
path, or `restore --staged` (or `unload`) over a path that covers it — the second one
abandons the incomplete load instead of finishing it. Any command that resets the
whole staging area (`shift`, `park`'s own reset back to the pallet head, and the
like) clears every recorded marker at once, the same way. A directory that failed
before anything of its own ever got staged has nothing to reset — `restore --staged`
over it still succeeds, as a no-op that just clears the marker.

A load refuses outright when a folder's stored inventory cannot be read (a truncated file
after a crash, say), rather than rebuilding it: the inventory is what says which files are
tracked, and a rebuild would quietly stop tracking any file an ignore rule covers.
`restore --staged <folder>` resets the inventory of that folder and every folder below it to
the pallet head, unstaging everything staged there; the error names the exact command. Then
load the folder again, and load by name any file an ignore rule covers that you had loaded by
name: a folder load never adds those.

Forklift never makes a link inside the staging area (`.forklift/inventory` may itself be a link,
for an inventory moved elsewhere). A link found inside it, whether at a folder's stored inventory,
at any folder above that, or in place of the inventory metadata file, is treated as damage. It is
never followed, even when what it points at looks valid: following it would take another folder's
state, or another staging area's list of folders, for this one's, or write outside the staging
area. Every command that reads it refuses and names the link. Remove the link, then run
`restore --staged .` to reset the staging area to the pallet head. When the refusal comes from
`gc` or `heal`, which read every bay's staging area, it names the checkout the staging area
belongs to; run the reset there.

The inventory folder itself is different. If `.forklift/inventory` is a link whose target is out of
reach, such as a drive that is not mounted, Forklift refuses rather than treat the staging area as
empty. The way out is to make the target available again, not to reset: a reset would abandon
everything staged there.

A folder below the warehouse root that holds its own `.forklift` (say you ran `prepare` in
`vendor/lib`) is another working tree: commands run inside it use that warehouse, not this one.
`load .` leaves it out entirely, `stocktake` lists it once as an untracked `vendor/lib/`, and
`load vendor/lib` or a path inside it refuses. If this warehouse already tracks that folder,
`load` and `stocktake` refuse and name the way out: `forklift remove vendor/lib`, which keeps the
files on disk, then `stack` (re-run the `load` first if a `load` was what refused). Or move the
inner `.forklift` out of the folder.

A path you name must be spelled the way its folder lists it, as it would on a case-sensitive file
system. On a case-insensitive one (the default on macOS and Windows), or one that ignores Unicode
normalization, `load readme` would otherwise open a file listed as `README` and stage a second
entry for it. `load` refuses such a path, names the spelling the file system opens instead, and
gives the one way to stage it. (Beneath a folder you may enter but not list, the spelling cannot be
checked, so the path is taken as typed.)

- For a typo, load the listed spelling (`forklift load README`).
- After a case-only rename you have not loaded yet (`README` renamed to `readme`), naming the old
  spelling offers `forklift remove README`, then `forklift load readme`. That stages just the
  rename and leaves your other changes as they were.
- When the renamed name is a folder (`Docs` to `docs`), it offers loading the nearest folder above
  it whose spelling is listed: `forklift load .` from the working tree's root for a top-level
  folder. That stages every change under that folder, unstaged edits and untracked files included,
  so check `stocktake` before you stack.

The inventory can still come to hold two spellings of one entry, for instance when a folder is
renamed again after its contents were loaded, or when both spellings are staged and the file is
then deleted. `stack` and `park` refuse it before recording anything, because no command could
write such a parcel back on this file system. The refusal names the pair nearest the root and the
same kind of way out: `forklift remove` for each spelling the folder does not list, then
`forklift load` of the one it does, or `forklift load .` in the nearest listed folder. Then run
the command again.

### `unload` — unstage (`ul`)

```sh
forklift unload src/main.rs
```

The inverse of `load`: resets the path's inventory entries to the pallet head,
so the next parcel won't record the staged change. The working directory is
**not** touched — your edits stay, they're just no longer staged. (`restore
--staged` does the same thing; `unload` is its natural name.) To stage a
removal instead, use `remove`.

### `remove` — stage a removal (`rm`)

```sh
forklift remove old.txt
```

Marks the path's inventory entries as deleted so the next parcel won't contain
them. The working directory is **not** touched (the file stays on disk). To
un-stage the removal, `load` the file again (if it's still there) or use
`unload`. A file that stays on disk after its removal is stacked is untracked, so
this is how a tracked path starts being covered by a `.forkliftignore` rule.

### `stocktake` — status (`st`)

```sh
forklift stocktake            # full report
forklift stocktake --summary  # just the counts (cheap; good for scripts/agents)
```

Reports the current pallet and its head, then two sets of changes:
- **Staged** — inventory vs the pallet head (what the next `stack` will record).
- **Not loaded** — working directory vs the inventory (what a `load` would stage).

Moves are detected and shown as `moved: old -> new` when a file's content is
unchanged but its path changed.

A change of kind is a change even when the bytes are the same: a `chmod +x` (or
`-x`) on a tracked file, or a file replaced by a symlink, reports as `modified` on
both sides, and `shift` refuses to reset it like any other edit. Where the file
system has no executable bit (Windows, and on Unix a volume such as exFAT, FAT, or
some network and FUSE mounts, which Forklift detects by trying a `chmod` in the
warehouse's metadata folder), a tracked file keeps the bit its entry records, a
new file is recorded without it, and `load` never clears it, not even over a
staged removal.

### `diff` — line-by-line changes (`d`)

```sh
forklift diff                       # working directory vs inventory (what load would stage)
forklift diff --staged              # inventory vs pallet head (what stack would record)
forklift diff main feature          # two revisions, tree vs tree
forklift diff main feature src/     # ...limited to a path
forklift diff --verbose             # include unchanged context lines
forklift diff :empty main           # main's root parcel vs nothing: every file is "added"
```

Binary files are reported, not printed. A file whose kind changed carries a
`(mode: normal -> executable)` line under its header, so a chmod alone shows up even
though no line differs. Whitespace-only changes are shown faint
(they're still real changes — the object store hashes them). With `--json`, diff
reports the changed-file set (path + kind) rather than every line.

A large file (stored in chunks — see below) is a binary as far as diff is concerned:
a changed one reports as `(binary contents; not shown line by line)`, never assembled
or line-diffed.

The two-revision form accepts anything `show` and `history` do — a pallet name, an
`@`-qualified meta pallet, or a parcel hash prefix — plus one reserved token,
`:empty`, meaning the empty tree. It exists for a root parcel, which has no real
"before": `diff :empty <revision>` compares it against a clean slate, so every file
it introduces lists as `Added` instead of `diff` refusing for want of a second
revision. `:empty` can never collide with a real revision — a pallet/meta name is
restricted to ASCII letters, digits, `.`, `_`, `-` and `/`, and a hash prefix is hex
digits only, so neither grammar can ever contain a `:`.

### `restore` — discard changes (`r`)

```sh
forklift restore path            # rewrite path from the inventory (drop unstaged edits)
forklift restore --staged path   # reset the inventory entry to the pallet head (unstage)
```

`restore path` overwrites the working file(s) from what's staged — it throws away
unstaged edits, so use it deliberately. Outside a conflict, a file that already holds the
staged content is left alone, so its modification time does not change and build tools don't
see it as edited. `restore --staged path` leaves the
working directory alone and only un-stages (the inverse of `load`) — the same
operation as `unload`. Files that already hold the pallet head's content usually keep counting
as unchanged without being re-read on the next `stocktake`; one edited moments before is
re-read once.

### `stack` — commit (`s`)

```sh
forklift stack "a clear description"
forklift stack                       # opens nothing; a description is optional but recommended
```

Builds tree objects from the inventory (bottom-up, staged removals excluded,
empty directories pruned), creates a parcel (recording you as the author with a
timestamp), advances the current pallet's head, and cleans up consumed staged
state. If a consolidation is in progress, `stack` completes it. On a warehouse
where trust is established, `stack` also signs the parcel (see
[Identity, signing, and agents](#7-identity-signing-and-agents)).

`stack` requires staged changes — it refuses to create an empty parcel. It refuses a
top-level entry that some file system opens as the warehouse's `.forklift` folder (`.Forklift`,
`.forklift.`, …), since no working directory there could check it out: rename it, or unload it
and add it to `.forkliftignore`. Below the top level it refuses the same spellings, since
checking one out would plant another working tree inside this one. It also
refuses if a previous `load` over any recorded path never finished cleanly
(`incomplete_load`, exit 20) — see the note under `load` above; either re-run the
named `load`(s), or `restore --staged` the affected path(s) to abandon them, then
stack again. The same code covers a command that rewrites the whole staging area (a shift, park,
merge, cherry-pick, fast-forward, `lower`, `import-git` or bay creation) and was interrupted
before it finished: then the message names that command, and `forklift heal` is the way out (see
`heal` below). And it refuses with `staged_content_missing` (exit 25), naming the path and the
hash, when the store no longer holds the content of a file staged since the head (for a large
file, its recipe or any of its chunks) — lost after the `load`, say to a crash or a manual
deletion. A parcel signed over it would pass `audit` and fail only at a later `restore`. Loading
the file again is not enough while it is unchanged on disk, because `load` then stores nothing:
run `restore --staged` on it, then `load` it again from its working copy. Files the head already
holds unchanged are not checked here; a lost object of theirs surfaces at `restore` (or at
`audit --full`). The check is for presence, not integrity: content that is in the store but
damaged is not re-read, and surfaces the same way. Two kinds of damage do stop the stack, with an
error naming the object rather than `staged_content_missing`, since loading again cannot replace
an object already in the store: a head tree that cannot be read, and a staged large file's recipe
that cannot be read. For the recipe, `restore --staged` on that file leaves it out so the rest
can be stacked.

### Large files

Large files are handled natively — no separate tool, no pointer files. A file at or above
8 MiB is split into content-defined chunks and stored as a small **recipe** (the ordered list
of chunk hashes) plus the chunks themselves; everything below stays a single object. This is
transparent: you `load`, `stack`, `shift`, `park` and `restore` large files exactly like any
other, and they round-trip byte-for-byte. The wins are streaming (a multi-GB file is never held
whole in memory), resumable transfer, and deduplication — an edit re-stores only the chunks it
actually touched, and two files (or two revisions) that share content share their chunks.

Chunking is content-addressed like everything else, so a chunked file's authorship is signed and
offline-verifiable down to every byte. Two commands treat a chunked file as the opaque binary it
is: [`diff`](#diff--line-by-line-changes-d) reports it changed without a line diff, and
[`blame`](#blame--who-wrote-each-line-bl-annotate) refuses it (there are no lines to attribute).

Over the wire, chunked files travel **per object**: a `lift` uploads the recipe and every chunk
(re-uploading only the chunks that changed — an appended-to file re-sends its new chunks and
nothing else), and a `franchise`/`lower`/`expand` fetches each in-scope chunk it doesn't already
have. A lift only advances the remote's ref once every chunk is confirmed present, so a chunked
file is never left half-transferred. Two limits are worth knowing: a `bundle` never carries a
chunked file (its chunks are reachable only through the recipe, which a bundle doesn't descend —
move that history over the wire instead), and lifting a chunked file to a remote that predates
chunking support is refused up front, naming the file (`chunked_transport_unsupported`, exit 14) —
upgrade the remote, or keep the file under the 8 MiB threshold.

### `undo` — reverse the last operation

```sh
forklift undo
forklift undo --as rescued   # undo a pallet delete, bringing the pallet back as "rescued"
```

Reverses the **last state-changing operation**, using an undo journal — not just the
last `stack`. Each `stack`, `consolidate`, `cherry-pick`, `shift` and `palletize --delete` records the state
before and after it (the current pallet, the pallet refs it moved, and any consolidation or
cherry-pick left in progress — all tiny, since the objects are content-addressed and already
stored), so `undo` can walk them back one at a time. Only the refs the command itself wrote are
recorded: a pallet another bay moved while it ran is never part of the entry, so `undo` never
reverts it.

- Reversing a **`stack`**, **`consolidate`** or **`cherry-pick`** is a *soft reset*: the head moves back while
  the working directory and inventory are kept, so the undone changes are staged again
  (like git's `reset --soft HEAD~1`). This is how a **merge is reversed** too — merge
  parcels are no longer refused.
- Reversing a **`shift`** moves back to the previous pallet and re-materializes its tree (it
  refuses on a dirty working directory, just like a forward `shift`).
- Reversing a **`palletize --delete`** recreates the pallet at the head it was deleted at, and
  touches nothing else — not the current pallet, not the working directory. If a pallet of that
  name exists again at another head, `undo` refuses rather than overwrite it, and keeps the entry:
  `undo --as <new-name>` brings the deleted pallet back under another name instead (`--as` is
  refused for any other operation). A pallet already back at exactly the deleted head counts as
  restored. It also refuses, keeping the entry, while a checkout stands on the name at another
  head (or at none) — the new ref would be out of step with its files; when the deleted history is
  no longer complete here (it names `lower <name>` to bring it back from a remote); and, in a
  warehouse with trust, when that history no longer passes the trust check as it stands now. After
  `undo --as`, older entries still name the old pallet: undoing them acts on whatever that name
  holds then.

Re-`stack` to redo (e.g. with a corrected message). An empty journal means there is nothing
to undo. Undoing a pallet's *very first* parcel, pure-staging (`load`/`remove`/`unload`)
and trust/remote commands are out of scope; `park` is reversed with `park pop`. Anything that
happened between the journaled operation and the `undo` but was never journaled itself stays
exactly as it is — trust operations such as a key retirement are preserved, never rolled back.

`undo` reverses only your *last* operation, never an older one behind it. Except for a delete,
which changed nothing in the bay, it refuses outright when the bay no longer matches what the
journaled operation left: the current
pallet was switched since (`palletize` and `deliver` switch pallets without being
journaled), the current pallet's head moved (a merged haul, a fast-forwarding `lower`), or a
consolidation or cherry-pick was started or finished. It also refuses to undo the `stack` that
completed a cherry-pick once the picked parcel is gone from the warehouse (a `gc` can collect it
when no pallet reaches it), since the pick it would put back could never complete. Continue forward instead. Working-tree
commands in between — `load`, `remove`, `park` — change none of that and do not block it:
after a `park`, `undo` soft-resets beneath the parked work. `park pop` then re-applies it,
unless the parked work touches a path the undone parcel changed — it refuses that until the
undone changes are re-stacked.

Undoing a `consolidate` or `cherry-pick` that stopped at conflicts takes no parcel off the
pallet; it only abandons the merge in progress, leaving the working directory (markers
included) as it is. Undoing the `stack` that completed one puts it back in progress, so the
next `stack` completes it again.

A journal that cannot be read — corrupt, truncated, or written by a forklift with a
different journal format — makes `undo` fail naming the file, never "nothing to undo".
Remove the file to start a fresh journal; the undo history is lost, nothing else. Other
commands keep working meanwhile and leave the file alone.

### `peek` — inspect an object (`pk`)

```sh
forklift peek <object-hash>          # print a blob / tree / parcel
forklift peek --inventory src        # print a folder's inventory entries
forklift peek --inventory src -v     # ...with full stat detail
```

A debugging/inspection tool: it decodes any object by hash (a blob's text, a
tree's entries, a parcel's tree/parents/actions/description) or dumps a folder's
inventory shard. With `--json`, a binary blob (a NUL byte anywhere, or bytes that
are not valid UTF-8) reports `"binary": true` and omits `content` instead of
mangling raw bytes through a lossy text conversion.

### `show` — a file's content at a revision

```sh
forklift show main:src/app.rs               # the file as of main's head
forklift show a1b2c3:src/app.rs             # ...at a parcel hash prefix instead
forklift show main:logs/build:latest.txt    # a path may itself contain ":" — only
                                             # the first ":" splits revision from path
```

The one-invocation equivalent of resolving a revision, walking its tree to a path
and peeking the blob by hash yourself — a single call in and out for a caller like
an editor's review panel. The argument is `<revision>:<path>`, split on the *first*
`:` — a revision (a pallet name, an `@`-qualified meta pallet, or a hash prefix)
can never contain `:`, so the split is unambiguous even when the path does.

A large file (stored in chunks) reports its metadata — content hash, total size,
chunk count — instead of assembling it; non-text content (a NUL byte anywhere, or
bytes that are not valid UTF-8) reports binary. Either way `content` is absent and
`binary` is `true`. In human mode a short notice prints instead of raw bytes; in
`--json` mode see [`docs/MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md) for the
exact shape.

---

## 3. Inspecting history

### `history` — the log (`hi`)

```sh
forklift history                 # from the current pallet's head, newest first
forklift history feature         # from another pallet
forklift history 1a2b3c          # from a parcel (hash prefix, ≥ 4 chars)
forklift history @office          # the office's audit trail (users & keys — a meta pallet)
forklift history --class agent    # only parcels an agent authored
forklift history -n 20            # only the 20 newest parcels (bounded walk)
forklift history --oneline        # one terse line per parcel: abbreviated hash + subject
forklift history -n 20 --json                  # a page + a `next` cursor (for agents)
forklift history --after <cursor> -n 20 --json # the following page
```

Walks the parcel graph newest-first (ordered by the latest action timestamp, so
merged lines interleave sensibly). `-n`/`--limit` bounds the walk to the newest N
parcels — on a large history that loads only those N and their frontier, not the
whole graph. Shows each parcel's hash, the parents it
consolidates (for merge parcels), its actions (author/stack with operator and
time), and the description. `--oneline` prints just the abbreviated hash and the
description's first line (git's `log --oneline`) — and, since it shows no author or
class, it skips the office read and the display-name resolution the full form does,
so it is the fastest way to scan history. **Machine authorship is legible:** an author that is an
agent, bot or service is shown as `[agent, supervised by <human>]`, read from the
signed office record — so authorship stays forge-proof, and `--class
<human|agent|bot|service>` filters the log to answer "which parcels did agents write,
under whose supervision".

With `--json`, each entry also carries `parents` — every parent of that parcel, in
their stored (canonical) order — base-first for a consolidation — `[]` for a root —
which is what a caller building a graph (rather than just reading a log) walks:

```json
{ "parcel": "<hash>", "parents": ["<base>", "<other>"], "consolidates": ["<base>", "<other>"], "actions": [/* … */] }
```

`history` on a pallet with nothing stacked on it yet fails with the `empty_history`
error code (exit 19) rather than a generic one.

One trust note: `history --class` reads the **recorded** (self-declared) identity —
each action in `--json` says so with `"trust": "recorded"`. `query --class` is the
verified counterpart.

### `query` — filter history on signed identity (`q`)

```sh
forklift query --class agent                    # parcels verifiably signed by an agent
forklift query office --class agent --unsupervised   # the compliance headline, scoped
forklift query --supervisor alice@corp          # work supervised by one human
forklift query --signer 91ff02                  # parcels signed by one key (prefix)
forklift query --grep 'fix*walk' --no-merges    # parcel-local facts compose in
forklift query --from v1-parcel feature         # scope: feature's line since a base
forklift query --recorded --class agent         # the cheap, labeled, unverified read
forklift query --model 'claude-*'               # recorded machine-authorship model
forklift query --tag v1.2.0                     # carries this signed tag
forklift query --touches src                    # changed this path (or under it)
forklift query --where '{"any":[…]}' --json     # full or/not/nesting as a JSON tree
```

The compliance question `history` cannot answer: *"all parcels authored by an agent,
unsupervised"* — answered **verified**. Where `history --class` trusts what the parcel
records, `query --class` checks the signature and joins the office off the *verified
signer*; a parcel that merely *claims* a human author but is signed with an agent's
key matches `--class agent`, and an unsigned parcel matches no identity predicate at
all (its class is unknowable, and a compliance filter must not guess). Answers carry a
trust label per match (`verified`, `signed-revoked`, `unsigned`, `unknown-key`, or
`recorded` under `--recorded`) and a `scope` block saying what the pass covered. A
`signed-revoked` match also carries `signer.boundary` — `"vouched"` when it sits inside
the revoking key's distrust boundary, or `"suspect"` when it sits outside it (a forged
backdate, or the key's holder kept signing after the revocation); `audit` refuses a
suspect parcel outright, but a read-only query labels it loudly instead. It can also
read `"unresolved"`, when this store cannot settle *this parcel's* place in the
boundary at all — the parcel is not reachable from any part of the boundary this
store actually holds, and the boundary walk also crossed a gap (an absent
boundary head, or an absent ancestor behind one) that could have been exactly
what would have vouched for it. A boundary can legitimately name a head for a
pallet this store never fetched at all; that alone never turns a parcel a
*present* boundary head already vouches for into `"unresolved"` — never treat
`"unresolved"` as `"suspect"` either way; query the origin, or fetch the full
history, for a definitive answer. Filter on it with
`--where '{"field":"signer.boundary","op":"eq","value":"suspect"}'`.

`--model`/`--tool` read recorded machine-authorship provenance (a `manifest provenance`
entry): a parcel with *no* provenance recorded at all never matches, in either
direction — absence is not a claim to negate, so `--where '{"not": {…matches…}}'`
still excludes it. `--tag` is plain membership: an untagged parcel simply does not
carry the tag (excluded, and included by its negation) — only a warehouse with no
`@tags` pallet at all reads every `--tag` predicate as unknowable. `--touches <path>`
tests whether the parcel's tree differs from its **first parent's** at that path (a
file or a directory prefix); a merge parcel is judged the same way `blame` already is
for line attribution — if the merge's own result at the path already matches what its
first parent had (e.g. both branches added the identical content), it does not match,
even though its other parent touched the path.

Flags AND together; `--where` takes a JSON predicate tree (`all`/`any`/`not` over
`{"field","op","value"}` leaves) for anything flags can't express. Match patterns are
globs (`*`, `?`) or plain substrings — never regex. Pagination works like history
(`-n`, `--after`, `--oneline`).

The honest cost: `--limit` bounds *output*, not verification — a verified identity
query resolves every candidate its walk considers. Scope the walk (a pallet argument,
`--from`) to bound the work; unscoped over all of history costs about an `audit`.

### Paging

Forklift pages output the way git does. When stdout is a **terminal**, long human
output (`history`, a big `diff`, …) is piped through a pager so it is scrollable
instead of scrolling off the top:

- The pager is `$FORKLIFT_PAGER`, then `$PAGER`, then `less` (with git's `LESS=FRX`
  when unset: quit if it fits one screen, keep color, don't clear on exit).
- Short output that fits one screen prints inline; there is no pager to dismiss.
- `--no-pager` disables it, and `--json` or any **non-terminal** target (a pipe, a
  file) streams raw — never a pager. `history` streams as it walks, so
  `forklift history | head` (or quitting the pager early) stops the walk instead of
  reading the whole graph.

**For agents**, page `--json` output with a cursor rather than a pager:
`forklift history -n 20 --json` returns 20 parcels plus a `next` cursor; pass it as
`forklift history --after <cursor> -n 20 --json` to read the following page, until
`next` is absent (the history is exhausted). The cursor is opaque — treat it as a
token to hand back, not something to construct.

### Record text is printed inert

Descriptions, titles, comments, operator names and the other free text in a record were
written by whoever authored the record — once it syncs, possibly another operator, and
nothing checks that text on the way in. Human output, error messages included, never lets
it drive your terminal: control characters (ANSI escapes, carriage returns) and invisible
bidirectional overrides print as spaces; tabs are kept. A multi-line text is printed line
by line, except in a listing that gives each record one row, where it is flattened onto
that row. `--json` is never touched — it carries the record's text exactly as stored.
File content (`show`, `peek` of a blob, `blame`'s lines) prints as-is, the way `cat` and
git do. The hashes a parcel names are checked when it is read, and a parcel naming
anything that is not a hash is refused.

### `blame` — who wrote each line (`bl`, `annotate`)

```sh
forklift blame src/main.rs            # each line, attributed to the parcel that introduced it
forklift blame src/main.rs --rev v1   # blame as of another revision (a pallet or parcel hash)
forklift blame src/main.rs --json     # structured: line → parcel, parcel → signed author
```

Attributes every line of a file to the parcel that last changed it — and, because
authorship is signed and classed, to the author's **identity class and
supervisor**. That is blame git structurally cannot express: *"was this line written by a
human or an agent, under whose supervision"*, offline and forge-proof. An agent-authored
line reads `[agent, supervised by <human>]` in the gutter, exactly like `history`. The walk
follows the first-parent chain from the revision (git's `blame --first-parent`); a line a
merge brought in from a side line is attributed to the merge parcel. On a long history the walk
uses the commit-graph's **changed-path filters** (built by [`compact`](#compact--pack-the-object-store))
to skip the parcels that never touched the file, so it stays fast as history grows. Each
version is matched against the one before it the way `git diff` matches them, so where a
repeated line (a blank, a closing brace) could be credited to either of two parcels, blame
breaks the tie the way git's diff slides its hunks (without git's indentation heuristic, so
next to an indented block the two can still differ). A version whose matching would be
unusually expensive (a long file whose shared lines were reordered wholesale) is not matched
line by line: all of its lines are credited to the parcel that made it.

blame refuses a large binary file (one stored in chunks — see below): there are no lines to
attribute, so it fails with a clear message rather than assembling gigabytes to guess.

### `audit` — verify signed history offline (`a`)

```sh
forklift audit          # verify the office chain, then the current pallet
forklift audit feature  # verify a specific pallet
forklift audit @office  # verify just the office chain (the office is a meta pallet)
forklift audit --full   # also re-read every chunk and re-verify each large file's content hash
```

Requires established trust. Verifies the office chain from genesis (each office
parcel signed by a key that was active in the previous state), then the pallet's
parcels (each signed by a tracked key; parcels stacked before trust was
established are tolerated as "legacy"). Any tampering — stripped or corrupted
signatures, an unknown key, a chain that doesn't reach genesis — fails with a
non-zero exit. See [`trust-and-identity.md`](trust-and-identity.md).

"Legacy" is decided by ancestry from the trust boundary the anchor pins, and the
same honesty rule applies there as to revocations below. This store can lack a
boundary head outright — `enroll` pins a remote's declared heads by hash without
fetching them, and `franchise` copies the anchor verbatim while fetching only the
one pallet it franchises — or can have genuinely lost one it once held, or lost
an ancestor behind one it does have. Any of those is a gap the boundary walk
cannot see past: `audit` refuses instead of concluding a parcel was stacked
after trust whenever it cannot otherwise place that parcel inside the boundary,
because there is no such thing as a boundary reference that is "unrelated" to a
given parcel — whether it would have vouched for the parcel is exactly the
question the walk cannot answer once it has met an absence. It names the
specific boundary parcel it could not find and says plainly that it cannot tell
whether the parcel predates trust or was stacked after it. Still fail-closed,
still a non-zero exit, but not an accusation. The walk covers the whole
boundary once per audit, not once per parcel, so the object it names is not
necessarily the one this specific parcel needed — supplying it does not
guarantee resolution, since another gap may surface behind it on a rerun.
Verify against a store with the full history for a definitive answer.

A parcel signed by a *revoked* key is checked against that revocation's distrust
boundary — exact ancestry, so a forged timestamp changes nothing. If the boundary
genuinely does not cover the parcel, that is real tampering and `audit` fails
saying so. A parcel a boundary head this store *does* hold already vouches for
passes as before, whatever else is missing — the check asks that first. It is
only when the parcel is not vouched **and** this store cannot resolve the
boundary — it is missing one of the boundary's heads, or an ancestor behind one
— that `audit` refuses instead, naming the specific missing boundary parcel and
saying plainly that it cannot tell whether the parcel predates the revocation or
the key kept signing after it: a real fail-closed refusal, but not a claim that
tampering occurred.
Verify against a store with the full history for a definitive answer.

A large file is stored as chunks indexed by a recipe. A normal audit **presence-checks**
those chunks (confirms each is present without re-reading its bytes) — bounded and fast.
`--full` is the stronger, slower level: it **re-reads every present chunk**, re-hashing it on
the content-addressed read (so on-disk bit-rot a presence check cannot see is caught), and
**re-assembles each fully-present chunked file** to confirm `Blake3(assembled) ==` the recipe's
recorded content hash. Streamed one chunk at a time — never the whole file in memory. On a
sparse warehouse, out-of-scope chunks stay sealed by hash under both levels; the output states
exactly what was re-hashed, presence-checked, and sealed.

**Run `--full` periodically as your own scrub against bit-rot.** Every `lift`/push used to
incidentally re-presence-check a pushed pallet's whole tree, chunked files included; a large
chunked file's subtree that a push leaves untouched is now pruned from that check (it is proven
present by the prior head it is byte-identical to, not re-walked — the cost fix behind this
release). That is sound for the commit gate — store durability between commits was never its job
— but it does mean push time alone no longer doubles as an incidental content scrub for
unchanged large files. A self-hosted `forklift-server` has no audit subcommand of its own, so
periodic `forklift audit --full` against a clone is the recommended way to catch on-disk bit-rot
that pushing quietly stopped surfacing for you.

### `manifest` — signed post-metadata on parcels (`mf`)

```sh
forklift manifest approve 1a2b3c -m "LGTM"   # a signed sign-off on a parcel
forklift manifest note 1a2b3c -m "add a test" # a signed review note
forklift manifest provenance 1a2b3c \         # how an AI produced the parcel
    --model claude-opus-4-8 --tool claude-code --session sess-42
forklift manifest show 1a2b3c                 # the entries attached to a parcel
forklift manifest                             # the whole manifest
```

The manifest is Forklift's "GitHub layer" living inside the warehouse: approvals, review
notes, and machine-authorship **provenance** (which model/tool/session produced a parcel) — recorded as **signed tracked metadata** that *references* a parcel without ever
changing it. Because provenance is signed, paired with an agent-class identity (`office
admit --agent --supervisor …`) it answers *"which model produced this change, under whose
supervision"* forge-proof and offline — the AI-traceability question git cannot. Entries live on the
`@manifest` meta pallet, so they are forge-proof, portable, and offline-verifiable:
`forklift history @manifest` is their audit trail. **Authorship is the signature, not a
field you type** — you record as yourself, no one can attribute an entry to another operator
without their key, and only you can change or remove your own entries. Recording needs an
enrolled signing key and a writer or admin role — an approval is evidence, not an annotation.
The subject is any revision (a pallet name or a parcel hash). You hold one approval per parcel
(approving again replaces it); notes, provenance and deliveries accumulate, and recording the
same note twice changes nothing. An entry this forklift cannot read (a newer kind, say) is
listed as `unreadable` rather than hidden or failing the listing.

The manifest **syncs with the office and the working pallets**: `lift` pushes it, `lower`
and `franchise` pull it (never into the working directory — it is metadata). If two people
record entries concurrently the manifest diverges, and `lower` **replays** your entries onto the
remote's. Entries from different operators never collide; only two copies of your own approval
written apart can, and `--keep` settles that (see `kv`).

### `kv` — custom metadata on parcels

```sh
forklift kv set main ci green        # set a key on a parcel (any revision)
forklift kv get main ci              # its value
forklift kv unset main ci            # remove it
forklift kv list main                # every key on a parcel; `kv list` for all of them
forklift kv log main ci              # every change to a key, newest first
```

Free-form text values, keyed by parcel and key, for whatever your tooling wants to record — a CI
result, a ticket, a review state. A key is one name of at most 128 bytes; a value is UTF-8 text of
at most 64 KiB. Values live on the `@kv` meta pallet and sync like the manifest, but **each value
is signed by whoever wrote it**: every write is a parcel signed with your key, and a remote
refuses any value stamped by a key other than the one that signed it. Writing needs an enrolled
signing key and a writer or admin role (a reader may only read), and the office must exist —
`@kv` is never kept without one. A value about a parcel this warehouse does not hold stays
readable and removable by its full hash.

If two people change `@kv` concurrently, `lower` replays your changes onto the remote's and
lifts them, as long as they touched different keys. When both changed the same key, `lower`
lifts nothing for `@kv`, lists each such key with both values, and exits 24 (`meta_conflict`);
choose a side per key and lower again:

```sh
forklift lower --keep kv:<parcel-hash>/ci=theirs   # take the remote's value
forklift lower --keep kv:<parcel-hash>/ci=ours     # keep yours, lifted over theirs
```

`theirs` on a bare `kv:<parcel-hash>` takes the remote's side for every key on that parcel.
`theirs` also discards a local change nobody else made, even when the remote never moved: that is
the way out when `lift` reports a change the remote refuses to admit. It needs the pallet on the
remote already, since a pallet never lifted has no remote side to keep.

### `tag` — signed tags / releases

```sh
forklift tag create v1.2.0 main -m "the second release"   # an admin-signed release tag
forklift tag                                              # list every tag
forklift tag show v1.2.0                                  # one tag in full
```

A tag is a named, signed pointer at a parcel — a release or a milestone. Like the manifest,
who cut it is its **signature**, not a self-declared field, so it is verifiable offline against
the office chain. The release convention: a tag is signed by an **admin** key, so creating one
requires an admin — an authoritative act, which the remote enforces too. Tag names are
**immutable**: a name already in use is refused, and no one, the remote included, can move or
delete a tag. Tags live on the `@tags` meta pallet, reserving no user pallet name; without a
revision, `tag create` tags the current pallet's head. If two admins cut the same name
concurrently, `lower` reports the conflict: `--keep tags:<name>=theirs` drops your tag, and
keeping yours is refused, since that would move theirs. An admin demoted before lifting a tag
they cut is left holding a tag the remote refuses; `lift` says so, and
`lower --keep tags:<name>=theirs` drops it the same way, once `@tags` is on the remote.

A tag's subject is nothing more than a parcel hash: if the parcel was never fetched here,
or was collected because nothing else referenced it (e.g. after an `undo` moved the only
pallet head off it), the hash still exists in the record but the parcel is gone. Both
causes leave the identical local state, so nothing on disk can tell them apart — and a
sparse or franchised clone can hit the "never fetched" case in perfectly ordinary use
(cloning `--only` one pallet still adopts every tag, including ones cut on pallets it never
fetched).

Rather than refuse over a state that is often entirely healthy, both `tag show` and `tag
list` probe the subject and **mark**, not fail: `tag show` still renders the tag in full —
every field it prints comes from the tag record itself, never the subject — and adds a
`warning:` line naming the parcel; `tag list` marks the row with `(subject not in this store)`. Either way the exit code stays `0`.
Only a probe that genuinely could not determine presence (an I/O error, never a definite
answer either way) fails the command, and it names the affected tag when it does.

A tag's name and subject are checked on every write and every sync, so a tag never carries
a malformed one. Its message and tagger are another operator's text, so control characters
(including ANSI escapes) are neutralized before printing; `--json` always carries the raw
values. A tag this forklift cannot read (a newer schema, say) is listed as `unreadable`, and
`tag show` of it says why.

### `haul` — pull requests (reviewable merge proposals)

```sh
forklift haul open --target main --source feature --title "Add X" -m "why"
forklift haul list [--state open|merged|closed|all]
forklift haul show <id>
forklift haul update <id>                                          # propose the source's new head
forklift haul comment <id> -m "…"
forklift haul review <id> [--request-changes | --comment] -m "…"   # approves by default
forklift shift main && forklift haul merge <id>                    # merge lands on the target
forklift haul close <id> | reopen <id>
```

A **haul** proposes merging one pallet into another, with discussion and reviews — forklift's
pull request. Like the manifest, it lives on a meta pallet (`@haul`), as signed cells: the
proposal, the proposed head, the status, one review per reviewer, and the comments. It
lifts/lowers/franchises with your warehouse, and a diverged `@haul` replays your changes onto
the remote's like the other meta pallets. Because every cell is signed, authorship —
**including who approved** — is forge-proof and carries the reviewer's identity class, so a
human's approval is distinguishable from an agent's (`haul show` tags automated reviewers).
Only the haul's opener runs `haul update`; a rewound or rebased source moves the haul with it. A
review records the head it reviewed: after `haul update` moves the haul to new work, an
earlier approval is still shown, marked with the head it was of, but no longer counts. A merged
haul stays merged — no one can reopen or close it, on any head. `merge` reuses
`consolidate` (a clean disjoint merge auto-stacks; a conflict leaves the consolidation in
progress — resolve, `stack`, then re-run `haul merge` to record it). A haul opened in another
warehouse can propose a head this one never fetched; `merge` then refuses, naming the source pallet
to `lower` first. The head an open or closed haul proposes is kept by `gc`, `compact --all` and
`scope-prune` even after its source pallet moves off it or is deleted. A haul id is
content-addressed; use any unique prefix.

Only an operator who may write the target records a merge: `haul merge` refuses anyone else
before merging anything, and every remote refuses such a record. `haul show` and `haul list`
judge a merged haul against this warehouse: `--json` carries `merge_proof` (`verdict`
`verified`, `refuted` or `unproven`, with a `reason` unless verified), and the human views
mark anything not verified — a warehouse behind its remote shows `unproven: not on target`
until it lowers the target. `lift` sends `@haul` after the working pallet, and holds back a
merge whose parcel the remote's target does not hold yet; lifting the target (lowering it
first if someone else moved it) lets it go. A `lower` that replays a diverged `@haul` holds it
the same way: once the target is lifted and lowered here, `lower` again. Either way, an
operator who cannot lift the target drops the record instead, as the message names it:
`lower --keep @haul:<haul>/state=theirs` puts back the remote's state of the haul and keeps
its reviews and comments, or, for a haul the remote does not hold yet,
`lower --keep @haul:<haul>=theirs` drops the haul.

This release is **intra-warehouse** (pallet → pallet), and reviews are **recorded but not
enforced** — any writer of the target can merge (approval-gating is a later policy layer).
Cross-warehouse forks come later. Requires an enrolled signing key.

---

## 4. Branching and merging

### `palletize` — create a pallet / list pallets (`pz`)

```sh
forklift palletize                 # list pallets (the current one marked *)
forklift palletize --all           # also list the meta pallets (@office, …) under a heading
forklift palletize feature         # create "feature" at the current head and shift to it
forklift palletize hotfix 1a2b3c   # create "hotfix" at a revision and shift to it
forklift palletize --remote        # list the remote's pallets, against the ones here
forklift palletize --remote --all  # also the remote's meta pallets
forklift palletize --delete old    # delete "old" (its parcels stay; `undo` brings it back)
forklift palletize --delete old --force  # ...even when nothing else holds its work
```

The list shows user pallets only by default; `--all` adds the meta pallets (the office
and other tracked metadata), each in its `@`-qualified form — the address you reach it
with. Pallet names may contain `/` (mapped to subfolders). Each part is lowercase ASCII
letters, digits, `.`, `_` and `-`, does not start with `-` or end with `.`, and is not a Windows
device name such as `con` or `nul` (also with an extension, as in `con.txt`). So a name means
the same pallet on every file system: `Feature` is refused, with `feature` suggested, rather
than silently sharing a ref file with `feature` on macOS or Windows. Creating one at a revision
materializes that state (and refuses on a dirty warehouse); creating one at the
current head just moves refs.

With `--json`, each pallet in the list carries its `head` parcel hash (`null` for an
unborn one — the current pallet is listed even when unborn, so a caller never has to
special-case it):

```json
{ "current": "main", "current_unborn": false,
  "pallets": [
    { "name": "feature/x", "current": true, "head": "<hash>" },
    { "name": "main", "current": false, "head": "<hash>" }
  ] }
```

`--remote` asks the remote which pallets it holds and sets each beside this warehouse's: `absent`
(not here — `lower <name>` brings it in), `same`, `local_ahead` (this side holds everything the
remote has, and more — `lift` from it publishes it) or `differs` (anything else: the remote moved
on, or the two diverged — `lower <name>` fast-forwards it or reports the divergence). It fetches
nothing and writes nothing. With `--json`, each entry carries `name`, `remote_head`, `local_head`
(`null` when absent) and `relation`; with `--all` the meta pallets come under `meta`. A name the remote lists that this version cannot read (a server with other naming rules) is
not compared, and is listed apart, under `unnamed`.

A command naming a pallet this warehouse does not hold (`shift`, `consolidate`, `audit`,
`haul open`) refuses with `pallet_not_found` (exit 29), and names `lower <name>` when a remote is
configured, `palletize <name>` otherwise. `lower <name>` for a pallet the remote does not have
refuses the same way, before it writes anything, naming `palletize --remote`.

`--delete <name>` removes a pallet's ref and nothing else: its parcels stay, and the delete is
journaled, so [`undo`](#undo--reverse-the-last-operation) brings the pallet back at the head it
was deleted at. It refuses:

- a meta pallet (`@office`, the hauls, the tags) — those are never deleted;
- a pallet any checkout stands on — the main working tree or any bay; shift it elsewhere first;
- the source or target of an open haul, or any delete while a haul cannot be read (it might name
  the pallet). A *merged* haul whose target was the pallet does not refuse; the delete names it,
  since its proof then reads as unproven — no pallet holds the target any more;
- a pallet whose work nothing else holds, unless `--force`. "Held" means another local pallet
  reaches its head — that pallet must still be where it was when the delete writes, so two bays
  cannot each delete the other's only copy — or, when a remote is configured, one of the
  remote's heads reaches it (the delete asks the remote, after its usual trust check, and walks
  the history held here). The output says what holds it.

While the delete is in the bay's undo journal, its head is kept by `gc`, by automatic and manual
repacks and by `scope prune`, so its history stays here for `undo` to bring back — once the
delete is the last entry again, and subject to the checks under
[`undo`](#undo--reverse-the-last-operation). That window ends when the
delete is undone, when 100 later journaled operations in that bay push it out, or when the bay is
removed (`bay remove`); after that, work no other pallet holds is collectable. With `--json` the
delete reports `pallet`, `head`, `contained_by` (`pallet "<name>"`, `the remote`, or `null`
when forced) and, when any, `merged_hauls`.

### `shift` — switch pallets (`sh`)

```sh
forklift shift main
```

Materializes the target pallet's head tree in the working directory, repopulates
the inventory from it, and makes it the current pallet. Refuses to run when you
have staged or unstaged changes (they'd be overwritten) — stack, restore, or park
them first — or while a consolidation or cherry-pick is in progress, which is
completed on the pallet it started on (`palletize` and `deliver` refuse then too,
and `stack` refuses to complete it on any other pallet). Untracked files are left alone unless the
target needs to write over one.

`shift` to the pallet you are already on **catches up**: when another bay moved the pallet
since this bay's files were materialized (`stocktake` says so), it moves the files to the
pallet's head. With no local changes the files are moved; if they already match the head, only
the bay's record is updated. With local changes it refuses and names the way through: `park`,
`shift` again, `park pop`. A catch-up is not journaled, so `undo` never reverses it.

On a case-insensitive file system (the default on macOS and Windows), two names
that differ only in case, such as `README` and `readme`, are one file. Every
command that writes the working directory (`shift`, `consolidate`, `cherry-pick`,
`lower`, `franchise`, `bay add`, `park` and `park pop`) asks the file system what
a new path lands on before it writes anything:

- A case-only rename (`foo` to `Foo`, or `Docs/` to `docs/`) just works. A
  directory rename is refused if anything the pallet does not track is left in the
  old directory, such as an untracked or ignored file or an empty folder, because
  the new spelling would land inside it.
- A tree that holds two spellings of one name is refused before anything is
  written, and the refusal names both entries. Rename one of them on a
  case-sensitive file system. The pair stays valid in the warehouse and
  materializes normally wherever the file system tells the names apart.
- Leaving such a tree for one that keeps only one spelling never loses the kept
  file: removing the other spelling deletes the one file both name, so it is
  written back from the target under the spelling the target keeps.
- An untracked file reached through another spelling is refused under its own
  name, like any other untracked collision.

`restore` likewise refuses a path that names the same file as another entry on
disk, instead of overwriting it.

The same commands refuse, before anything is written, a pallet that would write below a folder
holding its own `.forklift` (another working tree; see [`load`](#load--stage-changes-l)) or
that names a `.forklift` spelling at any depth. Where the target replaces a tracked folder with a
file, the folder must hold nothing but tracked files: an untracked or ignored file, an empty
folder or another working tree inside it refuses the command up front.

### `consolidate` — merge (`con`)

```sh
forklift consolidate feature
```

Merges another pallet's head into the current pallet:
- **Already up to date** when the current head already contains it — nothing happens.
- **Fast-forward** when the current head is an ancestor of the target — the ref just moves.
- Otherwise a **three-way merge** against the common ancestor. Clean merges stack
  a two-parent merge parcel immediately. Conflicts are written into the working
  files with diff3 markers, the entries enter a conflict state, and the
  consolidation stays in progress until you resolve, `load`, and `stack`.

The executable bit merges like content: the side that changed a file's bit from the
common ancestor wins, so a `chmod -x` on one side survives an edit on the other, for
binary and large files too.

Requires a clean warehouse before it starts.

### `deliver` — squash a draft onto a target, keeping the trail (`dv`)

```sh
forklift deliver main -m "add the feature"   # run from the draft pallet
```

The squash agents need without losing the trail. Agents checkpoint constantly;
humans want one reviewed parcel. `deliver` takes the current (draft) pallet's net change
and lands it on the target as a **single clean signed parcel** — the draft head's tree
with the target as its *only* parent, so the checkpoints stay out of the target's history.
The full trail is **kept** (the draft pallet is left intact, browsable with `history`) and
recorded as a signed **delivery** entry on the new parcel's manifest, so "what the agent
tried, in what order" stays discoverable (`manifest show <parcel>`). The delivered parcel
preserves the trail's authors; the deliverer is the stacker.

Needs an enrolled key (the trail is signed evidence). The current pallet becomes the
target, and because the delivered tree equals the draft's, the working directory does not
change. A second delivery of an already-delivered draft is refused.

The squash replaces the target's tree with the draft's, so `deliver` refuses a target that
moved after the draft forked from it: those changes would be silently undone
(`target_not_covered`, exit 27). Consolidate the target into the draft first, then deliver. A target whose head is an earlier
delivery of a state the draft has been through is fine, so a draft (or a pallet branched
from it) can keep working and deliver again.

### `cherry-pick` — apply one parcel's change onto the current pallet (`cp`)

```sh
forklift cherry-pick 1a2b3c            # apply that parcel's diff here, as a new parcel
forklift cherry-pick feature -m "..."  # pick a pallet's head, with a custom message
```

Applies a parcel's diff — its change against its first parent — onto the current pallet as
a new parcel. Unlike rebase, cherry-pick only **adds**: no rewrite, no
force-push, nothing an audit has to bless twice. The picked parcel's **authors are
preserved** and you are recorded as the stacker (the same author/stacker split as
`import-git`/`export-git` and `deliver`), and the new parcel is freshly signed. It is
applied by three-way merge, so a **clean** pick is stacked immediately as a single-parent
parcel; a **conflicting** one writes diff3 markers and leaves a cherry-pick in progress —
resolve, `load`, and `stack` to complete it (still single-parent, still author-preserving),
or remove `.forklift/cherry-pick` to abort. A pick whose changes are already present is
refused. Requires a clean warehouse.

### `conflicts` — list unresolved conflicts

```sh
forklift conflicts            # human list
forklift conflicts --json     # structured: each file's base/ours/theirs as content addresses
```

Lists the files a consolidation or cherry-pick left in conflict. With `--json`, each file's
three sides (base, ours, theirs) are reconstructed from the diff3 markers and
given as content-addressed blob hashes you can `peek` — designed for agents,
which resolve merges well given structure instead of marker soup. An empty list
is a valid answer (nothing to resolve).

**Resolving a conflict:** edit each conflicted file to remove the markers,
`forklift load` it, then `forklift stack` to complete the consolidation or cherry-pick.

### `bay` — parallel working directories

```sh
forklift bay add feature ../myrepo.feature   # a new working dir on a new pallet "feature"
forklift bay                                 # list the bays
forklift bay remove feature                  # de-register a bay (its pallet + files are kept)
```

A bay is an additional working directory bound to this warehouse — git's worktrees,
designed in. It **shares** the object store, the refs (pallets/meta) and trust, but keeps
its **own** working tree, inventory, current pallet and lock. So several agents (or you and
an agent) work one machine without cloning the objects N times or serializing through one
lock: N bays, N pallets, one warehouse. `bay add <name>` checks out a new pallet named
after the bay (branched from the current head) into a fresh directory (default: a sibling
of the warehouse); `cd` into it and it behaves like any warehouse — `load`, `stack`, `lift`
all operate on the bay's pallet against the shared object store. The bay's directory holds
its own `.forklift` folder, whose `redirect` file names the warehouse back, and its local state
lives under the warehouse's `.forklift/bays/<name>/`. That folder sits on the bay's own volume,
so the probes that learn how a volume stamps file times and keeps the executable bit measure
the bay's volume even when the warehouse lives on another one. `prepare` in a bay's directory
refuses rather than turn it into a warehouse.

A bay made by an older version of Forklift has a `.forklift` *file* instead, which is refused,
naming the conversion: create a new folder (the error names one nothing in the directory
holds, such as `.forklift.new`), move the `.forklift` file into it as `redirect`, then rename
the folder to `.forklift`. `bay remove` deletes only the bay's own `.forklift`, one whose
redirect names that bay of that warehouse (a link there is removed as a link, never followed);
anything that redirects elsewhere (another warehouse, another bay) is left in place and named,
and one it cannot read keeps the bay. It first moves the entry aside, which is the step that
ends the bay; if that fails, the bay is kept and the error says how to remove it by hand.

A bay name follows the pallet-name rules (see `palletize`) and is one folder, so it has no `/`.
`bay remove <name>` matches the
name exactly against the folders under `.forklift/bays/`, so it also removes a bay whose name
the rules refuse (one an older version created), and it never treats `.`, `..` or a path as a
bay. `bay list` refuses a bay folder whose name breaks the rules, and a bay whose position
cannot be read, naming it, instead of listing it; for a folder it gives the `bay remove` command
that removes it. A command run inside a bay whose redirect names a bay outside the rules refuses
too, naming the bay's `.forklift` folder: remove the bay from the warehouse, or delete that
folder.

Because the refs are shared, two bays can reach for the same pallet — say both are on `main`.
Each bay records the parcel its files stand on. When another bay stacks on the pallet, this bay
is **behind**: `stocktake` reports that the pallet moved on, and `stack`, `consolidate`,
`cherry-pick`, `haul merge`, `lower`, `deliver` and `lift`'s auto-merge refuse, naming both
parcels, rather than build on the old tree and revert the other bay's work. `shift <pallet>`
catches up (see `shift`), and shifting to another pallet works as usual. A merge or pick
started before the pallet moved cannot be completed or caught up past; abort it and redo it
after catching up.

Each command moves a pallet only from the head it started from: if another bay moved it in
the meantime, the command is refused with a message naming both heads, and whatever it had
already written to its own working tree is put back. A head another bay moved is never
silently overwritten.

That holds for merges too. A `consolidate`, `haul merge`, `lift` auto-merge or `cherry-pick`
that completes on its own is stacked on the head it merged against; if that head moved, the
merge is undone in the working tree and inventory and nothing is left in progress, so running
the command again is safe. A merge or pick you resolved by hand is stacked on the head it
started from too, but there a refused `stack` keeps it in progress, since putting the tree back
would throw your resolutions away. To redo it on the new head: abort it, `restore --staged .`
then `restore .`, delete the files the merge added (they are left untracked), catch up with
`shift <pallet>`, and merge again. Stacking the merge output instead is refused while the bay is
behind. And `undo` only ever reverses refs your own command moved; a move another bay made in
the meantime is never undone.

### `bay add --scope` — a scoped (sparse) bay

```sh
forklift bay add api ../myrepo.api --scope src/api          # materialize only src/api
forklift bay add api ../myrepo.api --scope src/api --scope docs   # several subtrees
forklift scope                                              # show the current scope
```

A **scoped bay** materializes and operates on only the subtree(s) you name, instead of the
whole working tree — handy for a large monorepo, or to hand an agent exactly the corner it
should touch. In this release the object store still holds **everything** (the sparseness is
materialization-only), so a scoped bay is a local view over a full warehouse; nothing changes
on the remote and nothing is fetched differently.

Inside a scoped bay:

- `load`, `stack`, `stocktake`, `diff`, `shift`, `park` all work on the in-scope subtree(s).
  A `stack` rebuilds the root tree as *"the head with your in-scope subtree swapped in"* —
  every out-of-scope sibling is carried forward by the exact hash the signed head already
  commits, so the parcel you stack is **byte-identical** to what a full workspace stacking the
  same change would produce. Out-of-scope content is cryptographically pinned, never guessed.
- `stocktake`/`diff` report only in-scope changes; an out-of-scope path that only exists in
  history is *sealed by hash*, not shown as removed.
- A path argument outside the scope (`load src/web`, `blame src/web/x.rs`, `diff a b src/web`)
  refuses with the `out_of_scope` code rather than doing something surprising.
- `consolidate` merges in a scoped bay. In-scope content merges normally; an out-of-scope
  sibling (a subtree, file or symlink) that changed on only one side is adopted **by hash** —
  never materialized, never fetched — so the merge parcel is byte-identical to what a full
  workspace merging the same two heads would commit. An out-of-scope entry that changed on
  **both** sides has no content here to reconcile, so the merge refuses with
  `out_of_scope_conflict`; widen the scope to include that path and retry, or resolve the merge
  in a full workspace.
- `export-git`, `import-git` and `cherry-pick` refuse in a scoped bay (`sparse_workspace`): the
  first two bypass the sparse overlay and would export/import a truncated or scope-inconsistent
  view, and a cherry-pick materializes a diff that may touch paths this bay never fetched. Run
  them from a full workspace.
- `park` produces a parcel that stacks over the head's spine exactly like `stack`, so a parked
  parcel from a scoped bay is byte-identical to what parking the same work in progress in a
  full workspace would commit.
- If a subtree you scoped to has since been replaced by a file (or vice versa) at the revision
  you move onto, the operation refuses with `scope_path_type_changed` rather than guess — the
  scope is no longer valid there, so re-scope or resolve in a full workspace.

Scope is a property of **this checkout**, not the project: it is recorded bay-locally and is
**never tracked**, so it is never pushed to the remote or imposed on collaborators.

Bay creation is exposed over MCP (`bay_add`, with the same `scope` argument), so an
orchestrator agent can open task-scoped sandboxes for its sub-agents directly, without
shelling out. The scope it sets is advisory local setup, not a security boundary — see
**[`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md)**.

### `scope` — show the sparse-workspace scope

```sh
forklift scope            # this bay's materialization scope + the warehouse fetch scope
forklift scope --json     # the same, as a machine envelope
```

Read-only. In a plain bay or the main tree it reports the full tree; in a scoped bay it lists
the in-scope subtree prefixes. The warehouse fetch scope is reported too — *full* for an
ordinary warehouse (the store holds everything), or the fetched prefixes for a sparse
(`franchise --only`) one. A scoped checkout and a sparse fetch scope also hold the root `.forkliftignore`
without it being a prefix; the report lists it as always in scope (`implicit_scope` and
`implicit_fetch_scope` in `--json`). Widen the fetch scope with `expand`; shrink a checkout's
materialization scope with `narrow`.

A scope recorded by a Forklift older than the one that put the ignore file in every
scope is refused by every command that reads it. For a sparse warehouse, franchise the
remote again, into a new directory; for a scoped bay, remove the bay and add it again
with `bay add --scope`. The same holds for a scope file whose format header names no
prefix.

---

## 5. Parking work in progress

### `park` — stash (`pa`)

```sh
forklift park            # park tracked changes and reset to the pallet head
forklift park list       # list parked parcels, newest first
forklift park pop         # re-apply the most recent parked changes (staged) and drop it
```

`park` saves your staged and unstaged changes to **tracked** files as a parked
parcel and resets the warehouse to the pallet head (untracked files are left
alone). `park pop` re-applies the most recent parked changes as a clean re-apply
— it must be popped onto the head it was parked on, or it reports a conflict. If
staging the re-applied changes fails, the parcel stays parked, and the error says that the
working directory already holds the changes.

`park`'s push durably commits the staged inventory into a parked parcel, exactly
like `stack` commits into a stacked one — so it refuses under the same
`incomplete_load` guard (see the note under `load` above) if a previous `load`
never finished cleanly, and under the same `staged_content_missing` refusal (exit 25, see
`stack`) if the store lost a staged file's content. Here it matters more: the push resets the
working copy, so a parked parcel naming lost content could never be popped back. That check,
and the damage it can run into (see `stack`), comes after `park` has staged your tracked
changes, as `load` would have, so they stay staged when it stops.

---

## 6. Working with remotes

Forklift's remote is untrusted storage plus thin verifiers: bytes move in
parallel, and anything you download is verified locally by hash. Set a remote
once, then push and pull.

A remote also cannot make the client hold more memory than an answer can legitimately need. Every
answer has a size limit: small for an error or a signature, 64 MiB for one object or for the
handshake, and one that scales with the request for negotiation answers. An answer over its limit
is refused with a message naming what was refused and the limit. Bundles (clone, pull, sparse
fetches) are written to disk or imported as they arrive rather than held whole, so they have no
memory limit. What they write is bounded instead: a pull or sparse fetch stores only the objects
the client asked for, each once and each within the 64 MiB object limit. A clone is different
in kind, since the remote decides how much history there is: its bundle stores no more than the
downloaded file holds, and the file is kept afterwards, so a clone can briefly need about twice the
history's size. The client accepts gzip-compressed answers only, and a zstd frame received from a
remote may use at most an 8 MiB window; a frame asking for more is refused.

### `franchise` — clone (`fr`)

```sh
forklift franchise http://forklift.example.com:9418 my-project
forklift franchise <url> <dir> --pallet main --token <secret>
forklift franchise <url> <dir> --only src/api            # sparse: fetch one subtree
forklift franchise <url> <dir> --only src/api --only docs # several subtrees
forklift franchise <url> <dir> --expect <genesis>          # pin the remote's trust anchor
```

Prepares a fresh warehouse in the target directory, remembers the remote, adopts
its trust anchor, downloads the history (using the remote's bundle when it has
one, then loose objects for the rest), and materializes the chosen pallet
(default: the remote's default pallet). The directory must be new or empty, or hold an unfinished
franchise of the same remote, which is then resumed.

Adopting a trust anchor is announced, never silent: a warning names the remote and the adopted
genesis, and the report records it (`adopted_genesis`). Verify it out of band — or pin it in advance
with `--expect <genesis>`, which is checked against the remote's first response before anything is
downloaded or created; a mismatch refuses with `trust_changed` (exit 22). See
[trust-and-identity.md](trust-and-identity.md).

The remote is contacted before anything is written, and a `--pallet` typo is checked against the
handshake's own answer before anything is fetched, so a bad URL, a bad token, an unreachable
host, or a pallet that does not exist all leave the target exactly as it was found.

**A franchise that fails partway keeps what it fetched.** Once franchise has claimed the target
directory (created it, or found it empty, and put its `.forklift` folder there), the directory
holds a franchise record until the franchise finishes. A failure after that point — a dropped
connection, a full disk, an `--only` path that is not a directory in the pallet, a path the
working tree cannot hold — or a kill keeps the directory with every verified object it fetched,
and exits `franchise_incomplete` (exit 30) naming the cause and the command to run next. Every
other command run in that directory refuses with the same code until the franchise finishes.

To finish it, run `franchise` again with the same URL and directory. The fetched objects are kept
and everything else is rebuilt, so `--pallet` and `--only` may change — a typo'd `--only` is
corrected without fetching the history again. An omitted `--token` reuses the one given before,
and the trust anchor the first attempt saw (or its `--expect` pin) stays enforced: a remote that
changed its anchor in between is refused with `trust_changed`, never adopted. A resume of another
URL is refused before anything is contacted. The `retry` field of the error's `details` says
whether running the same command again can help (`same`, `unknown`) or whether it fails the same
way (`change`), and `next_step` spells out the command. To give up instead, delete the directory.
Until the franchise finishes the directory belongs to it: a resume deletes everything in it but the
fetched objects (and a finished bundle download), files added there since the failure included.

Any *parent* directories needed for a nested target path (e.g. `some/new/path`) are created the
ordinary way and left standing. Claiming the target directory is exclusive: franchise builds
`.forklift` under a temporary name with the record already inside and moves it into place only if
none exists, so two concurrent `franchise` runs into the same directory cannot both claim it, and a
directory with a `.forklift` folder is never left without the record that says what it is. A
directory that holds anything franchise did not put there is refused as "not empty".

**Sparse franchise (`--only`).** With one or more `--only <path>`, franchise fetches the
whole signed history — every parcel, signature and the tree spine — but only the **content**
under the named subtree(s). Out-of-scope subtrees and files are never downloaded; they stay
pinned by the exact hash a signed parcel already commits, so nothing can be forged, it is
simply not fetched. The working tree materializes only the in-scope subtree(s), and the
remote's whole-store bundle is skipped. History, `audit`, `blame` and `diff` on in-scope paths
all work; office and every meta pallet are always fetched in full, so a sparse franchise of a
trusted warehouse still audits offline exactly as a full clone does. Widen later with `expand`.
A sparse franchise records its origin — see the origin-only lift rule under `lift`.

When the remote's bundle installs native packs straight into the new warehouse, `franchise`
tips the same densify suggestion `import-git` does: those packs are the far end's own bulk
ingest, unseen by a cross-store `compact --all --redelta` pass.

### Configuring a remote on an existing warehouse

```sh
forklift config remote.url http://forklift.example.com:9418
forklift config remote.token <secret>      # only if the server requires one
```

### `lift` — push (`li`)

```sh
forklift lift
```

Uploads the current pallet's new parcels to the remote and moves the remote's ref
with a compare-and-swap. When trust is established, the office pallet and trust
anchor are lifted first (the server verifies every signature before accepting).
The remote only accepts **fast-forward** updates — no force push.

**Optimistic lift:** if the remote moved and the two changes are cleanly
mergeable (they touch different files), `lift` **auto-lowers, consolidates and
retries** on its own — so a fleet of agents stacking to one pallet stops
serializing through a human. It reports how many times it auto-merged. Only a
clean warehouse qualifies (the merge materializes), and a **true overlap** (both
sides edited the same file) still stops with the diverged error (`diverged`, exit 5), leaving
the warehouse untouched: palletize the fetched remote head, shift back to your pallet,
consolidate the new one, and lift the result. A remote holding parcels you have not lowered yet — its head unknown here, or moved
since you read it — is a different refusal, `remote_ahead` (exit 28): `lower` first, which
fast-forwards, or reports `diverged` if you moved too. A bay another bay left behind
its pallet is refused with `behind` (exit 26) by `stack`, `deliver` and this auto-merge alike:
`shift` to the pallet to catch up.

**Origin-only lift from a sparse workspace.** A sparse (`franchise --only`) warehouse only
ever proved its out-of-scope content present on the remote it fetched from — its origin. Lifting
to a *different* remote could fail late at that remote's closure check, so `lift` refuses up
front with the `non_origin_lift` code, naming the origin. Point `remote.url` back at the origin,
or run a full (unscoped) franchise against the new remote. A full warehouse holds the whole
closure and can lift anywhere.

**Very large lifts and old remotes.** A lift touching more objects than fit in one commit batch
(tens of thousands of distinct objects — realistic for a single maximal chunked file, or simply a
lot of small tracked files at once) needs a remote that understands paginated commits. Against a
remote that doesn't, `lift` refuses up front, before uploading anything
(`commit_pagination_unsupported`, exit 16) — upgrade the remote, or lift in smaller stages.

**Lifting to a serverless remote.** There, uploads go straight to storage and the remote checks
them before the lift may move the pallet. A remote that has not finished checking an upload is
waited for, up to about two minutes (for each batch of the commit, when a very large lift sends
several); past that, the lift fails without moving the remote's
pallet, and running it again is safe. An upload the remote rejected or never received fails the
lift at once (`staged_upload_discarded`, exit 32), again without moving anything: run the lift
again, which uploads it afresh.

### `lower` — pull (`lo`)

```sh
forklift lower
forklift lower feature              # bring in, or fast-forward, another pallet; this bay stays put
forklift lower --expect <genesis>   # refuse unless the remote's trust anchor is this one
forklift lower --keep kv:<hash>/<key>=theirs   # resolve a custom-metadata conflict
```

Fetches the remote's new parcels for the current pallet and fast-forwards to them
— working directory and inventory included. On first contact with a trusted
remote, its trust anchor is adopted (a one-way door, like enrolling) and announced with a
warning naming its genesis. `--expect <genesis>` pins the anchor before anything is fetched, as
for `franchise`. A remote whose anchor changed from the one held is refused with `trust_changed`
(exit 22) — after a legitimate re-genesis, accept it with
`office accept-regenesis --confirm <genesis>`. A remote that holds an office but declares no
anchor had it stripped, and is refused the same way, by `franchise` and `lift` too. A diverged
pallet is never merged implicitly: `lower` fetches the parcels and reports the
divergence, and you consolidate deliberately (`palletize` the remote head into
its own pallet, `consolidate` it, then `lift` the merge). Requires a clean
warehouse. In a sparse warehouse, `lower` stays pruned — it fetches new in-scope
content and leaves out-of-scope changes sealed by hash, exactly as the franchise did.

**A slow or busy remote.** Objects normally arrive in a few large batches. A remote that takes
longer than about 45 seconds to start answering a batch does not fail the command: the rest of
that command fetches one object at a time, which is slower but still finishes, and the command
says so in one closing line (`batch_lane: "degraded"` under `--json`). A remote that serves no
batch downloads gets the same line, worded for that (`batch_lane: "endpoint_absent"`). A remote
that rate-limits its fetches is waited out, for up to 30 seconds per request. The same holds
for every command that fetches (`franchise`, `lower`, `expand`, and `lift` when it auto-merges);
`office accept-regenesis` adds the line to its message instead of a `batch_lane` field. A fetch
that fails reports only its own failure, not whether the batch download had stopped first.

`lower <pallet>` names a pallet other than the current one (`palletize --remote` lists the
remote's). It fetches that pallet's history, then creates the pallet here at the remote's head,
fast-forwards it, or reports it ahead or diverged, exactly as for the current pallet, but nothing
else moves: this bay's pallet, files and inventory stay where they are, so it needs no clean
warehouse. `shift <pallet>` then moves the bay onto it. A bay already standing on that pallet is
left behind it and catches up with `shift`. The pallet is created or moved only once its history
is fully fetched, so a lower that fails part-way leaves it as it was. Naming the current pallet is
the same as a bare `lower`.

**A pallet never points at history this warehouse lacks.** Every command that moves a pallet to a
parcel it does not already stand on (`stack`, `lower`, `consolidate`, `palletize`, `undo`, and the
rest) first checks that the parcel's history is completely present, back to what some pallet
already holds. In a sparse warehouse it checks only what the fetch scope holds. When something
is missing, for instance after a fetch that stopped part-way, the command refuses, names the
missing object, and writes nothing; running that fetch again completes the history.

The meta pallets sync first, each on its own: one that fails or conflicts is reported, and the
working pallet still lowers. A diverged `@kv`, `@manifest`, `@tags` or `@haul`
replays your changes onto the remote's, and `--keep` resolves the paths both sides changed (see
`kv`).

### `expand` — widen a sparse warehouse's fetch scope

```sh
forklift expand src/web            # fetch a subtree the sparse franchise left sealed
forklift expand src/web docs       # several at once
```

Adds subtree path(s) to what the warehouse has fetched, and downloads their content across the
whole history from the remote. Incremental and precise — only the newly in-scope objects are
fetched; what is already present is skipped, and the content is hash-verified against the seals
the history commits (so widening is safe from any remote — only publishing is origin-bound).
After expanding, a bay can be scoped to the new path (`bay add --scope`). A full warehouse
already holds everything, so there is nothing to expand.

Every history the warehouse holds is widened, not just the current pallet's: every pallet, and
every remote head a `lower` or `lift` fetched without a pallet yet (such as the head of a
diverged `lower`, which you may palletize later), and every head a pallet moved off that `undo`
could bring back. If the remote can no longer serve one of those unpalletized heads, `expand`
still succeeds, and that head is refused as a pallet head (its history "is not completely
present") until a `lower` from a remote that has it fetches it in full. If it
cannot serve a pallet's history, `expand` refuses and names the pallet. When another command
changes the warehouse while `expand` runs, it walks the change and tries again, and after three
tries it asks you to run it again.

### `narrow` — shrink this checkout's materialization scope

```sh
forklift narrow docs               # stop materializing a subtree here
```

Drops subtree path(s) from what **this** checkout (a bay, or a sparse main tree) materializes,
and removes those files from the working directory. This **frees nothing** in the shared object
store — the dropped content is ordinary reachable history, not garbage — it only shrinks what
this checkout shows. A checkout must keep at least one in-scope path; to stop scoping entirely,
open a fresh full checkout. `narrow` is the counterpart of a bay's `--scope`, not of `expand`:
`expand` widens what the *warehouse* fetched, `narrow` shrinks what *this checkout* materializes.
It refuses (`narrow_unclean`) a subtree holding uncommitted work (staged or unstaged changes, or
untracked files), and one holding another working
tree (a folder with its own `.forklift`) at any depth, whatever `.forkliftignore` says.

### `scope-prune` — reclaim disk from a sparse warehouse

```sh
forklift scope-prune docs            # forget a fetched path and free its content
forklift scope-prune docs --dry-run  # show what it would free, change nothing
```

The deliberate, **destructive** counterpart of `narrow`. Where `narrow` is bay-local and frees
nothing, `scope-prune` forgets a path **warehouse-wide**: it drops the path from the shared
warehouse fetch scope and deletes the objects under it from the object store, reclaiming disk
reachability-`gc` never could (narrowed-away content is still reachable history, so `gc`
correctly keeps it). It is **multi-bay-aware** — it refuses (`scope_prune_blocked`, exit 13) to
free a path any checkout (the main tree or a bay) still materializes, so narrow that path away
everywhere first. The pruned history is sealed by hash and re-fetchable from the origin with
`expand`, so the prune frees only history the origin holds. It asks the remote for its pallet
heads first, and refuses, changing nothing, if the remote cannot be reached or is not trusted.
History those heads do not reach (stacks never lifted, or a remote that has moved on since your
last `lower`) keeps whatever lifting it would read under the path. The report counts what was kept
(`kept_unlifted`), names the pallets (`unlifted_pallets`), and asks you to lift (or lower) and
then run the same `scope-prune` again, which frees the rest. Other work that exists only here is
never freed either, whatever its path: content loaded but not yet stacked, parked parcels, an
unfinished consolidation, the head a haul not yet merged proposes, and the history of a deleted
pallet while `undo` can still bring it back, in any bay. The report counts those last two keeps
apart (`kept_for_hauls`, `kept_for_deleted`), since lifting cannot free them; a haul's goes once
it is merged and its target lifted (a closed haul keeps it, since reopening proposes the same
head), a deleted pallet's once the delete leaves the undo journal. So you can park edits
under the path, narrow and prune; the parked edits survive, and `park pop` re-applies them once
`expand` has brought the path back. If any of that state cannot be read, the prune refuses
before changing anything. A full (non-sparse) warehouse has nothing to prune. Use `--dry-run`
before you leap. (Objects already inside a pack are reported but not reclaimed yet — a scope-aware
repack is future work.) A prune, dry run included, is **refused while a mutating command runs in
another bay**, since that writer could reuse an object the prune is about to free; run it again
once the other command finishes. (A `forklift-server` never serves a sparse warehouse, so no
server is in the way.)

Pruning a **large (chunked) file** frees its recipe **and every chunk** it names, not just the
recipe — chunks are content-addressed objects reachable only through the recipe, so a
recipe-only delete would orphan them. A chunk the pruned file shares with a still-fetched file
elsewhere is **kept** (freeing it would break that file), exactly as a shared blob is. The chunks
are freed before their recipe, so a killed prune resumes cleanly on the next run.

If a prune gets interrupted (killed, crashed) before it finishes freeing everything, the fetch
scope has already narrowed but some objects are left behind. Running `scope-prune` again on the
*same* path resumes rather than refusing "not a fetched path" — it finishes freeing whatever is
left, or reports there is nothing left to free.

`lift` and `lower` keep a small record of the remote heads they landed or were served
(`.forklift/held-heads`, at most 16 per pallet), so that a prune can later ask the remote about
history its current heads no longer reach; nothing reads it yet. A `lift` or `lower` that cannot
write the record prints a `held_head_unrecorded` warning and keeps its result and exit code. A
record that does not parse can be removed: it only names heads to ask the remote about, and the
next `lift` or `lower` starts a new one.

---

## 7. Identity, signing, and agents

The `office` command manages who may contribute and how contributions are signed
— users, keys, roles, agents, and trust resets. This is a big topic with its own
guide: **[`trust-and-identity.md`](trust-and-identity.md)**. In brief:

```sh
forklift office enroll            # establish trust (one-way; every parcel is signed after this)
forklift office keygen            # generate a key and print your enrollment line
forklift office admit <id> <pub> <pop>   # an admin admits a newcomer (or an agent/bot)
forklift office list              # who's enrolled, their roles, keys, and classes
forklift office rotate            # replace your key
forklift office retire <key>      # revoke a key
```

`profile` lets one machine act as different operators in different warehouses:

```sh
forklift profile list
forklift profile create work --display-name "Work Me"
forklift profile use work
```

---

## 8. Machine & agent output

Every command takes `--json` and emits one structured envelope instead of prose;
errors carry a stable code and a deterministic exit code. Forklift also ships an
MCP server (`forklift mcp`) that exposes the command surface as native tools for
AI agents. Full details — the envelope schema, the error/exit-code taxonomy, the
`conflicts` content addresses, and wiring `forklift mcp` into a client — are in
**[`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md)**.

A program running `compact`, `lift`, `lower` or `import-git` can follow its progress
without parsing stdout: set `PULT_EVENTS` to the number of an inherited pipe's write end
(above 2), and Forklift writes `status <phase>` and `progress <0-100|?> [text]` lines to
it. An fd that is not a pipe, FIFO or socket is ignored. See
[Progress events](../MACHINE_INTERFACE.md#progress-events).

```sh
forklift stocktake --json
forklift stack "x" --json
forklift mcp --root /path/to/warehouse    # run the MCP server against a warehouse
```

### `self-update` — check for and apply a newer release

```sh
forklift self-update            # update in place if you installed via the script
forklift self-update --check    # only report; change nothing
```

Compares this binary against the latest published release. If you installed via the
install script (or a manual copy), it updates in place by re-running that verified script.
If you installed via a **package manager**, it never overwrites the binary — it prints the
right upgrade command instead (`cargo install … --force`, `brew upgrade forklift`), so it
can't fight your package manager. `--check` (and `--json`) just reports `{current, latest,
update_available, install_method, update_command}`. There is **no server self-update** by
design: a server is redeployed (new container / Lambda version / package), never
self-mutated. If the [`fl` alias](#alias--the-fl-short-name) was already installed, self-update
keeps it working — the binary is replaced at the same path, and self-update also restores the
alias if it ever goes missing.

### `self-schema` — describe the command surface for Pult

```sh
forklift self-schema           # the document, pretty-printed
forklift self-schema --json    # the same document as the envelope's data
```

Prints Forklift's command surface in [Pult](https://github.com/lonic-software/pult)'s
manifest vocabulary, generated from this binary's own argument definitions, so it always
matches the binary that prints it. Each runnable command path (`office admit`, and a group
such as `office` that runs on its own) becomes one command with an id (`office-admit`), a
title, its params and a `run`: the argument tail after the program name, with options and
switches before a literal `--` and positionals after it, so a value is never mistaken for a
flag. A text option's value that looks like a flag (`-x`, `--json`) has to ride inside its
option as one word, `--grep=-x`, which is how `pult tool` passes it; written as two words, it
is refused rather than read as a value. Each param carries its argument's help text as its
`description`, which Pult shows beside the prompt and in its own `--help`; when several
optional positionals share one param
(`config`'s key and value), each part is labelled with its name. `pult tool` reads it and
prefixes the path of the binary it found; the document is not a runnable `pult.yaml` on its
own.

The global `--json`, `--verbose` and `--no-pager` are not params: they are the caller's
choice of rendering. `help`, `mcp` and `self-schema` itself are left out. The bearer token
of `franchise --token` and a `config` value are marked secret. An argument shape the
document cannot express exactly is refused with an error naming the command and argument,
never dropped: you get the whole surface or none of it.

### `compact` — pack the object store

```sh
forklift compact
```

Forklift stores every object as its own compressed file. That is simple and safe, but at
scale (say after importing a large git history) it becomes hundreds of thousands of tiny
files: each pays filesystem slack, and a full-history walk pays an open per object. `compact`
sweeps the loose objects into a few dense **pack** files — an append-only data file plus a
sorted index — so the store is a handful of large files and a read is a binary search plus one
seek. Each file's successive versions are stored as **deltas** — the change from the previous
version of the *same file* rather than a whole new copy — which is git's biggest space win.
`compact` finds that previous version the way git does, by walking history to associate each
blob with its path, so the deltas are as tight as git's: **git.git packs to 261 MB, smaller
than git's own 310 MB pack.** A loose object is removed only after the pack that holds it is
durably written, and a delta is content-verified on read, so compacting can never lose or
corrupt an object (interrupt it freely). Packs roll over at a size/count threshold, so no
single pack grows unbounded. `--json` reports
`{objects_packed, packs_written, loose_removed, deltas, bytes_packed, corrupt_skipped,
over_ceiling_skipped}`.
If a loose object's bytes do not decode, or decode but do not hash to the address its filename
promises, `compact` skips it and leaves it in place rather than packing or removing it;
`corrupt_skipped` counts those. A nonzero count is worth investigating — run `forklift audit`
to check the store. A loose object larger than the 64 MiB object ceiling is skipped the same
way, but it is not corrupt — it predates the ceiling — so it is counted separately as
`over_ceiling_skipped` and left loose, fully readable, forever.

`compact` also **builds the commit-graph** (`.forklift/graph/`) while it is here: a sharded,
self-healing cache that gives each parcel a generation number — so ancestry checks
(`consolidate`, `haul`) find a merge base without walking history to the roots — and a
changed-path filter — so `blame` skips parcels that did not touch the file. It is derived and
repairs itself, so you never manage it; building it during `compact` just means it is warm the
first time you need it.

**You rarely need to run this by hand.** `import-git` writes packs directly on the way in
(unless you pass `--no-compact`), and afterwards forklift **compacts automatically** in the background of a
mutating command once the store has accumulated enough loose objects or packs to warrant it
(git's `gc --auto`). It runs synchronously, under that command's lock, so it is correct and
never races — which means it can add a brief pause when it fires (rarely). Turn it off with
`config maintenance.auto false`, or tune the thresholds (`maintenance.loose`, `maintenance.packs`).

Compaction also takes a **shared object-store lock** (separate from the per-bay lock ordinary
commands hold), so two bays or two processes never compact the same store at once. If another
compaction is already running, an explicit `forklift compact` reports it and stops; the
automatic background compaction simply skips (the other run is doing the same work). An
incremental compaction never blocks ordinary mutating commands; a full repack does, briefly (see
below). Compaction of either kind is refused while a `scope-prune` or `forklift-server gc` runs
on the warehouse.

Plain `compact` is **incremental**: it packs the loose objects into a *new* pack and leaves
existing packs alone (cheap, but packs accumulate over time). `compact --all` is a **full
repack**: it rewrites every pack too, **dropping unreachable objects** that were stuck in
packs (garbage from undone stacks, abandoned pushes) and **consolidating many packs into
few**. Content you have loaded but not yet stacked is never garbage: every bay's staged files
are kept, alongside everything reachable from a pallet or from the head an open or closed haul
proposes (so a haul still merges after its source pallet moved off that head). It **reuses** each object's existing
delta (a byte-copy, not a re-compress), so it is
fast and never balloons the store — but heavier than incremental; run it occasionally (auto
does when packs pile up). Because it drops unreachable packed objects with no grace period,
`compact --all` is **refused while a `forklift-server` is serving that root** (or a
`forklift-server gc` is running against it): a lift in flight would lose objects it had already
uploaded. Stop the server, repack, then restart. Plain `compact` drops nothing and stays
allowed; the automatic repack simply waits until the root is no longer served. A server that was
hard-killed leaves its `serve.lock` behind, which blocks repacks (automatic ones silently) until
it is removed, exactly as it blocks `gc`.

A full repack checks each pack's whole index before copying anything out of it. A pack whose
index fails that check (a **damaged** pack, as `store` lists it) is **salvaged** instead of copied:
before anything else, the repack reads the pack's data file record by record without trusting the
index, and stores every object it can decode as an ordinary loose object, so an object only a
damaged part of the index named reads again. Nothing is copied out of a damaged pack verbatim; each
live object it serves is read and verified first. The pack is then **removed** only when every live
object it might hold reads from another copy (`Salvaged and removed the damaged pack …`; `--json`:
`damaged_packs_dropped`, the index paths). Otherwise it is **kept**, and the note names the live
objects no other copy serves (`--json`: `damaged_packs_kept`, `[{index_path, error, unaccounted}]`):
this store cannot recover those (a good copy of the pack's files, or of the objects from
elsewhere, can). Salvage writes every object it decodes, garbage included; the garbage is collected
later by `gc`, and until then it can need up to the pack's decompressed size in free space. Only an
explicit `compact --all` salvages: the automatic repack leaves a damaged pack exactly as it found it.
Ordinary commands never pay that whole-index check: they verify only the slice of an index they
actually read.

Telling garbage from live content means reading every parcel, tree and large-file recipe still
in use. If one of those is missing but a broken pack might hold it (a damaged part of a pack, a
pack whose data file is missing, or a pack index that cannot be read at all), the full repack
**refuses and deletes nothing**, naming the object: what it refers to cannot be told apart from
garbage. `scope-prune` and `forklift-server gc` refuse the same way. `forklift store` lists the
pack under `quarantined_packs`, `damaged_packs`, `unenumerable_indexes` or
`foreign_version_indexes`; once the pack is restored or moved aside, the sweep can run. A full
repack salvages a damaged pack before this walk, so it refuses only on what salvage could not
recover, and then says that the pack could not be completely salvaged: nothing was deleted, but an
object read through the pack's damaged part may be held nowhere else, and this store cannot recover
it. A damaged pack it salvaged completely (every record of its data file decoded and readable
again) no longer blocks the walk on an object its index never named.

For the same reason a full repack is **refused while a mutating command is running in another
bay** of the same warehouse: that command may have just decided to reuse an object the repack
was about to drop. Run it again once the other command finishes. The opposite case waits
instead of failing. A mutating command that starts while a full repack, a `scope-prune` or a
`forklift-server gc` runs on the warehouse prints `Waiting for object-store maintenance in
another process…` and carries on once it finishes. The coordination is a kernel lock on
`.forklift/store-access.lock`, so a process that dies releases it and leaves nothing to clean
up.

```sh
forklift compact          # incremental: pack the loose objects
forklift compact --all    # full repack: drop garbage, consolidate every pack
```

`compact --all --redelta` goes further: instead of reusing each object's existing delta, it
re-reads and re-compresses **every** live object (packed or loose, files and directories alike)
and re-runs delta selection on the whole store together. That matters because a repack's
copy-fast-path can only keep whatever delta an object was *originally* given — and an object
packed in isolation (say, on import, or by an earlier incremental compact whose delta window
only ever saw a few neighbors) has no way to know a much better match (a rename, a moved file,
anything similar outside its own path) sits elsewhere in the store. `--redelta` gives every
object one more shot at the best base across the *entire* live set. It costs a full re-encode —
one-shot and CPU-bound, not something to run routinely — and it is only valid together with
`--all`. Safe to repeat any number of times on the same store. `store` (and `import-git`'s and
`franchise`'s own output) suggest running it when the store was bulk-ingested and has not been
through a redelta pass yet; a successful run clears that suggestion.

> This step removes per-file slack and the open-per-object read cost. Delta compression
> between similar versions (the rest of the size gap vs git) is additional work on top —
> see [`../OBJECT_STORE_SCALING.md`](../OBJECT_STORE_SCALING.md).

### `store` — object-store health

```sh
forklift store
```

The read-only counterpart of `compact` (and the object-store analog of `stocktake`, which
reports the working tree). It takes an **exact** census of `.forklift/objects`: how many
objects are **loose** (unpacked) versus **packed**, how many pack files there are and how
**delta-dense** they are, the on-disk sizes, and — against the `maintenance.loose` /
`maintenance.packs` thresholds — whether an incremental compaction or a consolidating repack
is currently **due**. It answers "how much of the store is packed, and does it need
maintenance?" without doing any work.

```
Object store
  loose:   12 objects   (48.0 KiB)
  packed:  402118 objects in 1 pack   (236.0 MiB, 261000 deltas)   99% packed
  total:   236.0 MiB on disk

  maintenance: auto on
    compaction  not due — 12 / 6700 loose objects
    repack      not due — 1 / 20 packs
```

`--json` reports `{loose_objects, loose_bytes, packed_objects, pack_files, deltas, pack_bytes,
total_bytes, packs:[{id, objects, deltas, bytes}], maintenance:{auto, loose_threshold,
pack_threshold, compaction_due, repack_due}, densify_suggested, quarantined_packs, damaged_packs,
unenumerable_indexes, foreign_version_indexes}`
— every size an exact byte count. Nothing is written; run `compact` to act on what it reports.
`densify_suggested` (and, in the human view, a one-line tip) appears once the store was
bulk-ingested (`import-git` or a franchise's bundle install) and clears once a `compact --all
--redelta` pass has run (see [`--redelta`](#compact--pack-the-object-store)).

The four problem lists are ordinarily all empty. A pack pair loses its
data file only in unusual circumstances (a file removed by hand, a partial restore); when that
happens, `store` lists it under `quarantined_packs` — `[{index_path, error}]` — rather than
failing the census: the objects it named are not lost, only unreachable through that copy, and
`heal` can refetch from a configured remote any of them still under a durability taint (for the
rest, restore the pack's data file). `store` checks every index in full: a pack whose
index is damaged in places is listed under `damaged_packs` (same shape) and left out of the counts.
Ordinary reads keep using its sound parts; an object in a damaged part reads as unverifiable.
`forklift compact --all` salvages it (see [`compact`](#compact--pack-the-object-store)). `unenumerable_indexes` — the same `[{index_path,
error}]` shape — lists any `.idx` file that could not even be parsed (its own magic/version/shape
is wrong); its hash set is unknown, so nothing can be recovered through it automatically — move it
aside and re-run. `foreign_version_indexes` (same shape) lists an index written by a build with a
different pack format, naming its version. Every read it might answer fails with that error rather
than reporting the object missing, so re-create the warehouse with this build — do not move the
file aside, which would make its objects read as missing. In the human view all four appear as a `pack problems:` section, only when non-empty.

### `heal` — resolve a durability taint or an interrupted operation

```sh
forklift heal
```

You will rarely see this: after certain rare write failures (a directory-sync error right
after every file in a batch was already renamed into place — the one window a crash could
still lose a dentry a durable reference points at), the affected paths are recorded and every
command refuses with a machine-coded `durability_taint` error (exit 21) until it is resolved.
Before you ever need `heal` by hand, the very next command usually resolves it silently on its
own — an automatic check at the start of every command restages whatever it can prove is
still good.

Run `forklift heal` when a command keeps refusing with `durability_taint`. It repeats that
same restage pass and, for whatever it still cannot resolve on its own, walks every pallet
head, parked change, staged file, and trust pin (the anchor's boundary, and every
revoked key's own distrust boundary) to work out whether the affected object is still
needed by anything. If a remote is configured, `heal` also tries fetching from it itself, right
here, before giving up on anything — you never need to run `lower` by hand for this (and it
would not help: `lower` is subject to the very same taint check, so it would just hit the same
refusal before ever reaching its own fetch):

- **Nothing was actually lost, or what vanished is not needed by anything anymore** — `heal`
  clears the taint and reports what it restaged or dropped. Exit 0; go on with what you were
  doing. Tags are not part of that walk, so a tag never holds a heal up, and neither does
  anything only a tag reaches. A tagged parcel may never
  have been fetched (a clone with `--only` still copies every tag) or may have been collected
  after an `undo` moved the history off it, and nothing could bring it back. `heal` names such a
  tag in a `Note:` line and clears, or notes that the tags could not be read at all. The same goes for a trust boundary naming a head this store
  never fetched. A lost parcel that a pallet's history still needs is reported as below, even if
  a tag also names it.
- **Something is genuinely gone, and something still needs it** — `heal` reports exactly what,
  with the remedies left once its own fetch attempt has already run: franchise a fresh clone
  from a configured remote if one might still have it, or reproduce it by re-running whatever
  created it if the content still exists in your working tree (`load` then `stack`). The taint
  stays standing on exactly that remainder, and every other command keeps refusing (exit 21)
  until it is dealt with.
- **The record of what was affected is itself incomplete** (a crash cut off the record before
  it named everything) — this is rare; it needs a second, unrelated crash on top of the original
  failure. `heal` no longer treats this as a dead end: it re-derives the full scope itself by
  checking every object actually present in the warehouse and re-walking every pallet head,
  parked change, staged file, and trust pin to see what is still needed, then records
  exactly what came out dangling. In a clone made with `--only`, pallet content outside the
  clone's paths was never fetched, so its absence is not reported; if the file recording those
  paths cannot be read, the incomplete record is left standing and `heal` says so. This can take
  noticeably longer than the ordinary case (it is
  checking the whole object store, not just the affected paths), and it prints progress as it
  goes. If nothing turns out to be missing, the taint clears — exit 0, same as any other clean
  heal. If something is genuinely gone and still needed, the taint stays standing on exactly
  that (no longer incomplete) remainder; running `heal` again resolves it exactly like the
  "something is genuinely gone" case above, including trying a configured remote.
- **A source `heal` itself needs could not be read** — a bay's own saved state, or the office
  record behind a revoked key's distrust boundary. When this run actually needs that source,
  `heal` refuses rather than guess, naming what was unreadable and the actual remedy: a bay names
  the in-tool cleanup route; the office record gets the remedy that fits what went wrong — an
  object that was never written durably says to roll the office pallet back to a head you
  actually hold, or fetch the missing object from a configured remote; a record that loaded fine
  but does not parse says to fix (or restore a good copy of) that record, since running `heal`
  again alone does not change its bytes; anything else names the underlying error to resolve.
  Re-run once the named source is fixed. A run that has nothing to look up — every recorded entry
  resolved on its own, so no reachability question was ever asked — clears without consulting
  these sources at all, and so never reports on them.
- **An object the check walks through could not be read** — one that only a damaged pack index
  might hold (its data file missing, a corrupt part of the index, another format version, or an
  index that does not parse). What that object references is unknown, so `heal` clears no vanished
  object on that run: every one it could not confirm is safe to drop keeps its taint, not only those
  beneath the object (paths it restaged or read back verified still clear). The note names the
  objects and the index. `heal` does not refetch these objects itself, so fix the index by hand:
  restore its pack files from a good copy (a missing data file or a damaged index); move an index
  that does not parse out of the pack folder if it holds nothing this warehouse needs; or, for an
  index of another format version, re-create the warehouse — never move that one aside, which would
  make its objects read as missing. Then re-run `heal`. A torn record is likewise left standing.

`heal` and the read-only `audit` are the only two commands that run while a taint is standing —
every other command refuses first, precisely so nothing durable is ever recorded on top of
unproven state.

Every `heal` run also checks the pack folder for the same problem indexes `store` reports (see
above) and prints a warning line for each one it finds — unconditionally, whether or not anything
else was tainted this run, and whether or not that index is what caused the taint. A quarantined
index's objects are left recoverable (`heal` refetches those under a durability taint, and
restoring the pack's data file recovers the rest); a damaged pack still serves its
sound parts, and once no taint is standing `forklift compact --all` salvages it; an index of another format means re-creating the warehouse; an unenumerable one names
the file to move aside. This appears even when `heal` still has to refuse — as a
clearly-separated "Pack index problems in this store (clear each as its line says)" section. An
index the refusal itself names (see the bullet above) is part of its cause; the time one coincides
with a refusal is exactly when an operator most needs to be told about it.

#### Interrupted operations

Every command that rewrites the whole staging area — `shift`, `park`, `consolidate` (a merge or a
fast-forward, and the merges `haul` and `lift` run for you), `cherry-pick`, `lower`, `import-git`,
and creating a bay — first records that it has begun, and clears the record once it is done. If
one is killed or fails part-way (a crash, a full disk, a power cut), the record stays, and every
command that changes the bay or commits staged content (`stack`, `load`, `shift`, `park`, `lift`,
`haul`, and the rest) refuses with `incomplete_load` (exit 20), naming the command that was
interrupted. Committing then could record a half-written staging area — at worst, the removal
of every tracked file.

`forklift heal` finishes or undoes the operation, whichever the bay's state says it reached:

- **It had already finished its work** (its new head is recorded, even if later work was stacked
  on top) — the record is cleared and nothing else is touched.
- **It can be finished** — a shift (always: the tree it was moving to is already stored), a
  fast-forward or merge whose head landed, a park whose work is already in the parked list, an
  `import-git` or a bay creation — the files it had not written yet are written, staging is
  rebuilt from the tree it was moving to, and the bay is put at that position. For a park, that
  means the working files are reset to the head, as the park would have left them.
- **It did not get that far** — the head it was moving to never landed, or another bay moved the
  pallet meanwhile — the operation is undone: for a merge, cherry-pick, fast-forward or `lower`,
  the files it wrote are put back to the tree it started from, and staging and the position
  follow. A park that was interrupted before its work reached the parked list leaves that work
  where it was.

Either way, heal only rewrites a file that still holds one of the two trees' content, or is
missing. A file at anything else — an edit you made after the crash, or a file a merge combined
from both sides — is left as it is and reported as a local change (`unheld` in `--json`), never
staged: check it with `stocktake`, and `restore` it if it is not yours.

An interrupted `franchise` is not recovered by `heal`, which refuses there like every other
command: run the same `franchise` again to resume it (see `franchise`). `heal` reports what it did as an `operation` entry (`finished`,
`rolled_forward`, `rolled_back` or `cleared`) beside its taint report. A `load` run meanwhile does
not clear the record: only `heal` does.

---

## 9. Configuration reference

Two scopes: **warehouse** (`.forklift/config/warehouse.toml`, default) and
**global** (`~/.forkliftconfig`, with `--global`). Reads prefer the warehouse
value. Known keys:

| Key | Meaning |
|-----|---------|
| `operator.name` | Your display name (local only — never written on-chain). |
| `operator.identifier` | Your on-chain operator id (opaque; a UUID is minted if unset). |
| `operator.profile` | The named profile this warehouse acts under (see `profile`). |
| `remote.url` | The remote warehouse URL (for `lift`/`lower`). Can be a Tor `.onion` — see [Reaching a remote over Tor](#reaching-a-remote-over-tor). |
| `remote.token` | The bearer token, when the remote requires one. |
| `remote.tor` | Reach the remote over Tor: `auto` (default — only `.onion` remotes), `on` (every remote), `off` (never). |
| `remote.torProxy` | The Tor SOCKS proxy (default `socks5h://127.0.0.1:9050`). |
| `maintenance.auto` | Auto-compact after mutating commands: `true`/`1`/`on`/`yes` or `false`/`0`/`off`/`no` (default on). |
| `maintenance.loose` | Loose-object count that triggers an auto incremental compact (default 6700). |
| `maintenance.packs` | Pack count that triggers an auto consolidating repack (default 20). |

**A configuration file either parses completely, or the command that needed it refuses**, naming
the file and the offending key. There is no partial read. That matters because "unset" is not
inert — an unset `operator.identifier` mints a fresh id and writes it back, so a key that read as
absent because it was damaged would quietly replace the identity you configured.

One consumer warns instead of refusing, and only one: **background auto-compaction**, which runs
after a mutating command's own work has already succeeded. It cannot refuse without taking back
a result the command already reported, so it prints a `maintenance_unavailable` warning on
stderr and leaves the exit code alone. A command that reads no configuration of its own — `load`,
say — therefore still exits 0 on a broken file, with that warning. Every other command refuses,
whichever key it wanted: what is refused is the **file**, not the damaged key, so a typo in
`maintenance.auto` in the global file stops a command that only needed your identity from it.

A configuration file that is there but cannot be read is not "absent". That includes a symbolic
link to a path that does not exist — a dotfiles link whose target you have not created yet — so
create the target, or remove the link, before the first command that needs it.

**Every write replaces the file whole, and edits to one file take turns.** A command writes the new
configuration to a temporary file beside the old one and renames it into place, so a reader sees
either the old file or the new one — never a half-written file — and a crash mid-write leaves the
old file intact. Commands that edit the same file at once wait for each other on a lock held on a
`<file>.lock` sidecar beside it (`warehouse.toml.lock`, `.forkliftconfig.lock`); each re-reads the
file once its turn comes, so concurrent edits do not lose one another, and two first commands that
both need an `operator.identifier` agree on one instead of each minting its own. The sidecar is
empty and is left in place; deleting it while no command is running is harmless. Reads take no
lock. A configuration file that is a symbolic link is written *through* the link: the link stays,
its target is replaced, and the lock is taken beside the target. A link whose target does not
exist — to the file, or to a folder above it — is refused, rather than replaced with a plain file.
On Unix the replacement keeps the old file's permission bits, so a `chmod 600` on a file holding
`remote.token` survives every edit; its owner, extended attributes and any hard links do not,
since the file is replaced rather than rewritten.

Refused, in either file:

| Written | Why |
|---------|-----|
| `[opperator]`, or any other unknown section | a mistyped section header hides every key inside it |
| `identifer = "ada"` | an unknown key inside a known section — the typo that mints over your identity |
| `[[operator]]`, `[[profile.work]]` | an array-of-tables where a table is expected |
| `operator = 5` | a section that is not a table at all |
| `loose = 6700`, `tor = true` | every value is a **string on disk**, including the numeric and boolean ones — write `"6700"` and `"on"` |
| `torProxy` spelled `torproxy` | keys are case-sensitive |
| `tor = "onn"` | `remote.tor` accepts only `auto`, `on` or `off`. A typo here used to mean `auto` silently, un-proxying every non-onion remote |
| `auto = "flase"`, `loose = "10k"` | `maintenance.auto` accepts only the spellings listed above, and `maintenance.loose`/`maintenance.packs` only a whole number. A typo here used to leave maintenance on, or fall back to the default threshold, without a word |
| an unknown field in a `[profile.<name>]` section | profiles hold `identifier` and `name`, nothing else |
| any key set to `""` (or to whitespace) | a key holds a value or is absent, there is no third state — an empty `identifier` was read as an identity by one path and as "fill this in" by the other, which minted a new id *over* your entry. Remove a section key with `forklift config --unset <key>` (add `--global` for the global file); a `[profile.<name>]` field the command cannot address, so remove that one in an editor |

**A write is refused unless the file it would leave on disk parses completely.** The check is on
the result, not on what was there before — so `forklift config` *can* repair a broken entry it is
able to express: with `tor = "onn"` in the file, `forklift config remote.tor on` and `forklift
config --unset remote.tor` both succeed, because neither leaves the bad value standing. What is
refused is a write that would preserve it: setting `remote.url` while `tor = "onn"` is still
there fails, naming the `tor` entry rather than the key you were setting. A fault the command
cannot express — `operator = 5`, an unknown section, a mistyped key — still needs a text editor;
the refusal names the file and the key either way. Values set *through* `forklift config` are
quoted and range-checked for you, which is why none of the above can bite a file you have only
ever edited with the command.

Set / read / remove:

```sh
forklift config operator.name "Ada"
forklift config --global operator.identifier ada@example.com
forklift config remote.token
forklift config --unset remote.token
```

The `FORKLIFT_GLOBAL_CONFIG` and `FORKLIFT_KEYS_DIR` environment variables
relocate the global config file and the private-key directory (used by tests and
for isolating identities). `FORKLIFT_KEY_PASSPHRASE` supplies a protected key's
passphrase non-interactively (an automation escape hatch — see
[`trust-and-identity.md`](trust-and-identity.md#6-passphrase-protected-keys)).

`FORKLIFT_FSYNC` controls write durability. By default every object, ref and pack
write is fsynced (file **and** its directory) so an interruption — including power
loss — can never lose or truncate committed data. Set it to `0`, `off`, `false`,
or `no` to skip all fsyncing: markedly faster for **bulk, disposable** work (large
imports, CI, throwaway fixtures) where a mid-run crash just means re-running the
whole operation. Leave it on for any warehouse whose history you intend to keep.

### Reaching a remote over Tor

Forklift can reach a remote through Tor — the payoff is **NAT traversal**: collaborate with a
remote that has **no fixed IP and no port-forwarding**, because a Tor `.onion` address is
reachable from anywhere on the Tor network. Tor is the mechanism, not the point.

By default (`remote.tor = auto`) this is invisible and opt-in per remote: a `.onion` remote is
routed through your local Tor SOCKS proxy automatically, and every other remote is dialed
directly and unchanged. So an onion URL Just Works while a plain `http(s)` remote behaves
exactly as before.

```sh
forklift config remote.torProxy socks5h://127.0.0.1:9150   # Tor Browser's bundled SOCKS port
forklift config --global remote.tor on                     # route every remote through Tor
```

- `remote.tor` — `auto` (default, onion-only), `on` (every remote — e.g. reach a clearnet
  remote anonymously), or `off` (never, even a `.onion`).
- `remote.torProxy` — where your local `tor` listens; default `socks5h://127.0.0.1:9050`. The
  `socks5h` scheme resolves the host **at the proxy**, which is what lets an opaque `.onion`
  name (it has no DNS record) resolve inside the Tor network.

> **With `on`, use `https://` for any non-onion remote that carries a token.** Routing a plain
> `http://` clearnet remote through Tor sends its bearer token out through a Tor **exit relay** —
> a *worse* position than the direct network path, since hostile exits harvest plaintext
> credentials. Onion remotes are unaffected: traffic is end-to-end encrypted to the onion key, so
> `http://…​.onion` is fine.

You need a local `tor` daemon for this. The default `9050` is a stock system `tor`; the `9150`
example above is **Tor Browser's** bundled daemon — convenient, but it only listens while the
browser is open. Either way, nothing here needs the Tor Browser *UI*. The transport adds **no new
dependency** — it flips on the SOCKS5 client already vendored in the HTTP stack.

---

## 10. Command & alias reference

### Coming from git?

Forklift accepts the familiar git command names as hidden aliases, so your muscle memory
works on day one — while forklift's own vocabulary stays primary in `--help` (the aliases
nudge you toward the real names as you go):

| You type (git) | Runs (forklift) | | You type (git) | Runs (forklift) |
|---|---|---|---|---|
| `init` | `prepare` | | `checkout` / `switch` | `shift` |
| `add` | `load` | | `merge` | `consolidate` |
| `commit` | `stack` | | `clone` | `franchise` |
| `status` | `stocktake` | | `push` | `lift` |
| `log` | `history` | | `pull` | `lower` |
| `branch` | `palletize` | | `stash` | `park` |
| `annotate` | `blame` | | `rm --cached` | `remove` |
| `restore --staged` | `unload` | | | |

The mapping isn't always one-to-one in *behavior* (e.g. `commit` runs `stack`, which commits
what you've `load`ed — it doesn't stage for you). `diff`, `restore`, `tag`, `show` and
`cherry-pick` keep their git names outright. For the review workflow, git's "pull request" is
[`haul`](#haul--pull-requests-reviewable-merge-proposals).

| Command | Alias | Does |
|---------|-------|------|
| `prepare` | `p` | Create a warehouse here |
| `import-git` | | Migrate a git repo's history into the warehouse |
| `export-git` | | Export the warehouse's history back into a git repo |
| `alias` | | Manage the `fl` short alias next to this binary |
| `config` | `cfg` | Read/set/unset configuration |
| `profile` | | Manage named identity profiles |
| `load` | `l` | Stage changes into the inventory |
| `unload` | `ul` | Unstage (undo a `load`) |
| `remove` | `rm` | Stage a removal |
| `restore` | `r` | Restore from the inventory (discard changes) |
| `stocktake` | `st` | Show staged and unstaged changes |
| `diff` | `d` | Show line-by-line changes |
| `stack` | `s` | Record the inventory as a new parcel |
| `undo` | | Reverse the last stack, consolidate, cherry-pick or shift |
| `peek` | `pk` | Inspect an object or inventory |
| `show` | | Print a file's content at a revision (`<revision>:<path>`) |
| `history` | `hi` | Walk the parcel graph |
| `blame` | `bl` | Attribute each line to its author (with identity class) |
| `audit` | `a` | Verify signed history offline |
| `palletize` | `pz` | Create a pallet / list pallets |
| `shift` | `sh` | Switch pallets |
| `consolidate` | `con` | Merge another pallet in |
| `cherry-pick` | `cp` | Apply a parcel's diff here, preserving authorship |
| `deliver` | `dv` | Squash a draft onto a target, keeping the trail |
| `bay` | | Parallel working directories (add / list / remove) |
| `compact` | | Pack the loose object store into dense pack files |
| `store` | | Report object-store health (loose vs packed, sizes, maintenance due) |
| `heal` | | Resolve a standing durability taint, or finish or undo an interrupted operation |
| `conflicts` | | List unresolved conflicts |
| `park` | `pa` | Stash / list / pop work in progress |
| `franchise` | `fr` | Clone a remote warehouse (`--only` for a sparse clone) |
| `lift` | `li` | Push to a remote |
| `lower` | `lo` | Pull from a remote |
| `expand` | | Widen a sparse warehouse's fetch scope and fetch the new subtree(s) |
| `narrow` | | Shrink this checkout's materialization scope (frees nothing) |
| `office` | `o` | Manage users, keys, roles, agents, trust |
| `manifest` | `mf` | Record/read signed post-metadata on parcels |
| `tag` | | Signed tags / releases (admin-signed, offline-verifiable) |
| `mcp` | | Serve the command surface as MCP tools |
| `self-schema` | | Describe the command surface as a Pult manifest |
| `help` | `h` | Show help |
| `version` | `v` | Print the version |

---

## 11. Exit codes

Errors set a deterministic exit code so scripts branch without parsing prose. `0`
(success), `1` (unclassified failure), `2` (usage error), `130` (cancelled by the user) and
`128`+n (killed by signal n) mean the same for every Lonic tool; every other code is one of
forklift's own classified failures.

The full, always-current table — generated from the same enum the binary itself
branches on, so it can never fall behind — is
[`../generated/errors.md`](../generated/errors.md).

With `--json`, the same classification appears as `error.code` in the output
envelope, usage errors included. See [`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md).
