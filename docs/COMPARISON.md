# Forklift vs. other version control systems

An honest comparison of **Forklift** against **Git** and the modern git-alternatives
**Jujutsu (jj)**, **Sapling (sl)**, and **Pijul**.

The goal of this document is a *clear picture*, not a sales pitch. Where Forklift is
genuinely differentiated, it says so; where Forklift is missing something the others
have, it says that too — plainly, and marked **[Forklift lacks this]**. Features that
are designed but **not yet implemented** are deliberately kept out of the main
comparison and collected in [§8](#8-forklift-features-not-yet-implemented-future-work)
so the present-day picture stays honest.

This page compares; it does not specify. Where it names a Forklift behaviour, it links to the
page that documents it, and that page is the authority. For what Forklift does that git leaves
to a host or to you, with runnable examples, see [Why Forklift](guide/concepts.md).

> **Reading note.** Forklift's terminology is warehouse-themed. The mapping to
> git words (warehouse = repo, parcel = commit, pallet = branch, load = add,
> stack = commit, consolidate = merge, lift/lower = push/pull, franchise = clone,
> haul = pull request) is in the [README](../README.md) and the
> [glossary](guide/glossary.md). This document uses Forklift's words for Forklift and
> each other tool's own words for that tool.

**Status of the systems compared** (Forklift as of 2026-10, the others as of mid-2026):

| System | Since | Author | Language | Data model | Maturity |
|--------|-------|--------|----------|------------|----------|
| **Git** | 2005 | Linus Torvalds et al. | C | Content-addressed snapshots | Dominant; enormous ecosystem |
| **Jujutsu (jj)** | 2020 | Martin von Zweigbergk / Google | Rust | Snapshots + first-class conflicts (Git-compatible backend) | Young, fast-moving, widely admired |
| **Sapling (sl)** | 2022 (open-sourced) | Meta | Rust + Python | Snapshots (Mercurial lineage), monorepo-scale | Production at Meta scale; OSS server story thin |
| **Pijul** | 2015 | Pierre-Étienne Meunier | Rust | **Patches** (sound patch algebra) | Niche, theoretically distinctive |
| **Forklift** | 2026 | Máté Kolonics / lonic-software | Rust | Content-addressed snapshots | **v0.3.x — brand new, tiny user base** |

Forklift is by far the youngest and least proven system here. That is the single most
important honest caveat, and it colors everything below: Git, jj, Sapling, and Pijul
all have real users, real repos, and battle-tested edge-case handling. Forklift has a
coherent design and a working implementation, but not the mileage.

---

## 1. Philosophy — the core bet each system makes

Every VCS is an answer to a question. The questions differ, and that is the most
useful lens.

- **Git** — *"How do a thousand kernel developers collaborate with no central server?"*
  Git's answer is a distributed, content-addressed snapshot store where every clone is
  a full copy. It won so completely that its assumptions ("a repo is a `.git` directory",
  "the server runs a process to negotiate packs") now feel like laws of nature rather
  than design choices. Git optimizes for **distributed, offline, snapshot-based
  history**, and leaves identity and collaboration metadata largely to hosting services
  (GitHub, GitLab) and to configuration each user assembles.

- **Jujutsu** — *"What if the working copy were just a commit, and you never feared
  rewriting history?"* jj keeps Git's object model (it uses Git as a storage backend)
  but rethinks the **user-facing model**: no staging area, the working copy is always a
  commit, conflicts are first-class values you can commit and resolve later, and every
  operation is undoable via an operation log. It is a UX and history-editing revolution
  on top of Git's data model.

- **Sapling** — *"How do you version a monorepo with tens of millions of files and not
  melt?"* Meta's answer is lazy everything: a virtual filesystem that fetches file
  content on demand, commit-graph algorithms that never walk the whole history, a
  smartlog view of your stack, and a server (Mononoke) built to serve that scale.
  Sapling optimizes for **scale and stacked-diff workflows**.

- **Pijul** — *"What if merging were mathematically sound?"* Pijul models a repository
  as a **set of patches** with a real algebra: independent patches commute, so merges
  are associative and cherry-picking doesn't create duplicate-commit headaches. It
  optimizes for **correctness of merging and change reordering**, a property snapshot
  systems fundamentally cannot offer.

- **Forklift** — *"What if a VCS were designed from day one for serverless hosting and
  for AI agents as first-class authors?"* Forklift keeps a Git-like snapshot model but
  shapes the **data format and protocol** so a repository can live on object storage
  (S3) with only thin verification functions in between — no stateful pack-negotiation
  server. On top of that it builds **cryptographic identity and tamper-evident history**
  *into the repository* (signed parcels, a genesis trust anchor, an in-repo user
  registry), makes **collaboration metadata a signed in-repo object** (reviews, pull
  requests, tags, provenance), and treats **AI agents as enrolled, signed, supervised
  operators**. Forklift optimizes for **serverless hosting economics + verifiable
  authorship in an AI-authored world**.

The one-line contrast: Git decentralized *storage*; jj rethought the *editing model*;
Sapling conquered *scale*; Pijul made *merging sound*; Forklift is going after
*serverless hosting* and *provable AI authorship*.

---

## 2. Git vs. Forklift (the full section)

Git has, by a vast margin, the most users, so it gets a dedicated head-to-head. Forklift
is deliberately close to Git in *shape* — a snapshot store, a staging area, cheap
branches, push/pull — so a Git user is productive quickly. Forklift accepts Git's names for
its everyday commands as hidden aliases (`add`, `commit`, `status`, `log`, `branch`,
`checkout`, `merge`, `clone`, `push`, `pull`, `stash` and more), and keeps `diff`, `restore`,
`tag`, `show` and `cherry-pick` under their Git names outright; see
[Coming from git?](guide/cli.md#coming-from-git). The differences are where it counts.

### 2.1 Data model & object store

| | Git | Forklift |
|---|-----|----------|
| Objects | blob / tree / commit / tag | blob / tree / **compact parcel** (commit), plus recipe and chunk objects for large files; tags are signed records on the `@tags` meta pallet, not an object type |
| Addressing | SHA-1 (SHA-256 opt-in, migration ongoing) | **Blake3** (fast, parallel, keyed-hashing capable) |
| Content model | Snapshots (trees), delta-compressed in packs | Snapshots (trees), delta-compressed in packs |
| On-disk compression | zlib; delta chains in packfiles | zstd; bounded delta chains in native packs |
| Staging area | The index (single file) | **Sharded per-directory inventory** ("the dock") |
| Large files | Whole objects, or the Git LFS add-on | **Native content-defined chunking** from 8 MiB |

Both are snapshot systems: identical content yields an identical hash and is stored
once (dedup). The meaningful divergences:

- **Blake3 vs SHA-1.** Forklift chose Blake3 for speed, parallelism (one large file hashes
  across cores), and the option of *keyed hashing* (relevant to the future hidden-files
  feature). Git's SHA-1 is a known weakness it is still migrating away from.
- **Sharded staging.** Git's index is one file per working tree. Forklift shards the staging
  area per directory, which lets commands touch only the directories they need (**bounded
  memory** is an explicit design principle) and underlies its parallel walks and its
  per-working-directory [bays](guide/cli.md#bay--parallel-working-directories).
- **Delta compression.** Both systems delta-compress packed history, and Forklift finds delta
  bases the way Git does, by walking history to pair each version of a file with the one
  before it. New objects stay loose until automatic or explicit
  [`store compact`](guide/cli.md#store-compact--pack-the-object-store); the measured result on git.git is
  in [the benchmark guide](BENCHMARK.md).
- **Large files.** Forklift chunks large files natively, with no pointer files and no second
  tool: an edit re-stores only the chunks it touched. See
  [Large files](guide/cli.md#large-files) for how they travel and their limits.

### 2.2 The staging area

Both keep a staging step (Git's index, Forklift's inventory/dock) — a deliberate
similarity, and a deliberate *difference* from jj and Sapling, which drop it. Forklift's
`load` = `git add`, `stack` = `git commit`. The behavioral wrinkle: Forklift's `commit`
alias runs `stack`, which commits what you've `load`ed — it does **not** stage-and-commit
in one step. Forklift stages whole files: there is no interactive hunk staging.
**[Forklift lacks `add -p` hunk staging.]**

### 2.3 Branching, merging, history editing

- **Branches.** Equivalent: Forklift pallets are mutable pointers to head parcels, cheap
  and atomic, like Git branches.
- **Merging.** [`consolidate`](guide/cli.md#consolidate--merge-con) is a three-way merge:
  already-up-to-date, fast-forward, or a merge against the common ancestor with diff3 conflict
  markers when the sides overlap. On par with Git merge for the common cases.
- **History tools Forklift has:**
  - [`cherry-pick`](guide/cli.md#cherry-pick--apply-one-parcels-change-onto-the-current-pallet-cp)
    — apply one parcel's change onto the current pallet as a new parcel. The original authors
    are preserved and the pick is freshly signed.
  - [`blame`](guide/cli.md#blame--who-wrote-each-line-bl-annotate) — attribute each line to
    the parcel that last changed it, along the first-parent chain (Git's
    `blame --first-parent`). Because authorship is signed, each line also carries the author's
    identity class, so blame can say whether an agent wrote it.
  - [`tag`](guide/cli.md#tag--signed-tags--releases) — admin-signed, immutable release tags.
    Git has signed tags too; Forklift's differ in that the signer is checked against the
    in-repo office (§2.4) rather than a keyring each reader assembles.
  - [`undo`](guide/cli.md#undo--reverse-the-last-operation) — a journaled undo that walks
    back `stack`, `consolidate`, `cherry-pick`, `shift` and pallet deletes one at a time.
    Simpler than choosing among Git's kinds of reset, but shallower than Git's reflog for
    arbitrary recovery.
  - [`deliver`](guide/cli.md#deliver--squash-a-draft-onto-a-target-keeping-the-trail-dv) —
    squash a draft pallet's checkpoint trail into one clean signed parcel *while keeping the
    trail* as signed evidence. Unlike `git rebase -i` squash, the trail is not destroyed;
    but it is narrower in scope.
- **What Git has and Forklift does not:** `rebase` (including interactive), `commit --amend`,
  `bisect`, and a reflog. **[Forklift lacks these.]** Rebase is declined on purpose: it
  rewrites signed history and needs a force-push, and Forklift's remotes accept only
  fast-forwards on a pallet (the one exception is a deliberate trust reset, re-genesis, of the
  office). `cherry-pick` plus `consolidate` cover moving changes between pallets. For anyone whose
  Git workflow is rebase-heavy, this is the most noticeable absence.

### 2.4 Identity, signing & trust — Forklift's clearest win over Git

This is where Forklift is *structurally* ahead, not just differently-shaped. The full model is
in [Trust and identity](guide/trust-and-identity.md).

| | Git | Forklift |
|---|-----|----------|
| Author identity | Freetext `name <email>`, trivially forgeable | Signed by an **enrolled key**; the history records an opaque operator id |
| Commit signing | Optional per author (GPG, SSH or X.509) | **Mandatory once you enroll** — a one-way door, no unsigned escape hatch |
| Who can contribute | Whoever the host lets push | On a trusted warehouse, **only holders of admitted keys** |
| User registry | None (hosts keep one outside the repo) | **In-repo `office`** — users, roles, keys, all signed |
| Offline verification | `git verify-commit` against a keyring you supply | [`audit`](guide/cli.md#audit--verify-signed-history-offline-a) — verifies the chain from a **genesis trust anchor** offline |
| Key management | Out of band | `office keygen` / `admit` / `authorize` / `rotate` / `retire`, with revocation decided by ancestry |
| Privacy | Name + email baked into immutable history forever | **Pseudonymous: history has no name or email fields**, only an operator id |

Git can sign, but which keys count is decided outside the repository, so a clone cannot tell
from the repository alone who was allowed to write. Forklift's trust model lives *in the
repository* and is verifiable by any clone with no server and no network. That is the part Git
cannot retrofit without an identity registry to verify signatures against. Forklift keeps no
names of its own, and an operator id is opaque unless you choose a revealing one: a hosting provider may map operator ids to names behind its own policy (see
[Profiles and pseudonymity](guide/trust-and-identity.md#11-profiles-and-pseudonymity)).

### 2.5 Collaboration metadata in the repo

Git has signed tags and `git notes`, but pull requests, reviews and approvals live in
**GitHub's or GitLab's database**: not in the repo, not portable, not signed, and left behind
if you change hosts. Forklift puts them *in the warehouse* as signed records on meta pallets:

- [`manifest`](guide/cli.md#manifest--signed-post-metadata-on-parcels-mf) — signed
  notes, approvals and **AI provenance** attached to parcels.
- [`kv`](guide/cli.md#kv--custom-metadata-on-parcels) — your own signed keys and values on
  any parcel (CI status, for example).
- [`tag`](guide/cli.md#tag--signed-tags--releases) — releases.
- [`haul`](guide/cli.md#haul--pull-requests-reviewable-merge-proposals) — pull requests,
  where **every review is signed** and names the head it reviewed, so an approval is
  forge-proof, never silently carries over to new work, and carries the reviewer's identity
  class (human vs. agent). Reviews are recorded but do not yet gate a merge (§8).

These sync with `lift`, `lower` and `franchise` like any other pallet and survive moving
hosts.

### 2.6 Remote & hosting architecture — Forklift's founding bet

Git's wire protocol needs a **stateful server process** to negotiate packs (compute what
to send by walking history and building a packfile on the fly). That is what makes
serverless Git hosting awkward.

Forklift's protocol is designed so the remote is **untrusted object storage + thin
verifier functions** (the [remote protocol](format/REMOTE_PROTOCOL.md) is the reference):

- The remote checks every object it accepts against its hash; the client checks everything
  it downloads by hash, and by signature in a warehouse with an office.
- Pallet refs move by **compare-and-swap**, fast-forward only — there is no force-push. The
  one sanctioned non-fast-forward is the office's after a deliberate trust reset (re-genesis).
- Two server **heads** implement it, and both ship:
  - **`forklift-server`** — a single self-hostable binary with multi-warehouse serving,
    per-operator tokens with pallet grants, GC and a Docker image. See
    [Running a server](SERVER.md).
  - **The AWS head** (`forklift-aws-lambda`) — API Gateway and two Lambda functions in front
    of S3 and DynamoDB. Object uploads and downloads go directly between the client and S3
    on presigned URLs instead of through the API (signatures, which are small, come through
    it); the functions verify what is uploaded and audit each ref update. You deploy it into your own AWS account; see
    [the deployment reference](DEPLOYMENT.md). Like the server, it takes per-operator tokens
    judged by the office's roles and pallet grants
    ([what the tokens control](DEPLOYMENT.md#what-the-bearer-tokens-do--and-do-not--control)).

There is no managed Forklift hosting service. **[Forklift has no hosted offering; you run the
server or deploy the AWS head yourself.]**

Against Git's ecosystem, this is lopsided the other way: GitHub, GitLab, Gitea, Bitbucket,
CI everywhere, IDE integration everywhere, an ocean of Stack Overflow answers. Forklift has
none of that yet. The bridge it offers is
[`git mirror`](guide/cli.md#git-mirror--keep-a-git-remote-in-step), which keeps a Git remote in step
with a Forklift remote, one way. **[Forklift's ecosystem is essentially nonexistent by
comparison — this is the honest, dominant practical gap.]**

### 2.7 Performance & concurrency

- **Parallelism.** Git parallelizes some operations by default, such as packing, and others,
  such as checkout, only behind a setting. Forklift is designed to use every core by default
  and spreads work across them wherever measurement shows it pays; some operations were
  measured and kept serial. [The parallelization plan](PARALLELIZATION_PLAN.md) lists each
  operation and its status.
- **Bounded memory.** Forklift makes "no operation needs RAM proportional to repo size"
  a design principle (streaming shards). Git is generally fine here too, but Forklift
  designs for it explicitly.
- **Caveat.** Git has 20 years of profiling and pathological-case hardening. Forklift has been
  measured against Git on git.git, where it is faster on some operations and slower on others
  ([the benchmark guide](BENCHMARK.md)), but not at monorepo scale.

### 2.8 AI-agent orientation

Git has stable machine formats for some commands (`--porcelain`, `--format`, `-z`), each its
own, and freetext authors. Forklift is built for machine consumers: global `--json` with one
versioned envelope, stable error and exit codes, structured `conflicts` output (base/ours/theirs
as content addresses), a built-in `forklift mcp` MCP server, agent identity classes with
supervisors, machine-authorship provenance, `deliver` for squash-with-evidence, `bay`s for
fleet-on-one-box, and optimistic lift so a fleet stacking to one pallet stops serializing.
This is covered in [§7](#7-the-ai-agent-dimension) and in
[the machine and agent interface](MACHINE_INTERFACE.md).

### 2.9 Git vs. Forklift — bottom line

Forklift is a bet that *identity, collaboration metadata, and hosting economics* belong
**inside** the VCS, and that AI authorship makes that urgent. Where it wins over Git today
is signed/verifiable in-repo trust, in-repo portable collaboration data, a storage-centric
remote protocol, and machine-first ergonomics. Where Git wins — and will for years — is
**maturity, ecosystem, tooling, proven scale, and the rest of the history-editing toolbox**
(rebase, amend, bisect, reflog, hunk staging). If you need those, or you need it to work
everywhere with everyone today, use Git.

---

## 3. Forklift vs. Jujutsu (jj)

jj is the most direct "modern VCS" comparison, and in several areas jj is ahead.

**Where jj is ahead of Forklift:**

- **First-class conflicts.** jj can *commit* a conflict, keep working, and resolve it
  later; operations are never blocked by an unresolved conflict. Forklift's conflicts
  block `stack` until resolved and loaded — the Git model. **[Forklift lacks committable
  first-class conflicts.]**
- **The operation log.** jj records *every* operation and can undo/redo any of them
  (`jj op undo`), including restoring the repo to any past operation state. Forklift's
  journaled `undo` covers `stack`, `consolidate`, `cherry-pick`, `shift` and pallet deletes,
  one step at a time, with no redo. jj's model is more general. **[Forklift's undo is
  narrower.]**
- **No staging area friction & the working-copy-as-commit model** make history editing
  effortless in jj; automatic rebase of descendant commits when you edit an ancestor is a
  standout. Forklift has no rebase. **[Forklift lacks this.]**
- **Stable change IDs + revsets.** jj gives every change a stable ID that survives
  rewrites, plus a `revset` query language. Forklift identifies parcels by hash, and its
  [`query`](guide/cli.md#query--filter-history-on-signed-identity-q) filters history by
  signed identity, provenance, tags and paths, but is not a revset algebra. **[Forklift lacks
  stable change IDs and revsets.]**

**Where Forklift is ahead of jj:**

- **Identity, signing & trust.** jj inherits Git's model — freetext authors, optional
  signing, no in-repo user registry. Forklift's signed office, genesis anchor, and offline
  `audit` have no jj equivalent.
- **Its own storage-centric protocol and server heads.** jj relies on Git remotes and Git
  hosting (its default backend *is* Git). Forklift has its own protocol, a self-hostable
  server and an AWS serverless head.
- **In-repo collaboration metadata** (haul/manifest/tags) and **AI-agent primitives**
  (identity classes, provenance, MCP, bays). jj has none of these as first-class features.
- **Zero-PII pseudonymous history** by design.

**Shared ground:** both are Rust, both are snapshot-based, both care about a better CLI
than Git's. The clean summary: **jj is the better *history-editing and local-workflow*
tool; Forklift is the better *trust, hosting, and AI-authorship* tool.** They optimize
different halves of the VCS.

---

## 4. Forklift vs. Sapling (sl)

Sapling's whole reason for being is **scale** — Meta's monorepo — and that is exactly
where Forklift is least proven.

**Where Sapling is ahead of Forklift:**

- **Monorepo scale, proven.** Sapling runs on repos with tens of millions of files at
  Meta. Forklift has been benchmarked on git.git, not on anything of that size.
  **[Forklift is unproven at scale.]**
- **Virtual filesystem / lazy fetch (EdenFS).** Sapling fetches file content on demand,
  so you can work in a giant repo without materializing it. Forklift has
  [sparse warehouses](guide/concepts.md#4-sparse-warehouses): a `franchise --only` fetches
  and materializes only the paths you ask for, and `expand` widens that later. But there is
  no virtual filesystem, and fetching history lazily, so a clone's cost is bounded by the tip
  rather than by history length, is still future work (§8). **[Forklift lacks a virtual
  filesystem.]**
- **Smartlog & stacked-diff workflow polish.** Sapling's `smartlog`, `absorb`, and
  stacked-PR review (ReviewStack) are mature and loved. Forklift's `deliver` is in the
  same spirit but far less developed. **[Forklift lacks smartlog/absorb-grade tooling.]**
- **Commit Cloud** — seamless cross-machine sync of in-progress work. Forklift has bays
  and remotes but nothing as frictionless. **[Forklift lacks this.]**

**Where Forklift is ahead of Sapling:**

- **Signed, verifiable, in-repo identity & trust** — Sapling, like Git and jj, has no
  in-repo cryptographic user registry or genesis-anchored offline audit.
- **Serverless-native hosting design.** Sapling's scale server (Mononoke) is a heavy
  stateful service; its open-source self-hosting story is thin. Forklift's remote is
  storage + thin verifiers, self-hostable or deployable on AWS today.
- **In-repo collaboration metadata and AI-agent primitives.**
- **A single self-contained binary** with no Python runtime; Sapling's client mixes
  Rust and Python.

**Shared ground:** both descend spiritually from the "improve on the incumbent" impulse,
both are Rust-forward, both care about large-scale performance. The summary: **Sapling is
the tool if your problem is *a huge monorepo today*; Forklift is the tool if your problem
is *verifiable authorship and serverless hosting* — and you can wait for the scale work.**

---

## 5. Forklift vs. Pijul

Pijul is the odd one out — the only **patch-based** system here — and the comparison is
mostly about data model.

**Where Pijul is ahead of Forklift:**

- **Sound merges (the whole point).** Pijul's patch algebra makes independent changes
  *commute*: merges are associative, and you get the same result regardless of the order
  changes are applied. Snapshot systems — Git, jj, Sapling, **and Forklift** — cannot
  offer this; merge results can depend on order and ancestry. **[Forklift, being
  snapshot-based, structurally lacks patch commutativity.]**
- **Clean cherry-picking & change reordering**, a direct consequence of the patch model.
  Forklift has `cherry-pick`, but as in Git a pick is a new parcel with its own hash, so the
  same change then exists twice in history; Pijul applies the same patch. **[Forklift lacks
  patch identity across picks.]**
- **First-class stored conflicts**, like jj. Forklift blocks on conflicts. **[Forklift
  lacks this.]**

**Where Forklift is ahead of Pijul:**

- **Familiarity & adoption path.** Forklift's snapshot model + Git-aliased commands make
  it immediately legible to Git users; Pijul's patch model is powerful but requires
  rethinking your mental model, which has limited its adoption.
- **Identity, signing & trust as a first-class subsystem.** Pijul signs patches and has
  keys, but nothing like Forklift's genesis anchor, in-repo office/role registry, agent
  identity classes, and offline chain audit.
- **Serverless-native hosting design.** Pijul has The Nest for hosting, but not Forklift's
  storage-centric architecture with object bytes on presigned URLs.
- **In-repo collaboration metadata (haul/manifest/tags) and the whole AI-agent track.**
- **Git interoperability** — Forklift ships
  [`git import`](guide/cli.md#git-import--migrate-a-git-repository),
  [`git export`](guide/cli.md#git-export--migrate-back-to-git) and `git mirror`. Pijul's Git
  interop is more limited.

**Shared ground:** both are Rust, both make conflicts and signing serious concerns, both
challenge Git's assumptions. The summary: **Pijul is the tool if *merge correctness and
change algebra* are your priority; Forklift keeps the snapshot model on purpose and spends
its novelty budget on *trust, hosting, and AI authorship* instead.**

---

## 6. Feature matrix

Legend: ✅ present · ⚠️ partial / weaker · ❌ absent · 🔭 **designed but not implemented**
(see [§8](#8-forklift-features-not-yet-implemented-future-work)).

| Capability | Git | jj | Sapling | Pijul | **Forklift** |
|---|:--:|:--:|:--:|:--:|:--:|
| Snapshot data model | ✅ | ✅ | ✅ | ❌ (patches) | ✅ |
| Patch commutativity / sound merges | ❌ | ❌ | ❌ | ✅ | ❌ |
| Staging area | ✅ | ❌ | ❌ | ⚠️ | ✅ (sharded) |
| Cheap branches | ✅ | ✅ | ✅ | ✅ | ✅ |
| Three-way merge | ✅ | ✅ | ✅ | ✅ | ✅ |
| First-class / committable conflicts | ❌ | ✅ | ⚠️ | ✅ | 🔭 (parked) |
| Rebase | ✅ | ✅ (auto) | ✅ | n/a | ❌ (declined) |
| Cherry-pick | ✅ | ✅ | ✅ | ✅ | ✅ |
| Interactive history editing | ✅ | ✅ | ✅ | ⚠️ | ⚠️ (`deliver`) |
| Universal undo / operation log | ⚠️ (reflog) | ✅ | ✅ | ⚠️ | ⚠️ (journaled `undo`) |
| Stable change IDs / revsets | ❌ | ✅ | ⚠️ | ⚠️ | ⚠️ (`query`, no change IDs) |
| Bisect | ✅ | ⚠️ | ✅ | ❌ | 🔭 |
| Blame / annotate | ✅ | ✅ | ✅ | ✅ | ✅ |
| Tags | ✅ | ✅ | ✅ | ⚠️ | ✅ (signed) |
| Hunk-level staging (`add -p`) | ✅ | ✅ | ✅ | ✅ | 🔭 |
| Parallel core ops | ⚠️ | ⚠️ | ✅ | ⚠️ | ✅ |
| Lazy / virtual filesystem checkout | ❌ | ❌ | ✅ | ⚠️ | ❌ |
| Partial / sparse workspace | ⚠️ (two features) | ⚠️ | ✅ | ✅ | ✅ |
| Large-file handling | ⚠️ (LFS add-on) | ⚠️ | ✅ | ⚠️ | ✅ (native chunking) |
| Delta compression | ✅ | ✅ | ✅ | ✅ | ✅ |
| Parallel working dirs (worktrees) | ✅ | ✅ | ✅ | ⚠️ | ✅ (bays) |
| Mandatory signed history | ❌ | ❌ | ❌ | ⚠️ | ✅ (once enrolled) |
| In-repo user/key registry | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| Genesis trust anchor + offline audit | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| Pseudonymous / zero-PII history | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| In-repo collaboration metadata (PRs/reviews) | ⚠️ (notes) | ❌ | ❌ | ⚠️ | ✅ |
| AI-agent identity classes + provenance | ❌ | ❌ | ❌ | ❌ | ✅ |
| Machine-first CLI (`--json`, stable errors) | ⚠️ | ⚠️ | ⚠️ | ⚠️ | ✅ |
| Built-in MCP server | ❌ | ❌ | ❌ | ❌ | ✅ |
| Serverless-native remote protocol | ❌ | ❌ | ❌ | ❌ | ✅ |
| Serverless deployment | ❌ | via Git | ❌ | ❌ | ✅ (AWS head, self-deployed) |
| Managed hosting | ✅ (huge) | via Git | ⚠️ (Mononoke) | ✅ (The Nest) | ❌ |
| Git interop (import & export) | n/a | ✅ (native) | ✅ | ⚠️ | ✅ (import, export, mirror) |
| Ecosystem / IDE / CI integration | ✅✅✅ | ⚠️ (growing) | ⚠️ | ❌ | ❌ |
| Proven at massive scale | ✅ | ⚠️ | ✅ | ❌ | ❌ |
| Years in production | ~20 | ~4 | production @ Meta | ~10 (niche) | **<1** |

The matrix is intentionally unflattering where it should be: Forklift's ✅ column is real
and distinctive (trust, in-repo collaboration, AI agents, serverless protocol), but its
❌ and 🔭 rows are real too, and its **ecosystem and scale rows are its weakest** — the
things that take *time and users*, not code, to earn.

---

## 7. The AI-agent dimension

This is the axis on which Forklift is most differentiated from *all four* alternatives,
because none of them were designed for it. Forklift's bet is that AI coding agents are
becoming primary consumers of version control, which changes five things:

1. **Authorship becomes a compliance question** — *what* wrote this, under *whose*
   authority. Forklift answers it cryptographically: agents are enrolled operators with an
   **identity class** (human / agent / bot / service) and a **supervisor**, every parcel
   is signed by the agent's own key, and `query --class agent` answers "which parcels
   did agents write, under whose supervision" from the verified signer, offline.
   A human's key can be
   [passphrase-protected](guide/trust-and-identity.md#6-passphrase-protected-keys), so an
   unattended agent with no terminal cannot sign with it (opt-in, with the limits that page
   states).
2. **The CLI's consumer is a program on a token budget** — Forklift has global `--json`,
   stable error/exit codes, token-cheap summaries, and a built-in `forklift mcp` MCP
   server so agents call typed tools instead of scraping output.
3. **Concurrency goes fleet-size** — `bay`s give N agents parallel working directories on
   one machine sharing one object store; [optimistic lift](guide/cli.md#lift--push-li)
   auto-merges concurrent pushes that merge cleanly, so a fleet stacking to one pallet stops
   serializing through a human.
4. **History splits into a noisy trail and a clean story** — `deliver` squashes an agent's
   checkpoint trail into one clean signed parcel *while keeping the trail as signed
   evidence*.
5. **Provenance is evidence** — `manifest provenance` records model / tool / session as
   *signed* metadata, answering EU-AI-Act-shaped "which model produced this change"
   questions offline.

Git, jj, Sapling, and Pijul treat all of this as out of scope. This is the clearest place
Forklift is doing something the others structurally are not — though it is worth being
honest that it is also a **bet on a future** that has not fully arrived.

---

## 8. Forklift features **not yet implemented** (future work)

> Everything in this section is **designed or planned but not shipped** as of 2026-10. It is
> listed separately, on purpose, so the rest of this document reflects only what Forklift can
> actually do today. [The design document](DESIGN.html) is the roadmap and holds each item's
> current status; this list is a summary of it and loses to it where they differ.

**Hosting & scale:**

- 🔭 **Lazy history** — fetch parcels and trees on demand, so clone cost is bounded by the tip
  rather than by history length, as part of making a warehouse a bounded cache over its remote.
  *Planned.*
- 🔭 **Server metrics** (bytes and objects transferred, CAS conflicts, audit failures) and a
  **bundle auto-rebuild policy**. *Planned* (logs and a health check have shipped).

**Working-copy & history tools:**

- 🔭 **First-class, committable conflicts** — a conflicting merge stacked as a signed parcel
  and resolved later, as jj does. *Designed, parked until there is demand.*
- 🔭 **`bisect`** and **hunk-level partial staging**. *Planned.*
- 🔭 **Per-file privileges / hidden files** — files some users cannot even see, enforced by the
  remote. The hardest feature in the backlog. *Needs design.*
- 🔭 **Per-parent conflict stages** for merges with more than two parents. *Needs design.*

**Collaboration & identity (provider layer):**

- 🔭 **Org-tier chains & delegation hierarchy** — per-org signed chains above the warehouse
  office. *In progress*: pseudonymous chains, sigchain endorsements, proof-of-possession and
  admin-authorized keys, re-genesis, revocation records and the hook protocol have shipped;
  org-tier chains and embedded-key offline audit remain.
- 🔭 **Web-UI signing** — producing signed content from a browser "merge" button without
  the provider being able to forge. *Future.*
- 🔭 **Cross-warehouse forks & approval-gated merges for `haul`** — hauls merge within one
  warehouse today, and reviews are recorded without gating the merge. *Future.*

**Agent track remainders:**

- 🔭 **`events --follow` event stream** — so fleet members react to each other's stacks
  instead of polling. *Deferred* pending an internal event-stream refactor.
- 🔭 **Auto-checkpoint mode** for draft pallets. *Planned.*
- 🔭 **Full transcript stored as a blob-in-tree** for provenance (regulated shops).
  *Future* (a transcript hash already ships).

**Declined:** `rebase` and interactive history rewriting are not planned. They rewrite signed
history and need a force-push; `cherry-pick` and `consolidate` cover moving changes between
pallets instead.

---

## 9. Summary — when to reach for which

- **Use Git** if you need it to work everywhere, with everyone, today — mature tooling,
  vast ecosystem, and a full history-editing toolbox (rebase, amend, bisect, reflog).
  The safe default for almost everyone.
- **Use Jujutsu** if you want the best *local workflow and history-editing* experience —
  first-class conflicts, universal undo, effortless rewriting — while staying
  Git-compatible.
- **Use Sapling** if your problem is *a genuinely huge monorepo* and you want lazy
  checkout and stacked-diff tooling that is proven at scale.
- **Use Pijul** if *merge correctness and change algebra* matter more to you than
  ecosystem — sound, commutative merges snapshot systems can't offer.
- **Use Forklift** if you care about **verifiable, signed, in-repo identity and
  collaboration data**, a **serverless-native hosting model**, and **first-class AI-agent
  authorship** — and you can accept that it is **brand new, unproven at scale, lacks a
  large ecosystem, has no managed hosting, and has no rebase**. Forklift is the most
  opinionated bet in this list, aimed at where version control might be going rather than
  where it has been.

The honest one-sentence verdict: **Forklift has a genuinely distinctive and coherent
position — trust, serverless hosting, and AI authorship built into the VCS, each with a shipped
core and the remainders listed in §8 — but it is years behind Git, jj, Sapling, and Pijul on maturity, ecosystem and proof at scale, and
it deliberately leaves out the history-rewriting half of Git's toolbox.**

---

*This comparison reflects Forklift at v0.3.x (2026-10). For the authoritative, dated
status of every Forklift feature, see [`docs/DESIGN.html`](DESIGN.html). Descriptions of
Git, Jujutsu, Sapling, and Pijul reflect those projects as of mid-2026 and are necessarily
a snapshot of fast-moving software — corrections welcome.*
