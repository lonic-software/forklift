# Parallelization plan

A survey of every Forklift operation: its computational structure, what serializes it, and how
much parallelism could actually help. Forklift's stated goal is to use all cores; this maps
where that pays and — as importantly — where it does not, so effort goes to real wins.

The verdicts are grounded in the code (file:line in the notes below the table), and in a hard
lesson from a failed attempt (parallel `blame`, see the bottom): **parallelizing a loop only pays
if the parallel work is both a large fraction of the runtime *and* free of shared-lock
contention.** Two things decide almost every row:

- **Object-*store* writes are lock-free.** Each object is a distinct content-addressed file
  written by atomic rename (`file_utils::write_file_atomically`) — no global lock, so importing and
  fetching parallelize cleanly at the storage layer. **Working-*tree* writes are a different
  story**: materializing a tree writes many small files into shared directories, and *those*
  serialize on the OS's filesystem-metadata locks (see `materialize` below — measured, reverted).
- **Object *reads* funnel through single `Mutex`es** — the read cache (`file_utils`), the pack
  registry (`pack_utils`, but it only *briefly* holds the lock to hand back an `Arc`, after which
  mmap'd reads are lock-free), and the commit-graph cache (`graph_utils`). A read-*bound* parallel
  loop contends on these; a compute-bound one barely touches them. Together with the filesystem
  wall above, these two shared resources decide almost every row.

## The map

| Operation | How often | Work unit × count | Structure | What serializes it | Parallel win | Effort |
|-----------|-----------|-------------------|-----------|--------------------|--------------|--------|
| **stocktake** | very hot | hash working file, per dir | tree recursion | brief `changes` mutex; barrier before move-detect | **Already** (`TaskExecutor`) | — |
| **inventory build** | hot (part of stack) | stat/hash file, per dir | tree recursion | per-shard write (distinct files) | **Already** (`TaskExecutor`) | — |
| **tree build (stack)** | hot | build+store tree, per dir | bottom-up join (parent waits on children) | `built`/`pending` mutexes | **Already** (`TaskExecutor`) | — |
| **fetch / franchise / lift / lower** | occasional | GET+store object, per object | parallel within a wave, barrier between waves | `Semaphore(24)`; wave ordering; lift's serial negotiate + signature-upload loop | **Already** (network `JoinSet`) | — |
| **audit** | occasional | sig read + ed25519 verify, per parcel | office chain sequential; **pallet history independent** | phase-2 per-parcel object reads (loose `.sig` sidecars, parcel bodies) share the object caches — the read ceiling, not the ed25519 CPU | **Done — ~2.4×** (18 cores; read-bound, so sub-linear) | — |
| **checkout / materialize** (`shift`, `lower`, `restore`, `franchise`, `park`, `bay`) | hot–occasional | `load_blob` + `fs::write`, per changed file | independent per file at forklift's level | **the filesystem serializes the writes** — concurrent create/write/`chmod` of many small files contend on APFS inode/dirent metadata locks | **Tried, reverted — net loss** (~0.87×: 341→391 ms for 8000 files on 18 cores) | — |
| **diff** (cross-revision) | occasional | histogram diff, per changed file | **independent per file** | output must be path-ordered (collect-then-print); blob reads hit the read-cache | **Done — 1.4–5.7×** (18 cores; scales with per-file diff size — compute-bound at the high end) | — |
| **consolidate / merge** | occasional | 3-way LCS, per *divergent* file | **independent per file** | walk records deferred jobs; end-to-end diluted by the sequential apply-writes + stack | **Done — 6.4× merge / 2.5× end-to-end** (18 cores; compute-bound; only wide merges) | — |
| **compact** | occasional (auto) | zstd delta-compress, per object | path-deltas independent; window + `PackWriter` sequential | the sliding delta `window` (each object deltas against just-packed neighbours) and the single-file append stay serial | **Done — ~2.1×** (18 cores; **byte-identical output**; read-cache-bound below the CPU ceiling) | — |
| **import-git** | rare (once) | read pipe → build → **store loose file** | the store is the bottleneck | *measured*: **71% is writing loose object files** — the same FS-metadata wall as `materialize`; pipe read 21% (serial source), compress 4% | **Not worth it** — parallel stores regress (materialize lesson); pipe is serial | — |
| **export-git** | rare | spawn a `git` subprocess per object | DAG-ordered; subprocess per unit | one `git` process per commit/tree/blob, invoked serially; `&mut` memo | **Low** — commits are DAG-ordered; only independent blob `hash-object`s could overlap | med |
| **bundle build** | occasional (server) | write object into one zstd stream | **sequential sink** | a single `zstd::Encoder` — one compression stream, inherently serial | **None** without a reframed multi-frame format | high |
| **history** (full log) | hot | decode+render parcel | **sequential heap walk** | must read every parcel to display it; ordering dependency; read/graph mutexes | **None** — see the blame lesson | — |
| **blame** | hot | changed-path filter check + LCS | **sequential chain + LCS** | first-parent chain (linked list), LCS attribution (oldest→newest); filter checks too small a fraction | **None** — *measured*: no speedup even with an `RwLock` cache (below) | — |
| **ancestry / merge-base** | hot (internal) | generation-pruned walk | bounded walk | already O(gap) via generation numbers; graph mutex | **None** — already fast + tiny | — |
| **stack (parcel), undo, palletize, shift-ref, tag, peek, config, park, office/haul/manifest/provenance subcommands** | mixed | O(1) — a ref move, one object, one signature | trivial | — | **N/A** — constant-size work | — |

## Where to spend the effort (ranked)

1. **`audit` — done, ~2.4× on 18 cores (`verify_pallet_history`).** Verifying a pallet's history
   is one independent signature check per reachable parcel, each against the already-built,
   **read-only** `office_state` (no shared write). The implementation is a three-phase split:
   phase 1 discovers the parcel set by a breadth-first walk over commit-graph parent edges
   (content-addressed, so complete; near-free on a graph-warm warehouse, which avoids a
   sequential `load_parcel` floor at git.git scale); phase 2 fans the per-parcel verify across
   `num_cpus` scoped threads (which re-enter the caller's thread-local storage scope — the server
   serves several warehouses); phase 3 resolves the trust/distrust **boundary** decisions serially,
   preserving the lazy closures, exact error messages and first-failure order of the serial walk.
   This is a real "beat git" shape (git has no signed-history verification at all).

   The **lesson**, measured on a 5000-parcel signed warehouse (236 ms → 96 ms): the plan's
   "pure CPU, near-linear" premise was **wrong**. Phase 2 is dominated by the per-parcel *reads* —
   a loose `.sig` sidecar (which `compact` deliberately never packs) and the parcel body, both
   through the shared object caches — not by the ed25519 CPU, so it scales ~2.7× even on 18 cores.
   Still a clean, real win with no semantic change (unlike parallel `blame`, which got 1.0×), but a
   reminder that almost every forklift loop touches the object caches, and *that* is the recurring
   ceiling (see the cross-cutting lever below). A near-linear audit would need the reads off the
   shared `Mutex`es first.

2. **`checkout` / `materialize_tree` — tried, reverted, a net loss.** This was ranked the
   highest-leverage change ("write-bound, one shared helper speeds six callers"). It was
   implemented — a shared `apply_file_ops` doing removes-first then a parallel write fan-out,
   routed through `materialize_tree` and the `shift`/`park`/`consolidate` loops — and **measured
   slower**: on a 8000-file full-tree `shift`, the write phase went **341 ms → 391 ms** (~0.87×),
   so the whole `shift` regressed 418 → 460 ms. Compacting the blobs (mmap, lock-free reads)
   changed nothing, which pins the cause on the **filesystem, not the object caches**: forklift's
   writes are lock-free (distinct paths), but creating/writing/`chmod`-ing thousands of small
   files serializes on the OS's per-directory and inode-allocation metadata locks (APFS here), and
   18 threads contending on those kernel locks is worse than one thread streaming them. The change
   was reverted. (It might scale on a different filesystem, or for a tree of large files where
   write *bandwidth* dominates metadata — but the common checkout is many small files, and it
   regressed on the platform it was measured on, so it does not ship without cross-platform
   evidence.) The lesson mirrors `blame`: "embarrassingly parallel" at forklift's level says
   nothing about the shared resource one layer down.

3. **`diff` — done, 1.4–5.7× on 18 cores (`diff_pallets`).** A cross-revision diff is one
   independent unit per changed file: load the two blobs, run the histogram diff, format the block.
   `diff_pallets` now fans those across the cores (`std::thread::available_parallelism`), each
   worker formatting into a string, and prints the blocks in path order afterwards — the collect-
   then-print that ordered output requires. Serial and parallel output are byte-identical. The win
   **scales with how much real diffing there is**: measured **5.7× on 3000 files of ~400 lines with
   a quarter changed** (the histogram diff dominates → compute-bound, dodges both the FS wall and
   largely the read ceiling), but only **1.4× on 8000 files with a two-line diff each** (little CPU
   per file → back to the read/print floor). Never a regression, unlike `materialize` — because
   the work being parallelized is CPU, not filesystem metadata. (The `worktree`/`staged` diff modes
   were left sequential: they compare against the working tree/inventory and are almost always a
   handful of files.)

4. **`consolidate` — done, 6.4× on the merge / 2.5× end-to-end (`compute_merge_actions`).** Same
   shape as `diff`: a file both sides changed since the base is one independent 3-way line merge.
   The tree walk now stays cheap — for such a file it records a deferred `MergeJob` (hashes only,
   no blob load) instead of merging inline — and a second phase runs the jobs across the cores
   (`num_cpus` scoped threads, re-entering the storage scope), reassembling the actions in walk
   order so the result is identical to the serial merge (verified: same merged tree hash). Measured
   on a 2000-file clean merge (400-line files, a half changed on each side): the merge computation
   itself went **989 ms → 153 ms (6.4×)** — the cleanest compute-bound win here, since a 3-way LCS
   is heavier per file than a 2-way diff. End-to-end `consolidate` went **1495 ms → 590 ms (2.5×)**:
   the merge is now only ~150 ms of it, and the rest is the *sequential* tail — writing the 2000
   merged files (the filesystem wall from `materialize`) and the `stack`. Only wide merges pay;
   most touch few files and stay under the threshold (sequential).

5. **`compact` — done, ~2.1× on 18 cores, byte-identical output (`pack_utils::compact`).** The
   profile of a 6480-object compaction: ~40% is the path-delta zstd compression, ~38% is reading
   and decompressing each object, ~21% is the size-window fallback (`best_delta`), and the pack
   append is ~1%. The first two — the path-deltable objects' read + delta-compress — are
   independent (a path delta is the object against the *previous version of the same file*, fetched
   from the store, and it never seeds the sliding window), so they now fan out. The catch was doing
   it *without* changing the pack: the size-window fallback deltas each object against the ones just
   packed (a sequential chain), and reordering would change the content-derived pack filename (and
   break idempotent repack). So it is a **byte-bounded batch pipeline** — `prepare_batch` compresses
   each batch's path deltas in parallel, then the writer walks the batch *in order* doing only the
   sequential work (window fallback + append). Verified serial and parallel produce **byte-identical
   `.pack` and `.idx` files** on a 6480-object store; measured **1290 ms → 610 ms (~2.1×)**. The
   window and the append stay serial by nature, and the delta bases are object reads through the
   shared caches, so it lands at the read-cache ceiling rather than near-linear — the expected place.

## Where parallelism does *not* pay (and why)

- **`history` / `blame` / ancestry — Amdahl-bound.** These are sequential walks whose parallel
  fraction is small; the dominant cost is the ordered walk itself and per-item decode/LCS. `blame`
  was tried twice (a `Mutex` cache and an `RwLock` cache with truly-concurrent reads) and measured
  on git.git (18 cores): **serial 420 ms vs parallel 422 ms — no gain**, because the first-parent
  chain, the ~340 tree/blob resolves, and the LCS attribution are all sequential, and an
  `RwLock`'s reader-count atomic contends on a hot path anyway. See `OBJECT_STORE_SCALING.md` §C.

- **`checkout` / `materialize_tree` — filesystem-metadata-bound.** *Measured* slower (write phase
  341 → 391 ms for 8000 files, 18 cores) and reverted — concurrent small-file create/write/`chmod`
  contends on the OS's directory/inode metadata locks. See the ranked recommendations above.

- **`bundle` — a single zstd stream.** One sequential compression sink; parallelizing emission
  would mean reframing the on-disk format into independent frames. High effort, occasional op.

- **`import-git` — filesystem-metadata-bound (measured).** The guess was "input-bound on the
  `git cat-file` pipe"; the profile said otherwise: of a ~915 ms import, **71% is `store` — writing
  the thousands of small loose object files** (the same APFS metadata wall as `materialize`), 21%
  is the pipe read (a serial source that cannot be parallelized), and the compress is 4%. So there
  is nothing to parallelize that would help: the dominant cost is exactly the small-file-write
  pattern that *regressed* under threads in `materialize`. (A structural win exists — writing
  imported objects straight into a pack instead of loose-then-`compact` — but that is a redesign,
  not parallelism.)

- **`export-git` — bound by an external boundary** (a `git` subprocess per object), not CPU.

## The cross-cutting lever — the object caches now hold their locks pointer-only (shipped)

Every read-bound parallelization used to be capped by the shared object caches holding a single
`Mutex` **across real work** — the read cache copied the object bytes out under the lock, and the
graph cache read, parsed, and even fsync'd a shard file under the lock. Both now hold the lock only
for pointer-sized, in-memory work (milestone D, P1/P2):

- **Read cache (`file_utils`).** Entries were already `Arc`-shared, but a hit `clone`d the *bytes*
  out under the lock. It now clones the `Arc` (a pointer bump) and returns it —
  `retrieve_object_by_hash_shared` hands the shared allocation to the borrow-only readers
  (`object_utils::load_tree`/`load_blob`, the pack delta-base reads in `pack_utils`), and
  `retrieve_object_by_hash` still returns owned `Vec<u8>` for the callers that keep the bytes (the
  server's object-GET, bundles) — but that one copy now happens **outside** the lock. So the
  critical section is a pointer clone regardless of object size.
- **Graph cache (`graph_utils`).** `with_shard` took the cache lock and then did the shard-file
  `read`+`parse` inside it; `persist_records` and eviction wrote (fsync included) inside it. Now
  the lock is taken only to serve a resident shard or to install/mutate one: a cold shard is
  read+parsed with the lock **released** (re-locked to insert, the double-load race resolved
  first-insert-wins on immutable content), and the eager flush serializes a snapshot under the lock
  but writes it off the lock. The on-disk copy is deliberately last-write-wins and the shard stays
  dirty until re-flushed, so a stale racing write self-heals — the graph is a derived accelerator,
  never a source of truth.
- **Pack registry (`pack_utils`).** Already the idiom: a brief lock hands back an `Arc<Vec<LoadedPack>>`
  and the mmap'd reads run lock-free. Left as-is; it was the reference for the graph-cache rework.

**What it measured (18 cores).** On a 2500-parcel signed warehouse and a large-blob warehouse
(120 files × 2000 lines, so a cached blob is ~100 KB), before vs after: `audit`, `diff`, and
`stocktake` are unchanged within noise; `compact` is ~1.03× (1219 → 1186 ms, fsync off) with
**byte-identical** `.pack`/`.idx` output; no op regressed. So the foundation shipped without a
speed cost, but the near-linear payoff did **not** materialize on these CLI ops — and the reason is
worth recording honestly:

- **`audit`'s 2.4× ceiling did not move, because audit no longer reads through the read cache at
  all.** Its phase-2 loop reads the parcel body *uncached* (`load_parcel` bypasses the cache — a
  parcel is read about once) and the `.sig` sidecar via a direct `fs::read`; the only shared lock
  it touches per parcel is the pack registry's brief `Arc` hand-back. The plan's earlier
  "read-cache ceiling" description of audit is stale relative to the uncached-parcel + direct-sig
  read path the code actually takes now.
- **The parallel read-bound loops that *do* use the read cache (`diff`, `compact`) are
  miss-dominated, not hit-dominated.** A cross-revision `diff` loads each blob about once (a cache
  *miss*: disk read + zstd decode, identical before and after); `compact`'s path deltas each read a
  distinct previous version. The pointer-vs-copy difference only bites when many threads read the
  **same** cached object at once, and these loops rarely do.

So the lever's real beneficiary is **concurrency the single-process CLI cannot reproduce**: a
multi-warehouse server serving the same hot objects to many clients (its object-GET now copies
outside the cache lock), or many concurrent ancestry queries on one warehouse (each graph shard
read/flush is now off the global lock). What the rework still does **not** help is unchanged:
`checkout`/`materialize` and `import-git` (bound by the *filesystem* metadata wall) and
`consolidate`'s write tail (that same wall) — a storage-format change (writing packs directly) is
the only lever there. The two ceilings remain cleanly separated; this change lifted the object-cache
one to pointer-sized locks, and a genuinely read-*hit*-bound parallel consumer is what would now
turn that into a measured near-linear win.
