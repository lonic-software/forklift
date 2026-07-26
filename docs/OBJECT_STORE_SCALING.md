# Object-store scaling — design note

A design note motivated by benchmarking Forklift against git on git.git (81,348
commits, ~402k objects). See [`BENCHMARK.md`](BENCHMARK.md) to reproduce.

## The two signals

| Signal | Forklift | git | Ratio |
|--------|----------|-----|-------|
| On-disk store | `.forklift` = **4.8 GB** | `.git` = 316 MB | **~15×** |
| `history` (whole log) | **3.59 s** | 0.93 s | ~3.9× |

> **Update — resolved.** These were the *loose* baseline. `forklift compact` (phases 1,
> 2 and 2b, below) packs git.git to **~230 MB — smaller than git's own 316 MB pack** — and
> the `history` walk is now **0.90 s** — at parity with `git log`'s verbose default (0.88 s),
> the fair like-for-like comparison. The rest of this note is the design as built.

> **A note on the git size figure (measured 2026-07-07).** The 316 MB above is git's
> `gc`-optimized pack from an earlier git.git snapshot. On a fresh *local* clone today
> (81,470 commits, not aggressively repacked) `.git` measures **599 MB**, against a
> complete `.forklift` of **236 MB** (228 MB packs + 7.2 MB commit-graph + 1.3 MB inventory).
> The honest, apples-to-apples comparison is pack-to-pack — forklift's **228 MB** delta pack
> vs git's `gc`-tightest **~316 MB** — and forklift is smaller on both. (A mid-compaction
> `du` transiently shows a larger store because the loose objects are deleted only after
> every pack is durably written; that is a checkpoint, not the resting size.)

**Reads and deltas (a caveat the numbers hide).** Deltas shrink the store but a delta read
*reconstructs its chain*, so a read-heavy walk can pay for compression. Two things keep that in
check:

- **Parcels are stored full, never delta'd** (measured the hard way). `history` reads every
  parcel, and when parcels were window-delta'd the walk went to **27.7 s** (worse than loose)
  reconstructing a chain per parcel; storing parcels full — they are tiny, so the store stayed
  ~230 MB — dropped it to 1.7 s. A cheap `is_parcel` (reads the type from the header) skips the
  delta for them.
- **A content-addressed read cache** (`file_utils`) fronts every object read. Reconstruction-
  heavy walks (`blame`, `export`, cross-revision `diff`) resolve the same objects and the same
  delta *bases* over and over; the cache reconstructs each once. Because a hash *is* its content
  (immutable), it never needs invalidation — even across a `compact` that relocates the bytes.
  It cut `blame` on a git.git file from **19.7 s → 4.7 s**; the commit-graph's changed-path
  filters (B) then took it to **0.94 s** by skipping the parcels that never touched the file.
- **The read funnel itself was the last tax.** Fixes on the shared object read (`file_utils`,
  `pack_utils`): a packed object no longer pays a guaranteed-to-fail loose `open` (packs are
  consulted first — a syscall-free index search); pack data files are **mmap'd**, so a read is a
  zero-copy slice into mapped pages, no syscall or buffer copy per object; and parcels bypass the
  read cache (they are read once and stored full, so it only taxed them). With `blame`'s
  first-parent chain read from the commit-graph rather than by decoding every ancestor, this took
  **`blame` 0.94 s → 0.43 s** and **`history` 1.9 s → 0.90 s**.

