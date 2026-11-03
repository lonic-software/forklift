# Recovery and troubleshooting

Something refused, or something looks broken. Find the code below and do what its entry says.

Every failure prints a message and exits with a fixed status, and almost every message names its next
step. With `--json`, the same code is in `error.code` and the step in `error.next_step`. The
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

### `warehouse_locked` (exit 6)

Another forklift process holds the warehouse lock, or one was killed and left the lock file behind.
The message names the file, and the process id if the lock file holds one.

First check that process is gone. With no id to check, look for any forklift still running in this
warehouse. If one is, wait for it. If none is, remove the file the message names and run your command
again.

**Do not** remove the lock while that process is alive: it guards the staging area while it is being
rewritten.

### A stale `serve.lock`

`forklift-server gc` or `forklift-server serve` refuses with "The warehouse root is locked by another
forklift process (held by process N)" after a server was hard-killed. A graceful stop removes this
file; `kill -9` or a power cut does not.

Check process N is gone, then remove the `serve.lock` the message names and run the command again.
**Do not** remove it while the server is running: `gc` would then sweep objects its in-flight lifts
still need.

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
[`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation) for the rarer cases.

### `clone_incomplete` (exit 30)

A clone stopped part-way, and the directory keeps what it fetched. Run the same `forklift clone`
again to resume it, correcting whatever the message names, such as an `--only` path. `heal` does not
apply here, and **do not** delete the directory first unless you want to fetch everything again.

## Damage to the store

### `staged_content_missing` (exit 25)

A staged file's content is gone from the store, so no parcel was made. Stage it again from your
working copy, for each file the message names:

```sh
forklift unload -- <path>
forklift load -- <path>
```

### Pack problems

Commands failing with "whose data file could not be loaded", "is corrupt", "which could not be parsed"
or "which this build cannot read" point here. Run
`forklift store status`, whose `pack problems:` section names each file:

| Listed as | What to do |
|---|---|
| `quarantined` | The pack's data file is missing. Restore it from a backup, or clone afresh from a remote. `heal` and `lower` do not bring it back: `heal` only refetches objects under a durability taint, and `lower` sees nothing new to fetch. |
| `damaged` | Run `forklift store compact --all`, which salvages the pack. If it keeps the pack or refuses, the objects it reports cannot be recovered from this store. |
| `unenumerable` | Move the `.idx` file it names out of the pack folder, then run the command again. If a command then reports objects missing, put it back and clone afresh. |
| another format | The warehouse was written by a build with another pack format. Re-create it with this build. **Do not** move the file aside, which makes its objects read as missing. |

Keep a copy of anything before you move it.

### A journal that cannot be read

`undo` fails naming the journal file. Remove that file to start a fresh journal: you lose the undo
history and nothing else.

## Working with a remote

`lift` merges by itself when the remote moved and the changes combine cleanly. These refusals are the
cases it will not decide for you.

### `remote_ahead` (exit 28)

The remote holds parcels you have not lowered, and you have nothing new. Run `forklift lower`.

### `diverged` (exit 5)

You and the remote both hold parcels the other lacks, and they do not merge by themselves. Nothing
was lifted or lowered. The message spells out the exact commands, with the hash filled in:

```sh
forklift pallet create main-remote-<hash> <hash>   # the remote's head, as its own pallet; moves you onto it
forklift shift main
forklift consolidate main-remote-<hash>            # resolve any conflicts, load them, then stack
forklift lift
```

`consolidate` exits 0 even when it stops at conflicts, so read what it prints. **Do not** look for a
force option: the wire is fast-forward only.

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

If the message says the origin record is missing, the `.forklift` folder is damaged: name a server
holding this warehouse's content with `forklift remote move <url>`, or clone afresh.

### `origin_move_pending` (exit 37)

A move of the origin has not finished. Run `forklift remote move <url>` to finish it. While it is
still checking, `forklift remote abort` abandons it; the message says which applies.

### `origin_move_incomplete` (exit 38)

The new server lacks content this warehouse relies on its origin for, so the origin stayed where it
was. Copy that content to the new server as the message describes, then run the move again.

### `only_prefix_not_found` (exit 31)

The `--only` path is not a directory in that pallet on the remote. A clone reports this as
`clone_incomplete`, with this code as its `cause_code`. Running it unchanged fails the same way: check
the spelling, and run the clone again with the right path. What it fetched is kept.

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

Your working state blocks this: unresolved conflicts, or changes the command would overwrite. Run
`forklift stocktake` to see which. Finish with `load` and `stack`, set the work aside with
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
at the first step not done. No upgrade can have started yet, since format 1 is the first format.

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
has no specific code yet, so the message is all there is. If it names a next step, take it; if nothing
here helps, please [report it](https://github.com/lonic-software/forklift/issues).
