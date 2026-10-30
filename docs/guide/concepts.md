# Why Forklift: what it does that git does not

Forklift keeps git's model of history (content-addressed snapshots, branches as movable pointers,
three-way merges), so most of what you know from git carries over. This page is about the rest:
what git leaves to a hosting service, to configuration you assemble yourself, or to you. Each
section names the problem, shows how Forklift handles it, and links to the reference that covers
it in full.

For a side-by-side comparison with git, jujutsu, sapling and pijul, see
[the VCS comparison](../COMPARISON.md). For a first walk through the commands, start with
[the tutorial](tutorial.md).

The examples build one small warehouse from section to section. As in the tutorial, each `$` line
is a command, the lines under it are what forklift prints, and a `…` stands for text that differs
on your machine. The test suite runs this page (`crates/forklift/tests/guide_pages.rs`), so the
examples cannot drift from the CLI.

```console
$ mkdir shop
$ cd shop
$ forklift prepare
Prepared warehouse.
$ mkdir src
$ echo 'fn main() {}' > src/main.rs
$ mkdir docs
$ echo '# Shop' > docs/readme.md
$ forklift load .
$ forklift stack -m "First parcel"
Stacked parcel … on pallet "main".
```

## 1. Signed, tamper-evident history

**The problem.** git can sign a commit, but whether to sign is up to each author, and which keys
count is decided by a keyring outside the repository. A clone cannot tell from the repository
alone who was allowed to write, so it cannot check that a server, or anyone with write access,
did not rewrite history.

**How Forklift handles it.** `office enroll` starts the warehouse's **office**: the signed record
of its operators, their keys and their roles, kept in the warehouse itself. Its first parcel, the
**genesis**, is the trust anchor. From then on every parcel stacked in the warehouse must be
signed, and that cannot be turned off.

```console
$ forklift office enroll
Enrolled "…" and established trust.
Genesis parcel:    …
Identity root key: …

From now on every parcel stacked in this warehouse must be signed. This cannot be undone (a lockout is handled by archiving and re-preparing the warehouse).
$ echo 'Prices in euros.' > docs/pricing.md
$ forklift load docs
$ forklift stack -m "Add pricing"
Stacked parcel … on pallet "main".
```

`audit` verifies the history offline, against the anchor: the office chain forward from the
genesis, then every parcel of the pallet. The parcel stacked before enrolling stays as it was,
unsigned:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 1 signed parcel(s) valid, 1 legacy parcel(s) predate trust and are unsigned.
```

A stripped or forged signature, a key the office does not know, or an office chain that does not
reach the genesis fails the audit with a non-zero exit. A server that holds an anchor checks the
signatures of what is pushed to it too, but your own `audit` is the check that counts.

Two more properties follow from keeping trust in the warehouse:

- **First contact is explicit.** A clone adopts the remote's anchor the first time it meets it, as
  SSH adopts a host key, and warns you to check the genesis out of band. `--expect <genesis>`
  checks it before anything is fetched. You will see that warning in
  [section 4](#4-sparse-warehouses).
- **Revocation is decided by ancestry, not timestamps.** Retiring a key records which history it
  signed while it was valid, as the set of pallet heads at that moment. A signature by the
  retired key is valid only on parcels reachable from those heads, so a clock set back cannot
  make a later signature look earlier.

Reference: [Trust and identity](trust-and-identity.md), and
[`audit`](cli.md#audit--verify-signed-history-offline).

## 2. Pseudonymous identity

**The problem.** A git commit carries a name and an email address, in plain text, forever. Who an
author really is cannot be checked, and the personal data cannot be removed from history.

**How Forklift handles it.** History records an opaque **operator id** and a public key, nothing
else. You enrolled above with an id minted for you; `office` shows it with your role and key:

```console
$ forklift office
… — admin
  key … (active, on this machine, identity root)