That gap — `blame`/path-scoped `log` *examining every commit* — is what the **commit-graph
(B, shipped)** closes: a sharded, self-healing cache of the parcel DAG that gives each parcel a
generation number (so ancestry walks prune) and a changed-path Bloom filter (so `blame` skips
parcels that did not touch the file). See section B for the design as built and the numbers.
Against git's *equivalent* output, forklift is now at parity — `history` 0.90 s vs `git log`
0.88 s, `blame` 0.43 s vs `git blame` 0.38 s — so past this point concurrency (section C, #3) is
what would push the walk *below* git, not merely match it.

Both traced to a single fact: **the object store was loose and unpacked.** Every
object is its own zstd-compressed file (`loose_object.rs:store`); git.git becomes
~402k separate files. There is no delta compression and no packing.

- **Size (15×):** git stores each file version as a *delta* against a similar
  version inside a few packfiles; Forklift stores every version in full,
  individually compressed. A file edited 500× → ~500 small deltas in git vs 500
  full zstd copies in Forklift. Per-object zstd also can't share a window across
  similar objects (161k small tree objects compress poorly alone), and ~402k tiny
  files each pay filesystem slack.
- **History (3.9×):** the walk loads every parcel via a separate `fs::read` +
  `zstd::decode_all` (`file_utils.rs:retrieve_object_by_hash`) — 81k random loose
  reads vs git reading a handful of mmap'd packs.

## What already shipped from this investigation

- **`import-git` batching** — one `git cat-file --batch` pipe instead of a process
  per object (~30× faster import).
- **`history -n`/`--limit`** — a bounded walk. Because the walk is a max-heap on
  timestamp, a limit loads only the newest N parcels and their frontier, not the
  whole graph (3000-commit repo: 68 ms → 12 ms for `-n 20`). This is the biggest
  practical win for the *common* case, and it is what makes a commit-graph (B,
  below) worthwhile.
- **Timestamp round-trip removed** in `history` rendering (micro).
- **`compact` — packing with path-aware delta compression (A phases 1, 2 & 2b, shipped).**
  `forklift compact` sweeps the loose store into a few bounded packs and stores each blob
  as a delta against the previous version of the same file. Both structural signals move:
  **git.git packs 4.8 GB → 261 MB, smaller than git's own 310 MB pack, and faster than the
  size-only heuristic.** See the section below for the design as built.
- **Sharded commit-graph (B, shipped).** Generation numbers make ancestry walks (merge base,
  divergence checks) O(generation-gap) instead of O(history) — measured ~850× on `find_merge_base`
  and ~4000× on a diverged `is_ancestor` over a 50k-deep history — and changed-path Bloom filters
  let `blame` skip parcels that did not touch the file (git.git `blame` 4.7 s → 0.43 s, at git
  parity; `history` 1.9 s → 0.90 s, at parity with `git log` verbose). See section B.

---

## A. Packing + delta compression (the strategic fix — both signals)

### Phase 1 — the pack substrate (shipped as `forklift compact`)

The packing mechanism deltas ride on: implemented in
`forklift-core/src/util/pack_utils.rs`, driven by the `compact` command (and MCP
tool). (Phase 1 packed every record in full; phase 2, below, adds delta records to
the same substrate.) A pack is two files under `.forklift/objects/pack/`:

- `<id>.pack` — a 12-byte header then the objects' records concatenated. A full
  record's blob is **byte-identical** to the loose file (same zstd stream), so
  packing a full object re-hashes and re-compresses nothing — it only moves bytes.
- `<id>.idx` — a header then fixed-width records `(32-byte hash, u64 offset, u64
  length)` **sorted by hash**, so a lookup is a binary search over the resident
  index and a single `read` at the offset. `<id>` is the Blake3 of each record's
  `(hash, offset, length)` triple — layout-derived, never the record bytes — so a
  repack that reproduces the same layout collides onto the same filename rather
  than piling up a fresh one. (An earlier scheme hashed only the pack's sorted
  object hashes, which let a repack land the same id on a differently-laid-out
  pack and pair a fresh data file with a stale index mid-rename; folding in
  offset and length closes that window.)

The read fallback lives behind the two centralised object-store functions
(`retrieve_object_by_hash`, `does_object_exist` in `file_utils.rs`): the loose
store stays the fast path, and a loose miss consults the packs — so every caller
(history, audit, export, the server data plane) benefits with no change of its own.
Signature sidecars (`.sig`) are read by path, not through the store, so `compact`
leaves them loose; imported history is unsigned, so git.git has none anyway.

Two safety invariants, both verified by tests:

- **Durable before destructive.** A loose object is deleted only after the pack
  that now holds it is flushed, fsynced and renamed into place (data file before
  index, so a reader never sees an index without its data) *and the pack directory
  itself is fsynced* — so the renames survive power loss, not just a process crash,
  before anything is deleted. A crash at any point leaves every object readable —
  loose, packed, or harmlessly both. (The atomic loose/ref write path fsyncs the
  file and its parent directory too; the `FORKLIFT_FSYNC` env var disables all
  fsyncing for bulk, disposable work where a mid-run crash just means re-running.)
- **Plural and bounded.** A pack rolls over at 512 MiB *or* 100k objects, whichever
  comes first, so no single pack — or its resident index — grows without bound.
  This is the alignment with the per-directory-inventory philosophy discussed under
  B: lookup RAM is O(packed object count), never O(store bytes).
- **Verified on the way out.** Every object read out of the store — a loose file, a
  full pack record, or a reconstructed delta — is re-hashed and checked against the
  address it was fetched by before the bytes are returned (`object_utils::verify_object_bytes`,
  called from `pack_utils::resolve_record` and the loose path in `file_utils`). A corrupt
  pack, a torn loose file, or a delta rebuilt against the wrong base *fails* the read
  rather than silently serving wrong bytes — the enforcement behind the content-addressing
  the delta and cache layers already assume. Blake3 is fast enough that this is a small
  fraction of the surrounding decompression, and it runs only on a cache miss.
- **Multi-process correct.** The store is shared across bays and can be served by a
  long-running process, so two more things hold. `compact` takes a shared-scope
  `StoreLock` (at `forklift_root/store.lock`, distinct from the bay-local lock) so two
  bays or processes cannot enumerate the same loose set and race each other's deletions;
  it errors on contention (an explicit `compact` reports it, auto-maintenance skips), and
  the loose/old-pack sweep tolerates `NotFound`. And a read whose cached pack registry
  predates an *external* `compact` — the object moved into a new pack, its loose source
  swept — reloads the registry once and retries the packs before declaring a miss, instead
  of erroring on a healthy store (`pack_utils::retrieve_from_packs_reloading`).

The substrate alone (full records only) already shrinks a tiny repo `.forklift`
164K → 32K (5×) from per-file slack removal, and at git.git scale collapses ~400k
random loose reads in a `history` walk into a few packs. What it does **not** do on
its own is close the version-to-version size gap — that is the delta records phase 2
adds, below.

**Automatic on import; not (yet) on a recurring trigger.** `import-git` runs
`compact` on the way out (`--no-compact` opts out), because that is the one
operation that dumps a huge loose set in a single shot — the case a user would most
need to remember to pack, and one that produces a single clean pack. A *recurring*
auto-compaction (git's `gc --auto`: after mutating commands, estimate the loose
count from one fan-out folder × 256, and above a threshold spawn a detached
background pack) is deliberately **deferred to phase 3**. The reason is the missing
repack: phase-1 `compact` only packs the *currently loose* objects into a *new*
pack, so firing it on a recurring drip would slowly proliferate packs (each read
scans them). It belongs with the repack that keeps the pack count self-limiting —
exactly how git pairs `gc.auto` with `gc.autoPackLimit`. For a hosted server,
"regularly" is better served by *scheduled* off-peak maintenance than by
opportunistic background runs (the server already has an admin `collect`).

### Phase 2 — delta compression (shipped)

Packs are now format **version 2**: every data record carries a one-byte kind, so a
record is either a **full** object (its zstd blob, as before) or a **delta** — the
object stored as its difference from a similar *base*. Version-1 packs (no kind byte)
are still read, so upgrading strands nothing.

**Reuse, not reinvention.** A delta is exactly the bundle machinery (`delta_utils`):
zstd with the base object installed as the **dictionary**, never a bespoke diff
format. A delta record is `base hash (32) || target length (VLQ) || zstd frame`.
Reconstruction fetches the base through the normal object read (so the base may be
loose, in another pack, or itself a delta — a bounded chain, `MAX_DELTA_CHAIN = 50`)
and zstd-decompresses against it. Correctness rests on Blake3 content-addressing, not
on the delta format: the reconstruction is re-hashed against the object's address on
every read (see "Verified on the way out" above), so a wrong or truncated delta can only
*fail* a read, never return wrong bytes — the same safety net bundles rely on.

**Base selection — path-aware (phase 2b, shipped).** The base picks the delta's quality,
and the ideal base for a blob is the *previous version of the same file*. The object
store is a flat hash space with no path context — paths live in trees — so `compact`
first walks the reachable DAG (all parcels + their trees, never blob content) and builds
`blob → previous-version-at-path`, reusing the exact traversal bundles use
(`bundle_utils`). Each blob then deltas against that one ideal base; objects with no path
history (trees, parcels, first versions, unreachable blobs) fall back to a small
size-sorted **window** (10 objects, ≤64 MiB). Chains are bounded to `MAX_DELTA_CHAIN`,
and base pointers are acyclic (a base is always an earlier version), so reconstruction
always terminates. Loose deletion is deferred to the end of the run so every delta base
stays readable while packing.

This was measured against the earlier size-only heuristic, on **git.git** (85k commits,
402k objects):

| Base selection | Pack size | `compact` time |
|----------------|-----------|----------------|
| size window only | 1.1 GB | 460 s |
| **path-aware** | **261 MB** | **296 s** |
| git's own pack (zlib) | 310 MB | — |

Path-aware is **4.3× smaller *and* faster** — one correct base per blob instead of ten
blind window attempts. At 261 MB it **beats git's 310 MB** (same path-quality bases, but
zstd deltas rather than git's zlib). 98.6% of objects delta'd.

**Cost.** The DAG walk is cheap relative to the delta step it shrinks: for git.git it
loads ~252k parcels+trees (~15–25 s, ~15 MB RAM). At **linux-kernel scale** (~1.3M
commits, millions of trees) the walk's resident seen-tree set (~200–300 MB) is the thing
to bound next — a capped-history walk or a Bloom-filtered seen-set (a false "seen" only
skips a re-walk → a missed base → safe). Time there is dominated by the delta step
regardless, which path-aware also reduces.

**Still plural and bounded.** Rollover (512 MiB / 100k objects) is unchanged; deltas
change the *contents* of records, not the bounded-pack guarantee.

**Payoff.** Phases 1 + 2 + 2b close the 15× size gap outright (git.git: 4.8 GB loose →
261 MB, past git's own pack) and the open-per-object read cost. What remains is repacking
existing packs (`gc`, so packs are not append-only forever) — see the sequencing below.

---

## B. Sharded commit-graph (shipped)

A denormalized, self-healing cache of the parcel DAG (`graph_utils`), stored at
`.forklift/graph/`. It is **two layers** that share one store:

1. **The substrate — parents + a generation number per parcel.** Ancestry walks
   (`merge_utils::is_ancestor`, `find_merge_base`) read the graph instead of decoding
   parcel objects, and prune on the generation number: an ancestor never outranks its
   descendant, and each step toward the parents strictly lowers the number, so a branch
   is abandoned the moment it drops below the target's generation.
2. **Changed-path Bloom filters** — one per parcel, over the paths that changed relative
   to its first parent. `blame`/path-scoped `log` test the filter before reading a tree and
   skip parcels that did not touch the path. A Bloom filter has no false negatives, so a
   skip is always safe; a false positive only costs a redundant real check.

Layer 2 is a chunk *on* layer 1 (its record field), exactly as git bundles its changed-path
filter into its commit-graph — so they coexist by construction, not as two competing stores.

### Why two axes, and why this order

The substrate is the **collaboration/scale** lever. Before it, `find_merge_base` and
`is_ancestor` walked to the roots decoding every ancestor — O(history) on *every* consolidate
and *every* haul divergence check. Generation numbers make those O(generation-gap). Measured on
a 50k-deep shared history with a recent fork (the realistic merge shape):

| Op | plain walk | commit-graph | speedup |
|----|-----------|--------------|---------|
| `find_merge_base` | 28.8 ms | 0.034 ms | **~850×** |
| `is_ancestor` (diverged) | 27.7 ms | 0.007 ms | **~4000×** |

and the gap grows linearly with depth. This is what scales collaboration on a large warehouse,
not local `history` display (which was already fast: `-n` bounded walks beat git, and an
unlimited log must read every parcel to *print* it regardless of any graph).

The changed-path filters are the narrower **path-query** lever — they close the last `blame`
gap to git (blame examines every commit; the filter skips the ~99% that did not touch the file).
Measured on git.git: `blame Documentation/Makefile` **4.7 s → 0.94 s** (and 19.7 s before the
read cache), then **→ 0.43 s** once blame's first-parent chain was read from the graph (parent
edges, no ancestor decode) and the pack read funnel was tightened — **parity with git (0.40 s
here)**. One caveat learned along the way: a blame visits every parcel, so it touches every shard
— the resident shard set must hold the whole 2-hex fan-out (256 shards) or the walk thrashes,
re-parsing a shard per parcel (a 128-shard cap made blame *slower* than no graph at all).

### Keeping Forklift's bounded-RAM promise

> Doesn't putting all parcels into one file go against the sharded-inventory philosophy?

Yes — a monolithic graph file would. So the graph is **sharded by parcel-hash prefix**, the same
2-hex fan-out as the object store (`file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS`): one file
per prefix, an LRU-capped resident set, a walk touching only the shards its frontier falls in.
No single file slurped into a `Vec`. (Git's single-file commit-graph stays bounded only because
it is mmap'd; sharding gets the same property while matching Forklift's storage model.)

### Self-healing, and never a source of truth

A record is keyed by the parcel's immutable hash, so it can never be *stale* — only *missing*.
A miss decodes the parcel and writes the record; there are no write hooks in
`stack`/`consolidate`/`import`. Generation numbers are computed by an **iterative** post-order
walk (no recursion — the parent chain runs tens of thousands deep), and a record is persisted
only from a *fully loaded* ancestry, so a stored generation is always exact.

Because the graph is derived, it is only ever an accelerator. When a generation cannot be
computed — an ancestor object is not present locally yet, as when a diverged remote head is
fetched before all its deep ancestry — the ancestry query **falls back to the plain object
walk**, which needs only the descendant's own reachable history. (The cross-remote divergence
tests exercise exactly this.) A corrupt or foreign shard reads as empty and is rebuilt.

