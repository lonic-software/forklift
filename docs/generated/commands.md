<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run
`bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit
the result after a change that affects it (an error code, a command, alias or help line,
or a `--json` output struct).
-->

# Commands and aliases

Every `forklift` command and subcommand, generated from the same command definition the binary parses its arguments with; CI fails when this page falls behind it. Commands are in alphabetical order, each followed by its subcommands. An **alias** is a short name shown in `forklift help`. A **hidden alias** is accepted but not shown; most are git's name for the same command, so `forklift commit` runs `forklift stack`. `forklift help <command>` explains any row in full, and `docs/guide/cli.md` walks through them by task.

| Command | Alias | Hidden alias | Does |
|---|---|---|---|
| `audit` |  |  | Verify the warehouse's signed history offline |
| `bay` |  |  | Manage bays: parallel working directories bound to this warehouse |
| `bay add` |  |  | Open a new bay (a new working directory on a new pallet) |
| `bay remove` |  |  | De-register a bay (its pallet ref and materialized files are kept) |
| `blame` |  | `annotate` | Show, for each line of a file, the parcel and author that last changed it |
| `clone` |  |  | Clone a remote warehouse into a new directory |
| `config` |  |  | Read or change configuration values |
| `config list` |  |  | List every known key and its value (the default) |
| `config get` |  |  | Print the value of a key |
| `config set` |  |  | Set a key to a value |
| `config unset` |  |  | Remove a key (e.g. clearing a "remote.token") |
| `conflicts` |  |  | List the files left in conflict by an unresolved consolidation |
| `consolidate` | `con` | `merge` | Consolidate (merge) another pallet into the current one |
| `deliver` |  |  | Squash the current draft pallet onto a target as one clean parcel, keeping the trail |
| `diff` | `d` |  | Show line-by-line changes |
| `git` |  |  | Move history between this warehouse and git: import, export and mirror |
| `git import` |  |  | Import a git repository's history into this warehouse (one-way migration) |
| `git export` |  |  | Export this warehouse's history into a new git repository (one-way, lossy) |
| `git mirror` |  |  | Mirror the remote's pallets to a git remote (mirror.url), never by force |
| `haul` |  |  | Reviewable merge proposals (pull requests) on the @haul meta pallet |
| `haul open` |  |  | Open a merge proposal |
| `haul list` |  |  | List proposals |
| `haul show` |  |  | Show one proposal in full |
| `haul update` |  |  | Propose the source pallet's current head |
| `haul comment` |  |  | Add a comment |
| `haul review` |  |  | Record a signed review: approve, request changes or comment |
| `haul consolidate` |  | `merge` | Land a proposal: consolidate its source into the target |
| `haul close` |  |  | Close a proposal without merging |
| `haul reopen` |  |  | Reopen a closed proposal |
| `heal` |  |  | Resolve a standing durability taint, or finish or undo an interrupted operation |
| `help` |  |  | Print the command list, or detailed help about one command |
| `history` | `hi` | `log` | Print the parcels of a revision, newest first |
| `lift` | `li` | `push` | Upload the current pallet's new parcels to the remote (push) |
| `load` | `l` | `add` | Take the current state of a file or directory into the inventory (stage) |
| `lower` | `lo` | `pull` | Download the remote's new parcels and fast-forward the current pallet, or another (pull) |
| `manifest` |  |  | Facts recorded about a parcel after the fact: notes, approvals, provenance, keyed values |
| `manifest note` |  |  | Attach a free-form note to a parcel |
| `manifest approve` |  |  | Record a signed approval (sign-off) of a parcel |
| `manifest provenance` |  |  | Record how a parcel was produced: model, tool, session (AI provenance) |
| `manifest show` |  |  | Show the manifest entries attached to a parcel |
| `manifest set` |  |  | Set a keyed value on a parcel |
| `manifest get` |  |  | Show a keyed value on a parcel |
| `manifest unset` |  |  | Remove a keyed value from a parcel |
| `manifest keys` |  |  | List every keyed value, or a parcel's |
| `manifest log` |  |  | Show every change to a key, newest first, with who made it |
| `mcp` |  |  | Serve the command surface to an AI agent as MCP tools (stdio) |
| `office` |  |  | Manage the warehouse office: users and signing keys |
| `office enroll` |  |  | Enroll the configured operator and establish trust — one-way! |
| `office admit` |  |  | Enroll another operator with their public key — admins only |
| `office link` |  |  | Link another device's key to your own identity |
| `office authorize` |  |  | Authorize a new key for an already-enrolled operator — admins only |
| `office role` |  |  | Change a user's role (and a writer's pallet grants) — admins only |
| `office rotate` |  |  | Issue a fresh key for the configured operator and retire the old ones |
| `office retire` |  |  | Revoke a key: retire it, or mark it compromised |
| `office regenesis` |  |  | RESET trust: new root, prior history pinned as attested — recovery only |
| `office accept-regenesis` |  |  | Consciously accept a remote's trust reset (re-genesis) |
| `office list` |  |  | List the users and their keys (the default) |
| `pallet` |  | `branch` | Manage pallets (branches): list, create, delete and restore them |
| `pallet list` |  |  | List the pallets, marking the current one (the default) |
| `pallet create` |  |  | Create a new pallet and shift to it |
| `pallet delete` |  |  | Delete a pallet: remove its ref, keeping its parcels |
| `pallet restore` |  |  | Bring back a deleted pallet at the head its tombstone holds |
| `park` | `pa` | `stash` | Park the work in progress and reset to the pallet head |
| `park pop` |  |  | Re-apply the most recently parked changes (staged) and drop them from the list |
| `park list` |  |  | List the parked parcels, newest first |
| `pick` |  | `cherry-pick` | Apply a parcel's changes onto the current pallet as a new, author-preserving parcel |
| `prepare` |  | `init` | Prepare a new warehouse in the current directory |
| `profile` |  |  | Manage identity profiles (named operator ids) and select one per warehouse |
| `profile list` |  |  | List the profiles and the local keys each one holds (the default) |
| `profile create` |  |  | Create a named profile (mints an operator id unless one is given) |
| `profile use` |  |  | Act as this profile in the current warehouse |
| `profile keygen` |  |  | Generate a keypair locally and print its public half + proof-of-possession |
| `query` |  |  | Query parcel history by signed identity, signer and parcel facts |
| `remote` |  |  | Show the remote and the origin, or move a partial warehouse's origin to another server |
| `remote move` |  |  | Move a partial warehouse's origin to another server that holds its content |
| `remote abort` |  |  | Abandon a move that stopped before it started rewriting the origin |
| `remove` | `rm` |  | Stage a file or directory for removal |
| `restore` | `r` |  | Restore a file or directory from the inventory (discard unstaged changes) |
| `scope` |  |  | Show and change the sparse scopes: this checkout's scope and the warehouse fetch scope |
| `scope status` |  |  | Show this checkout's scope and the warehouse fetch scope (the default) |
| `scope fetch` |  |  | Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s) |
| `scope narrow` |  |  | Narrow this checkout's scope and remove the dropped subtree(s) from the working directory |
| `scope prune` |  |  | Reclaim disk from a sparse warehouse: shrink its fetch scope and free the content |
| `self` |  |  | Manage this binary itself: its update, its short alias and its self-description |
| `self alias` |  |  | Manage the `fl` short alias for this binary |
| `self alias install` |  |  | Create the alias next to this binary (default name: "fl") |
| `self alias uninstall` |  |  | Remove the alias next to this binary (default name: "fl") |
| `self alias status` |  |  | Report whether the alias is installed and where it points (the default) |
| `self schema` |  |  | Describe this binary's command surface as a Pult manifest |
| `self update` |  |  | Check for a newer release and update this binary (client only) |
| `shift` | `sh` | `checkout`, `switch` | Shift to another pallet |
| `show` |  |  | Print a file's content at a revision, as git show does for REV:PATH |
| `stack` | `s` | `commit` | Stack the inventory as a new parcel (commit) on the current pallet |
| `stocktake` | `st` | `status` | Report the staged and unstaged changes of the warehouse |
| `store` |  |  | Inspect and maintain the object store: its health, packing, raw records and cached content |
| `store status` |  |  | Report object-store health: loose vs packed objects, packs, and whether compaction is due (the default) |
| `store compact` |  |  | Compact the object store: pack loose objects into a few dense pack files |
| `store peek` |  |  | Peek into an object (print its content) |
| `store evict` |  |  | Free cached content the origin holds, to be fetched again when read |
| `tag` |  |  | Manage signed tags: named, admin-signed pointers at a parcel (releases) |
| `tag create` |  |  | Create a signed tag pointing at a revision (admin only) |
| `tag show` |  |  | Show one tag in full (subject, tagger, message) |
| `tag list` |  |  | List every tag (the default) |
| `undo` |  |  | Undo the last stack, consolidate, pick, shift or pallet delete |
| `unload` | `ul` |  | Unstage a file or directory (keep your changes) |
| `version` |  |  | Print the version of Forklift |