```

The id is a pseudonym. A hosting provider can map ids to names behind its own policy, but no name
or address is written into history. One machine can act as different operators in different
warehouses through **profiles**.

Each operator also has an **identity class**, fixed when it is admitted: a human, an AI agent, a
bot or a service. An agent must name a supervising human. Because the class is part of the signed
office record, history can answer "was this written by an agent, and under whose supervision", and
that answer is as hard to forge as the signature itself.

Reference: [Agents, bots, and services](trust-and-identity.md#7-agents-bots-and-services) and
[Profiles and pseudonymity](trust-and-identity.md#11-profiles-and-pseudonymity).

## 3. Collaboration metadata in the repository

**The problem.** git can sign a tag and attach a note to a commit, but reviews, approvals, pull
requests and CI status live in a hosting service's database. They are not signed, they are not in
your clone, and they stay behind if you move.

**How Forklift handles it.** They live in the warehouse, on **meta pallets**: `@manifest` for
approvals, review notes and AI provenance, `@kv` for your own keys and values on any parcel,
`@tags` for releases, and `@haul` for pull requests. Each entry is signed by whoever wrote it, so
authorship is the signature, not a field anyone can type:

```console
$ forklift manifest set main ci green
Set ci on parcel ….
$ forklift manifest get main ci
green
$ forklift manifest approve main -m "Ready to ship"
Recorded approval of parcel … by "…". "Ready to ship"
$ forklift tag create v0.1.0 -m "First release"
Created tag "v0.1.0" -> … (signed; @tags head …).
$ forklift tag
v0.1.0  …  by … (admin)  First release
```

Meta pallets travel with `lift`, `lower` and `clone`, and `history @kv` (or `@manifest`,
`@tags`, `@haul`) is the audit trail of every change to one. When two people change one
concurrently, `lower` replays your changes onto the remote's instead of merging, since the
entries are independent signed records; only two writes to the same entry conflict, and
`lower --keep` settles which one stays.

Reference: [`manifest`](cli.md#manifest--signed-post-metadata-on-parcels),
[`manifest set`](cli.md#manifest-set--keyed-values-on-parcels), [`tag`](cli.md#tag--signed-tags--releases) and
[`haul`](cli.md#haul--pull-requests-reviewable-merge-proposals).

## 4. Sparse warehouses

**The problem.** A git clone downloads every file of every version unless you combine partial
clone, for what is downloaded, with sparse-checkout, for what is on disk. They are separate
features with separate commands, and a partial clone's filter usually selects by object type or
size rather than by the paths you work on.

**How Forklift handles it.** Sparseness is one concept across the commands: a sparse warehouse
fetches only the paths you ask for, and everything else stays **sealed by hash**, pinned to the
content the signed parcels commit to and fetched later only on request. `audit`, `scope` and
`scope fetch` all know where the boundary is.

To try it, share the warehouse through a local server. Prepare an empty warehouse for it to serve:

```console
$ cd ..
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Start the server in a **second terminal**, in the same folder, and leave it running:

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --open
forklift-server listening on http://127.0.0.1:7171
```

Back in the first terminal, lift the warehouse, then clone only `src`:

```console
$ cd shop
$ forklift config set remote.url http://127.0.0.1:7171
$ forklift lift
Lifted the office pallet: 1 new parcel(s).
Lifted the @kv pallet.
Lifted the @manifest pallet.
Lifted the @tags pallet.
Lifted pallet "main" to …: 2 new parcel(s), … object(s) and 1 signature(s) uploaded.
$ cd ..
$ forklift clone http://127.0.0.1:7171 slim --only src
Warning: FIRST CONTACT with http://127.0.0.1:7171: adopted its trust anchor, genesis …. Verify that genesis out of band. To check it before anything is fetched, pass --expect … to clone or lower.
Adopted the remote's trust anchor (genesis …); every parcel is signed from now on.
Adopted the @kv pallet from the remote.
Adopted the @manifest pallet from the remote.
Adopted the @tags pallet from the remote.
Cloned http://127.0.0.1:7171 into "slim": pallet "main" at … (… object(s) fetched loose).
Sparse: fetched and materialized only src. The rest of the tree is sealed by hash — widen with "forklift scope fetch".
```

There are two scopes. The **fetch scope** is what the warehouse has downloaded; the
**checkout scope** is what one checkout shows on disk. Here they are the same:

```console
$ cd slim
$ forklift scope
the main tree is scoped (sparse). Checkout scope:
  src
  .forkliftignore (always in scope)
Warehouse fetch scope:
  src
  .forkliftignore (always in scope)
```

A sparse warehouse still audits in full: every signature is checked, and the sealed content is
pinned by the hashes those signatures cover:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 1 signed parcel(s) valid, 1 legacy parcel(s) predate trust and are unsigned.
This warehouse is sparse; content outside the fetched scope (src) is sealed by hash, not downloaded.
Signatures are verified in full; within the fetched scope every tree was re-hashed and every blob and chunk confirmed present.
Out-of-scope content is pinned to the hash a signed parcel commits — it cannot be forged or substituted, only fetched. The boundary is advisory, not enforced by the remote.
```

`scope fetch` widens the fetch scope later, checking what it downloads against those seals.
`scope narrow` shrinks what one checkout shows, and `scope prune` forgets a path warehouse-wide to
reclaim the disk:

```console
$ forklift scope fetch docs
Widened the fetch scope with docs (… object(s) fetched). Fetch scope now: docs, src.
Scope a bay to a newly fetched path with "forklift bay add --scope".
$ forklift show main:docs/pricing.md
Prices in euros.
```