`compact` populates the whole reachable graph — generation numbers and changed-path filters —
while it holds the warehouse lock and the object caches are warm, so a warehouse is graph-ready
right after an import or a repack; anything added since self-heals on the next read.

---

## C. Read-speed levers (the backlog)

**Standings on git.git (81,470 commits; git references from the same box), with a fair
like-for-like comparison:**

| Op | forklift | git (equivalent output) | verdict |
|----|----------|-------------------------|---------|
| store size | 236 MB | 316–599 MB | **forklift wins** |
| merge-base / ancestry | — | — | **forklift faster** (generation numbers) |
| `blame Documentation/Makefile` | 0.45 s | `git blame` 0.38 s | ~parity |
| `history` (verbose) | 0.90 s | `git log` 0.90 s | **parity** |
| `history --oneline` (terse) | **0.35 s** | `git log --oneline` 0.69 s | **forklift wins ~2×** |

**A methodology correction worth recording.** An earlier draft reported `history` as ~1.5×
git — but that compared *verbose* `forklift history` (parcel, author, stack, full message per
entry) against `git log --oneline` (hash + subject only). The honest comparison is against
`git log`'s **verbose default** (0.90 s), which renders the same shape — and there
`forklift history` (0.90 s) is at **parity** (and faster than `git log --format=fuller`, 0.96 s).
So there is no real single-threaded deficit; the per-object constants below were the last of it.

