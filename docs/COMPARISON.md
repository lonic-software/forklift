# Forklift vs. other version control systems

An honest comparison of **Forklift** against **Git** and the modern git-alternatives
**Jujutsu (jj)**, **Sapling (sl)**, and **Pijul**.

The goal of this document is a *clear picture*, not a sales pitch. Where Forklift is
genuinely differentiated, it says so; where Forklift is missing something the others
have, it says that too — plainly, and marked **[Forklift lacks this]**. Features that
are designed but **not yet implemented** are deliberately kept out of the main
comparison and collected in [§8](#8-forklift-features-not-yet-implemented-future-work)
so the present-day picture stays honest.

> **Reading note.** Forklift's terminology is warehouse-themed. The mapping to
> git words (warehouse = repo, parcel = commit, pallet = branch, load = add,
> stack = commit, consolidate = merge, lift/lower = push/pull, franchise = clone,
> haul = pull request) is in the [README](../README.md) and
> [CLI guide](guide/cli.md). This document uses Forklift's words for Forklift and
> each other tool's own words for that tool.

**Status of the systems compared (as of 2026-07):**

| System | Since | Author | Language | Data model | Maturity |
|--------|-------|--------|----------|------------|----------|
| **Git** | 2005 | Linus Torvalds et al. | C | Content-addressed snapshots | Dominant; enormous ecosystem |
| **Jujutsu (jj)** | 2020 | Martin von Zweigbergk / Google | Rust | Snapshots + first-class conflicts (Git-compatible backend) | Young, fast-moving, widely admired |
| **Sapling (sl)** | 2022 (open-sourced) | Meta | Rust + Python | Snapshots (Mercurial lineage), monorepo-scale | Production at Meta scale; OSS server story thin |
| **Pijul** | 2015 | Pierre-Étienne Meunier | Rust | **Patches** (sound patch algebra) | Niche, theoretically distinctive |
| **Forklift** | 2026 | Máté Kolonics / lonic-software | Rust | Content-addressed snapshots | **v0.1.x — brand new, tiny user base** |

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
  history**, and treats identity, collaboration metadata, and hosting as *someone else's
  problem* (yours, or GitHub's).

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
  requests, provenance), and treats **AI agents as enrolled, signed, supervised
  operators**. Forklift optimizes for **serverless hosting economics + verifiable
  authorship in an AI-authored world**.

The one-line contrast: Git decentralized *storage*; jj rethought the *editing model*;
Sapling conquered *scale*; Pijul made *merging sound*; Forklift is going after
*serverless hosting* and *provable AI authorship*.

---

## 2. Git vs. Forklift (the full section)

Git has, by a vast margin, the most users, so it gets a dedicated head-to-head. Forklift
is deliberately close to Git in *shape* — a snapshot store, a staging area, cheap
branches, push/pull — so a Git user is productive quickly (Forklift even ships Git
command names as hidden aliases: `add`, `commit`, `status`, `log`, `branch`,
`checkout`, `merge`, `clone`, `push`, `pull`, `stash`). The differences are where it
counts.

### 2.1 Data model & object store

| | Git | Forklift |
|---|-----|----------|
| Objects | blob / tree / commit / tag | blob / tree / **compact parcel** (commit); no tag object yet |
| Addressing | SHA-1 (SHA-256 opt-in, migration ongoing) | **Blake3** (fast, parallel, keyed-hashing capable) |
| Content model | Snapshots (trees), delta-compressed in packs | Snapshots (trees), zstd full/delta records in native packs |
| On-disk compression | zlib; delta chains in packfiles | zstd; bounded delta chains in native packs and whole-clone bundles |
| Staging area | The index (single file) | **Sharded per-directory inventory** ("the dock") |
| Hashing large files | Single-threaded | **Parallel — Blake3 parallelizes one big file across cores** |

Both are snapshot systems: identical content yields an identical hash and is stored
once (dedup). The meaningful divergences:

- **Blake3 vs SHA-1.** Forklift chose Blake3 for speed, parallelism, and the option of
  *keyed hashing* (relevant to the future hidden-files feature). Git's SHA-1 is a known
  weakness it is still migrating away from.
- **Sharded staging.** Git's index is one file the whole repo shares — the source of
  `git worktree`'s sharp edges. Forklift shards the staging area per directory, which is
  what lets commands touch only the directories they need (**bounded memory** is an
  explicit design principle) and is the foundation of Forklift's parallel walks and its
  per-working-directory "bays".
- **Delta compression.** Both systems delta-compress packed history. Git uses its pack delta
  machinery; Forklift uses zstd dictionary frames against similar objects, with bounded chains.
  Whole-clone bundles now carry Forklift's native pack/index pairs directly, while newly authored
  objects remain loose until automatic or explicit compaction.

### 2.2 The staging area

Both keep a staging step (Git's index, Forklift's inventory/dock) — a deliberate
similarity, and a deliberate *difference* from jj and Sapling, which drop it. Forklift's
`load` = `git add`, `stack` = `git commit`. The behavioral wrinkle: Forklift's `commit`
alias runs `stack`, which commits what you've `load`ed — it does **not** stage-and-commit
in one step. If you value `git add -p`-style hunk staging, note Forklift stages at file
granularity today (no interactive hunk staging). **[Forklift lacks `add -p` hunk staging.]**

### 2.3 Branching, merging, history editing

- **Branches.** Equivalent: Forklift pallets are mutable pointers to head parcels, cheap
  and atomic, exactly like Git branches.
- **Merging.** Forklift `consolidate` is a real three-way merge (common-ancestor BFS,
  exact whitespace-preserving LCS diff3, clean fast-forward when possible, diff3 conflict
  markers when not). On par with Git merge for the common cases.
- **History editing — the big Git-side gap in Forklift.** Git has `rebase` (including
  interactive), `cherry-pick`, `commit --amend`, `bisect`, `blame`, `reflog`, and tags.
  Forklift has:
  - `undo` — a **journaled, universal undo** that reverses the last `stack`,
    `consolidate` (merges included), or `shift`. Cleaner than Git's five kinds of reset
    for the last operation, but shallower than Git's reflog for arbitrary recovery.
  - `deliver` — squash a draft pallet's checkpoint trail into one clean signed parcel
    *while keeping the trail* as signed evidence. This is nicer than `git rebase -i`
    squash (which destroys the trail), but narrower in scope.
  - **No `rebase`, no `cherry-pick`, no `bisect`, no `blame`/annotate, no tags, no
    interactive history rewriting.** **[Forklift lacks all of these.]** For anyone whose
    Git workflow is rebase-heavy, this is the most noticeable absence.

### 2.4 Identity, signing & trust — Forklift's clearest win over Git

This is where Forklift is *structurally* ahead, not just differently-shaped.

| | Git | Forklift |
|---|-----|----------|
| Author identity | Freetext `name <email>`, trivially forgeable | Signed by an **enrolled key**; the operator id is on-chain |
| Commit signing | Optional GPG/SSH; almost nobody configures it | **Mandatory once you enroll** — a one-way door, no unsigned escape hatch |
| Who can contribute | Anyone who can write to the repo | On a trusted warehouse, **only holders of admitted keys** |
| User registry | None (GitHub bolts one on outside the repo) | **In-repo `office`** — users, roles, keys, all signed |
| Offline verification | `git verify-commit` if signed, no registry to check against | `audit` — verifies the whole chain from a **genesis trust anchor** offline |
| Key management | Out of band | `office keygen` / `admit` / `rotate` / `retire` / `authorize`, with rotation continuity proofs and revocation records |
| Privacy | Name + email baked into immutable history forever | **Pseudonymous by default, zero PII on-chain**; GDPR-style name erasure is possible |

Git's trust model is "trust the server's account system." Forklift's trust model lives
*in the repository* and is verifiable by any clone with no server and no network. This is
the feature Git structurally cannot retrofit, because it has no identity registry to
verify signatures against.

### 2.5 Collaboration metadata in the repo

In Git, pull requests, reviews, and approvals live in **GitHub's/GitLab's database** —
not in the repo, not portable, not signed, lost if you change hosts. Forklift puts them
*in the warehouse* as signed, append-only objects on dedicated meta-pallets:

- `manifest` — signed post-metadata attached to parcels (notes, approvals, **provenance**).
- `haul` — pull requests as an append-only log of signed events (Opened / Pushed /
  Comment / Review / Merged / Closed), where **every review is signed**, so an approval
  is forge-proof and even carries the reviewer's identity class (human vs. agent).

These sync across remotes like any other object and survive moving hosts. Git has
nothing equivalent in-repo; the closest is under-used `git notes`.

### 2.6 Remote & hosting architecture — Forklift's founding bet

Git's wire protocol needs a **stateful server process** to negotiate packs (compute what
to send by walking history and building a delta-compressed packfile on the fly). That is
precisely what makes serverless Git hosting awkward.