Reference: [`clone`](cli.md#clone--clone-a-remote-warehouse),
[`scope fetch`](cli.md#scope-fetch--widen-a-sparse-warehouses-fetch-scope),
[`scope narrow`](cli.md#scope-narrow--shrink-this-checkouts-scope),
[`scope prune`](cli.md#scope-prune--reclaim-disk-from-a-sparse-warehouse) and
[scoped bays](cli.md#bay-add---scope--a-scoped-sparse-bay).

## 5. Built for serverless hosting

**The problem.** A git remote that accepts pushes runs git on the server: a process that
negotiates which objects to send and builds a pack for each fetch. Hosting it means running and
scaling those processes.

**How Forklift handles it.** A Forklift remote is storage plus thin verifiers. It checks every
object it accepts against its hash. The client checks everything it downloads by hash, and
in a warehouse with an office, every parcel since the anchor by signature too, so forged history
is caught as well as altered content. What you still rely on a
remote for is availability and freshness: it could withhold data or keep serving an older signed
head, and at first contact you verify its genesis yourself. That is what lets it be object storage
behind small functions.

There are two server **heads** behind one protocol:

- **`forklift-server`**, the one you ran above: a single self-hosted binary, for your own machine
  or a container. See [Running a server](../SERVER.md).
- **The AWS head** (`forklift-aws-lambda`): API Gateway and two Lambda functions in front of S3
  and DynamoDB. Uploads and downloads of objects go directly between the client and S3 on
  presigned URLs instead of through the API; the functions read an upload only to check its
  hash. Signatures, which are small, come through the API. See
  [the deployment reference](../DEPLOYMENT.md).

Reference: [Working with remotes](cli.md#6-working-with-remotes) and the
[remote protocol](../format/REMOTE_PROTOCOL.md).

## 6. Durability you can reason about

**The problem.** When a git command is killed part-way, cleaning up is left to you: a
half-finished operation has to be untangled by reading what it left behind.

**How Forklift handles it.** Forklift keeps one rule: anything another command can see is already
on disk durably. If a write fails in the one window where that could break, Forklift records what
is affected, and every command refuses with a coded error until `heal` resolves it. The next
command usually resolves it on its own. An operation that is interrupted part-way (a merge, a
shift, a park) is likewise recorded, and `heal` finishes or reverses it. On a healthy warehouse
there is nothing to do:

```console
$ forklift heal
Nothing to heal — no durability taint is standing.
```

Locks are the exception. Like git's `index.lock`, most of Forklift's locks are files, and a
command that is killed hard leaves its lock behind. The next command refuses, usually naming the
process that held it, and you remove the lock once that process is gone. A hard-killed
`forklift-server` leaves its serve lock the same way; see [Running a server](../SERVER.md).

Reference: [`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation) and
[Interrupted operations](cli.md#interrupted-operations).

## 7. Parallel by default

**The problem.** git parallelizes some operations by default, such as packing, and others, such
as checkout, only behind a setting. Work done one file at a time leaves most of a modern machine
idle.

**How Forklift handles it.** Forklift is designed to use every core by default, and spreads work
across them wherever measurement shows it pays: hashing the working tree for `stocktake` and
building the inventory, for example. Not every operation gains from it, and some were measured
and kept serial. [The parallelization plan](../PARALLELIZATION_PLAN.md) lists every operation,
whether it runs in parallel today, and why.

## 8. A first-class machine interface

**The problem.** git has stable machine formats for some commands (`--porcelain`,
`--format`, `-z`), each its own. Errors are messages written for people, and an agent has no
description of the command surface to work from.

**How Forklift handles it.** Every command takes `--json` and prints the same versioned envelope:

```console
$ forklift stocktake --json
{
  "lonic_json": 1,
  "tool": "forklift",
  "schema": …,
  "command": "stocktake",
  "ok": true,
  "data": {
    "head": "…",
    "pallet": "main",
    "staged_count": 0,
    "summary": false,
    "unstaged_count": 0
  }
}
```

A failure carries a stable error code and a fixed exit code, so a script can branch without reading
the message. `self schema` describes the whole command surface, and `forklift mcp` serves it to AI
agents as native tools.

Reference: [the machine and agent interface](../MACHINE_INTERFACE.md),
[Machine & agent output](cli.md#8-machine--agent-output) and
[the exit codes](../generated/errors.md).

## 9. What is not there yet

Forklift is young, and some of what it is designed for is not shipped yet. Each gap below links to
the page that describes the shipped behaviour:

- Hauls merge within one warehouse, and reviews are recorded but do not gate a merge: see
  [`haul`](cli.md#haul--pull-requests-reviewable-merge-proposals).
- File-level access control is designed but not built: see
  [Keeping these guides current](README.md#keeping-these-guides-current).

[The design document](../DESIGN.html) is the roadmap for all of them.
