<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run
`bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit
the result after a change that affects it (an error code, a command, alias or help line,
or a `--json` output struct).
-->

# Commands and aliases

Every `forklift` command and subcommand, generated from the same command definition the binary parses its arguments with; CI fails when this page falls behind it. Commands are in alphabetical order, each followed by its subcommands. An **alias** is a short name shown in `forklift help`. A **hidden alias** is accepted but not shown; most are git's name for the same command, so `forklift commit` runs `stack`. `forklift help <command>` explains any row in full, and `docs/guide/cli.md` walks through them by task.

| Command | Alias | Hidden alias | Does |
|---|---|---|---|
| `alias` |  |  | Manage the `fl` short alias for this binary |
| `alias install` |  |  | Create the alias next to this binary (default name: "fl") |
| `alias uninstall` |  |  | Remove the alias next to this binary (default name: "fl") |
| `alias status` |  |  | Report whether the alias is installed and where it points (the default) |
| `audit` | `a` |  | Verify the warehouse's signed history offline |
| `bay` |  |  | Manage bays: parallel working directories bound to this warehouse |
| `bay add` |  |  | Open a new bay (a new working directory on a new pallet) |
| `bay remove` |  |  | De-register a bay (its pallet ref and materialized files are kept) |
| `blame` | `bl` | `annotate` | Show, for each line of a file, the parcel and author that last changed it |
| `cherry-pick` | `cp` |  | Apply a parcel's changes onto the current pallet as a new, author-preserving parcel |
| `compact` |  |  | Compact the object store: pack loose objects into a few dense pack files |
| `config` | `cfg` |  | Read or change configuration values |
| `conflicts` |  |  | List the files left in conflict by an unresolved consolidation |
| `consolidate` | `con` | `merge` | Consolidate (merge) another pallet into the current one |
| `deliver` | `dv` |  | Squash the current draft pallet onto a target as one clean parcel, keeping the trail |
| `diff` | `d` |  | Show line-by-line changes |
| `expand` |  |  | Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s) |
| `export-git` |  |  | Export this warehouse's history into a new git repository (one-way, lossy) |
| `franchise` | `fr` | `clone` | Open a local franchise of a remote warehouse (clone) |
| `haul` |  |  | Reviewable merge proposals (pull requests) on the @haul meta pallet |
| `haul open` |  |  | Open a merge proposal |
| `haul list` |  |  | List proposals |
| `haul show` |  |  | Show one proposal in full |
| `haul update` |  |  | Propose the source pallet's current head |
| `haul comment` |  |  | Add a comment |
| `haul review` |  |  | Record a signed review (approve by default) |
| `haul merge` |  |  | Merge a proposal (consolidate its source into the target) |
| `haul close` |  |  | Close a proposal without merging |
| `haul reopen` |  |  | Reopen a closed proposal |
| `heal` |  |  | Resolve a standing durability taint, or finish or undo an interrupted operation |
| `help` | `h` |  | Print the command list, or detailed help about one command |
| `history` | `hi` | `log` | Print the parcels of a revision, newest first |
| `import-git` |  |  | Import a git repository's history into this warehouse (one-way migration) |
| `kv` |  |  | Custom metadata: signed text values keyed by parcel and key |
| `kv set` |  |  | Set a key on a parcel |
| `kv get` |  |  | Show a key's value on a parcel |
| `kv unset` |  |  | Remove a key from a parcel |
| `kv list` |  |  | List every value, or a parcel's |
| `kv log` |  |  | Show every change to a key, newest first, with who made it |
| `lift` | `li` | `push` | Upload the current pallet's new parcels to the remote (push) |
| `load` | `l` | `add` | Take the current state of a file or directory into the inventory (stage) |
| `lower` | `lo` | `pull` | Download the remote's new parcels and fast-forward the current pallet, or another (pull) |
| `manifest` | `mf` |  | Attach signed post-metadata to a parcel: approvals, review notes |
| `manifest note` |  |  | Attach a free-form note to a parcel |
| `manifest approve` |  |  | Record a signed approval (sign-off) of a parcel |
| `manifest provenance` |  |  | Record how a parcel was produced: model, tool, session (AI provenance) |
| `manifest show` |  |  | Show the manifest entries attached to a parcel |
| `mcp` |  |  | Serve the command surface to an AI agent as MCP tools (stdio) |
| `mirror` |  |  | Mirror the remote's pallets to a git remote (mirror.url), never by force |
| `narrow` |  |  | Narrow this checkout's materialization scope and de-materialize the dropped subtree(s) |
| `office` | `o` |  | Manage the warehouse office: users and signing keys |
| `office enroll` |  |  | Enroll the configured operator and establish trust — one-way! |
| `office keygen` |  |  | Generate a keypair locally and print its public half + proof-of-possession |
| `office admit` |  |  | Enroll another operator with their public key — admins only |
| `office link` |  |  | Link another device's key to your own identity |
| `office authorize` |  |  | Authorize a new key for an already-enrolled operator — admins only |
| `office role` |  |  | Change a user's role (and a writer's pallet grants) — admins only |
| `office rotate` |  |  | Issue a fresh key for the configured operator and retire the old ones |
| `office retire` |  |  | Revoke a key: retire it, or mark it compromised |
| `office regenesis` |  |  | RESET trust: new root, prior history pinned as attested — recovery only |
| `office accept-regenesis` |  |  | Consciously accept a remote's trust reset (re-genesis) |
| `office list` |  |  | List the users and their keys (the default) |
| `palletize` | `pz` | `branch` | Create a new pallet (branch) and shift to it; without a name, list the pallets |
| `park` | `pa` | `stash` | Park the work in progress and reset to the pallet head |
| `park pop` |  |  | Re-apply the most recently parked changes (staged) and drop them from the list |
| `park list` |  |  | List the parked parcels, newest first |
| `peek` | `pk` |  | Peek into an object (print its content) |
| `prepare` | `p` | `init` | Prepare a new warehouse in the current directory |
| `profile` |  |  | Manage identity profiles (named operator ids) and select one per warehouse |
| `profile list` |  |  | List the profiles and the local keys each one holds (the default) |
| `profile create` |  |  | Create a named profile (mints an operator id unless one is given) |
| `profile use` |  |  | Act as this profile in the current warehouse |
| `query` | `q` |  | Query parcel history by signed identity, signer and parcel facts |
| `remove` | `rm` |  | Stage a file or directory for removal |
| `restore` | `r` |  | Restore a file or directory from the inventory (discard unstaged changes) |
| `scope` |  |  | Show this bay's sparse materialization scope, and the warehouse fetch scope |
| `scope status` |  |  | Show the current bay's materialization scope and the warehouse fetch scope (the default) |
| `scope-prune` |  |  | Reclaim disk from a sparse warehouse: forget a fetched path and free its content |
| `self-schema` |  |  | Describe this binary's command surface as a Pult manifest |
| `self-update` |  |  | Check for a newer release and update this binary (client only) |
| `shift` | `sh` | `checkout`, `switch` | Shift to another pallet |
| `show` |  |  | Print a file's content at a revision (`git show <rev>:<path>`'s equivalent) |
| `stack` | `s` | `commit` | Stack the inventory as a new parcel (commit) on the current pallet |
| `stocktake` | `st` | `status` | Report the staged and unstaged changes of the warehouse |
| `store` |  |  | Report object-store health: loose vs packed objects, packs, and whether compaction is due |
| `tag` |  |  | Manage signed tags: named, admin-signed pointers at a parcel (releases) |
| `tag create` |  |  | Create a signed tag pointing at a revision (admin only) |
| `tag show` |  |  | Show one tag in full (subject, tagger, message) |
| `tag list` |  |  | List every tag (the default) |
| `undo` |  |  | Undo the last stack, consolidate, cherry-pick or shift (soft — keeps your changes) |
| `unload` | `ul` |  | Unstage a file or directory (keep your changes) |
| `version` | `v` |  | Print the version of Forklift |