Forklift's protocol is designed so the remote is **untrusted object storage + thin
verifier functions**:

- The control plane exchanges hash lists and (in the hosted design) presigned URLs;
  **object bytes move directly between client and storage**, never through a compute layer.
- Refs update via **compare-and-swap** (fast-forward only, no force-push) — the single
  point of mutable consistency.
- Everything a client downloads is **verifiable offline** (content addressing +
  signatures); everything it uploads is **unverified until the server re-hashes it**.
- A self-hostable server head (`forklift-server`, shipped) already implements all of
  this — hash verification before an object is fetchable, CAS refs, full signature audit
  on trusted warehouses, multi-warehouse serving, GC, a Docker image.

The AWS-serverless head (Lambda + S3 + DynamoDB) that motivated the whole project is
**designed but not yet built** ([§8](#8-forklift-features-not-yet-implemented-future-work)).
So today Forklift can be self-hosted, but the marquee "runs on S3 with no server"
deployment does not exist yet. **[Forklift's serverless hosting is designed, not shipped.]**

Against Git's ecosystem, this is lopsided the other way: GitHub, GitLab, Gitea, Bitbucket,
CI everywhere, IDE integration everywhere, `git-lfs`, an ocean of Stack Overflow answers.
Forklift has none of that yet. **[Forklift's ecosystem is essentially nonexistent by
comparison — this is the honest, dominant practical gap.]**

### 2.7 Performance & concurrency

- **Parallelism.** Git is largely single-threaded in its core paths. Forklift is
  **parallel by default** — per-directory scans, per-file hashing, and compression fan
  out over a shared worker pool sized to core count, and Blake3 parallelizes even a
  single large file. On many-core machines with large working trees this is a real edge.
- **Bounded memory.** Forklift makes "no operation needs RAM proportional to repo size"
  a design principle (streaming shards). Git is generally fine here too, but Forklift
  designs for it explicitly.
- **Caveat.** Git has 20 years of profiling and pathological-case hardening. Forklift's
  performance claims are architecturally sound but not yet proven on huge real-world
  repos at scale.

### 2.8 AI-agent orientation

Git treats machine consumers as an afterthought (scrape porcelain, parse `--porcelain`
output, freetext authors). Forklift is built for them: global `--json` with a versioned
envelope, stable error + exit codes, structured `conflicts` output (base/ours/theirs as
content addresses), a built-in `forklift mcp` MCP server, agent identity classes with
supervisors, machine-authorship provenance, `deliver` for squash-with-evidence, `bay`s
for fleet-on-one-box, and optimistic lift so a fleet stacking to one pallet stops
serializing. This is covered in [§7](#7-the-ai-agent-dimension). Git has no answer here
that isn't a retrofit.

### 2.9 Git vs. Forklift — bottom line

Forklift is a bet that *identity, collaboration metadata, and hosting economics* belong
**inside** the VCS, and that AI authorship makes that urgent. Where it wins over Git today
is signed/verifiable in-repo trust, in-repo portable collaboration data, parallelism, and
machine-first ergonomics. Where Git wins — and will for years — is **maturity, ecosystem,
tooling, delta compression, and the rich history-editing toolbox** (rebase, cherry-pick,
bisect, blame, tags). If you need those, or you need it to work everywhere with everyone
today, use Git.

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
  journaled `undo` covers `stack`/`consolidate`/`shift` only, one step at a time.
  jj's model is more general. **[Forklift's undo is narrower.]**
- **No staging area friction & the working-copy-as-commit model** make history editing
  effortless in jj; automatic rebase of descendant commits when you edit an ancestor is a
  standout. Forklift has no rebase at all. **[Forklift lacks this.]**
- **Stable change IDs + revsets.** jj gives every change a stable ID that survives
  rewrites, plus a powerful `revset` query language. Forklift identifies parcels by hash
  (which changes on rewrite) and has no query language. **[Forklift lacks stable change
  IDs and revsets.]**

**Where Forklift is ahead of jj:**

- **Identity, signing & trust.** jj inherits Git's model — freetext authors, optional
  signing, no in-repo user registry. Forklift's signed office, genesis anchor, and offline
  `audit` have no jj equivalent.
- **Serverless-native protocol & self-hostable server.** jj relies on Git remotes and Git
  hosting (its default backend *is* Git); it has no independent hosting story. Forklift has
  its own storage-centric protocol and a self-hostable server head.
- **In-repo collaboration metadata** (haul/manifest) and **AI-agent primitives**
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
  Meta. Forklift's scale claims are design-stage; **it has not been benchmarked on a large
  real repo** (its own design doc flags this as an open task). **[Forklift is unproven at
  scale.]**
- **Virtual filesystem / lazy fetch (EdenFS).** Sapling fetches file content on demand,
  so you can work in a giant repo without materializing it. Forklift materializes the
  working tree; its task-scoped sparse workspaces are **future work**
  ([§8](#8-forklift-features-not-yet-implemented-future-work)). **[Forklift lacks lazy/
  partial checkout today.]**
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
  storage + thin verifiers, self-hostable today.
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
- **Clean cherry-picking & change reordering** with no duplicate-commit / rebase-hell
  problems, a direct consequence of the patch model. Forklift has no cherry-pick at all.
  **[Forklift lacks this.]**
- **First-class stored conflicts**, like jj. Forklift blocks on conflicts. **[Forklift
  lacks this.]**

**Where Forklift is ahead of Pijul:**

- **Familiarity & adoption path.** Forklift's snapshot model + Git-aliased commands make
  it immediately legible to Git users; Pijul's patch model is powerful but requires
  rethinking your mental model, which has limited its adoption.
- **Identity, signing & trust as a first-class subsystem.** Pijul signs patches and has
  keys, but nothing like Forklift's genesis anchor, in-repo office/role registry, agent
  identity classes, and offline chain audit.
- **Serverless-native hosting design + self-hostable server.** Pijul has The Nest for
  hosting, but not Forklift's storage-centric, presigned-byte-plane architecture.
- **In-repo collaboration metadata (haul/manifest) and the whole AI-agent track.**
- **Git interoperability** — Forklift ships bidirectional `import-git` / `export-git`.
  Pijul's Git interop is more limited.

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
| First-class / committable conflicts | ❌ | ✅ | ⚠️ | ✅ | ❌ |
| Rebase | ✅ | ✅ (auto) | ✅ | n/a | ❌ |
| Cherry-pick | ✅ | ✅ | ✅ | ✅ | ❌ |
| Interactive history editing | ✅ | ✅ | ✅ | ⚠️ | ⚠️ (`deliver`) |
| Universal undo / operation log | ⚠️ (reflog) | ✅ | ✅ | ⚠️ | ⚠️ (journaled `undo`) |
| Stable change IDs / revsets | ❌ | ✅ | ⚠️ | ⚠️ | ❌ |
| Bisect | ✅ | ⚠️ | ✅ | ❌ | ❌ |
| Blame / annotate | ✅ | ✅ | ✅ | ✅ | ❌ |
| Tags | ✅ | ✅ | ✅ | ⚠️ | ❌ |
| Hunk-level staging (`add -p`) | ✅ | ✅ | ✅ | ✅ | ❌ |
| Parallel core ops | ❌ | ⚠️ | ✅ | ⚠️ | ✅ |
| Lazy / virtual filesystem checkout | ❌ | ❌ | ✅ | ⚠️ | 🔭 |
| Partial / sparse workspace | ⚠️ (painful) | ⚠️ | ✅ | ✅ | 🔭 |
| Large-file handling | ⚠️ (LFS add-on) | ⚠️ | ✅ | ⚠️ | ⚠️ |
| Delta compression | ✅ | ✅ | ✅ | ✅ | 🔭 |
| Parallel working dirs (worktrees) | ⚠️ (bolt-on) | ✅ | ✅ | ⚠️ | ✅ (bays) |
| Mandatory signed history | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| In-repo user/key registry | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| Genesis trust anchor + offline audit | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| Pseudonymous / zero-PII history | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| In-repo collaboration metadata (PRs/reviews) | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| AI-agent identity classes + provenance | ❌ | ❌ | ❌ | ❌ | ✅ |
| Machine-first CLI (`--json`, stable errors) | ⚠️ | ⚠️ | ⚠️ | ⚠️ | ✅ |
| Built-in MCP server | ❌ | ❌ | ❌ | ❌ | ✅ |
| Serverless-native remote protocol | ❌ | ❌ | ❌ | ❌ | ✅ (self-host today) |
| Managed hosting / serverless deployment | ✅ (huge) | via Git | ⚠️ (Mononoke) | ✅ (The Nest) | 🔭 (AWS head planned) |
| Git interop (import & export) | n/a | ✅ (native) | ✅ | ⚠️ | ✅ (bidirectional) |
| Ecosystem / IDE / CI integration | ✅✅✅ | ⚠️ (growing) | ⚠️ | ❌ | ❌ |
| Proven at massive scale | ✅ | ⚠️ | ✅ | ❌ | ❌ |
| Years in production | ~20 | ~4 | production @ Meta | ~10 (niche) | **<1** |

The matrix is intentionally unflattering where it should be: Forklift's ✅ column is real
and distinctive (trust, in-repo collaboration, AI agents, serverless protocol,
parallelism), but its ❌ and 🔭 columns are long, and its **ecosystem and scale rows are
its weakest** — the things that take *time and users*, not code, to earn.

---

## 7. The AI-agent dimension

This is the axis on which Forklift is most differentiated from *all four* alternatives,
because none of them were designed for it. Forklift's bet is that AI coding agents are
becoming primary consumers of version control, which changes five things:

1. **Authorship becomes a compliance question** — *what* wrote this, under *whose*
   authority. Forklift answers it cryptographically: agents are enrolled operators with an
   **identity class** (human / agent / bot / service) and a **supervisor**, every parcel
   is signed by the agent's own key, and `history --class agent` filters "which parcels
   did agents write, under whose supervision" — forge-proof and offline. Passphrase-
   protected human keys mean an unattended agent **fails closed** rather than signing as a
   human.
2. **The CLI's consumer is a program on a token budget** — Forklift has global `--json`,
   stable error/exit codes, token-cheap summaries, and a built-in `forklift mcp` MCP
   server so agents call typed tools instead of scraping output.
3. **Concurrency goes fleet-size** — `bay`s give N agents parallel working directories on
   one machine sharing one object store; **optimistic lift** auto-merges disjoint
   concurrent pushes so a fleet stacking to one pallet stops serializing through a human.
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

> Everything in this section is **designed but not shipped** as of 2026-07. It is listed
> separately, on purpose, so the rest of this document reflects only what Forklift can
> actually do today. These are being actively worked on; several are what would close the
> most-cited gaps above. Status labels mirror Forklift's own roadmap
> ([`docs/DESIGN.html`](DESIGN.html)).

**Hosting & scale (the founding vision, partly unbuilt):**

- 🔭 **AWS serverless head** (`forklift-aws-lambda`) — the Lambda + S3 + DynamoDB control
  plane that motivated the whole project. *Planned.* Today Forklift can be **self-hosted**
  (`forklift-server`) but the "runs on S3 with no server" deployment is not built.
- 🔭 **Generational/ranged bundle delivery** — native indexes are shipped; range negotiation and
  incremental bundle rebuilds remain planned.
- 🔭 **A managed hosting service** (repo registry, web UI, auth, billing) — Forklift's
  GitHub/GitLab equivalent. *Planned as a separate product*, built on the AWS head.
- 🔭 **Server metrics/observability, bundle auto-rebuild policy, native TLS listener** —
  production-hardening items. *Planned* (logs + health checks already shipped).

**Working-copy scale (the Sapling-shaped gap):**

- 🔭 **Task-scoped sparse workspaces** (`franchise --only <subtree>`, scoped bays) — fetch
  and materialize only the subtree a task needs; the per-directory tree/shard model makes
  this natural. *Needs design.* This is Forklift's answer to sparse-checkout / lazy
  fetch, and it is **not built** — so Forklift has no partial-checkout story today.
- 🔭 **Per-file privileges / hidden files** (FORK-10) — files some users can't even see,
  enforced by the remote (keyed-hash or encrypted blobs). Flagged as the hardest feature
  in the backlog. *Needs design.*

**Collaboration & identity (provider layer):**

- 🔭 **Org-tier chains & delegation hierarchy** — per-org signed chains above the
  warehouse office, for scaling identity across an organization. *In progress* (pseudonymous
  chains, sigchain endorsements, PoP + admin-authorized keys, re-genesis, revocation
  records, and the hook protocol have **shipped**; org-tier chains and embedded-key
  offline audit / org-chain bundling remain).
- 🔭 **Per-org pseudonymous operator ids** — true cross-org unlinkability. *Future*
  (global id shipped; per-org is a provider-layer upgrade).
- 🔭 **Web-UI signing** — producing signed content from a browser "merge" button without
  the provider being able to forge. *Future* (part of the provider layer).
- 🔭 **Cross-warehouse forks & approval-gated merges for `haul`** — the pull-request MVP
  is intra-warehouse and records reviews without gating merges on them. *Future.*

**Agent track remainders:**

- 🔭 **`events --follow` event stream** — so fleet members react to each other's stacks
  instead of polling. *Deferred* pending the internal event-stream refactor.
- 🔭 **Auto-checkpoint mode** for draft pallets (checkpoint every `load`/`stack`), and
  **bundling a delivery's trail closure** so it travels with the delivery. *Future.*
- 🔭 **Full transcript stored as a blob-in-tree** for provenance (regulated shops).
  *Future* (hash-only provenance shipped).

**UX & smaller items:**

- 🔭 **"Did you mean …" command suggestions** and **per-command usage guides**. *Planned.*
- 🔭 **Per-parent conflict stages** for three-parent merges. *Needs design.*
- 🔭 Cosmetic: rename "inventory" → "dock" for terminology consistency. *Needs design.*

**Not on the roadmap at all** (worth stating so the picture is complete): `rebase`,
`cherry-pick`, `bisect`, `blame`/annotate, tags, hunk-level staging, and first-class
committable conflicts are **not currently planned** features — they are genuine gaps
versus the alternatives, not scheduled work.

---

## 9. Summary — when to reach for which

- **Use Git** if you need it to work everywhere, with everyone, today — mature tooling,
  vast ecosystem, and a full history-editing toolbox (rebase, cherry-pick, bisect,
  blame, tags). The safe default for almost everyone.
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
  large ecosystem, has no managed hosting yet, and is missing common history-editing
  tools**. Forklift is the most opinionated bet in this list, aimed at where version
  control might be going rather than where it has been.

The honest one-sentence verdict: **Forklift has a genuinely distinctive and coherent
position — trust, serverless hosting, and AI authorship built into the VCS — but it is
years behind Git, jj, Sapling, and Pijul on maturity, ecosystem, scale-proof, and the
everyday history-editing toolbox, and several of its headline differentiators (serverless
hosting at scale, sparse workspaces, delta compression) are still on the roadmap rather
than in the binary.**

---

*This comparison reflects Forklift at v0.1.x (2026-07). For the authoritative, dated
status of every Forklift feature, see [`docs/DESIGN.html`](DESIGN.html). Descriptions of
Git, Jujutsu, Sapling, and Pijul reflect those projects as of mid-2026 and are necessarily
a snapshot of fast-moving software — corrections welcome.*
