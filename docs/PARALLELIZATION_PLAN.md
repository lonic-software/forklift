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
| **compact --all** (repack) | occasional (auto) | reachability walk + verbatim record copy, per object | one shared reachability pass (D/P3, below); `CopyRecord` targets need no CPU, only a memory copy | the steady-state case (no garbage, no loose) is **not** CPU-bound at all — it is walk-reads and small memcpys | **Done — steady-state repack ~4.5×** (238 ms → 53 ms on a 401-parcel/7442-object corpus; **byte-identical output**; see D/P3 below) | — |
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
   sequential `load_parcel` floor at git.git scale); phase 2 fans the per-parcel verify out
   through the canonical fan-out helper (`fanout_utils::fanout_map` — see below), which spawns
   `num_cpus` scoped threads that each re-enter the caller's thread-local storage scope (the
   server serves several warehouses); phase 3 resolves the trust/distrust **boundary** decisions
   serially,
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
   `diff_pallets` now fans those out through the canonical fan-out helper (`fanout_utils::fanout_map`,
   see below) — the one production caller of it that lives outside `forklift-core`, since `diff`'s
   command handler is in the `forklift` CLI crate — each worker formatting into a string, and prints
   the blocks in path order afterwards — the collect-then-print that ordered output requires. Serial
   and parallel output are byte-identical. The win
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
   no blob load) instead of merging inline — and a second phase runs the jobs through the canonical
   fan-out helper (`fanout_utils::fanout_map`, see below), reassembling the actions in walk order so
   the result is identical to the serial merge (verified: same merged tree hash). Measured
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
   each batch's path deltas in parallel through the canonical fan-out helper (`fanout_utils::fanout_map`,
   see below), then the writer walks the batch *in order* doing only the sequential work (window
   fallback + append). Verified serial and parallel produce **byte-identical
   `.pack` and `.idx` files** on a 6480-object store; measured **1290 ms → 610 ms (~2.1×)**. The
   window and the append stay serial by nature, and the delta bases are object reads through the
   shared caches, so it lands at the read-cache ceiling rather than near-linear — the expected place.

