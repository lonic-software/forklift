# Forklift glossary

The terms Forklift uses, what each means, and its git equivalent where there is one. This page only
defines words. Flags, refusals and edge cases belong to the guides, [cli.md](cli.md) and
[trust-and-identity.md](trust-and-identity.md); where a guide section covers a term in depth, the
entry links to it.

Terms are grouped by topic, with the more basic terms first. A few words mean different things in
different places; [Overloaded words](#overloaded-words) lists them. Terms for features that are
designed but not shipped are kept apart, in [Planned, not shipped](#planned-not-shipped).

**Contents:**

- [The warehouse model](#the-warehouse-model)
- [Objects and storage](#objects-and-storage)
- [Staging and working directories](#staging-and-working-directories)
- [Merging and moving work](#merging-and-moving-work)
- [Sparse warehouses and scope](#sparse-warehouses-and-scope)
- [Durability and recovery](#durability-and-recovery)
- [Locks](#locks)
- [Remotes](#remotes)
- [Servers](#servers)
- [Identity and keys](#identity-and-keys)
- [The office and trust](#the-office-and-trust)
- [Meta pallets and metadata](#meta-pallets-and-metadata)
- [Hauls](#hauls)
- [Querying history](#querying-history)
- [The machine interface](#the-machine-interface)
- [Overloaded words](#overloaded-words)
- [Planned, not shipped](#planned-not-shipped)

## The warehouse model

**Warehouse** (git: repository). A project and all its history. The history lives in a
`.forklift/` folder beside the working files. See [`prepare`](cli.md#prepare--create-a-warehouse).

**`.forklift/`** (git: `.git/`). The warehouse's own folder: objects, inventory, pallets, config
and bays.

**Warehouse marker.** The file `.forklift/warehouse`, written last by `prepare`, which marks the
folder as a complete warehouse rather than a half-built one.

**Parcel** (git: commit). One immutable recorded snapshot: a root tree, its parents, who made it,
and a description. Facts learned later go into [post-metadata](#meta-pallets-and-metadata), never
into the parcel.

**Pallet** (git: branch). A named, movable pointer to a parcel. Working pallets hold your project's
history; [meta pallets](#meta-pallets-and-metadata), named with `@`, hold Forklift's own signed
metadata. See [`palletize`](cli.md#palletize--create-a-pallet--list-pallets-pz).

**Head** (git: branch tip). The parcel a pallet points at. A server is also called a head; see
[Overloaded words](#overloaded-words).

**Unborn pallet** (git: unborn branch). A pallet with nothing stacked on it yet.

**Default pallet** (git: default branch). The pallet a new clone starts on.

**Revision** (git: revision). Any way of naming a point in history, such as a pallet name or a
parcel hash prefix.

**Operator** (git: author or committer, but broader). An identity in Forklift, recorded as an
opaque id: whoever makes parcels and, in a signed warehouse, whoever holds keys and roles in the
office. See [Identity and keys](#identity-and-keys).

**Office.** The signed record of operators, keys and roles; see
[The office and trust](#the-office-and-trust).

The everyday verbs map onto git like this:

| Forklift | git | What it does |
|----------|-----|--------------|
| `prepare` | `init` | create a warehouse |
| `load` | `add` | stage changes |
| `unload` | `restore --staged` | unstage |
| `remove` | `rm --cached` | stage a removal |
| `stocktake` | `status` | show staged and unstaged changes |
| `stack` | `commit` | record the inventory as a new parcel |
| `history` | `log` | walk parcels, newest first |
| `palletize` | `branch` / `checkout -b` | create, list or delete pallets |
| `shift` | `checkout` / `switch` | switch to a pallet |
| `consolidate` | `merge` | merge another pallet in |
| `park` | `stash` | set work aside |
| `franchise` | `clone` | copy a remote warehouse |
| `lift` | `push` | upload to a remote |
| `lower` | `pull --ff-only` | fetch and fast-forward |
| `diff` | `diff` | line-by-line changes |
| `restore` | `restore` | discard working-directory changes |
| `show` | `show <rev>:<path>` | a file's content at a revision |
| `peek` | `cat-file` | inspect one object |
| `blame` | `blame` | who wrote each line |
| `import-git` / `export-git` | none | migrate a git repository in, or back out |
| `mirror` | none | keep a git remote's branches in step with the remote's pallets |

Every command, with its short alias, is listed in
[the generated command list](../generated/commands.md).

## Objects and storage

**Object** (git: object). An immutable piece of stored content, named by a Blake3 hash of its
type and content, so identical content is stored once.

**Blob** (git: blob). A file's contents.

**Tree** (git: tree). A directory listing: sorted entries, each with a type, a name and a hash.

**Recipe** (git: none; closest is a Git LFS pointer, but built in). The index of a large file: the
ordered list of its chunks. See [Large files](cli.md#large-files).

**Chunk.** One content-defined piece of a large file. An edit stores only the chunks it changed.

**Object store** (git: object database). `.forklift/objects/`, where objects live.

**Loose object** (git: loose object). An object stored as its own file.

**Pack** (git: packfile). Many objects in one data file plus an index.

**Delta** (git: delta). An object stored compressed against a similar base object.

**Compact** (git: `repack`). Moves loose objects into packs; with more flags, rewrites the store.
See [`compact`](cli.md#compact--pack-the-object-store).

**Maintenance** (git: `gc --auto`). The automatic compaction a command runs once the store passes
its thresholds.

**Commit-graph** (git: commit-graph). A derived cache that speeds up history walks. It is never a
source of truth.

**Salvage.** Recovering the readable objects out of a damaged pack, which `compact --all` does.
See [`compact`](cli.md#compact--pack-the-object-store).

**Object ceiling.** The largest object Forklift will write. Large files are chunked well below it.

**gc** (git: `gc` / `prune`). Deleting objects nothing reaches any more. On a server it is the
`forklift-server gc` command.

**Parcel actions.** The authorship records inside a parcel. *AUTHOR* names who wrote the change;
*STACK* names who recorded it (git: committer).

## Staging and working directories

**Inventory**, also called the **dock** (git: index, staging area). What the next parcel will
contain. Unlike git's single index file, it is split into one **shard** per directory.

**Stat cache** (git: the index's stat data). File metadata kept with each inventory entry, so
unchanged files need not be re-read.

**Racily clean** (git: racy git). A file changed so soon after it was last checked that its
metadata cannot prove it unchanged. Forklift re-reads such files.

**Bay** (git: worktree). An extra working directory sharing one warehouse's history, with its own
inventory and current pallet. See [`bay`](cli.md#bay--parallel-working-directories).

**Redirect.** The file in a bay's `.forklift/` folder naming the warehouse it belongs to.

**Position.** The parcel a bay's files currently stand on. A bay whose pallet another bay moved is
**behind**.

**Incomplete load.** A `load` that was interrupted. Recording a parcel is refused while it stands,
so a half-staged change is never stacked. See [`load`](cli.md#load--stage-changes-l).

**Ignore file** (git: `.gitignore`). `.forkliftignore`; see
[`.forkliftignore`](cli.md#forkliftignore--files-forklift-shouldnt-track).

## Merging and moving work

**Consolidate** (git: `merge`). Merges another pallet in, recording a two-parent **merge parcel**
when it is not a fast-forward. See
[`consolidate`](cli.md#consolidate--merge-con).

**diff3.** The exact three-way algorithm `consolidate` merges with, separate from the diff used for
display.

**Conflict** (git: conflict). A file both sides changed in ways that cannot be combined. See
[`conflicts`](cli.md#conflicts--list-unresolved-conflicts).

**Cherry-pick** (git: `cherry-pick`). Applies one parcel's change onto the current pallet as a new
parcel.

**Deliver** (git: squash merge, but git loses the trail). Squashes a draft pallet onto a target as
one signed parcel, keeping a link back to the draft. See
[`deliver`](cli.md#deliver--squash-a-draft-onto-a-target-keeping-the-trail-dv).

**Draft pallet, checkpoint, trail.** A convention for `deliver`: checkpoints are stacked on a draft
pallet, and after delivery the draft's history (the trail) stays browsable.

**Optimistic lift** (git: none). A `lift` that finds the remote moved merges and retries on its
own, when the working directory is clean and the merge has no conflicts. See
[`lift`](cli.md#lift--push-li).

**Park** (git: `stash`). Sets staged and unstaged changes to tracked files aside as a **parked
parcel**; untracked files are left alone. `park pop` brings the changes back. See
[`park`](cli.md#park--stash-pa).

**Undo** (git: roughly `reset --soft` plus the reflog). Reverses the last recorded operation,
using the bay's **undo journal**. See [`undo`](cli.md#undo--reverse-the-last-operation).

## Sparse warehouses and scope

**Sparse warehouse** (git: partial clone plus sparse checkout). A warehouse that has all of history
but file content only for some paths. See [`franchise`](cli.md#franchise--clone-fr).

**Fetch scope.** The paths a warehouse holds content for, shared by every bay.

**Materialization scope** (git: sparse-checkout). The paths one checkout writes to disk, always
within the fetch scope. See [`scope`](cli.md#scope--show-the-sparse-workspace-scope).

**Sealed.** Content outside the fetch scope: not on disk, but pinned by the hashes of the trees
that hold it. See [`franchise`](cli.md#franchise--clone-fr).

**Spine.** The trees from the root down to each scoped folder, which a sparse warehouse always
holds.

**Expand.** Widens the fetch scope and fetches the new content. See
[`expand`](cli.md#expand--widen-a-sparse-warehouses-fetch-scope).

**Narrow.** Shrinks a checkout's materialization scope without freeing storage. See
[`narrow`](cli.md#narrow--shrink-this-checkouts-materialization-scope).

**scope-prune** (git: none). Shrinks the fetch scope and deletes dropped content it can safely
fetch back, to free disk.
See [`scope-prune`](cli.md#scope-prune--reclaim-disk-from-a-sparse-warehouse).

**Held-heads record.** A record of pallet heads this warehouse saw the remote accept or serve. See
[`scope-prune`](cli.md#scope-prune--reclaim-disk-from-a-sparse-warehouse).

**Fetched-heads ledger.** A sparse warehouse's record of the heads its fetches brought in, so that
`expand` widens their history too.

**Origin.** The remote a sparse warehouse fetched from.

## Durability and recovery

**visible ⟹ durable.** Forklift's core durability rule: once an object's final name is visible on
disk, the object should be safely on disk. Where a write fails in a way that leaves this unproven,
a durability taint records it.

**Durability taint** (git: none). A record of paths whose durability could not be proven after a
rare kind of write failure. See
[`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation).

**Torn taint.** A taint record that was itself cut off by a crash.

**Restage.** Reading an object back, checking its hash, and rewriting it durably.

**heal** (git: none). The recovery command for taints and interrupted operations.
See [`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation).

**Operation marker.** A record a multi-step command writes before it starts and removes when it
finishes. One left behind marks an **interrupted operation**. See
[Interrupted operations](cli.md#interrupted-operations).

**Store health.** What [`store`](cli.md#store--object-store-health) reports about the object store.

## Locks

Forklift has several locks, each guarding a different thing.

| Lock | Guards |
|------|--------|
| Warehouse lock (git: `index.lock`) | one bay's inventory and working directory |
| Ref lock (`refs.lock`) | pallets and other state every bay shares |
| Object-store lock (`store.lock`) | the object store, against overlapping compactions |
| Store-access lock (`store-access.lock`) | the object store, ordering commands that write objects against those that delete them (`compact --all`, `scope-prune`, gc) |
| Config lock | one config file |
| Serve lock (`serve.lock`) | a warehouse a server is serving; gc and `compact --all` also take it while they run |
| Server base lock (`.forklift-server.lock`) | a server's base folder |
| Build lock | a warehouse still being created |

## Remotes

**Remote** (git: remote). Another warehouse you sync with, treated as untrusted storage. See
[Working with remotes](cli.md#6-working-with-remotes).

**Franchise** (git: `clone`). Copies a remote warehouse. See
[`franchise`](cli.md#franchise--clone-fr).

**Lift** (git: `push`, never forced). Uploads a pallet and moves the remote's head. See
[`lift`](cli.md#lift--push-li).

**Lower** (git: `pull --ff-only`). Fetches and fast-forwards. A diverged working pallet is
reported, never merged on its own. See [`lower`](cli.md#lower--pull-lo).

**Diverged, remote ahead.** Refusals meaning the remote's history moved: *diverged* when both sides
have parcels the other lacks, *remote ahead* when you need to lower first.

**Bundle** (git: bundle, or a packfile in transit). Many objects downloaded as one package, for a
`franchise`, a `lower` or a sparse fetch. It is never a source of truth. See
[Working with remotes](cli.md#6-working-with-remotes).

**Batch endpoint, batch lane.** The request that fetches many objects at once. A client that finds
it missing or slow **closes the lane** and fetches one at a time. See
[`lower`](cli.md#lower--pull-lo).

**Tor.** A transport for reaching remotes behind NAT; see
[Reaching a remote over Tor](cli.md#reaching-a-remote-over-tor).

## Servers

**Head** (in the server sense). A server program that answers the remote protocol. There are two:
**`forklift-server`**, the **direct head**, which serves warehouses from its own disk; and the
**Lambda head**, the **serverless head**, which runs on AWS. See [SERVER.md](../SERVER.md).

**Served root.** A warehouse folder that `forklift-server` serves.

**Control plane, data plane.** The *control plane* carries requests such as the handshake and refs;
the *data plane* (also *byte plane*) carries object bytes.

**Handshake.** The first request a client makes, which describes the remote.

**Staging (server).** Where the Lambda head holds uploads until their hashes are checked.

**Hooks.** Programs a server calls to fit into a hosting provider. They never take part in
verifying history.

**Principal.** Who the server decided a request comes from.

## Identity and keys

**Operator id** (git: author name and email, but pseudonymous). An opaque id. History never stores
a name or email. See
[Profiles and pseudonymity](trust-and-identity.md#11-profiles-and-pseudonymity).

**Display name** (git: `user.name`). Your name as shown on your own machine. It is never sent
anywhere.

**Profile.** A named identity, so one machine can act as different operators.

**Key** (git: GPG or SSH signing key). An Ed25519 key pair. The public half is recorded in the
office; the private half stays on your machine.

**Identity root.** The key the office pins for each operator. The operator's other keys are
vouched for by it, or by an admin. See [Concepts](trust-and-identity.md#1-concepts).

**Endorsement.** A key's signature vouching for another key: one of your keys vouching for your
new device, or an admin's key vouching for a newly admitted operator.

**Proof of possession.** A new key's own signature, so nobody can enroll a key they do not hold.

**Admit, link, authorize.** The ways a key enters the office: adding an operator, adding your own
device, and re-keying an operator who lost every device. See
[Adding people](trust-and-identity.md#3-adding-people) and
[Multiple devices](trust-and-identity.md#5-multiple-devices).

**Rotate, retire, compromised.** Replacing a key, revoking it, and marking it as possibly in other
hands. See [Rotating and revoking keys](trust-and-identity.md#8-rotating-and-revoking-keys).

**Distrust boundary.** The pallet heads vouched for when a key was revoked. The revoked key's
signatures still count only on parcels reachable from those heads, decided by ancestry rather
than timestamps. See
[Rotating and revoking keys](trust-and-identity.md#8-rotating-and-revoking-keys).

**Role** (git: none; like a host's collaborator permissions). *admin*, *writer* or *reader*.

**Grant.** A list limiting which pallets a writer may move. See
[Roles and grants](trust-and-identity.md#4-roles-and-grants).

**Identity class.** Whether an operator is a *human*, *agent*, *bot* or *service*.

**Agent, supervisor.** An agent is an operator marked as an AI; each names a human supervisor.
See [Agents, bots, and services](trust-and-identity.md#7-agents-bots-and-services).

## The office and trust

**The office** (`@office`) (git: none; like a host's collaborator list, but inside the repo and
signed). The meta pallet holding operators, keys and roles. Its history is the audit trail of who
was allowed to do what. See [Concepts](trust-and-identity.md#1-concepts).

**Office chain.** The office's history, a straight line from genesis.

**Genesis.** The first office parcel, which signs itself.

**Trust anchor.** The record that pins genesis. Once a warehouse has one, new parcels must be
signed. See [Establishing trust](trust-and-identity.md#2-establishing-trust).

**Trust boundary.** The pallet heads at the moment trust was switched on. Parcels before it, the
**legacy parcels**, may be unsigned. See
[Establishing trust](trust-and-identity.md#2-establishing-trust).

**Trust pin.** A point in history the trust state keeps alive, so gc never deletes what it reaches.
See [`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation).

**First contact, TOFU** (like an SSH host key). Adopting a remote's genesis the first time you
connect. See [First contact](trust-and-identity.md#joining-a-trusted-warehouse-first-contact).

**Trust changed.** The refusal when a remote's trust no longer matches what you pinned, such as a
different genesis. See
[First contact](trust-and-identity.md#joining-a-trusted-warehouse-first-contact).

**Re-genesis** (trust reset). Recovery for an office no one can sign any more: a new genesis, with
the old history kept as **attested** but no longer verified. See
[Recovery: re-genesis](trust-and-identity.md#9-recovery-re-genesis).

**Audit** (git: none; loosely `verify-commit`). Checks signed history offline. See
[`audit`](cli.md#audit--verify-signed-history-offline-a).

**Parcel signature.** An Ed25519 signature over a parcel's hash, stored beside the parcel rather
than inside it.

## Meta pallets and metadata

**Meta pallet** (git: roughly `refs/notes/*`). A pallet named with `@`, in a namespace of its own:
`@office`, `@manifest`, `@kv`, `@tags` and `@haul`.

**Tracked metadata.** Metadata stored as ordinary objects, so it is hashed, signed and transferred
like everything else.

**Post-metadata.** Anything recorded about a parcel after it was made. It never changes the parcel.

**Cell.** One signed value at one path in a meta pallet.

**Replay** (git: none). What happens when a meta pallet other than `@office` has diverged from
the remote: instead of merging, your change is re-applied onto the remote's head as a new parcel.
A meta pallet that has not diverged fast-forwards like any other. See
[`lower`](cli.md#lower--pull-lo).

**Meta conflict.** Both sides changed the same meta value differently. See
[`lower`](cli.md#lower--pull-lo).

**Manifest** (`@manifest`) (git: notes, or a host's reviews). Signed statements about a parcel, such
as approvals and provenance. See [`manifest`](cli.md#manifest--signed-post-metadata-on-parcels-mf).

**Provenance.** A manifest entry recording which model, tool and session produced a parcel.

**kv** (`@kv`) (git: notes, loosely). Free-form signed values keyed by parcel. See
[`kv`](cli.md#kv--custom-metadata-on-parcels).

**Tag** (`@tags`) (git: `tag -s`, but git tags can be moved or deleted). A named, signed pointer at
a parcel that can never move. See [`tag`](cli.md#tag--signed-tags--releases).

## Hauls

**Haul** (git: none; like a pull request). A reviewable proposal to merge one pallet into another,
stored on `@haul`. See [`haul`](cli.md#haul--pull-requests-reviewable-merge-proposals).

**Haul id.** The hash that names a haul.

**Opener.** The operator who opened a haul.

**Review.** An approval, a request for changes, or a comment on a haul.

**Merge proof.** Your own warehouse's check of whether a haul marked merged really is.

## Querying history

**Query** (git: `log` filters; jj revsets). Filters history, above all on signed facts such as who
signed a parcel and what produced it. See
[`query`](cli.md#query--filter-history-on-signed-identity-q).

**Verified by default.** `query` checks a parcel's signature before matching it on identity.

**Recorded.** What a parcel says about itself, unchecked, as opposed to verified.

**Trust label.** How far a parcel's identity was confirmed, such as *verified* or *unsigned*.

## The machine interface

**`--json`.** Every command's machine-readable output, for scripts and AI agents. See
[Machine & agent output](cli.md#8-machine--agent-output) and
[MACHINE_INTERFACE.md](../MACHINE_INTERFACE.md).

**Schema version.** The version number carried in all `--json` output, which a script can check.

**MCP server** (`forklift mcp`). Serves Forklift's commands as tools to an AI client over the Model
Context Protocol.

**Exit code.** The number a command exits with. Forklift's refusals each have a stable one; see
[Exit codes](cli.md#11-exit-codes).

**Error code.** The stable name of a refusal in `--json` output, such as `warehouse_locked`.

**Protocol version.** The version of the remote protocol client and server speak.

**`self-schema`, Pult.** `self-schema` describes Forklift's commands for Pult, a separate Lonic
tool that drives command-line tools. See
[`self-schema`](cli.md#self-schema--describe-the-command-surface-for-pult).

## Overloaded words

| Word | Meanings |
|------|----------|
| head | (1) the parcel a pallet points at; (2) a server program: `forklift-server` or the Lambda head |
| scope | (1) fetch scope, what the warehouse holds; (2) materialization scope, what one checkout writes to disk |
| lock | several different locks; see [Locks](#locks) |
| marker | the warehouse marker; the operation marker; conflict markers inside a file |
| staging | (1) the inventory; (2) the Lambda head's holding area for unchecked uploads |
| gc | (1) the general idea of deleting unreachable objects; (2) the `forklift-server gc` command |
| admission | (1) a server hook; (2) admitting an operator to the office |
| redirect | (1) a bay's redirect file; (2) an HTTP redirect |

## Planned, not shipped

These terms appear in design notes and tickets. The features they describe are designed or
proposed, but not built.

**Tombstone.** A record of a deleted pallet's last head on a server, keeping that history safe.

**Holds request.** A remote request asking whether it still holds a given head.

**Cache mode.** A warehouse that keeps every parcel but may evict content, fetching it back on
demand.

**Lazy history, remote-first reads, signed trust checkpoints.** Clones whose cost depends on the
tip rather than history length; reading history from a remote with no local warehouse; and
verifying from a signed snapshot of trust instead of from genesis.

**Approval gating.** A signed merge policy hauls must satisfy before they can merge.

**File-level access control.** Per-file grants that a remote enforces.

**First-class conflicts.** Conflicts that can be stacked and shared, rather than blocking `stack`.

**Locker.** A private, per-operator tracked namespace.

**Peer mode.** Warehouses syncing with each other directly.
