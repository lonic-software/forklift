<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run
`bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit
the result after a change that affects it (an error code, a command, alias or help line,
or a `--json` output struct).
-->

# MCP tools

Every tool `forklift mcp` serves, generated from the same command definition the binary parses its arguments with, as the server's own tool list is; CI fails when this page falls behind it. A tool is a command path joined by `_` (`forklift tag create` is `tag_create`, `forklift cherry-pick` is `cherry_pick`), and its description is the command's one-line description followed by its long one; each argument's own help is that argument's description in the tool's input schema. An argument is an option's long name (with `_` for `-`) or a positional's name. `docs/MACHINE_INTERFACE.md` describes the server.

| Tool | Runs | Does | Arguments |
|---|---|---|---|
| `audit` | `forklift audit` | Verify the warehouse's signed history offline | `pallet` (a string)<br>`full` (a boolean) |
| `bay` | `forklift bay` | Manage bays: parallel working directories bound to this warehouse |  |
| `bay_add` | `forklift bay add` | Open a new bay (a new working directory on a new pallet) | `name` (a string, required)<br>`path` (a string)<br>`scope` (an array of strings) |
| `bay_remove` | `forklift bay remove` | De-register a bay (its pallet ref and materialized files are kept) | `name` (a string, required) |
| `blame` | `forklift blame` | Show, for each line of a file, the parcel and author that last changed it | `path` (a string, required)<br>`rev` (a string) |
| `cherry_pick` | `forklift cherry-pick` | Apply a parcel's changes onto the current pallet as a new, author-preserving parcel | `revision` (a string, required)<br>`message` (a string) |
| `compact` | `forklift compact` | Compact the object store: pack loose objects into a few dense pack files | `all` (a boolean)<br>`redelta` (a boolean) |
| `conflicts` | `forklift conflicts` | List the files left in conflict by an unresolved consolidation |  |
| `consolidate` | `forklift consolidate` | Consolidate (merge) another pallet into the current one | `pallet` (a string, required) |
| `deliver` | `forklift deliver` | Squash the current draft pallet onto a target as one clean parcel, keeping the trail | `target` (a string, required)<br>`message` (a string) |
| `diff` | `forklift diff` | Show line-by-line changes | `staged` (a boolean)<br>`targets` (an array of strings) |
| `haul_open` | `forklift haul open` | Open a merge proposal | `target` (a string, required)<br>`source` (a string)<br>`title` (a string, required)<br>`message` (a string) |
| `haul_list` | `forklift haul list` | List proposals | `state` (a string) |
| `haul_show` | `forklift haul show` | Show one proposal in full | `id` (a string, required) |
| `haul_update` | `forklift haul update` | Propose the source pallet's current head | `id` (a string, required) |
| `haul_comment` | `forklift haul comment` | Add a comment | `id` (a string, required)<br>`message` (a string, required) |
| `haul_review` | `forklift haul review` | Record a signed review (approve by default) | `id` (a string, required)<br>`request_changes` (a boolean)<br>`comment` (a boolean)<br>`message` (a string) |
| `haul_merge` | `forklift haul merge` | Merge a proposal (consolidate its source into the target) | `id` (a string, required) |
| `haul_close` | `forklift haul close` | Close a proposal without merging | `id` (a string, required) |
| `haul_reopen` | `forklift haul reopen` | Reopen a closed proposal | `id` (a string, required) |
| `heal` | `forklift heal` | Resolve a standing durability taint, or finish or undo an interrupted operation |  |
| `history` | `forklift history` | Print the parcels of a revision, newest first | `revision` (a string)<br>`limit` (an integer of at least 1)<br>`after` (a string)<br>`oneline` (a boolean) |
| `lift` | `forklift lift` | Upload the current pallet's new parcels to the remote (push) | `recreate` (a boolean) |
| `load` | `forklift load` | Take the current state of a file or directory into the inventory (stage) | `path` (a string, required) |
| `lower` | `forklift lower` | Download the remote's new parcels and fast-forward the current pallet, or another (pull) | `pallet` (a string)<br>`expect` (a string)<br>`keep` (an array of strings) |
| `kv_set` | `forklift kv set` | Set a key on a parcel | `revision` (a string, required)<br>`key` (a string, required)<br>`value` (a string, required) |
| `kv_get` | `forklift kv get` | Show a key's value on a parcel | `revision` (a string, required)<br>`key` (a string, required) |
| `kv_unset` | `forklift kv unset` | Remove a key from a parcel | `revision` (a string, required)<br>`key` (a string, required) |
| `kv_list` | `forklift kv list` | List every value, or a parcel's | `revision` (a string) |
| `kv_log` | `forklift kv log` | Show every change to a key, newest first, with who made it | `revision` (a string, required)<br>`key` (a string, required) |
| `manifest` | `forklift manifest` | Attach signed post-metadata to a parcel: approvals, review notes |  |
| `manifest_note` | `forklift manifest note` | Attach a free-form note to a parcel | `revision` (a string, required)<br>`message` (a string, required) |
| `manifest_approve` | `forklift manifest approve` | Record a signed approval (sign-off) of a parcel | `revision` (a string, required)<br>`message` (a string) |
| `manifest_provenance` | `forklift manifest provenance` | Record how a parcel was produced: model, tool, session (AI provenance) | `revision` (a string, required)<br>`model` (a string, required)<br>`tool` (set by the server)<br>`session` (set by the server)<br>`transcript` (a string)<br>`message` (a string) |
| `manifest_show` | `forklift manifest show` | Show the manifest entries attached to a parcel | `revision` (a string, required) |
| `office_list` | `forklift office list` | List the users and their keys (the default) |  |
| `palletize` | `forklift palletize` | Create a new pallet (branch) and shift to it; without a name, list the pallets | `name` (a string)<br>`revision` (a string)<br>`all` (a boolean)<br>`remote` (a boolean)<br>`delete` (a boolean)<br>`force` (a boolean)<br>`restore` (a boolean) |
| `park` | `forklift park` | Park the work in progress and reset to the pallet head |  |
| `park_pop` | `forklift park pop` | Re-apply the most recently parked changes (staged) and drop them from the list |  |
| `park_list` | `forklift park list` | List the parked parcels, newest first |  |
| `peek` | `forklift peek` | Peek into an object (print its content) | `inventory` (a string)<br>`object` (a string) |
| `query` | `forklift query` | Query parcel history by signed identity, signer and parcel facts | `revisions` (an array of strings)<br>`from` (a string)<br>`class` (a string)<br>`unsupervised` (a boolean)<br>`supervisor` (a string)<br>`signer` (a string)<br>`author_after` (a string)<br>`author_before` (a string)<br>`merges` (a boolean)<br>`no_merges` (a boolean)<br>`grep` (a string)<br>`model` (a string)<br>`tool` (a string)<br>`tag` (a string)<br>`touches` (a string)<br>`verify` (a boolean)<br>`recorded` (a boolean)<br>`where` (an object)<br>`limit` (an integer of at least 1)<br>`after` (a string)<br>`oneline` (a boolean) |
| `remove` | `forklift remove` | Stage a file or directory for removal | `path` (a string, required) |
| `restore` | `forklift restore` | Restore a file or directory from the inventory (discard unstaged changes) | `staged` (a boolean)<br>`path` (a string, required) |
| `shift` | `forklift shift` | Shift to another pallet | `pallet` (a string, required) |
| `show` | `forklift show` | Print a file's content at a revision (`git show <rev>:<path>`'s equivalent) | `target` (a string, required) |
| `stack` | `forklift stack` | Stack the inventory as a new parcel (commit) on the current pallet | `description` (a string) |
| `stocktake` | `forklift stocktake` | Report the staged and unstaged changes of the warehouse | `summary` (a boolean) |
| `expand` | `forklift expand` | Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s) | `paths` (an array of strings, required) |
| `narrow` | `forklift narrow` | Narrow this checkout's materialization scope and de-materialize the dropped subtree(s) | `paths` (an array of strings, required) |
| `scope_status` | `forklift scope status` | Show the current bay's materialization scope and the warehouse fetch scope (the default) |  |
| `store` | `forklift store` | Report object-store health: loose vs packed objects, packs, and whether compaction is due |  |
| `tag_create` | `forklift tag create` | Create a signed tag pointing at a revision (admin only) | `name` (a string, required)<br>`revision` (a string)<br>`message` (a string) |
| `tag_show` | `forklift tag show` | Show one tag in full (subject, tagger, message) | `name` (a string, required) |
| `tag_list` | `forklift tag list` | List every tag (the default) |  |
| `undo` | `forklift undo` | Undo the last stack, consolidate, cherry-pick or shift (soft — keeps your changes) | `as` (a string) |
| `unload` | `forklift unload` | Unstage a file or directory (keep your changes) | `path` (a string, required) |

## Not served

Each of these commands, and every command below it, is a tool for nobody: a person runs it.

- `forklift alias`: manages the `fl` shell alias next to this binary, a host concern.
- `forklift prepare`: creates a warehouse, which is setup.
- `forklift config`: reads and sets configuration, which is setup.
- `forklift profile`: manages identity profiles, which is setup.
- `forklift franchise`: clones a remote, which is setup.
- `forklift import-git`: migrates a git repository in, which is setup.
- `forklift export-git`: migrates out to git, which is setup.
- `forklift mirror`: pushes to an external git remote under the user's own git credentials.
- `forklift office enroll`: establishes the operator's trust, a one-way setup step.
- `forklift office keygen`: creates a signing key, which is setup.
- `forklift office admit`: enrolls another operator: trust administration.
- `forklift office link`: links another device's key: trust administration.
- `forklift office authorize`: authorizes a key for another operator: trust administration.
- `forklift office role`: changes a user's role and grants: trust administration.
- `forklift office rotate`: replaces the operator's keys: trust administration.
- `forklift office retire`: revokes a key: trust administration.
- `forklift office regenesis`: resets trust, a recovery step.
- `forklift office accept-regenesis`: accepts a remote's trust reset, a recovery step.
- `forklift self-update`: updates the binary, a host concern.
- `forklift self-schema`: describes the CLI for Pult; agents get the tool list instead.
- `forklift scope-prune`: destructive, warehouse-wide disk reclamation: an admin verb, never a sub-agent's.
- `forklift evict`: warehouse-wide disk reclamation in a cache: an admin verb, like scope-prune.
- `forklift remote`: repoints a partial warehouse at another server, which is setup.
- `forklift mcp`: this server itself.
- `forklift help`: meta.
- `forklift version`: meta.

These arguments are refused together, though each is served alone:

- `remote` with `delete` of `forklift palletize`: deletes the remote's pallet, which every clone shares: an admin's call, never a sub-agent's.
- `remote` with `restore` of `forklift palletize`: restores the remote's pallet, which every clone shares: an admin's call, never a sub-agent's.

These families' bare form runs the same operation as one of their subcommands, so only the subcommand is a tool:

- `forklift haul` is `haul_list`.
- `forklift office` is `office_list`.
- `forklift scope` is `scope_status`.
- `forklift tag` is `tag_list`.