6. **`compact --all` (repack) — done, milestone D/P3: one shared reachability pass + mmap'd
   verbatim copies + `[u8; 32]` path-base keys, all byte-identical.** This is *not* a
   parallelization change (repack's reachability walk is a single-threaded DAG traversal, not a
   fan-out candidate — see the "sequential-chain" exclusion in `fanout_utils`'s doc comment); it
   is a redundant-I/O and per-record-copy fix that pays off specifically in the **steady-state**
   repack, `compact --all`'s common case once a warehouse is compacted and stays garbage-free.

   **What the roadmap assumed vs what the code did.** The roadmap estimated "~5 parcel re-reads"
   per parcel across the repack phases; instrumenting the actual walk (temporarily, on a
   401-parcel/7442-object synthetic corpus) confirmed it exactly: **2005 logical parcel reads /
   401 parcels = 5.0**, when both the garbage-liveness walk (`gc_utils::collect_live_set` →
   `audit_utils::collect_reachable_present`, then a second read per parcel for its `tree_hash`)
   and the path-base walk (`compute_path_bases` → `audit_utils::collect_reachable`,
   `bundle_utils::topo_order_oldest_first`, then a third read per parcel for its `tree_hash`) run
   — the general case, whenever any loose object or packed garbage is present. In the *pure*
   steady-state case (no garbage, no loose — every target is a `CopyRecord`), the path-base walk
   is already skipped by an existing `needs_delta` check, leaving **802 / 401 = 2.0** reads/parcel.
   Each "read" is a pack lookup + zstd decode + Blake3 verify, so both counts were real,
   measurable waste, not a rounding artifact.

   Separately, every `CopyRecord` — the fast path that carries an already-good delta or full
   record from an old pack into the new one verbatim — went through a **fresh `File::open` +
   `seek` + `read_exact`** per record (`read_pack_slice`), even though the same packs had *just*
   been mmap'd once by `loaded_packs()` earlier in the same function to enumerate them.

   **The fix, three pieces:**
   - **One shared reachability pass.** A `ParcelReadMemo` RAII guard (`object_utils.rs`) turns on
     a thread-local decode cache (parcel *bytes*, not the parsed `Parcel`, so no `Clone` is needed
     on the model) for exactly the reachability phase in `compact` — `collect_targets` (the
     liveness walk) and `compute_path_bases` (the path-base walk) — and is dropped before the
     parallel batch-write loop starts, so no fan-out worker thread ever sees it (it is
     deliberately `!Send` via `Rc`, not `Arc` — this is single-threaded work, no synchronization
     needed). A parcel is decoded once; every further logical read in the phase — of which there
     are up to 5 — is a pointer clone. This does not change *what* any phase decides, only how
     often the bytes are fetched from the store.
   - **mmap'd repack copies.** `collect_targets` now returns the `Arc<Vec<LoadedPack>>` it already
     built (renamed `source_packs`), kept alive for the whole `compact` call. Each `CopyRecord`
     target carries a `pack_index` into that vector instead of a `PathBuf`, and the copy
     (`framed_record`) is a `Cow::Borrowed` slice straight out of the pack's `Mmap` — zero-copy,
     zero-syscall — for the common framed (version ≥ 2) case; only the rare unframed (version-1)
     record still allocates, to prepend the missing kind byte. The **only** thing proven to
     matter for the byte-identical-output contract is that the copied bytes are unchanged — a
     borrow of the same bytes a fresh `read_exact` would have produced is trivially
     byte-identical, so this is a pure latency win with no risk to the D5 pack-id/idempotent-repack
     invariant (the id is computed from the *records themselves*, not from how they were sourced).
   - **`[u8; 32]` path-base keys.** `compute_path_bases`'s `blob hash → base hash` map (and the
     `latest_at_path` chain-depth map) switched from 64-byte hex `String` keys to raw 32-byte
     Blake3 digests (`sign_utils::from_hex`/`to_hex` already existed for the boundary conversions
     needed at the object-store read). This map is purely an in-memory intermediate — never
     serialized, never crosses a wire format — so the change is invisible outside this function;
     `prepare_target`'s lookup also got simpler (`path_bases.get(&target.hash)` directly, no
     re-hexing the target hash to look it up, since `target.hash` was already `[u8; 32]`).

     **A pitfall caught before it shipped, worth recording.** The same code path also touches
     `seen_blobs`, the `Bloom` filter that deduplicates the walk (§ above, "Bounding the phase-2b
     walk memory"). It would have been natural to feed the Bloom filter the new raw `[u8; 32]`
     bytes too, alongside the map key change — same logical value, seemingly free. It is **not**
     free: a `HashMap` lookup is exact, so any byte encoding of the same key gives the same answer
     — but a Bloom filter is *probabilistic*, and which bytes it hashes decides its false-positive
     *pattern*. A false positive on `seen_blobs` is the mechanism that makes a blob fall back to
     the size window instead of getting a path-base delta (a deliberately safe degrade, per the
     Bloom section above) — so a different FP pattern means a different object gets that fallback,
     which means different delta bytes in the pack. The 401-parcel/~7 500-object synthetic corpus
     used to verify byte-identity here is far below this Bloom filter's sizing floor (4096 elements
     minimum), so it never actually triggers a false positive either way — meaning this specific
     regression would have passed every check in this change *and still broken before-vs-after
     byte-identity on a large enough store* (git.git-scale, where the doc above already establishes
     false positives are expected and accepted). The fix: `seen_blobs` keeps hashing exactly the
     same hex-string bytes it always did (`record_path_base` now takes both the raw `[u8; 32]` for
     the exact maps and the original hex `&str` for the Bloom calls); only the exact-lookup maps
     changed representation. General lesson for this codebase: an exact structure (`HashMap`,
     `BTreeMap`, sorted comparison) is safe to re-key to an equivalent encoding; a probabilistic
     one (`Bloom`, any hash-bucketed sketch) is not — its output depends on the literal bytes
     hashed, not just their logical value, and that can only be caught by testing at the scale
     where it actually engages, which a small synthetic corpus will not do.

   **Byte-identity verification.** `repacking_is_byte_reproducible` (`crates/forklift/tests/determinism.rs`)
   passes unchanged. Additionally verified end-to-end on the synthetic corpus: `compact` then
   `compact --all` (twice) with the pre-change binary, and again with the post-change binary,
   comparing SHA-256 of every `.pack`/`.idx` file — **identical hashes, both for the fresh pack
   and for the steady-state repack.** This is the strongest test available short of a formal
   proof: real binaries, real files, real hashes, not just the unit test's assertion.

   **Measured (18 cores, 401-parcel/7442-object synthetic corpus, 5 runs each, min/median ms):**

   | Path | Before | After | Change |
   |------|--------|-------|--------|
   | `compact` (fresh, loose→pack) | min 2230 / med 2255 | min 2242 / med 2249 | unchanged (expected — untouched path, no `CopyRecord`s, zstd-delta-bound) |
   | `compact --all` (steady-state repack, all `CopyRecord`) | min 236 / med 238 | min 52 / med 53 | **~4.5× faster** |
   | `compact --all` (first repack, from all-loose — exercises both walks) | min 2274 / med 2280 | min 2254 / med 2268 | ~0.5% faster (noise-level — this path is zstd-delta-compute-bound, same as fresh `compact`; the reachability-walk savings are real but small next to the delta compression cost) |

   The steady-state win is the headline because it is `compact --all`'s *common* case in
   practice: a warehouse settles into "already packed, nothing new to delta" between imports, and
   every repack from there on was paying the 5×-read + per-record-file-open tax for work that
   produces the exact same bytes as a cheaper path. The all-loose/fresh-compact paths are
   dominated by zstd delta compression (already parallelized, item 5 above) and were never the
   target of this change — their near-zero movement is the expected, honest result, not a miss.

   **Memory.** `/usr/bin/time -l` on the same corpus: peak RSS for the steady-state repack was
   ~17.6 MB before, ~17.0 MB after — no measurable regression from retaining the source packs'
   mmaps for the run. This corpus's whole pack is ~1 MB (well under the 512 MiB rollover), so it
   does not stress-test mmap retention at scale; the honest caveat is that a repack consolidating
   many large packs (git.git scale, hundreds of MB across several pack files) would hold all of
   them mapped simultaneously for the run, where before only one was file-read at a time. Virtual
   address space is not a practical constraint on 64-bit, and *resident* memory only grows with
   pages actually touched (the copy only touches the bytes it copies), so this is not expected to
   regress kernel-scale RSS materially — but it has not been measured at that scale in this
   change, and is worth re-checking if `compact --all` is ever profiled on git.git again.

   **Durable-before-destructive, unaffected — with one portability fix caught along the way.**
   This change touches only *reads* (the reachability walk, the verbatim-copy source), so the
   fsync-before-delete sequence itself is untouched: new pack data fsynced → index fsynced → pack
   directory fsynced → loose sources removed → old packs removed, in the same order as before.
   But holding the source packs' `Mmap`s for the *whole* `compact` call (rather than the old
   per-record `File::open` that closed the instant each record was read) is a real change to how
   long an old pack file stays open — and on Windows, `remove_file` on a file that is still open
   (or mapped) without `FILE_SHARE_DELETE`, the platform default, fails outright. POSIX permits
   unlinking an open/mapped file unconditionally, so this would not have shown up testing only on
   macOS/Linux. The fix: `compact` explicitly `drop(source_packs)` right after the write loop's
   last use of it, *before* `sync_dir` and both removal sweeps — restoring the old code's
   mmap-lifetime guarantee (gone by the time anything is deleted) despite now holding one shared
   handle for the whole run instead of one per record. Pinned by a new unit test,
   `a_repack_physically_removes_old_pack_files_it_supersedes`, which repacks a store with no
   pallet refs (so every packed object is legitimately unreachable) and asserts the old pack
   files are actually gone from disk afterward — the outcome the drop exists to protect, even
   though the specific Windows failure mode can't be exercised from this (POSIX) dev environment.

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