**Why forklift *wins* the terse comparison (0.35 s vs git's 0.69 s), and why lever G is
therefore shelved.** `history --oneline` reads each parcel for its subject — exactly as
`git log --oneline` reads each commit — yet is ~2× faster. The reason is a design choice that
pays off here: **git delta-compresses commit objects in its packs (for size), so it *un-deltas*
each commit it reads; forklift stores parcels *full*, so a subject is one mmap slice + a zstd
decode with no delta reconstruction.** Add zstd-vs-zlib, a fixed abbreviation (git computes a
unique-prefix length), and terse mode skipping the office read and display-name resolution, and
forklift is ~2× ahead. git's own size optimization works *against* its terse read speed — a gap
forklift opens for free. So **lever G (subjects in the graph) is unnecessary**: it would trade
content duplication and graph bloat for a lead we already hold. It stays recorded below only as a
deliberately-not-done, with its rationale.

**The goal now is to open a gap in forklift's favour, not to catch up.** The levers, with
status — those marked *(beat git)* are places forklift can go faster than git can, because git
either does not do them (single-threaded) or cannot (its graph does not carry the message):

| # | Lever | Helps | From git? | Effort | Status |
|---|-------|-------|-----------|--------|--------|
| 1 | Read `blame`'s first-parent chain from the graph (parents, no ancestor decode) | blame | — (we have the graph) | low | **SHIPPED** — 0.94 → 0.43 s |
| 2a | Packs-first read funnel (kill the always-failing loose `open`/stat) + cached pack handle | all reads | partly | med | **SHIPPED** — history 1.9 → 1.06 s |
| 2b | mmap the packs — zero-syscall, zero-copy reads (records resolved from a slice into the map) | all reads | yes | med | **SHIPPED** — with C1, history 1.06 → 0.90 s (parity) |
| C1 | Bypass the read cache for parcels — read once, stored full, so the cache never hits yet each read paid ~5 string allocs + churn | history | no (git has no such layer) | low | **SHIPPED** — with 2b |
| 6 | Terse `history --oneline` (abbrev hash + subject; skips office + display-name resolution) | history | yes | low | **SHIPPED** — see below |
| 3 | *(beat git)* Parallelize the walk across cores | history, blame | **no — git's log is single-threaded** | high | **TRIED & REVERTED (twice)** — no speedup even with concurrent reads; see below |
| 3′ | Concurrent graph reads — make the shard cache an `RwLock` so reads take a shared guard | parallel everything | — | med | **TRIED & REVERTED** — did not unblock #3 (see below) |
| C3 | Memoize the objects *and* graph roots per scope — every object/graph read resolved a root (a bay-context lock + path allocations) for the pack-registry, read-cache, and graph-shard keys | all reads | — | low | **SHIPPED** — per-scope memo, invalidated on scope/bay change; ~5% off blame |
| — | O(1) graph-shard LRU — the resident-shard cache scanned a `VecDeque` on every access (O(resident), and a `blame` hits it ×10⁴); now an access-clock stamp | blame, graph reads | — | low | **SHIPPED** — found while profiling #4 |
| C2 | Tighten parcel decode/parse — a parcel carries richer signed-provenance *actions* than a git commit | history, blame | — | med | TODO — partly inherent to what forklift records |
| 5 | 256-entry first-byte fanout in the `.idx` (narrows each binary search) | all lookups | yes | low | TODO — skipped for now (medium format work, ~1 ms at this scale) |
| 4 | Store the commit **timestamp** in the graph record → order/traverse without decoding parcels | `history -n` | yes (git's graph stores commit date) | low-med | **TRIED & REVERTED** — see below |
| G | Carry the **subject** in the graph record too, so `--oneline`/`-n` render with *zero* parcel reads | history --oneline/-n | **no — and git declines this on purpose** | med | **SHELVED** — terse already beats git ~2× without it (below) |
| H | *(beat git differently)* Reachability bitmaps for the transfer axis (clone/fetch/haul enumeration) — git's pack `.bitmap` equivalent | sync at scale | yes | high | TODO — a separate axis; measure a large fetch first |

**C2** is the remaining per-parcel constant. **H** is the separate network-scaling story (only
after a large fetch is benchmarked). And **#3 (parallelism) was tried thoroughly and does not
pay off** for blame — the detailed reason below is the useful part.

**Lever 3 — tried twice, reverted, and why (the real lesson).** Two builds, both measured on
git.git (81,470 commits) on an 18-core box, same-warehouse A/B:

1. **Parallel blame on the `Mutex` cache** — serial 407 ms vs parallel 413 ms. The changed-path
   filter check does nearly all its work inside the single graph-cache `Mutex`, so N threads just
   contend on that one lock.
2. **Parallel blame on an `RwLock` cache (3′)** — made reads take a *shared* guard, so they truly
   run at once. Still serial 420 ms vs parallel 422 ms — **no gain**.

So concurrent reads were *not* enough, which points at the actual ceiling: **blame's cost is
dominated by its sequential parts** — the first-parent chain walk (a linked list, inherently
serial), the ~340 tree/blob resolutions, and the LCS attribution (a line's history must thread
oldest-to-newest). The changed-path filter checks — the only embarrassingly-parallel part — are
too small a fraction to move the total (Amdahl), and an `RwLock`'s reader-count atomic contends
on a hot path anyway. Making blame actually parallel would need a lock-free read architecture
*and* a way to parallelize the sequential walk — a large, risky investment for a command already
at parity with git (0.42 s vs 0.38 s). Not worth it; reverted both. The general lesson stands:
parallelizing *callers* is worthless until the parallelized work is both a large fraction and
free of shared-lock contention.

**Lever 4 — tried and reverted (a lesson).** Storing the timestamp in the graph so `history`
orders without decoding parcels sounds like git's own commit-date-in-the-graph, but it *regressed*
the common cases on git.git — full `history` 0.90 → 1.11 s, `--oneline` 0.35 → 0.54 s. The reason:
full-log **must load every parcel to display it anyway**, and the timestamp is already in that
parcel, so reading it from the graph **adds** a per-parcel lookup rather than removing one. git
gets a real win from its graph-stored date because its *traversal* (ancestry, `--since`) can then
skip decoding commits entirely — but forklift's `history` decodes for display regardless, so the
only beneficiary was bounded `-n` (already ~30 ms). A net loss; not kept. (The profiling that
found it did surface the O(1)-LRU win above, which was kept.)

**On lever G — a caveat worth recording (why git does *not* do this).** git's commit-graph
stores the tree OID, parents, generation and commit *date*, but deliberately **not** the message.
The message is variable-length *content* that already lives in the packs; copying it into the
graph duplicates it, bloats a structure meant to stay small and cheap to regenerate (subjects
alone ≈ +5–6 MB, roughly doubling ours and growing with history), and only helps `--oneline`
(verbose reads the object anyway). git's commit reads are also already cheap (tiny, delta'd,
mmap'd), so the saving is marginal — the read is not its bottleneck. The case that G is *more*
defensible for forklift is real but narrow: a parcel is richer than a commit and our decode/parse
is heavier, so our per-parcel read costs more. So G is **measure-gated**: only if the terse
measurement shows the parcel read/parse dominating (rather than being near git's terse number
already) does it earn the duplication — and even then, lever 4 (the date, git-blessed) plus a
leaner parse gets most of it *without* copying content. G (the subject) is the last, most dubious
step, not a headline.

### `history --oneline` (shipped)

The terse form prints one line per parcel — the abbreviated hash and the description's first
line — and, showing no author or class, skips the office read *and* the display-name resolution
(a network round-trip) the verbose form does. It still reads each parcel (for the subject,
timestamp and parents), exactly as `git log --oneline` reads each commit — yet at **0.35 s it is
~2× faster than git's 0.69 s**, because forklift's full-stored parcels need no delta
reconstruction where git un-deltas each commit (see the standings note above). So it beats git
outright *without* lever G.

---

## Sequencing

1. **Shipped:** `import-git` batching, `history -n`, timestamp micro-fix.
2. **Shipped — A phase 1** — `forklift compact`: pack objects without deltas
   (concatenate + sorted resident index + loose fallback). Removes per-file slack
   and the open-per-object history cost. Highest structural payoff for the effort.
3. **Shipped — A phase 2** — delta records (reuse the bundle zstd-dictionary encoder).
4. **Shipped — A phase 2b** — path-aware base selection (walk the DAG, delta each blob
   against its previous version at path). git.git: 1.1 GB → **261 MB, past git's 310 MB**,
   and faster than the size heuristic.
5. **Shipped — A phase 3 (repack)** — `forklift compact --all` rewrites existing packs:
   it keeps only the **live** set (so packed garbage is dropped) and **consolidates** many
   packs into few, **reusing** each object's existing delta record (a byte-copy, not a
   reconstruct-and-re-delta) — so it is fast and never balloons the store. Incremental
   `compact` (loose → new pack) is unchanged. See below.
6. **Shipped — recurring auto-compact trigger** — after a mutating command, if the store has
   accumulated enough loose objects (`maintenance.loose`, default 6700) or packs
   (`maintenance.packs`, default 20), forklift compacts/repacks automatically (git's
   `gc --auto`), synchronously under the command's own lock. Opt out with
   `maintenance.auto = false`. See below.
7. **Shipped — bounded phase-2b walk memory** — the walk's "seen trees" and "seen blobs" sets
   are Bloom filters, so its memory is a fixed bit budget instead of one entry per object (the
   kernel-scale concern). A false positive only skips an object → size-window fallback, never a
   wrong result. See below.
8. **Shipped — B (sharded commit-graph)** — generation numbers make ancestry (merge base,
   divergence checks) O(generation-gap) instead of O(history); changed-path Bloom filters let
   `blame` skip parcels that did not touch the file. Sharded, self-healing, with a plain-walk
   fallback. See section B.

(Note: `gc_utils` still owns *loose*-garbage collection with its grace period; the repack only
drops garbage that was already **packed**, and leaves loose garbage to `gc_utils`.)

Each step is independently shippable and measurable with `bin/benchmark`
(re-run and diff the size + `history` rows).

### Phase 3 — repack (`compact --all`), shipped

Plain `compact` is incremental (loose → a new pack; existing packs untouched), so packs
accumulate and packed garbage is never dropped. `compact --all` fixes both:

- **Keep the live set only.** It computes the reachable set (`gc_utils::collect_live_set`,
  the same liveness the loose collector uses); unreachable objects stuck in packs are not
  carried over, so packed garbage is dropped. Unreachable *loose* objects are left alone for
  `gc_utils`' grace-period-aware sweep — the repack only ever drops garbage that was already
  packed (packed objects are never mid-operation, since `compact` holds the warehouse lock).
- **Reuse deltas — do not reconstruct.** This is the load-bearing decision. A live object
  already stored as a delta is **copied verbatim** into a new pack (its record bytes moved,
  not decompressed), so the original — already good — delta is preserved and the repack is a
  filtered byte-copy. The only objects rebuilt are the few whose delta base is being dropped
  (reconstructed and re-deltated so nothing points at the dropped base) and any loose objects
  (packed path-aware). A repack that copies everything skips the phase-2b DAG walk entirely.

  *Why this matters (measured):* the first cut re-deltated every object from scratch, which
  was both slow (each read reconstructed a delta chain) and — because a packed delta's size is
  its tiny payload, scrambling the size-window order used for trees — **larger** than the input:
  git.git **261 MB → 1.4 GB in 952 s**. Copying records verbatim sidesteps both: the same repack
  is **261 MB → 249 MB in 142 s** (6.7× faster, and a touch smaller from consolidation).
- **Consolidate.** All existing packs are read and their live records re-packed into fresh
  packs; the old packs are deleted. Many packs → few.

Durability is unchanged: new packs are written and fsynced — and the pack directory is
fsynced — before any original (loose file *or* old pack) is removed, so an interruption
never loses an object, across a power loss and not just a process crash. One sharp edge the
tests pin: the pack id is layout-derived, so an **idempotent** repack writes a pack with
the *same name* as the one it supersedes — the deletion step must therefore never remove a
file a new pack was just written to (it skips new-pack paths).

### Recurring auto-compaction (`gc --auto`), shipped

Compacting on import handles the big one-time case; ongoing use accumulates loose objects a
few at a time. So after any *mutating* command (`stack`, `consolidate`, `lower`, … — not
`compact` itself, or `import-git`, which already compacts), forklift checks — **cheaply, no
full scan** — whether maintenance is due: it estimates the loose count from one fan-out folder
× 256 (git's trick) and counts packs. Over `maintenance.loose` (default 6700) it runs an
incremental `compact`; over `maintenance.packs` (default 20) it runs a consolidating
`compact --all`. `maintenance.auto = false` turns it off; both thresholds are configurable.

It runs **synchronously, under the command's own warehouse lock**, not as a detached
background process. That is a deliberate constraint of the current lock: it is exclusive and
fail-fast, so a background compaction holding it would make the user's *next* command fail. A
truly detached maintenance needs compaction that does not hold the exclusive lock (concurrent
GC) — a separate project. The cost of the synchronous choice is a rare, threshold-gated pause;
the escape hatch is the config toggle.

**Inspecting it: `forklift store`.** The auto-maintenance *decision* is sampled (one fan-out
folder × 256) so it stays cheap on the hot path, but a user sometimes wants the real numbers —
how much of the store is packed, how delta-dense the packs are, whether maintenance is due.
`forklift store` is that read-only readout: an **exact** census (loose vs packed object counts,
per-pack delta counts and sizes, on-disk totals) and the maintenance verdict against the
`maintenance.loose` / `maintenance.packs` thresholds. It is the object-store counterpart of
`stocktake` (which reports the working tree) and the read counterpart of `compact` (which acts).

### Bounding the phase-2b walk memory, shipped

The path-aware walk's two "seen" sets — trees, and blobs (which also carried each blob's chain
depth) — grew to one entry per reachable object: hundreds of MB at kernel scale. They are now
**Bloom filters** (a fixed ~10-bits-per-element budget, sized from an object-count estimate),
and each blob's depth moved into the per-path `latest_at_path` map (bounded by distinct paths,
not history). A Bloom false positive only makes the walk *skip* an object — it then gets no
path base and falls back to the size window: a smaller delta, never a wrong object, because the
content-address check is the real safety net. The walk's memory is now a bounded bit budget,
and git.git delta quality did not suffer — it improved slightly (261 MB → 230 MB), because not
tracking per-object depth means the chain-depth bound is approximate and a few chains run a
little deeper (more deltas). That is safe: base pointers stay acyclic so reconstruction always
terminates, with `MAX_RECONSTRUCT_DEPTH` as a hard backstop against a corrupt pack.
