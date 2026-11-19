# Recovery and troubleshooting

Something refused, or something looks broken. Find the code below and do what its entry says.

Every failure prints a message and exits with a fixed status, and almost every failure prints its next
step after `Next step:`. With `--json`, the same code is in `error.code` and the step in
`error.next_step`. The
[table of error and exit codes](../generated/errors.md) lists them all; this page says what to do
about each one.

**A refusal changed nothing, unless its entry says otherwise.** Read the message before you delete or
re-create anything: the commonest way to lose work here is to "fix" a refusal that was protecting it.

- [Undo that](#undo-that)
- [Interrupted and locked](#interrupted-and-locked)
- [Damage to the store](#damage-to-the-store)
- [Working with a remote](#working-with-a-remote)
- [Trust and lost keys](#trust-and-lost-keys)
- [Partial clones and scoped bays](#partial-clones-and-scoped-bays)
- [Everything else](#everything-else)

## Undo that

```sh
forklift undo                # reverse the last stack, consolidate, pick, shift or pallet delete
forklift undo --as rescued   # bring a deleted pallet back under another name
forklift park pop            # bring back work you parked
```

`undo` keeps your files after a `stack`, `consolidate` or `pick`: the changes are staged again. It
only reverses your *last* operation, and refuses once something else has moved the pallet; then
continue forward by stacking a parcel that reverts the change. Use `undo --as` when a pallet of the
deleted name exists again at another head. The [`undo` reference](cli.md#undo--reverse-the-last-operation)
lists every case.

`undo` does not cover `load`, `remove` or `unload`, and `park` is reversed with `park pop` only.
To throw away edits on purpose, `forklift restore` previews what it would discard, and acts with
`--confirm`.

## Interrupted and locked

### `warehouse_locked` (exit 6), `store_locked` (exit 6), `serve_locked` (exit 6)

Another forklift process that is still running holds a lock the command needs:

- `warehouse_locked`: the bay's lock, held by any command that changes the bay;
- `store_locked`: the object store's lock, held by `store compact`, `store evict`, a redaction's
  deletion and heal's rebuild of an index while they run;
- `serve_locked`: the warehouse root's lock, held by a `forklift-server` serving it, and by `gc`,
  `store compact --all` and `store evict` while they run.

Wait for that process to finish, or stop the server, then run the command again. There is nothing
to remove. Each lock is a kernel lock on a file that stays in place, and the kernel releases it when
its process ends, however it ends: a `kill -9`, a crash or a power cut leaves nothing held, so the
next command and a restarted server go ahead on their own.

### `incomplete_load` (exit 20)

A `load` failed part-way, or a command that rewrites the staging area was killed. Stacking now could
record a half-written staging area, so `stack`, `park`, `consolidate` and `pick` refuse. After an
interrupted command, so does anything else that changes the bay, including `load`. The message says
which case you are in.

After a failed load, fix what stopped it (the load names the file), then do what the message says:
run the same `forklift load` again, or `forklift unload` the same path to abandon it.

After an interrupted command, run:

```sh
forklift heal
```

`heal` finishes or undoes the operation, whichever its state allows, and reports which; see
[interrupted operations](cli.md#interrupted-operations). It leaves any file holding other content
alone and reports it as a local change, so check `forklift stocktake` afterwards.

**Do not** delete the marker by hand.

### `durability_taint` (exit 21)

A write failed in a way that a crash could have lost data, so forklift refuses to record anything on
top of it. The next command usually clears this by itself. If a command keeps refusing, run:

```sh
forklift heal
```

Exit 0 means nothing needed is lost, and you can carry on. If it names something gone and still
needed, `heal` has already tried your remote: clone afresh if a remote might hold it, or re-create it
with `load` and `stack` if the content is still in your working tree. `audit` also runs while a taint
stands, but says nothing about this damage; see
[`heal`](cli.md#heal--resolve-a-durability-taint-a-damaged-ref-or-an-interrupted-operation) for the rarer cases.

### `ref_damaged` (exit 53)

A pallet's ref is missing although a checkout stands on it, or, for a meta pallet, although its copy
stands, or its file does not hold a parcel hash: a crash, a disk fault or a hand edit damaged it. Run:

```sh
forklift heal
```

`heal` rebuilds the ref at the head the warehouse agrees on: the bases your checkouts stand on, their
journals, an interrupted operation, and what the damaged file still names. If those disagree or name
nothing, it changes nothing and lists every head it found. Pick one with
`forklift heal --ref <pallet>=<parcel>`. If nothing names a head and no checkout stands on the pallet,
`forklift heal --remove-ref <pallet>` drops the ref and keeps its bytes under `.forklift/removed-refs`;
what only that ref reached is then no longer kept by any pallet.

**Do not** write the ref file by hand: `heal --ref` checks that the parcel's history is complete.

A meta pallet's ref (a name starting with `@`, such as `@tags`) keeps a copy beside it
(`.forklift/meta-copy/<name>`), written with every change of the ref. A meta ref whose file is missing
while its copy stands refuses as `ref_damaged` wherever it is read, in place of reading as an empty pallet,
and so does one whose file does not hold a parcel hash. `heal` rebuilds it from its copy, or from what
the damaged file still names, the office's and `@redactions`' among them, so the refusal names
`forklift heal`, which judges the ref. Where neither names a head `heal` can rebuild at (the copy does
not hold a parcel hash, and the damaged file names nothing, several parcels or one that cannot be read,
or is missing), or `heal` refuses the ref for a reason of its own, `heal`'s refusal names
`forklift heal --remove-ref @<name>`. It removes the ref and its copy, keeping the bytes of
each under `.forklift/removed-refs`, and the pallet then reads as empty: what it recorded (tags,
redactions, the office's keys) is gone from it, and what only its history reached may be freed.
Restore the files from a backup instead if you have one. It runs only once no durability taint
stands, and not for a ref `heal` refused only because the office's or `@redactions`' ref was refused,
or because the office is unborn beside the trust file: it names what to settle first. Nor does it run
where `heal`'s refusal is no verdict on the ref, and the step `heal` names then depends on why:

- It could not read what it needs (a file it may not read, a pack it cannot open or list in full): the
  step that read's own refusal names, else settling what it names, then `forklift heal` again, which
  judges the ref.
- It chose a head but could not write the rebuild, or could not read the ref back after writing it: the
  step the write's refusal names, else settling why it failed, then `forklift heal` again.
- What it needs reads in a format this Forklift does not know, as one a newer version wrote: upgrading
  Forklift.
- The trust state refuses: `forklift heal` again where `heal` restores the trust file from its copy.
  Otherwise, one of the ways out of that refusal (those a lost anchor has on this warehouse, or those
  the trust file's own refusal names, as where it is not a file or does not parse), or, where the
  office's history does not verify against the anchor, settling why.

A copy that cannot be read, a link at the
copy's path, or something other than a file at the ref's path is refused too, naming it: make the copy
and the folders on the way to it readable, replace the link with the copy's file (moving it aside would
leave the pallet reading as never written), or move aside what stands at the ref's path, and run the
command again. While a link or anything else that is neither a file nor a folder stands at the copy's
path, nothing can be kept from it, so beside a damaged ref `heal` and the removal name that same way
out instead of the removal.

A meta ref lost together with its copy leaves nothing that says the pallet was ever written, so it
reads as empty, as one never written does: `tag list` then lists no tags, for example, and refuses
nothing. Restore both files from a backup.

`heal` rebuilds the office pallet's ref (`.forklift/meta/office`) from its copy
(`.forklift/meta-copy/office`) only once the copy's head verifies against the trust file's anchor, and
not at all where a newer version of Forklift wrote the trust file or its copy, or the office's history
cannot be read from the store.

The office pallet's ref missing beside a trust file, with its copy missing too, refuses as
`ref_damaged` in the commands that need the verified office, such as `stack`, `audit` and
`office enroll`: the anchor stands, but there is no office to check anything against.
`forklift lower` takes the office pallet from a remote that holds it and verifies it against the
trust file. `forklift lift` asks the remote, so it names lower as the next step only where the remote
holds this warehouse's office; other commands name it as a way out where a remote is configured.
With neither a remote nor a copy to rebuild from, restore the ref from a backup, or write the office
pallet's head parcel hash into it by hand. Write the latest head you know of: an older one re-admits
any key revoked since. If `office enroll` stopped before it wrote the office pallet, there is no ref to
restore. Where no remote holds this anchor, move the trust file (`.forklift/trust`) and its copy
(`.forklift/trust-copy`) aside and run `forklift office enroll` again.

### `fetch_scope_damaged` (exit 54)

A sparse warehouse's fetch scope (`.forklift/config/fetch-scope`, the paths it fetched) is lost or
damaged, and `heal` could not rebuild it from your checkouts' scopes. Commands that met the damaged
file first named `forklift heal`; this is what it says when that is not enough. Nothing was changed.
Run the next step the message names:

```sh
forklift heal --fetch-scope <path> --fetch-scope <path>
```

It sets the fetch scope to those paths after fetching from the origin whatever the history there
lacks, so the origin must be reachable. The paths must hold every checkout's scope and cannot be the
warehouse root. Which step is named depends on why:

- **History is missing under the checkouts' scopes.** The step names their union; heal fetches what
  was lost.
- **No checkout's scope could be read.** The step offers the top-level folders of the working tree.
  Name fewer or others if you know what this warehouse fetched.
- **The fetch scope can be read but does not hold a checkout's scope.** One of the two files is
  damaged. If it is the fetch scope, the step widens it. If it is a checkout's, run `forklift heal`
  in that checkout, which restores its scope from its copy, or `forklift heal --scope <path>` there
  (see `checkout_scope_damaged` below).

A rebuilt fetch scope may be narrower than what was fetched before: paths only the old one named are
no longer checked for loss. `forklift scope fetch <path>` adds one back.

### `tombstones_damaged` (exit 55)

The list of pallets deleted on a server (`.forklift/tombstones`) or its `served` mark is lost or
damaged. That list keeps the deleted pallets' history from gc, since clones may have pruned
against it, so nothing that frees history runs until it is repaired. Nothing was changed.

**If you run the server**, stop it, then run the next step the message names:

```sh
forklift-server tombstone repair --root <warehouse>
```

It moves the damaged files aside, keeps every entry that still reads, and gives a lost deletion its
name back where a bay's undo journal settles it. All other history that no pallet, office pin or
tombstone holds is kept with no name, because it may be what the lost list held.
`forklift-server tombstone list --root <warehouse>` shows those heads as `(kept, no name)`. Once you
know no clone relies on one, `forklift-server tombstone drop --root <warehouse> --head <hash>`
releases it. A deleted pallet whose name was not recovered can be created again without a
`--recreate`. A list that does not match its digest may hold an entry that reads whole but was
altered, such as a digit of its head, so the repair says so: check the kept entries against the
pallets you deleted. Then run `forklift heal` if a ref was damaged too.

**If a remote refused**, only its operator can repair it. Your warehouse is unchanged. Try again once
they have.

### `withheld_damaged` (exit 56)

The record of content a server withholds (`.forklift/withheld`, its legal takedowns) is lost or
damaged, so what it must not serve cannot be told. Nothing was served or changed.

**If you run the server**, stop it, then run the next step the message names:

```sh
forklift-server withhold repair --root <warehouse>
```

It keeps a copy of the damaged record beside it and keeps every entry that still reads. A hash whose
entry lost its statement, or whose line was cut short or is not valid UTF-8, stays withheld under a
placeholder statement; restate it with
`forklift-server withhold add --root <warehouse> <hash> --statement "…"`. It then lists the files
the history names that are absent and that no record holds: what a lost entry withheld, once its copy
was deleted. Each is recorded as unaccounted, and the warehouse is not served while one is. Put back
the bytes of any you hold a copy of and run the repair again, so it stops counting for good if they
go once more, and withhold each one that was taken down, or is gone for good,
again. Absent folders and parts of history are listed apart: nothing beneath them could be walked,
so a takedown's file may sit there unseen, and the warehouse is not served while one is listed,
even once it is back. Restore each from a copy of the warehouse and run the repair again to walk
beneath it. Run the repair again too once nothing your pallets, hauls, bays or staged work keep
names a file or reaches a folder: it drops what it no longer finds. `forklift-server withhold list`
names everything still outstanding. Absent chunks are listed last: no takedown withholds one, so
they are damage to the store, to restore from a copy of the warehouse.
If the record matches its digest but its header names a newer version of Forklift, the repair
leaves it for that version. A record that does not match its digest may hold an entry that reads
whole but was altered, such as a digit of its hash, so the repair says so: check the kept entries
against your own record of takedowns. A lost entry whose content is still stored cannot be found,
so withhold anything you know of again before serving.

**If a remote refused**, only its operator can repair it. Your warehouse is unchanged. Try again once
they have.

### `sealed_damaged` (exit 59)

The record of objects a partial warehouse holds by hash alone (`.forklift/sealed`, those its remote
holds and a read may fetch) does not parse. Nothing was changed. When the next step is `forklift
heal`, run it: it rebuilds the record from the part before the damage and reports what it drops; an
object it held by hash then reads as missing, and a fetch fills it. When the next step is empty, the
record was written by another version of Forklift, or holds a whole frame this version cannot read:
use the version that wrote it, or restore the record from a good copy.

**If a remote refused**, only its operator can repair it. Your warehouse is unchanged. Try again
once they have.

### `unavailable_damaged` (exit 60)

The record of content the warehouse cannot get (`.forklift/unavailable`, left out because a remote's
host withholds it) does not parse. Nothing was changed. When the next step is `forklift heal`, run
it: it rebuilds the record from the entries that still read. When the next step is empty, a newer
version of Forklift wrote the record; use that version.

**If a remote refused**, only its operator can repair it. Your warehouse is unchanged. Try again
once they have.

### `origin_damaged` (exit 61)

The origin record of a partial warehouse (`.forklift/origin`, the remote it was cloned from) does
not parse or holds anything but one URL, so which remote it may talk to cannot be told. Nothing was
changed. Record it again with `forklift remote move <url>`, naming a server that holds this
warehouse's content: the move checks that it does before it records it.

**If a remote refused**, its warehouse is recorded as full but holds an origin record, so it may
lack content its history names. Only its operator can mend that, by serving a full clone instead.
Your warehouse is unchanged.

### `fetch_scope_stray` (exit 62)

A warehouse that is not sparse holds a fetch scope (`.forklift/config/fetch-scope`), which only a
sparse warehouse is given, so it was put there by hand. Every read refuses rather than guess what
the warehouse fetched. Nothing was changed. Delete the file: the warehouse is what its mode says
without it.

**If a remote refused**, only its operator can delete it. Your warehouse is unchanged. Try again
once they have.

### `clone_incomplete` (exit 30)

A clone stopped part-way, and the directory keeps what it fetched. Run the same `forklift clone`
again to resume it, correcting whatever the message names, such as an `--only` path. `heal` does not
apply here, and **do not** delete the directory first unless you want to fetch everything again.

## Damage to the store

### `staged_content_missing` (exit 25)

A staged file's content is gone from the store, so no parcel was made. Stage it again from your
working copy, for each file the message names, from the working tree's root:

```sh
forklift unload <path>
forklift load <path>
```

Quote a path your shell would split, and write one starting with `-` as `./<path>`, so it is not
read as an option.

### `object_unreadable` (exit 57)

An object the command needs cannot be read from the store. Either a pack file holding it is
damaged (an index is lost, cut short or corrupt, a pack's data file is missing, or one index entry is
damaged), its loose file or its record in a pack is corrupt (its bytes do not decompress, or
decompress to something else, or the delta it is stored as stands on such an object), or the object
is missing altogether. Nothing was changed. Run:

```sh
forklift heal
```

`heal` rebuilds a lost or damaged index from the pack's data file, keeping a replaced index beside it
as `<name>.idx.damaged-<time>`, and the command then works. If the data file itself is damaged or
gone, the loose file or pack record is corrupt, or the object is missing altogether, nothing in
this store can bring it back; when history still reaches it, `heal` refuses with `objects_lost`. A
corrupt loose file or pack record is left where it is, and `heal` notes one that no history needs.
A loose file `heal` cannot read at all, or a pack record whose delta base it cannot judge, is not
counted lost; it says its check may be short instead.
`forklift store status` lists each damaged pack file under `pack problems:`.

### `objects_lost` (exit 58)

`heal` ran every repair it could, and objects that history still reaches cannot be read from this
store. The message counts them (absent, unreadable, and corrupt: held only in loose files or pack
records whose bytes are not the object's), lists up to 20 hashes with the reason for each, and names the pallets
whose history reaches them, and every pallet whose ref could not be read, whose history may reach them
too. `--json` carries the same in
`details.objects_lost`. Do what the next
step names:

- **Restore the warehouse from a backup, or clone it afresh** from a remote that holds these objects.
  The store cannot give them back.
- **Re-create the warehouse with this build** when the pack folder holds an index another build of
  Forklift wrote. **Do not** move that index aside, which makes its objects read as missing.
- **Run `forklift heal` again** when the message says another process held the store, so the
  indexes were not rebuilt this run, or, in a cache, when its origin did not answer.

In a cache, `heal` first records again as held by hash each lost absent object its origin's held
history reaches (`details.resealed`), so a read fetches it again; what that history does not reach,
or the origin's host withholds, stays lost. With the origin record also missing or damaged, a note
names the move that records it again, after which `heal` re-seals.

Every other repair in the run still landed: a lost ref rebuilt before the refusal stays rebuilt.

### A journal that cannot be read

`undo` fails naming the journal file. Remove that file to start a fresh journal: you lose the undo
history and nothing else.

## Working with a remote

`lift` merges by itself when the remote moved and the changes combine cleanly. These refusals are the
cases it will not decide for you.

### `remote_ahead` (exit 28)

The remote holds parcels you have not lowered, and you have nothing new. Run `forklift lower`.

### `diverged` (exit 5)

You and the remote both hold parcels the other lacks, and `lift` could not merge them by itself.
Nothing was lifted. Merge the remote's head in, then publish the merge:

```sh
forklift lower    # merges the remote's head into your pallet and stacks the merge
forklift lift
```

If the merge conflicts, `lower` stops with `merge_conflicts` (exit 47). **Do not** look for a force
option: the wire is fast-forward only. A `lower <pallet>` of a pallet this bay is not on refuses as
`diverged` instead, naming `forklift shift <pallet>` first.

### `content_redacted` (exit 49)

The command would have written a pallet head that names content redacted in this warehouse, and a
redaction means no new head may name it again. Nothing was written. The error's `details.path` and
`details.hash` say which file and which content. Remove that file from what the command writes —
delete it from the bay and `load` again before `stack`, for instance — and run the command again.
The same bytes under another name are the same content, so renaming the file does not help.

A read refuses with this code too when it needs content that a redaction applied in this
warehouse removed: `restore`, `pick`, a `diff` showing changes, `blame` and the like. The
`details.hash` names the content; `forklift redact list` shows the redaction. That content is gone
for good, so there is nothing to fetch back: read a version of the file that does not name it.

`load`, `park`, `lower`, a merge (`consolidate`, `haul`, `lift`'s merge) and `pick` refuse with this
code too, before anything reaches the bay, when they would stage, park or write the content. Change
or delete the file and run `load` or `park` again. `lower`, a merge or a pick brings the content from
the other side, so take the file out there first, or make the change by hand without it. A
`consolidate` that would fast-forward to a head stacked before the redaction came down names that
pallet: shift to it, take the file out and stack, then shift back and consolidate again.

`lift` refuses with this code too, before it sends the pallet, when a head you stacked earlier names
the content; the remote may not hold the redaction yet, so its own check cannot be relied on. From
a bay standing on that head with the file in its scope, `forklift remove` the file, `stack`, and
lift again: the new head no longer names it. The parcels before it still do, and they lift with
it as history, so the remote receives the content. If the lift then fails over
`@redactions`, `lower` reconciles it with the remote's, and the next lift sends the redaction.

`clone` and `lower` take a pallet head exactly as the remote serves it, even when it names content
redacted here, as long as the remote holds that redaction too: the remote keeps serving the pallet
until its owner takes the file out, and that owner must still be able to fetch it. A redaction
recorded only here still refuses, and so does anything you stack on that head while it names the
content. In a warehouse a Forklift server has served, taking such a head is refused like any other
write.

A `forklift-server` remote applies a redaction once none of its pallet heads names the content,
and from then on refuses that content with this code; the message names the object. A `lift` of
history that names the content leaves it out, since the remote no longer takes it. When the remote
applied the redaction while the lift was running, the upload is refused: run `forklift lift` again.
A `clone` or `lower` that takes history naming the content never asks the remote for it: it
applies the redaction here before writing the pallet's head, so the history lands without it.

Once this warehouse has applied a redaction, it never takes back history naming that content, not
even a head or metadata history the remote serves. This happens when the redaction was applied
here while the warehouse had no remote, and the remote it has since still serves history naming
the content. When the refusal names a fresh clone, make one with `forklift clone` and work there:
the clone takes the remote's history and redactions as the remote holds them.

### `redaction_blocked` (exit 48)

The content you tried to redact is also part of a meta pallet's history: the office, the
manifest, the tags, the hauls, the custom metadata or the redactions themselves. That history is
never rewritten, so the content can never be removed from this warehouse or any copy of it, and
nothing was recorded. The error's `details.meta_pallets` names where it is. Treat the content as
exposed: rotate or revoke the secret it holds (see [`leaked-secret.md`](leaked-secret.md)).

### `head_names_content` (exit 50)

The content you tried to redact is still in the head of a pallet: a redaction removes content from
history only, so nothing was recorded. The error's `details.heads` names each pallet, its head and
the path the file has there. Take the file out of each head: `forklift remove` it and `stack`, from
a bay standing on that pallet's head with the path in its scope. Then record the redaction at a
head you left whose file is the object itself, as `forklift redact record <head>:<path>`. The
pallet's new head no longer has the file, and that form works even where this warehouse does not
hold the content. A chunked file can be in a head under another recipe with the same chunks; that
head's path names the other recipe, so where no head holds the object itself, record it by its
hash. The error's `next_step` names the commands for one of the heads and the record that fits.

### `remote_history_incomplete` (exit 52)

The remote refused to move the pallet because part of the new head's history is absent there or
cannot be read. A lift refused this way has already asked the remote again what it lacks and
uploaded that, and was refused a second time. Nothing moved, and your warehouse is unchanged. The
remote may have deleted content it owed a deletion again as the lift ran, or failed to read its own
store for a moment: run `forklift lift` again. If it is refused the same way each time, the
remote's store is damaged or lost content, and only whoever runs it can repair it.

### `config_damaged` (exit 65)

A configuration file holds bytes that are not text, or characters a configuration file never
holds, or it was cut short so that its last line no longer reads. Nothing was changed. It may be damage, or a
hand edit saved in an encoding other than UTF-8. Forklift keeps no second copy of the file, so the
only evidence of what it held is the damaged file itself.

In the warehouse's own file, `forklift heal` rewrites it when the damage provably cost no setting,
as when it lies in a comment, and the rest of the file is valid, keeping the file's own text and
comments. Otherwise heal changes
nothing and names, by line and key, each entry
the damage may have touched. `forklift heal --config` then rewrites the file with only the entries
the damage provably spared. When one entry of a section goes, the whole section goes, so a
remote's address is never kept without its token. The damaged bytes are set aside in
`.forklift/config/warehouse.toml.damaged`, which can hold a remote token: set again what was
dropped, then remove it. While an earlier copy is set aside, heal will not write over it, so move
it elsewhere first.

A last line with no newline after it may also be a hand edit: some editors save a file that way.
When the line heal names is one you edited, fix it by hand rather than running `--config`.

In the global file (`.forkliftconfig` in your home folder), fix or replace the file by hand: heal
mends only the warehouse it runs in. A file that holds a mistake made by hand, with no damaged
byte, is refused as "not valid" instead, naming the line to fix, and heal leaves it as it is.

### `bay_position_damaged` (exit 64)

The record of where this bay stands (the pallet it is on, and the parcel its files stand on) is
missing, cut short or garbled, so no command can tell what the next stack builds on. Nothing was
changed. Every time the position is written, a copy is written beside it, and `forklift heal`
puts the position back from that copy, or from an operation that was interrupted. If neither
says where the bay stood, heal changes nothing and lists the pallets: `forklift heal --position
<pallet>` puts the bay on that pallet, at its newest parcel, taking your files as standing there.
`forklift stocktake` then shows how they differ from it. If the bay had stood behind that
parcel, its next stack no longer warns that the pallet moved on, so check the history first.
While the copy or an interrupted operation says where the bay stood, `--position` is refused.

When the refusal names another bay, run `forklift heal` in that bay: a heal here does not reach
it. A position written by a newer version of Forklift is not this refusal, and no heal writes
over it.

### `inventory_damaged` (exit 66)

The inventory's record of which folders hold staged content is cut short, garbled or missing, so
no command can tell what is staged. Nothing was changed. The staged content itself is kept in the
staging area, one entry per folder, and an entry cannot be read back to the folder it belongs
to, so the record cannot simply be listed again.

`forklift heal` rebuilds it from every folder it can find whose staging is on disk: lines of the
damaged record that still read, the folders of your working tree, the folders of the parcel your
files stand on, and those of an operation that was interrupted. When that accounts for all the
staging on disk, heal writes the record, and nothing staged is lost. Otherwise it writes nothing,
finishes its other repairs, and says how much staging it could not match to a folder, or which
tree it could not read. If a refetch or a repair heal names can bring that tree back, do that and
run heal again. `forklift heal --inventory` writes the record anyway: the staging it could not
match is no longer registered, so check `forklift stocktake` and load again what is missing.

When a link or a folder stands where the record belongs, it is refused the same way. The next step
asks you to remove it first; Forklift never follows a link there. Heal then rebuilds the record.

### `checkout_scope_damaged` (exit 67)

The scope file of a checkout in a sparse warehouse, or of a scoped bay, is missing, cut short or
garbled, so no command can tell which paths it checks out. Nothing was changed. Every time a
checkout's scope is written, a copy is written beside it, and `forklift heal`, run in that
checkout, puts the scope back from the copy.

Heal does not use a copy that names a path the warehouse never fetched, or the whole tree in a
sparse warehouse. Nor does it use one that would leave a file outside the checkout where it could
never be stacked again: a committed file, or an untracked file under a fetched path that no ignore
rule covers. It then changes nothing, names the files, and asks for the scope:

```sh
forklift heal --scope <path> --scope <path>
```

Name the paths this checkout held. `--scope` is refused while the scope file can be used, and for
paths that do not match what the checkout holds: a path the warehouse never fetched, a folder the
store does not hold, a committed file in the named paths that is missing from your working tree,
or a file the scope would leave out. When the fetch scope is lost as well, the same heal then
rebuilds it from the scope you named.

When the refusal names another checkout, run `forklift heal` in that checkout: a heal here does
not reach it. A bay that recorded no folder was never checked out, since its `forklift bay add`
stopped first: remove it with `forklift bay remove`. A scope file written by a newer version of
Forklift is not this refusal, and no heal writes over it.

### `merge_state_damaged` (exit 68)

The record a bay keeps of the consolidation or pick in progress there is not a file, is not text,
is cut short or garbled, or does not match its digest, or a consolidation's skeleton is missing,
so no command can tell what the merge was. Nothing was
changed. Run the next step it names in that bay:

- For the consolidation or pick state, `forklift abort --keep-files` abandons the merge without
  reading the record. It removes the record and leaves every file as it is, the merge's own
  writes and conflict markers included, in the working directory and the inventory. Resolve
  what is still in conflict (`forklift conflicts` lists it), load it, and stack: the parcel is an
  ordinary one, with no second parent. To have the merge again, consolidate or pick again after
  putting the files back with `forklift restore`.
- For a consolidation's skeleton, `forklift abort` abandons the consolidation and puts back what
  it changed, as it always does; it does not need the skeleton.

When the refusal names another bay, run that command in the bay it names: an abort here does not
reach it. A record written by a newer version of Forklift is not this refusal; use the Forklift
that wrote it. Nor is a record the filesystem fails to read (a permission or device error): it may
be whole, so the refusal stays a plain read error. Fix the cause and run the command again, or, to
give the merge up anyway, `forklift abort --keep-files` still removes it.

### `object_kind_mismatch` (exit 63)

The remote refused to move the pallet because the new head's history names an object as a kind
it is not: a file entry naming a folder's tree or a piece of a large file, a large file's entry
naming a plain file's content, or a large file listing such content among its pieces. The error
names the object, the path, and both kinds. Nothing moved. A warehouse written by forklift never
makes such history, so the parcels were written by other tooling or damaged; lifting again sends
the same history and is refused the same way, and `forklift audit` checks only that objects are
present, so it does not find this. Only history that names the path otherwise can be lifted.

### `content_withheld` (exit 51)

The remote's host withholds content the command needed: a takedown, such as a legal notice, made by
whoever runs that server, not by the warehouse's admins. The message carries the host's statement.
The remote no longer serves that content, nor takes it, and nothing you do here changes that; a copy
you already hold stays yours. The history that names the file is unchanged. Take the content from
another remote that holds it, or ask the host about the statement.

`clone`, `lower` and `scope fetch` do not stop here: they take the history without the content and
check the head out without the file, which `forklift stocktake` then lists as not checked out.
`git export` and `git mirror` leave such a file out of the commits they write and name it.

A command that needs the bytes of such a file refuses with this code too, with the hash and the
host's statement in `details`: `show` and `blame` of it, a `diff` that prints its lines, a
`restore` of the file, a merge that must read it, and a `lift` of history naming it to a remote
that does not withhold it. This warehouse never received the content, so it cannot read it or send
it anywhere. If you get the same bytes some other way, put them at the path and `forklift load`
it: the content is known by its hash, so the file is then checked out again.

### `merge_conflicts` (exit 47)

`lower` merged the remote's diverged head into your pallet, and both sides changed the same paths.
The error's `details.paths` lists them, and the consolidation stays in progress, as a conflicting
`consolidate` leaves it. Resolve each path, `forklift load` it, then `forklift stack` to complete
the merge and `forklift lift` to publish it. `forklift abort` abandons the merge and puts your
pallet back as it was.

### `head_not_materializable` (exit 46)

The remote's head of the pallet holds an entry named like the warehouse's metadata folder, so no
client can check it out. A remote refuses such a lift now, so the head is older than that check.
Nothing was lowered or cloned. Repair it, then publish the repair:

```sh
forklift lower --repair      # or: forklift lower <pallet> --repair, the command the message names
forklift lift
```

The repair is a parcel stacked on the remote's head that removes every such entry. If your pallet
holds parcels the remote's head lacks, they are kept: the lower merges the repair into your pallet,
as it merges a diverged head. A sparse
warehouse cannot build the repair, so it names `forklift clone <url> <directory> --repair` into a new
directory. A clone refused this way reports `clone_incomplete` with this code as its cause: resume
it with `--repair` (the command its next step names), or with another `--pallet`, or with an `--only`
that leaves the named path out.

A `shift`, `consolidate` or `lift` refused with this code names `forklift lower <pallet> --repair`
(in a sparse warehouse, the clone): a named `lower` had moved that pallet onto the head, or the lift
would have merged it. A pallet the remote does not hold, such as a hand-made ref, is deleted
instead with `forklift pallet delete <pallet>`.

### `behind` (exit 26)

Another bay or process moved the pallet past the parcel this bay stands on. Run the `forklift shift`
the message names to catch up. With work in progress, `forklift park`, shift, then `forklift park pop`.

### `meta_conflict` (exit 24)

A metadata pallet such as `@kv` changed differently here and on the remote at the same path. The
message names each path and both values. Choose a side for each one:

```sh
forklift lower --keep @kv:<path>=ours
forklift lower --keep @kv:<path>=theirs --confirm   # discards your side, so it asks you to confirm
```

Spell the pallet with its `@`. Your files and your other pallets are not affected.

### `staged_upload_discarded` (exit 32)

The remote no longer holds an upload the lift was committing. Run `forklift lift` again; it uploads
it afresh.

### `pallet_not_found` (exit 29)

No pallet has that name. `forklift pallet` lists yours and `forklift pallet list --remote` the
remote's. `forklift lower <name>` brings in the remote's, and `forklift pallet create <name>` makes a
new one.

### `pallet_deleted_on_remote` (exit 35)

The pallet was deleted on the remote, and its name stays reserved for that history. Run
`forklift pallet restore <name> --remote` to bring it back, or `forklift lift --recreate` to re-create
it from work that descends from the deleted head.

### `target_not_covered` (exit 27)

`deliver` was aimed at a target holding changes your draft has not seen. Run the
`forklift consolidate <target>` the message names, then deliver again.

### `mirror_incomplete` (exit 33)

A `git mirror` run left a pallet out of step with git; the others went through. Follow the step each
pallet's message names, then run `forklift git mirror` again.

### `remote_protocol_violation` (exit 23)

The remote answered with something the protocol never produces, and retrying gets the same answer.
Nothing on your side will fix this: tell whoever runs that remote.

### `chunked_transport_unsupported` (exit 14), `oversized_transport_unsupported` (exit 15), `commit_pagination_unsupported` (exit 16)

A large file or a large lift needs something this remote or bundle cannot carry. The message names
the file or count and what would carry it. This is usually an older server: upgrading it is the
general way out.

## Trust and lost keys

### `trust_changed` (exit 22)

The remote's trust anchor is not the one this warehouse holds. This changes who controls the
warehouse, so it stops everything, like an SSH host key change.

Ask someone you know, by another channel, whether the reset was real and what the new genesis is.
Only then accept exactly that value:

```sh
forklift office accept-regenesis                      # describes the change, accepts nothing
forklift office accept-regenesis --confirm <genesis>
```

**Do not** accept a genesis just because the message printed it. If nobody can vouch for it, stop
using that remote.

### `trust_damaged` (exit 69)

First read the message. If it says the remote refused the handshake, and the next step names the
remote's operator, the damage is on the remote and nothing here needs repair: change nothing in
this warehouse, and ask the operator to restore the trust file where the warehouse is served from.

Otherwise the damage is here. This warehouse's trust file (`.forklift/trust`) is gone while trust was
established (the office pallet is born, or the file's copy `.forklift/trust-copy` stands), or it does
not parse, or something other than a file stands in its place. Nothing was changed: the warehouse
can neither sign nor write unsigned until its anchor is back.

When heal can mend it, the refusal's next step is `forklift heal`: the copy still verifies the office
pallet, while the trust file is missing, does not parse, or parses but verifies nothing. When the next
step is empty, heal cannot, and the message names the ways out below. A warehouse that is served, or
whose first serve began, gets ways out without `forklift lower`, since its trust moves only through
its server.

- Run `forklift heal` first. Where the copy `.forklift/trust-copy` still holds the anchor, heal
  writes the trust file back from it (and a missing or stale copy from the trust file), and reports
  it as restored. Where the two differ and both would serve, it refuses and changes nothing: move
  aside the one you do not trust, then run heal again. Where neither holds the anchor, it says so
  and changes nothing; go on below.
- Restore `.forklift/trust` from a backup, if you have one.
- Otherwise move aside whichever of `trust` and `trust-copy` stands, then take the configured
  remote's anchor:

  ```sh
  forklift lower
  ```

  Nothing here is compared against it, so do this only with a remote you trust. If that remote has
  reset its trust, clone it again instead.
- With neither, run `forklift prepare` in another folder, which starts without this warehouse's
  history, and enroll there.

A server's first serve of a warehouse refuses as `trust_damaged` when its trust file stands but does
not verify its office pallet, or the copy beside it differs from it. The refusal says what to do: in
the warehouse, run `forklift heal` when one of the two files verifies the office; when both do but
differ, move aside the one you do not trust first. When neither does, take the ways out above. It
refuses as `trust_damaged` too when the trust file is missing beside a born office pallet or a
standing copy, and as `ref_damaged` when the trust file stands beside an unborn office pallet with no
copy that matches it, or not reading as an anchor: an enrollment or a first `lower` stopped part way. To run it again, move the
trust file aside, and its copy if one stands. Where a remote holds the warehouse's office,
`forklift lower` takes it instead, and verifies it against the trust file.

### `signature_damaged` (exit 70)

A parcel's stored signature cannot be read or does not parse, so what signed the parcel cannot be
told. The message names the parcel. Nothing was changed. A signature is never stored twice with
different bytes, so fetching the parcel again does not replace it: restore the warehouse's objects
(`.forklift/objects`) from a backup, or clone the warehouse again from a remote that holds the parcel.
A signature naming a format version newer than this Forklift reads is refused the same way, since
damage can make it read as any version; if a newer Forklift wrote it, upgrading reads it. Told it by
a remote, the damage is in the server's store: ask its operator.

### `signature_missing` (exit 71)

A parcel stacked after trust was established carries no signature. A lost signature and a stripped
one look alike, so the warehouse may have been tampered with. As for `signature_damaged`, restore the
objects from a backup or clone again from a remote you trust. Signing the parcel now would not tell
you who stacked it.

### Lost keys: re-genesis

"No active key … is present on this machine" means this machine cannot sign. You may not need a reset:

- Restore a backup of your key folder, if you have one.
- If another of your own devices still holds a key, [link this machine](trust-and-identity.md#5-multiple-devices) from it.
- If another admin holds one, have them [re-key you](trust-and-identity.md#3-adding-people).

If every key is gone and no admin is left, reset trust:

```sh
forklift office regenesis             # shows what it would reset
forklift office regenesis --confirm
forklift lift                         # needs the server operator's static token
```

Everyone with a clone then gets `trust_changed` and must accept the new genesis, as above. Old history
stays readable, but its guarantee drops from verified to attested. It is refused while you are an
admin with a usable key. See [re-genesis](trust-and-identity.md#9-recovery-re-genesis).

### `history_unverified` (exit 34)

History about to be adopted does not verify against the trust anchor it was judged against, and
nothing was taken. Run `forklift audit` where that history came from, and take it only once that
passes.

## Partial clones and scoped bays

### `cache_miss` (exit 36)

A cache warehouse does not hold what you asked for and could not reach its origin. Get the origin
reachable and run the command again.

### `non_origin_remote` (exit 11)

A partial clone was pointed at a remote other than the one it was cloned from. Only its origin is
known to hold what it lacks. Set `remote.url` back to the origin the message names, or use
[`remote move`](cli.md#remote-move-and-remote-abort--move-a-partial-warehouse-to-another-origin) to move the origin properly. `config` cannot change the origin.

If the message says the origin record is missing or damaged, the `.forklift` folder is damaged:
run the move the next step names, which records the configured remote as the origin once it checks
that server holds this warehouse's content, or clone afresh. `heal` refuses with this code too when
nothing else is wrong.

### `origin_move_pending` (exit 37)

A move of the origin has not finished. Run `forklift remote move <url>` to finish it. While it is
still checking, `forklift remote abort` abandons it; the message says which applies.

### `origin_move_incomplete` (exit 38)

The new server lacks content this warehouse relies on its origin for, so the origin stayed where it
was. Copy that content to the new server as the message describes, then run the move again. When
the message says the new server withholds content the current origin does not, keep the current
origin: the move goes ahead once that origin withholds it too or no longer answers.

### `only_prefix_not_found` (exit 31)

The `--only` path is not a directory in that pallet on the remote. A clone reports this as
`clone_incomplete`, with this code as its `cause_code`. Running it unchanged fails the same way: check
the spelling, and run the clone again with the right path. The path is checked before the clone
stores any of it.

### `out_of_scope` (exit 7)

The path is outside this scoped bay. `forklift scope expand` brings it in.

### `scope_path_type_changed` (exit 8), `out_of_scope_conflict` (exit 10), `sparse_workspace` (exit 9)

A scoped bay cannot do this with the part of the tree it holds. The message names the path and the
way forward; running the command from a full bay always works.

### `narrow_unclean` (exit 12)

`scope narrow` would delete uncommitted work. Stack it or discard it, then narrow again.

### `scope_prune_blocked` (exit 13)

A checkout still uses that path. Narrow that checkout first, as the message says, then prune again.

## Everything else

### `conflict` (exit 4)

You have changes to tracked files, staged or not, that the command would overwrite: a
`consolidate`, `pick`, `haul consolidate`, `shift`, an `undo` of a shift, a `pallet create` at a
revision, or a `lower` merging a diverged head. Nothing was changed. Run `forklift stocktake` to see them. Finish with `load` and `stack`, set the work aside with
`forklift park`, or discard it with `forklift restore`.

### `confirmation_required` (exit 39)

A destructive command showed what it would destroy and changed nothing. Read that, then run it again
with `--confirm` if you mean it.

### `not_a_warehouse` (exit 3)

No `.forklift` here or in any parent. Change into the warehouse, or run `forklift prepare` to make one.

### `warehouse_format_older` (exit 40)

The warehouse is in an older [format](../format/WAREHOUSE_FORMAT.md) than this build reads. Nothing was
read or changed. Run the upgrade the refusal names: `forklift upgrade` in the warehouse, or
`forklift-server upgrade --root <root>` with no server serving it. When the format is older than any
this build upgrades, the refusal says to make the warehouse again instead. Format 1 is the first
format, so no warehouse is older yet.

### `warehouse_format_newer` (exit 41)

A newer Forklift made or upgraded this warehouse, and upgrades are one-way. Update this binary.

### `warehouse_upgrade_incomplete` (exit 42)

An upgrade stopped before it finished, so the warehouse is between two formats and every command
refuses it. Run the upgrade the refusal names, with a build that reads the target format; it resumes
at the first step not done. If the upgrade itself refuses this way, its steps are all done but they
left a mode this build does not read: update the binary to one that reads it, and run its upgrade. No
upgrade can have started yet, since format 1 is the first format.

### `warehouse_upgrade_running` (exit 43)

Another process holds `.forklift/format.lock` exclusively, which is what an upgrade does. Run the
command again once it finishes. A process that dies releases the lock.

### `warehouse_in_use` (exit 45)

The upgrade needs the warehouse to itself, and another process has it open: a command in the
warehouse or one of its bays, a server serving it, a bundle or a gc. Nothing was changed. Let those
finish and stop the server, then run the upgrade again.

### `warehouse_lease_missing` (exit 44)

`.forklift/format.lock` is gone and the folder cannot be written, so Forklift cannot make it again.
Restore write access to `.forklift`, or create that empty file there.

### `empty_history` (exit 19)

The pallet has nothing stacked yet, so there is no history to show. `load` and `stack` something.

### `query_predicate_invalid` (exit 18)

The `forklift query` predicate is malformed or too large. The message says which part; fix it and run
it again.

### `usage` (exit 2) and `error` (exit 1)

`usage` means the command line could not be parsed: `forklift help <command>` shows the form. `error`
has no specific code yet, so the message is all there is. If it names a next step (after `Next step:`), take it; if nothing
here helps, please [report it](https://github.com/lonic-software/forklift/issues).