## The canonical fan-out idiom (shipped, milestone D/P4)

Four of the "done" rows above — `audit`, `consolidate`, `compact`, `diff` — hand-rolled the exact
same shape independently: split a pre-collected `&[T]` into `min(num_cpus, items.len())` contiguous
chunks, spawn one `std::thread::scope` worker per chunk that first re-enters the caller's
thread-local storage-root scope (§ above, not inherited by spawned threads), map each item, and
reassemble the results positionally so the caller's own walk/path order stays deterministic. That
duplication is now one helper, `fanout_utils::fanout_map` (`crates/forklift-core/src/util/fanout_utils.rs`),
and every one of the four sites routes through it — this *is* the parallelism idiom the project
means when it says "use all cores": a flat, independent item list, fanned out, reassembled in order.

`fanout_map` never short-circuits on error — it always returns exactly `items.len()` results,
positionally aligned, whether or not the item's own result type is a `Result`. A caller that wants
first-error/original-order short-circuiting gets it by collecting the returned `Vec` into a
`Result` (`compact` and `diff` do this); a caller that must inspect every result before deciding
what is fatal keeps the raw `Vec` (`audit`'s trust/distrust boundary resolution, `consolidate`'s
walk-order reassembly). Each site keeps its own size threshold below which it never calls the
helper at all (audit: 256 parcels; consolidate: 8 jobs; compact: 8 objects to prepare; diff: 8
changed files) — that cutoff is a property of how expensive one item's work is, not of the fan-out
mechanism, so it stays at the call site rather than living in the helper.

