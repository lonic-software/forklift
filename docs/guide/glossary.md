# Forklift glossary

The terms Forklift uses, what each means, and its git equivalent where there is one. The
[guides](README.md) explain how to *use* each thing; this page only says what each word means.

Terms are grouped by topic. Within a topic, the more basic terms come first. A few words mean
different things in different places; [Overloaded words](#overloaded-words) lists each one with
all its meanings. Terms for features that are designed but not shipped are kept apart, in
[Planned, not shipped](#planned-not-shipped).

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
`.forklift/` folder; the working files sit beside it. `prepare` creates one.

**`.forklift/`** (git: `.git/`). The warehouse's own folder: the object store, inventory, pallets,
config, commit-graph and bays.

**Warehouse marker.** The file `.forklift/warehouse`, the last thing `prepare` writes. It certifies
that the folder is a complete warehouse rather than a half-built one. A marker this build cannot
read is an error, never treated as absent.

**Parcel** (git: commit). One immutable recorded snapshot: a root tree, its parents, authorship
actions and a description. Facts learned after a parcel is made go into
[post-metadata](#meta-pallets-and-metadata), never into the parcel.

**Pallet** (git: branch). A named, movable pointer to a parcel, its *head*. Working pallets hold
your project's history. [Meta pallets](#meta-pallets-and-metadata), whose names start with `@`,
hold Forklift's own signed metadata.

**Head** (of a pallet) (git: branch tip). The parcel a pallet points at. A server is also called a
head; see [Overloaded words](#overloaded-words).

**Unborn pallet** (git: unborn branch). A pallet with nothing stacked on it yet.

**Default pallet** (git: default branch). The pallet a new clone starts on.

**Revision** (git: revision). Any way of naming a point in history: a pallet name, an `@`-qualified
meta pallet, or a parcel hash prefix of at least 4 hex characters. Two forms belong to one command
each. `diff` also accepts `:empty`, the empty tree, so a root parcel can be compared against a
clean slate. `show` takes `<revision>:<path>` to name one file at that revision.

**Operator** (git: author). Whoever authored a parcel. History stores an opaque operator id, never
a name or email; see [Identity and keys](#identity-and-keys).

**Office.** The signed record of operators, keys and roles; see
[The office and trust](#the-office-and-trust).

The everyday verbs map one to one onto git:

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

Most commands have a short alias (`st` for `stocktake`, `s` for `stack`), and `fl` can be set up as
a short name for `forklift` itself (`alias`). The full command list with aliases is in
[cli.md](cli.md#10-command--alias-reference).

## Objects and storage

**Object** (git: object). An immutable piece of stored content, named by the Blake3 hash of its
bytes. Identical content always has the same name, so it is stored once.

**Blob** (git: blob). A file's contents. A symlink's blob holds its target path.

**Tree** (git: tree). A directory listing: sorted entries, each with a type, a name and a hash.
Entry types are normal file, executable, symbolic link, tree, and the chunked forms of the first
two.

**Recipe** (git: none; the closest is a Git LFS pointer, but this is built in). The index of a large
file (8 MiB or more): its total size and the ordered list of its chunks. A tree entry for a large
file points at its recipe.

**Chunk.** One content-defined piece of a large file, stored as its own object type. Because chunk
boundaries follow the content, an edit to a large file only stores the chunks it changed.

**Object store** (git: object database). `.forklift/objects/`, where objects live. Each one is
stored zstd-compressed.

**Loose object** (git: loose object). An object stored as its own file.

**Pack** (git: packfile). Many objects in one append-only data file plus a sorted index
(`.pack` + `.idx`).

**Delta** (git: delta). An object stored compressed against a similar *base* object, such as the
previous version of the same file. It is checked against its hash when rebuilt.

**Compact** (git: `repack`). Moves loose objects into packs. Plain `compact` adds a pack and drops
nothing, so it is safe beside a running server. `compact --all` rewrites every pack and drops
unreachable objects. `--redelta` re-chooses delta bases for the whole store.

**Maintenance / auto-compaction** (git: `gc --auto`). The automatic `compact` a mutating command
runs once the store passes its thresholds (`maintenance.loose`, `maintenance.packs`).

**Commit-graph** (git: commit-graph). `.forklift/graph/`, a derived cache of parent links,
generation numbers and changed-path filters that speeds up history walks. It is never a source of
truth and repairs itself.

**Salvage.** What `compact --all` does with a damaged pack: it reads the pack record by record into
loose objects, and deletes the pack only once every live object it may hold has another copy.

**Object ceiling.** 64 MiB, the largest object Forklift will write. Files are split into chunks
from 8 MiB up, well below it. Objects over the ceiling that predate it stay readable locally, but a
remote refuses to transfer them.

**gc** (git: `gc` / `prune`). Garbage collection: deleting objects nothing reaches any more. On a
server it is `forklift-server gc`, which keeps a grace period so lifts still in flight are safe.

**Parcel actions.** The authorship records inside a parcel. *AUTHOR* names who wrote the change.
*STACK* names who recorded it (git: committer). Operations that copy a change, such as
`cherry-pick`, keep the original AUTHOR actions and add their own STACK action.

## Staging and working directories

**Inventory**, also called the **dock** (git: index, staging area). What the next parcel will
contain. Unlike git's single index file, it is split into one **shard** per directory, so a change
rewrites only the shards it touches.

**Stat cache** (git: the index's stat data). The size, times, device and inode stored for each
inventory entry. It lets `load` and `stocktake` skip re-reading files that have not changed.

**Racily clean** (git: racy git). A file modified within the same clock tick as its last stat
could look unchanged when it is not. Forklift rehashes such a file until a later stat proves it
clean.

**Bay** (git: worktree). An extra working directory that shares one warehouse's objects, pallets,
trust and config, with its own inventory, current pallet and lock. Several bays can work on
different pallets at once.

**Redirect.** The file inside a bay's `.forklift/` folder that names the warehouse it belongs to.
The warehouse in turn records where each bay lives.

**Position.** The parcel a bay's files currently stand on. If another bay moves the shared pallet
past it, the bay is **behind** until `shift` catches it up.

**Incomplete load.** A `load` that was interrupted. It leaves a record behind, and `stack` and
`park` refuse until the load is finished, so a half-staged change is never recorded.

**Ignore file** (git: `.gitignore`). `.forkliftignore`, the patterns of files Forklift should not
track.

## Merging and moving work

**Consolidate** (git: `merge`). Merges another pallet in. The outcome is either "already up to
date", a fast-forward, or a three-way merge that records a two-parent **merge parcel**.

**diff3.** The exact, whitespace-preserving three-way algorithm `consolidate` merges with. It is
deliberately separate from the diff used for display. Conflict markers include the base text.

**Conflict** (git: conflict). A file both sides changed in ways that cannot be combined. A
conflicted file blocks `stack` until it is resolved. A binary file, or one whose type changed,
conflicts as a whole. `conflicts` lists them.

**Cherry-pick** (git: `cherry-pick`). Applies one parcel's change, relative to its first parent,
onto the current pallet as a new parcel.

**Deliver** (git: squash merge, but git loses the trail). Squashes a draft pallet's net change onto
a target as one signed parcel. The draft pallet is kept, and a delivery record links the new parcel
back to it.

**Draft pallet, checkpoint, trail.** A convention for `deliver`, not a special pallet type. You (or
an agent) stack checkpoints on a draft pallet. Delivering it squashes them into one parcel, and the
draft's history (the trail) stays browsable.

**Optimistic lift** (git: none). When `lift` finds the remote has moved, and the two sides changed
disjoint files, it lowers, consolidates and retries on its own.

**Park** (git: `stash`). Saves staged and unstaged changes as a **parked parcel** and resets to the
head. `park pop` brings them back, on the same head only.

**Undo** (git: roughly `reset --soft` plus the reflog). Reverses the last journaled operation:
stack, consolidate, cherry-pick, shift or a pallet delete. The **undo journal** is the bay-local
record that makes this possible. `undo --as <name>` brings a deleted pallet back under a new name.
Trust operations are never undone.

## Sparse warehouses and scope

**Sparse warehouse** (git: partial clone plus sparse checkout). A warehouse made with
`franchise --only <paths>`. It has every parcel and signature, so the whole history is there, but
it holds file content only for the listed paths.

**Fetch scope.** The paths a warehouse holds content for (`.forklift/config/fetch-scope`). It is
shared by every bay.

**Materialization scope** (git: sparse-checkout). The paths one bay writes to disk
(`bay add --scope`). It is always within the fetch scope. An empty or missing scope file means
everything. Every narrower scope also includes the root `.forkliftignore`.

**Sealed.** Content outside the fetch scope. It is not on disk, but the hash a signed parcel
commits to pins it, so it is shown as neither removed nor forged.

**Spine.** The trees on the path from the root down to each scoped folder. A sparse warehouse
always holds them.

**Expand.** Widens the fetch scope and fetches the newly included content, across all history held.

**Narrow.** Shrinks one checkout's materialization scope (a bay's, or a sparse main tree's) and
removes the dropped files from the working directory. It frees nothing in the object store: that
content is still ordinary history.

**scope-prune** (git: none). Narrows the fetch scope and deletes the content that falls out of it,
to free disk. This is safe only for content the remote still holds, because the remote is where
`expand` gets it back. So `scope-prune` checks with the remote first, and keeps anything it cannot
confirm.

**Held-heads record.** `.forklift/held-heads`: for each working pallet, up to 16 heads this
warehouse saw the remote accept or serve. Each entry is only a candidate to ask the remote about,
never proof on its own. It is written today. Nothing reads it yet.

**Fetched-heads ledger, fence list.** A sparse warehouse's record of every head a scoped fetch
brought in, including heads no pallet points at any more, so `expand` can widen their history too.
A recorded head the remote can no longer serve moves to the *fence list*, and no pallet may be
moved onto a fenced head until a scoped fetch of it completes.

**Origin.** The remote a sparse warehouse fetched from. A sparse warehouse may lift only to its
origin.

## Durability and recovery

**visible ⟹ durable.** Forklift's core durability rule: once an object's final name is visible on
disk, the object is safely on disk. Because of this, "the file exists" can be trusted as "the
object is stored".

**Durability taint** (git: none). A record of objects whose durability could not be proven, for
example because a crash hit mid-write. While a taint stands, every command except `heal` and
`audit` refuses (exit 21).

**Torn taint.** A taint record that was itself cut off by a crash. Its scope is unknown, so `heal`
rescans the whole store.

**Restage.** Reading an object back, checking its hash and rewriting it durably. Every command
restages any verified loose object recorded in a taint on entry, before it starts.

**heal** (git: none; loosely `fsck`). The recovery command. It resolves a durability taint, and
finishes, rolls forward or rolls back an operation a crash interrupted.

**Operation marker.** A record a multi-step command (shift, park, consolidate, lower, bay creation
and others) writes before it starts and removes when it finishes. A marker left behind tells
`heal` that something was interrupted.

**Interrupted operation.** An operation whose marker was left standing. `heal` finishes or undoes
it. An interrupted `franchise` is resumed by running the same command again.

**Store health.** What `store` reports: counts and sizes of loose and packed objects, delta
density, whether maintenance is due, and any damaged or unreadable packs.

## Locks

Forklift has several locks. Each guards a different thing.

| Lock | Guards | Taken by |
|------|--------|----------|
| Warehouse lock (`lock`, per bay) (git: `index.lock`) | one bay's inventory and working directory | every mutating command in that bay, including those that write refs; a killed process leaves it behind. It serializes only its own bay, so ref writes, which every bay shares, also take the ref lock |
| Ref lock (`refs.lock`) | state shared by every bay | each pallet ref compare-and-set, trust installs, the fetched-heads ledger and fence list, `expand`'s scope commit, and the held-heads record |
| Object-store lock (`store.lock`) | the object store | compaction, so two never overlap |
| Store-access lock (`store-access.lock`) | whole-store rewrites | `compact --all`, `scope-prune` and server gc, against mutating commands |
| Config lock (`<config file>.lock`) | one config file | every config edit; a second editor waits rather than failing |
| Serve lock (`.forklift/serve.lock`) | a warehouse being served | `forklift-server`; blocks gc and full repack |
| Server base lock (`.forklift-server.lock`) | a server's base folder | one server per folder |
| Build lock (`lock`, in `prepare`'s temporary folder) | a warehouse being built | `prepare`, until the finished folder is renamed into place |

## Remotes

**Remote** (git: remote). Another warehouse you sync with. A remote is treated as untrusted
storage: everything downloaded is checked against its hash, and signed history against the
office.

**Franchise** (git: `clone`). Copies a remote warehouse. If it fails partway, it keeps what it
fetched; running the same command again resumes.

**Lift** (git: `push`, never forced). Uploads a pallet and moves the remote's head, fast-forward
only. The office is lifted first.

**Lower** (git: `pull --ff-only`). Fetches and fast-forwards. A divergence is reported, never merged
on its own. Meta pallets sync first.

**Diverged / remote ahead.** The two refusals a lift can end in when it cannot proceed. *Diverged*:
both sides have parcels the other lacks. *Remote ahead*: the remote has moved on, and you need to
lower first.

**Bundle** (git: bundle, or a clone packfile). A whole-warehouse package the server offers so a
clone can download history in one piece. It only speeds up clones and is never a source of truth.

**Batch endpoint, batch lane.** The remote request that returns many objects at once. If the remote
lacks it or is too slow, the client **closes the lane**: for the rest of that command it fetches
objects one at a time.

**Tor.** Forklift can reach a remote over Tor (`remote.tor`). The point is reaching machines behind
NAT, not anonymity.

## Servers

**Head** (in the server sense). A server program that answers the remote protocol. This is a
different meaning from a pallet's head. There are two:

- **`forklift-server`, the direct head.** A program that serves warehouses from its own disk and
  checks uploads as they arrive.
- **The Lambda head, the serverless head.** The hosted service on AWS: API Gateway and Lambda,
  objects in S3, refs and trust in DynamoDB.

**Served root.** A warehouse folder that `forklift-server` serves. It is an ordinary warehouse on
the server's disk, and everyone's lifts land in it. A plain clone on someone's laptop is not a
served root.

**Single- and multi-warehouse serving.** `--root` serves one warehouse; `--warehouses` serves many
from one folder, each created explicitly.

**Control plane, data plane.** The *control plane* carries the JSON requests: the handshake, refs,
negotiation. The *data plane* (also *byte plane*) carries object bytes. On AWS the data plane goes
straight between the client and S3 through presigned URLs.

**Handshake.** The first request a client makes. It returns the protocol version, the pallets, the
default pallet and the trust anchor.

**Staging (server).** On the Lambda head, uploads land under a staging prefix. They become
fetchable only once their hashes are checked and they are promoted into the object store.

**Hooks.** Programs a server calls to fit into a hosting provider: *authentication*, *admission*,
*events* and *resolution*. Requests to them are signed. Hooks are never part of verifying history.

**Principal.** Who the server decided a request comes from: a named operator (per-operator token),
a shared static token, or nobody (an `--open` server). Only a named operator can be held to
per-pallet grants.

## Identity and keys

**Operator id** (git: author name and email, but pseudonymous). An opaque id: a minted UUID by
default, or any string you or a provider choose. History stores only this id and public keys, never
a name or email.

**Display name** (`operator.name`) (git: `user.name`). Your name as shown on your own machine. It
is never sent anywhere.

**Profile.** A named identity (id, name and keys), so one machine can act as different operators in
different warehouses.

**Key** (git: GPG or SSH signing key). An Ed25519 key pair. The public half is recorded in the
office; the private half stays in `~/.forklift-keys`, optionally protected by a passphrase.

**Identity root.** The one key per operator the office pins. Every other key of that operator must
chain back to it.

**Endorsement.** A signed statement by a key you already have: "this new key is also me". It is how
you add a new device.

**Proof of possession.** The new key's own signature over the operator id, so nobody can enroll a
key they do not hold.

**`office admit`, `office link`, `office authorize`.** The ways a key enters the office. `admit`: an
admin adds a new operator. `link`: you add a device of your own. `authorize`: an admin re-keys an
operator who lost every device. `office keygen` makes the key and prints the lines to hand over.

**Rotate, retire, compromised.** `office rotate` replaces a key with a fresh one, signed by the old.
`office retire` revokes a key. `--compromised` marks it as possibly in other hands. Retired keys
are kept forever, so old signatures can still be checked.

**Distrust boundary.** The pallet heads vouched for at the moment a key was revoked. The revoked
key's signatures stay valid only on parcels reachable from that boundary. This is decided by
ancestry, never by timestamps, which a key holder could forge.

**Role** (git: none; like a host's collaborator permissions). *admin*, *writer* or *reader*,
recorded in the office.

**Grant.** A list limiting which working pallets a writer may move. An empty list means all
pallets.

**Identity class.** Whether an operator is a *human*, *agent*, *bot* or *service*. It is set when
the operator is admitted and is separate from role.

**Agent, supervisor.** An agent is an operator marked as an AI. Every agent must name an enrolled
human supervisor; automation cannot supervise automation. The rule behind this: automation signs as
itself, never as a human.

## The office and trust

**The office** (`@office`) (git: none; like a host's collaborator list, but inside the repo and
signed). A meta pallet holding operators, keys and roles. Its history is the audit trail of who was
allowed to do what.

**Office chain.** The office's history, a straight line from genesis. Each office parcel must be
signed by a key that was valid in the state before it.

**Genesis.** The first office parcel, signed by itself. `office enroll` creates it.

**Trust anchor** (`.forklift/trust`). The record that pins genesis. Once a warehouse has one, every
new parcel must be signed, and signing cannot be switched off again.

**Trust boundary** (enrollment boundary). Every pallet head at the moment trust was switched on.
Parcels reachable from it predate trust and may be unsigned; these are **legacy parcels**.

**Trust pin.** A history root the trust state keeps alive: the trust boundary and every distrust
boundary. gc and compaction never delete what a trust pin reaches.

**First contact, TOFU** (like an SSH host key). The first `franchise` or `lower` adopts the remote's
genesis and warns that it did. `--expect <genesis>` pins the expected genesis in advance instead.
Every later answer from that remote is checked against the pinned genesis.

**Trust changed.** The refusal when a remote's genesis no longer matches the pinned one (exit 22).
The report says what changed, such as a re-genesis or an unrelated warehouse.

**Re-genesis** (trust reset). Recovery for an office no one can sign any more: a new genesis, with
the old history kept as **attested** (readable, but no longer verified). It is deliberately loud.
An admin runs `office regenesis`, and each clone must accept it explicitly with
`office accept-regenesis --confirm <genesis>`.

**Audit** (git: none; loosely `verify-commit` plus `fsck`). Checks signed history offline: the
office chain from genesis, then every parcel's signature against the office. A server runs the same
checks on upload. `audit --full` also re-hashes large-file chunks.

**Parcel signature.** An Ed25519 signature over a parcel's hash, stored beside the object in a
`.sig` file. Unlike git, the signature is never inside the commit.

## Meta pallets and metadata

**Meta pallet** (git: roughly `refs/notes/*`). A pallet in its own namespace, named with `@`. The
fixed set is `@office`, `@manifest`, `@kv`, `@tags` and `@haul`; any other `@name` is refused.
Meta pallet names never collide with working pallet names.

**Tracked metadata.** Metadata stored as ordinary blobs and trees, so it gets hashing, signing and
transport for free.

**Post-metadata.** Anything recorded about a parcel after it was made: approvals, notes,
provenance. It never changes the parcel itself.

**Cell.** One signed value at one path in a meta pallet. Every changed cell must be stamped with the
key that signed the parcel changing it.

**Replay** (git: none). How meta pallets sync. Diverged meta pallets are never merged: your net
change since the common ancestor is replayed onto the remote's head as one new parcel, re-signed
with your current key.

**Meta conflict.** Both sides changed the same meta path differently (exit 24). You pick a side
with `lower --keep <pallet>:<path>=ours|theirs`.

**Manifest** (`@manifest`) (git: notes, or a host's reviews). Signed statements attached to a
parcel: approvals (one per operator), notes, provenance and delivery records.

**Provenance.** A manifest entry recording which model, tool and session produced a parcel. It is
signed evidence, not a free-text comment.

**kv** (`@kv`) (git: notes, loosely). Free-form text values keyed by parcel and key, each signed by
whoever wrote it.

**Tag** (`@tags`) (git: `tag -s`, but git tags can be moved or deleted). A named, signed pointer at
a parcel. Only an admin can create one, and it can never be moved or deleted.

## Hauls

**Haul** (git: none; like a pull request). A reviewable proposal to merge one pallet into another
in the same warehouse, stored as signed cells on `@haul`. It is open, closed or merged.

**Haul id.** The hash of the haul's opening record. Any unique prefix works.

**Opener.** The operator who opened a haul, and the only one who may move its proposed head
(`haul update`).

**Review.** An approval, a request for changes, or a comment, pinned to the head the reviewer saw. A
review stops counting once the head moves. Reviews are recorded but not yet enforced.

**Merge proof.** Your own warehouse's check of a haul marked merged: verified (the target really
contains it), refuted, or unproven (the evidence is not held locally).

## Querying history

**Query** (git: `log` filters; jj revsets). `query` filters history on signed facts: identity
class, supervisor, signer, provenance, tag and paths touched.

**Verified by default.** Unless told otherwise, `query` never matches an identity filter against a
value it has not checked. It filters cheaply first, then verifies every survivor's signature.

**Recorded.** The cheap reading of identity: what a parcel says about itself, which can be forged.
`history --class` always reads it this way. `query` does so only with `--recorded`, and then labels
every answer `recorded`.

**Trust label.** How far a parcel's identity was confirmed: *verified*, *signed-revoked*,
*unsigned*, *unknown-key* or *recorded*.

## The machine interface

**`--json`.** Every command's machine-readable output, for scripts and AI agents.

**Schema version.** The version number carried in all `--json` output. It changes when a command's
output or the set of error codes changes incompatibly, so a script can pin it. It is also how a
script detects capabilities: check `schema` before relying on a field, rather than looking for the
field itself.

**MCP server** (`forklift mcp`). Serves Forklift's commands as tools to an AI client over the Model
Context Protocol.

**Exit code.** The number a command exits with, which scripts switch on. 0 is success, 1
unclassified, 2 a usage error. 128+n means the process was killed by signal n (130 is Ctrl-C);
Forklift never exits 130 on its own, and stdout may then be empty. Forklift's own refusals have
stable codes from 3 upward, such as 6 (warehouse locked), 21 (durability taint) and 22 (trust changed). The full list is
in [errors.md](../generated/errors.md).

**Error code.** The stable name of a refusal in `--json` output, such as `warehouse_locked`. Each
has an exit code.

**Protocol version.** The version of the remote protocol. A client refuses a server speaking one
it does not know.

**`self-schema`, Pult.** `self-schema` prints Forklift's command surface as a manifest for Pult, a
separate Lonic tool that drives command-line tools.

## Overloaded words

| Word | Meanings |
|------|----------|
| head | (1) the parcel a pallet points at; (2) a server program: `forklift-server` or the Lambda head |
| scope | (1) fetch scope, what the warehouse holds; (2) materialization scope, what one bay writes to disk |
| lock | several different locks; see [Locks](#locks) |
| marker | the warehouse marker; the operation marker; the incomplete-load record; conflict markers inside a file |
| staging | (1) the inventory; (2) the Lambda head's holding area for unverified uploads |
| gc | (1) the general idea of deleting unreachable objects; (2) the `forklift-server gc` command |
| admission | (1) the server's `admission` hook; (2) admitting an operator to the office (`office admit`) |
| redirect | (1) a bay's redirect file; (2) an HTTP redirect in the remote protocol |
| `--class` | recorded on `history`; verified on `query`, unless `--recorded` is given |

## Planned, not shipped

These terms appear in the design notes and tickets. The features they describe are designed or
proposed, but not built.

**Tombstone.** When a pallet is deleted on a server, a record of its last head. It would keep that
history safe from server gc, and require a later pallet of the same name to descend from it, so a
sparse clone that pruned against the old head never loses data.

**Holds request.** A remote request, "do you still hold head X?", answered held, not held or
unknown. It would let `scope-prune` use the held-heads record.

**Cache mode** ("a warehouse is a cache, not a replica"). A warehouse that keeps every parcel but
may evict file content and trees, fetching them back on demand.

**Lazy history, remote-first reads, signed trust checkpoints.** Clones whose cost depends on the
tip rather than the history length; history, blame and audit run against a remote with no local
warehouse; and verifying from a signed snapshot of trust instead of replaying from genesis.

**Approval gating.** A signed merge policy that hauls must satisfy before they can merge.

**File-level access control.** Per-file grants that a remote enforces on lift and withholds on
fetch.

**First-class conflicts.** Conflicts that can be stacked and shared, rather than blocking `stack`.

**Locker.** A private, per-operator tracked namespace.

**Peer mode.** Warehouses syncing with each other directly, with a server as an always-on peer.