This is deliberately a *different* idiom from [`TaskExecutor`](../crates/forklift-core/src/model/task.rs)
(`stocktake`/`inventory build`/`stack`'s tree build): `TaskExecutor` is async (`tokio::task::JoinSet`)
and recurses a *tree*, where a parent directory's result waits on its children enqueueing and
finishing first. `fanout_map` is sync (`std::thread::scope`) and flat — the whole item list is known
up front and every item is independent of every other. Neither replaces the other; see the doc
comment on `fanout_map` for the full "when not to use it" list (filesystem-metadata-bound writes —
the reverted `materialize` lesson above — and sequential-chain operations like compact's delta
window or blame's first-parent walk).

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

## CI benchmark-regression job (milestone D, T5)

Every number above was a one-off, hand-run measurement (`min`/`median` of N, release build, a
synthetic corpus built for that change). T5 turns that into a standing CI job so a future
regression on any of these hot paths gets caught automatically, not only when someone happens to
re-run a benchmark by hand.

- **Harness:** `bin/bench.sh` — builds a small, deterministically-generated warehouse fresh in the
  job (a seeded, index-derived corpus, not a committed binary blob: a few hundred files, a few
  hundred signed parcels, two diverging pallets), then times the hot ops already covered above
  (`stocktake`, `diff`, `compact`/`compact --all`) plus the axes this plan never measured in CI:
  `shift` (checkout), a signed `stack` + `audit` (after `office enroll` — every parcel from there
  on is signed), cold vs warm cache (first invocation vs. the mean of later ones — a CLI process
  never carries its object caches across invocations, so "cold" here is real: the first touch of a
  given file after the corpus is built), peak RSS (`/usr/bin/time -l`/`-v`), and core-count scaling
  (`audit` — which crosses `audit_utils::PARALLEL_THRESHOLD` at this corpus size — pinned to one
  core via `taskset` vs. unrestricted; Linux-only, since there is no worker-count env/flag to pass
  instead and macOS has no affinity equivalent `num_cpus` honors).
- **CI job:** `.github/workflows/bench.yml` — one `ubuntu-latest` job, runs on every push to `main`
  and on pull requests that touch a perf-sensitive path (`crates/forklift-core/src/**`,
  `crates/forklift/src/commands/**`, `bin/bench.sh`). Uploads `bench-results.json`/`.md` as a build
  artifact — **that is where the numbers live**; they are not committed to the repo (a fixed corpus
  regenerated every run keeps them comparable run-to-run without needing a tracked baseline file).
- **Gating philosophy — deliberately narrow.** A CI runner is noisy and shared; an absolute-ms
  baseline gate on it is flake theater. The job gates only on a handful of *same-run ratio* checks
  (repack ≤ 1.5× a fresh `compact`, per D/P3 above; unrestricted `audit` ≤ 2× `audit` pinned to one
  core) plus generous absolute ceilings sized to catch an order-of-magnitude regression (an
  accidental O(n²), a broken fan-out) and nothing tighter. Absolute-time trend-watching is left to
  a human reading the uploaded report over time, exactly as this plan's hand-run numbers have been
  — the gate's job is only to catch the kind of regression a noisy runner cannot hide.
