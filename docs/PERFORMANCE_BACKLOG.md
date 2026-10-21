# Performance backlog

A per-command follow-up to [`BENCHMARK.md`](BENCHMARK.md), [`PARALLELIZATION_PLAN.md`](PARALLELIZATION_PLAN.md)
and [`OBJECT_STORE_SCALING.md`](OBJECT_STORE_SCALING.md). Those documents cover the
structural, store-wide levers (packing, the commit-graph, parallel fan-out); this one
is narrower — a 2026-07-15 phase-timing investigation into per-command overhead on
git.git, and what is left to do about it after the obvious quick wins.

## What was measured

**Corpus and baseline:** git.git (81,348 commits, 4,775 files), warm cache, a
densified `.forklift` store (packed, path-aware deltas — the state `compact`
produces, see `OBJECT_STORE_SCALING.md`). Forklift 0.2.1 vs git 2.33.0, both run on
the same box. Before any fix in this document, the benchmark ratios (forklift ÷ git,
lower is better) were:

| Command | Ratio (forklift / git) |
|---------|------------------------|
| `status` (`stocktake`) | 0.73× (forklift already faster) |
| `log` (`history`) | 1.08× |
| `diff` | 2.41× |
| `commit` (`stack`) | 2.84× |

**Method.** Two complementary techniques, not a profiler run on the whole binary:

- **Phase-timing instrumentation** — coarse `Instant::now()` brackets dropped around
  each command's named phases (inventory load, tree walk, render, write, fsync, …),
  run on the real git.git corpus, to see where within a single invocation the wall
  clock actually goes.
- **Scaling-law experiments on synthetic corpora** — the same phases measured across
  a range of directory counts / file counts / commit depths on generated warehouses,
  to tell an O(1) fixed cost apart from an O(n) one that merely looks small at git.git's
  scale, and to confirm a suspected linear relationship (e.g. shard count) actually is
  one rather than an artifact of this one corpus.

**Fixed per-invocation overhead was ruled out.** It is tempting to blame process
startup — CLI argument parsing, TOML config load, warehouse-root discovery — for a
chunk of every command's time. Measured floor: forklift's own no-op path costs
**~7–8 ms**; git's is **~11–14 ms**. Forklift is not behind here, it is ahead — and
that git figure should be read generously in forklift's favor, not the other way:
the git binary measured then was **x86_64 running under Rosetta**, which
inflates every git measurement, most visibly on syscall-heavy operations (many small
`read`/`stat` calls each pay the translation tax). So the true native-vs-native gap
on fixed overhead likely favors forklift by even more than 7–8 vs 11–14 ms suggests.
*(Correction, 2026-10-17: this applies to the figures in this document, which were
taken against the Rosetta git 2.33.0. Later the same day (2026-07-15) `BENCHMARK.md`
was re-measured against native arm64 git 2.55.0, and every ratio there since is native
against native. The fixed-overhead figures above were not re-measured natively.)*
Whatever is making `diff` and `commit` slower than git is not a fixed floor — it is
work that scales with the corpus, per command, addressed below.

---

## Fixed on the quick-wins branch

Four fixes landed on `perf-quick-wins`, none structural — this section exists so the
doc reads correctly once that branch merges (numbers below either measured on that
branch or expected; `BENCHMARK.md` was re-run in full on 2026-10-17, see below).

1. **`BufWriter` on `history`/`query`'s streaming stdout.** Both commands were
   writing to `stdout` one record at a time, unbuffered — a syscall per printed
   line at git.git's 81k-commit depth. Wrapping the writer in a `BufWriter` took
   `history` from **962 ms → ~0.42 s**, which was **~2× faster than the Rosetta git**
   measured then. Against native arm64 git 2.55.0 the two are level (558 ms vs 565 ms
   on git.git, 2026-10-17).
2. **Collapsing `stack`'s three full inventory-shard scans into one.** `stack` was
   walking every shard three separate times (once each for staged detection, tree
   build input, and post-stack cleanup); collapsed into a single pass. Expected
   **~88 ms → ~45–60 ms**.
3. **Batching `stack`'s per-object fsyncs into one durability barrier.** Each newly
   written object was fsynced individually; now the objects are written, then
   fsynced as one barrier before the ref update (same durability guarantee — every
   object is still fsynced before the ref that points at it — just not one syscall
   per object). Folds into the same `stack` improvement above.
4. **Parsed-tree cache + invocation-scoped shard reuse in `diff`'s render loop.**
   `diff`'s tree-vs-tree walk was re-parsing and re-loading the same subtrees/shards
   it had already touched earlier in the same invocation. Expected **~53 ms → ~43 ms**.

None of these touch on-disk format, durability ordering (beyond the batching in #3,
which preserves fsync-before-ref-move), or output content — they are constant-factor
wins on paths that were already correct.

---

## Fixed on the rollup-hash branch

Landed on `perf-rollup-hash`: the structural fix from the "highest value" item this
document originally listed as remaining. Numbers below are measured on that branch;
`BENCHMARK.md` was re-run in full on 2026-10-17 (hyperfine, native arm64 git 2.55.0,
Forklift 0.3.1): status 1.40x, log 1.01x, diff 2.2–2.4x, commit 3.28x against git's
defaults and 1.55x against `core.fsync=all`.

**Mechanism.** Each inventory shard's header can now carry an optional rollup hash
(format `v2026_07_15`) — the tree hash `stack` would build for that shard's entire
staged subtree, directly comparable against the corresponding head subtree's hash.
`walk_directory_staged` (`stocktake`/`diff --staged`) and `stack`'s tree-build skip
plan (now worked out during the shard read itself, see FORK-67 below) both skip load+parse+descend for a subtree the
moment its shard's rollup matches the head subtree hash — the same short-circuit
`diff`'s pallet-vs-pallet walk already had
(`crates/forklift/src/commands/diff.rs:584-586`), now extended to the staged/stack
side. These are the two O(all tracked directories) walks this item originally
described.

**Stamp / invalidate rules.** A rollup is written whenever a shard is materialized
straight from a known tree hash: the materialize-from-tree paths (`shift`,
`park`/pop, `restore`, `consolidate`'s fast-forward) and `stack`'s post-cleanup
stamp; a shard that is merely re-verified with no content change (a pure stat-only
refresh) carries its existing rollup forward unchanged. Every content-mutating
writer funnels through a single maintenance path that clears the rollup of every
ancestor shard, root-first, and makes that clear durable *before* the mutated
shard's own write becomes visible — so a crash between the two can only ever lose
skips (falling back to a full walk), never leave a stale-valid rollup sitting above
genuinely changed content. A shard with no rollup — old-format or never stamped —
is treated as "unknown" and falls back to the pre-existing unconditional load; no
migration is required to keep reading an older store.

**Measured — synthetic 300-directory corpus, one file touched, main vs branch:**

| Command | main | branch |
|---|---|---|
| `stocktake` | 34 ms | 27 ms |
| `diff --staged` | 17 ms | 8 ms |
| `stack` (steady state) | 45 ms | 50 ms |
| `load .` | 68 ms | 43 ms |
| `load .` (no-op) | 54 ms | 33 ms |
| first `stack` after upgrade (one-time re-stamp) | 142 ms | 250 ms |

**Measured — git.git (81,489 commits, 4,775 tracked files, 224 tracked
directories), densified store, main vs branch:**

| Command | main | branch |
|---|---|---|
| `stocktake` | 50 ms | 29 ms |
| `diff --staged` (2 files staged) | 46 ms | 28 ms |
| `stack` (steady state, 1 file touched) | 63 ms | 57 ms |

`stocktake` and `diff --staged` drop by roughly the fraction of directories left
untouched, tracking the synthetic corpus's ratios. `stack` improves here (63→57 ms)
rather than showing the small regression the synthetic corpus does (45→50 ms) —
plausible given `stack`'s per-write overhead is closer to fixed-cost, so it shows up
more against a 300-directory synthetic corpus than against git.git's 224.

**The one-time cost.** The first `stack` after upgrading a warehouse to this format
pays to freshly stamp every shard that has never carried a rollup before — measured
142→250 ms on the synthetic corpus. This is paid once per warehouse, not once per
invocation.

**Two fixes shipped alongside it:**

- **Atomic shard writes**, closing a real crash-corruption hole. The higher
  shard-write volume this change introduced (an ancestor chain touched on both
  `load` and `stack`'s cleanup) exposed a latent bug: `save_inventory` was a plain
  `std::fs::write`, not the store's usual temp-file/fsync/rename contract — a crash
  mid-write could leave a torn shard. `crash_consistency.rs` caught it; every shard
  write now goes through the same atomic contract as every other state file in the
  store.
- **`load .` got faster than main**, not just neutral. Batching the maintenance
  funnel's per-shard durability barriers into a single-threaded, two-phase join
  point (at most two fsync barriers per `load`, not one per touched ancestor)
  benefited plain `load .` as a side effect: 68→43 ms on the synthetic corpus,
  faster than main despite doing strictly more bookkeeping (maintaining rollups)
  than main's `load` ever did.

**Adversarial review.** This shipped after an adversarial review pass, not just
tests. The review caught one critical bug: an early version of the maintenance
funnel (`pre_clear_rollups_for_load`) unconditionally wiped every rollup in the
loaded scope on every `load` — for `load .`, the whole warehouse — which meant the
standard `load .; stack` workflow got **zero benefit** from the entire feature
(every shard's rollup was cleared before `stack` could ever read it). Verified fixed
with a skip-count A/B: 0 shards skipped before the fix, 2 after, on the same
`stack` → `narrow load` → `stack` repro, with and without an interposed no-op
`load .`.

---

## FORK-183: `diff`, `diff --staged` and `stocktake` on git.git (2026-10-17)

**Baseline** (`bin/benchmark --repo git`, fresh work dir, native arm64 git 2.55.0, before
any fix below): status 15/21 ms (1.40×), diff unstaged 13/29 ms (2.23×), diff staged
9/20 ms (2.22×).

**Method.** A release build with debug symbols (`CARGO_PROFILE_RELEASE_DEBUG=true`, separate
target dir), sampled with `samply` at 4 kHz over 150 back-to-back runs per command. Three
steady copies of the benchmark's B were used: clean, 20 unstaged, and 20 staged. Each was
re-`load`ed after copying, because `cp -a` changes every ctime, and stocktake never persists
the stat refresh (FORK-182), so without it every file rehashes on every run. Off-CPU time
isn't sampled on macOS, so wall-clock attribution also came from `hyperfine` A/Bs and from
`--json` variants, which skip the content render. The floor (`forklift version`) is ~3 ms.

### Breakdown and what was done

| Cost | Measured share | Fix | Before → after |
|------|----------------|-----|----------------|
| Range sort in `validate_index_records`, on every process's first pack open | 5.1 of its 5.8 ms; ~50–75% of in-process main-thread time in stocktake, diff and diff --staged | **Fixed:** LSD radix sort (11-bit digits; three passes for any pack under 8 GiB). Same checks, same order. | stocktake 20.4 → 17.8 ms |
| Second, symlink-following `stat` per file (`file_id::get_file_id`) in the unstaged walk | 5.4 of ~28.7 ms walk CPU (~19%) | **Fixed:** device/inode from the `lstat` already in hand. A symlink now carries its own inode. This also fixed `load` aborting on a dangling symlink. | stocktake 17.2 → 16.4, diff 20.4 → 18.6 ms |
| stocktake ran the staged side (head pack open + staged walk) and then the unstaged walk | the staged side's whole wall time | **Fixed:** `tokio::join!` of the two. The unstaged tasks fan out while the command task opens the pack. | stocktake 16.1 → 12.1 ms clean, 17.2 → 11.8 ms staged |
| diff's first blob lookup opened the whole pack set (5 packs, ~500k records) after the walk | ~4.4 of the 7 ms render | **Fixed:** the unstaged walk preloads the pack set on a background thread once a directory reports a change the diff will render (inside its path filter, not untracked), and returns when it lands. A clean, untracked-only or out-of-filter walk never starts it. | diff 18.7 → 15.0 ms dirty; 11.3 → 11.1 ms clean |
| Serial per-file render in the worktree and staged modes | most of the 8.5 ms by which `diff --staged` exceeds its `--json` form (loose head after the benchmark's commit rows) | **Fixed:** both modes render through `fanout_map` past 8 files, like the pallet diff. Output is byte-identical, and a file that fails to render still prints every file before it, then the error. | diff --staged 17.5 → 12.5 ms, diff 14.7 → 13.4 ms |
| Verification of every intermediate base in a delta chain | ~20% of the staged render's main thread (blake3), plus zstd ~13% and delta apply ~14% | **Fixed (FORK-188 below):** a delta chain is reconstructed with unverified bases and only its target is hashed. | diff --staged 15.9 → 12.0 ms |
| One task per directory in the unstaged walk (t/ has 1129 files, the root 531) | walk ~7 ms wall for ~28 ms CPU over 6 P-cores | **Measured, not done (FORK-189 below):** ranges plus subdirectories-first saved ~0.7 ms median for +43% CPU and a fatter p90. | none |
| Residual pack-set open: index reads (~24 MB `read_to_end`) plus the linear validation pass (~0.7 ms) | ~4.4 ms per process wherever it can't overlap (e.g. `diff --staged`, whose first act is reading the head parcel) | **Fixed (FORK-118):** index format 3 — a checksummed fanout table; the index is mapped, and a command validates only the buckets it reads. | history -n 1 7.5 → 3.4 ms, diff --staged 7.4 → 3.3 ms |
| Line interning hasher (`diff::lines`, SipHash) | ~0.2–0.6 ms per 20 files | Not worth a ticket alone. It now runs fanned out, so the wall-clock share is smaller still. | < 0.3 ms |

**FORK-163.** This change covered that ticket's `diff` sites. Its materialize half is the
FORK-163 section below, and the rest is FORK-192.

**Found on the way:**
- FORK-187: `import-git` records git's LF blob against the stat of a CRLF-converted working
  file (`eol=crlf`), so the file reads modified once its stat cache misses.
- FORK-182: the stat refresh stocktake never persists, which made copied warehouses rehash
  everything.

No durability barrier or verification step was removed. The radix sort kept every check
the then whole-index validation made (FORK-118 has since replaced that pass with per-bucket checks); the preload reads the same packs, and a failed preload is dropped
so the first real lookup reports it as before.

**After** (`bin/benchmark --repo git`, fresh work dir, 2026-10-18):

| Operation | git | forklift before | forklift after | ratio before → after |
|-----------|-----|-----------------|----------------|----------------------|
| status (clean tree) | 16 ms | 21 ms | 14 ms | 1.40x → 0.88x |
| diff, unstaged (20 files) | 13 ms | 29 ms | 16 ms | 2.23x → 1.23x |
| diff, staged (20 files) | 9 ms | 20 ms | 12 ms | 2.22x → 1.33x |

The log and commit rows didn't move beyond noise (0.99x, 3.11x). The git-side numbers drift by
about 1 ms between runs.

## FORK-185: commit-graph write amplification on a cold build (2026-10-18)

**Measured** on the benchmark's git.git warehouse (24,423-parcel first-parent chain), with
`.forklift/graph` removed and `forklift blame Makefile` run once. Writes before the fix were
counted with a throwaway build that logged each shard write; after it, with the test-only
`FORKLIFT_DEBUG_GRAPH_WRITES` counter.

- **Before:** 105,037 shard writes, 1.37 GB written, to leave a 7 MB graph. One shard was
  rewritten 480 times. Blame walks its chain oldest first and filled the graph one query at a
  time: each step persisted the `ensure` for the next parcel's few missing ancestors, then that
  parcel's changed-path filter, and every persist rewrites whole shards.
- **Fix:** `graph_utils::ensure_path_filters`, which blame calls on its chain before the walk.
  One `ensure` from the newest parcel records the whole ancestry at once. The missing filters
  are then computed in parallel (`fanout_map`) and persisted 4,096 at a time, so each shard is
  rewritten once per batch. A finished batch is on disk, so a walk cut short by a closed pipe
  or a Ctrl-C keeps everything up to its last batch. The pass sits in core, so the server and
  the MCP head's blame get it too.
- **After:** 1,792 writes (7 per shard: one for the records, one per filter batch), about
  50 MB written. The blame output is byte-identical.

| Cold `blame Makefile` (hyperfine, 3 runs, graph removed before each) | before | after |
|---|---|---|
| wall time | 50.8 ± 0.4 s | 15.5 ± 0.1 s (3.3x) |
| peak memory | 1.5 GiB | 1.7 GiB (+9%) |

Peak memory rose because the filter workers each hold the trees they diff. A cold blame now
costs about what a warm one does (16.5 s). That remaining time is blame itself,
attributing the versions that touch the file: git blames the same file in 0.68 s, which is
FORK-190.

The same change fixed a latent dirty-tracking bug. A shard's version was a per-shard counter
reset to 0 on load, so a shard evicted and reloaded between a persist's snapshot and its
clean-mark could climb back to the snapshot's number and be marked clean with a record that
was in no file. Versions now come from one process-wide counter.

## FORK-163: materializers load ahead of their writes (2026-10-18)

**Measured** on the benchmark's git.git warehouse (copy C, delta-packed), with hyperfine
A/B tests against saved binaries on a quiet machine.

- **Before:** every materializer looped `load_blob` then write, one file at a time. On a packed
  warehouse, the load reconstructs a delta chain, which is CPU that the loop paid between writes.
  Parallelizing load and write together was already measured as a loss, because of the
  filesystem metadata wall (docs/PARALLELIZATION_PLAN.md, item 2).
- **Fix:** `fanout_utils::prefetch_in_order`. Workers load in a bounded window (256 items, 64 MiB
  held) ahead of a single consumer, which writes **serially and in input order**. That consumer
  is `shift_utils::apply_ops_each` (shift, park, lower, franchise, consolidate's fast-forward) or
  `consolidate::apply_merge_actions` (take-theirs writes). Below 8 writes it stays fully serial.
  Chunked entries still stream from the store. A load error is reported at that file's turn,
  after every earlier file has been written, which is the same first failure as the serial loop.
  A worker panic reaches the caller.

| `shift` on git.git (hyperfine) | before | after |
|---|---|---|
| full tree, `old` → `master` | 1.201 s | 0.497 s (2.4x) |
| 83 changed files, `near` → `master` | 182.9 ms | 150.7 ms (−18%) |
| peak memory, full tree | 676 MiB | 740 MiB (+9%) |

The memory rise does not depend on the window: a window of 32 measured the same. Remaining
work, a stat-clean worktree read and the other commands' serial loads, is FORK-192.

## FORK-188: delta chains hash only the object asked for (2026-10-18)

**Measured** on a v3 git.git warehouse with 24 staged files, using hyperfine A/Bs against a saved
release build of main. A throwaway build that skipped every base's hash check first sized the
prize at ~4 ms, twice the profile's estimate.

- **Before:** `resolve_record` hashed every object it reconstructed, and a delta's base was
  fetched through the ordinary verified read. So a chain of depth d paid d + 1 blake3 passes, one
  over each intermediate base's full bytes.
- **Fix:** a delta read a caller asked for reconstructs its chain with *unverified* bases and
  hashes only the result. A base is used only as its delta's dictionary, and no wrong base can
  yield bytes that hash to the target's address. Bases built this way live in a separate cache,
  so the read cache still holds only bytes that verified, and a base read directly is still
  hashed. If the fast attempt fails, the read is redone with every base verified, which is the
  old read, and its outcome is returned. Errors therefore still name the corrupt object.
- **Found on the way** (an adversarial review of the design): a read inside a reconstruction
  forced a full pack-registry reload at every chain level, doubling per level. A corrupt chain
  root cost 2^depth reloads (1023 at depth 10, pinned by a test). Only the outermost read reloads
  now. Also, `compact --all` reconstructs a copied delta whose base it repacks from a different
  copy, so an object that reads only against a corrupt base's stored bytes can't be dropped
  without ever being read.

| git.git (hyperfine) | before | after |
|---|---|---|
| `diff --staged`, 24 files (median) | 15.9 ms | 12.0 ms (−25%) |
| `diff --staged` CPU | 50.5 ms | 38.2 ms |
| `blame README.md` (mean) | 361.9 ms | 284.7 ms (−21%) |

Output is byte-identical, and memory is unchanged.

**Where the benchmark stands** (`bin/benchmark --repo git`, fresh work dir, after this change and
FORK-118): `diff --staged` 8 ms git / 7 ms forklift (0.88x, down from 1.33x after FORK-183);
status 0.79x; `diff` unstaged 1.25x. The FORK-183 → now drop folds in FORK-118's lazy index; the
A/B above is the part that is FORK-188's.

## FORK-189: splitting the unstaged walk's large directories — measured, not worth doing (2026-10-18)

**Measured** on a v3 git.git warehouse, clean tree, with a throwaway build that timed
`executor.execute` around the walk, and hyperfine A/Bs against a saved release build of main.

- **The critical path is t/.** Its directory task (1129 files) started ~1.5 ms in and ran
  5.7–6.3 ms; the walk ended ~50 µs after it. The root (531 files) ran 2.6 ms. Walk median
  6.62 ms.
- **Tried:** classifying a directory's files in 128-entry ranges, each its own task, with the
  last range to finish running the directory's reconcile (removals, missing subdirectories, the
  shard memo, the pack preload). Holding each `DirEntry` cost ~1 KiB apiece on macOS (+8 MiB, and
  slower), so ranges hold a name and file type only. Also tried: dispatching subdirectories
  before classifying any file, so they start earlier.

| clean tree | main | ranges + subdirectories first |
|---|---|---|
| walk only (median) | 6.62 ms | 5.70 ms |
| stocktake (median) | 10.5–10.7 ms | 9.75–9.81 ms |
| stocktake (p90) | 11.6–12.3 ms | 14.9–16.6 ms |
| stocktake CPU | 52.6 ms | 75.4 ms (+43%) |
| diff (median) | 10.4 ms | 9.66–9.84 ms |

Ranges alone (CPU 69 ms) and subdirectories-first alone (CPU 70 ms) each gave no wall-clock
gain on their own. System time rose from 36 to 57 ms: parallel `lstat` in one directory contends
in the kernel. A ~0.7 ms median for a fatter tail and ~45% more CPU is not a trade worth making,
so the walk stays one task per directory.

---

## FORK-67: `stack` reads only the shards beneath a changed rollup (2026-10-18)

**Problem.** `stack` read and parsed every registered inventory shard before building the
tree, and only then worked out which subtrees the rollup skip let it leave alone. The skip
saved the tree build, not the read. On a 20,421-directory synthetic warehouse (20 × 20 × 50
leaf directories, two files each), a one-file `stack` spent ~150 of its ~205 ms (fsync off)
reading shards it then ignored; on git.git's 224 directories the same read is a few ms.

**Fix.** `prepare_stack_inventory` now takes the head and the bay's scope and walks the shard
tree top-down, one depth at a time, reading each depth (and loading that depth's head trees)
through `fanout_map`. A shard whose rollup equals the head subtree hash becomes a skip root and
nothing beneath it is read; the skip plan the tree build used to compute afterwards is the
walk's by-product. The cleanup after the stack leaves every key beneath a skip root alone, so
the read set and the rewrite set are both O(changed spine), not O(directories).

**A bug fixed on the way.** The old cleanup unregistered a shard whenever its directory was
gone from disk, even when the removal had never been loaded and the parcel just stacked still
contained the directory. It also stamped the ancestors' rollups as if nothing were missing, so
`stocktake` then reported nothing staged and the *next* `stack` silently committed the removal.
Whether a key goes is now decided from the inventory alone; the removal stays reported as not
loaded until someone loads it.

**Measured** (hyperfine, 20–30 runs, one file appended and loaded before each run):

| Warehouse | before | after |
|---|---|---|
| 20,421 directories | 253 ms | 72 ms |
| 20,421 directories, `FORKLIFT_FSYNC=0` | 211 ms | 28 ms |
| git.git (densified store) | 51.2 ms | 50.0 ms |
| git.git, `FORKLIFT_FSYNC=0` | 10.1 ms | 8.6 ms |

When nothing can be skipped (every leaf changed, all 20,421 shards read), the depth-by-depth
read costs ~240 ms against ~225 ms for the one-shot read, inside a ~7 s stack dominated by its
writes.

## FORK-173: the per-write path checks — measured, not worth memoizing (2026-10-18)

Every materializer write or removal passes `check_working_path`. That function makes two checks:

- **The top-level name.** It refuses a path whose top-level name opens the metadata folder. For a
  top-level name holding a `~`, `opens_metadata_folder` also asks the volume whether the name
  resolves to `.forklift`. Only that first component is probed.
- **The ancestors.** It `lstat`s each existing ancestor, stops at the first missing one, and
  refuses a symlink.

The ticket asked whether the ancestor walk should be memoized per directory or replaced with
`openat`.

**Measured** on macOS (APFS) with hyperfine, using release builds of af5af44, the parent of the
FORK-175 merge. FORK-175 did not touch the per-write path checks. Both 40,000-file corpora were
timed in both orders. The checked build was the slower one in every pair, and its lead grew whenever
it ran second. The git.git row was timed once, with the walk first, which is the order that
showed the smaller gap.

The ancestor walk was A/B'd against a throwaway build that skips the walk alone
(`&components[..0]`); the top-level check still runs in both builds.

| `shift`, ancestor walk | with the walk | without | cost |
|---|---|---|---|
| git.git, `old` → `master` (`old` is git's first parcel, so nearly the whole tree is written) | 400.0 ± 4.7 ms | 393.3 ± 3.8 ms | +1.7% |
| 40,000 files, six ancestors each (240k `lstat`s), the walk timed first | 4.439 ± 0.031 s | 4.422 ± 0.099 s | +0.4% |
| the same, the walk timed second | 4.568 ± 0.113 s | 4.333 ± 0.073 s | +5.4% |

Averaged over both orders, the deep corpus pays about 2.9%, or roughly 0.5 µs per `lstat`.

The volume probe was measured on two 40,000-file corpora with 100 top-level directories of 400
files each: `t~00`…`t~99` against `t_00`…`t_99`. Every write in the first corpus pays a probe.

| full-tree `shift`, volume probe | `t_NN` | `t~NN` | cost |
|---|---|---|---|
| `t~NN` timed first | 2.135 ± 0.033 s | 2.191 ± 0.019 s | +2.6% |
| `t~NN` timed second | 2.131 ± 0.070 s | 2.280 ± 0.099 s | +7.0% |

The two orders put the cost between 1.4 and 3.7 µs per probe; the average is about 4.8%. Which
writes pay depends on the top-level entry:

- a top-level directory whose name holds a `~` makes every write beneath it pay;
- a top-level file such as an editor's `README~` pays for its own write only.

**Not measured: Windows.** There, every `lstat` costs more, and the probe compares file ids through
an opened handle. It is also where 8.3 short names exist.

**Not done.** Projected from the deep corpus, a checkout of 200,000 files six levels deep spends
about 0.6 s of ~22 s in the walk, about 3%. A memo would save that 3%, but it would add an
invariant that every write path must keep, and it would not close a race.

The check already leaves a window between the `lstat` and the write. That window covers loading
the blob, assembling a chunked file, and creating the missing parent directories, which the walk
never checks. A memo would only widen the window to the whole command. `openat` relative to a
verified directory handle is what would close it. That rewrites every write sink and is a safety
change, not a speed one, so it should be filed on its own if it is wanted. The probe's few percent
falls only on trees whose top-level names hold a `~`.

## FORK-201: the nested working-tree checks — measured (2026-10-20)

FORK-201 adds three things to the paths above:

- `check_working_path` now tests every component's name, not only the top-level one, and `lstat`s
  `<ancestor>/.forklift` for each existing ancestor, refusing a path below another working tree's
  root;
- every materializer runs `check_planned_paths` over all its paths before the first write. That
  pre-pass makes the same name and root tests against the working directory as it stands, and it
  remembers each ancestor directory, so it costs two `lstat`s per directory, not per file;
- the load and stocktake walks `lstat` `<dir>/.forklift` once for each directory the rules leave
  them to walk.

**Measured** on macOS (APFS) with hyperfine, release builds of 24b0dc1 against the branch. The
corpus was 40,000 files in 2,000 directories six levels deep, with `FORKLIFT_FSYNC=0`:

| | 24b0dc1 | FORK-201 | cost |
|---|---|---|---|
| `load .`, nothing changed | 410.5 ± 2.9 ms | 413.9 ± 7.2 ms | +0.8% |
| `stocktake`, clean | 310.9 ± 27.2 ms | 320.1 ± 13.5 ms | within noise |
| `shift` round trip, the whole tree written then removed | 7.449–7.513 s | 7.680 s | +2.4–3.1% |

For the shift row, a throwaway build that skips the root test in the per-write backstop alone came
in at 7.498–7.523 s, level with 24b0dc1, so the remaining cost is that one `lstat` per ancestor per
write: about 400,000 of them, at roughly 0.5 µs each. An uncached pre-pass had cost about 7%.

**Not done.** Dropping the root test from the backstop would recover those 2–3%. It can only fire
there on a concurrent change, because the pre-pass refuses every `.forklift` name, so no operation
writes one. But dropping it would require every writer, `restore` included, to run the pre-pass,
and that is the kind of invariant FORK-173 declined for a 3% walk.

---

## Remaining items

### b. Pack-index validation tax — fixed (FORK-118, 2026-10-18)

Every process used to read every pack index in full and validate every record on its first
packed-object read: ~6.4–6.9 ms warm (up to ~19 ms cold) before FORK-183's radix sort, ~4.4 ms
after it. Index format 3 adds a fanout table whose entries carry a crc32 of their bucket's
records. Opening a pack maps its index and checks only the header and the table; a lookup
validates the one bucket it reads (crc, order, prefix, bounds) and caches the verdict. A corrupt
bucket makes only its own prefix unreadable. Consumers that read every record — transport import,
`compact --all`, `store` — validate the whole index, tiling included, first.

**Measured** (git.git, densified, 5 packs / 409k objects, hyperfine, release, c245fcf → this
change):

| Command | Before | After |
|---------|--------|-------|
| `history -n 1` | 7.5 ms | 3.4 ms (`palletize`, which reads no object: 3.3 ms) |
| `diff --staged` | 7.4 ms | 3.3 ms |
| `stocktake` | 11.5 ms | 10.7 ms |
| `store` | 15.0 ms | 17.4 ms — the census now checks every bucket's crc and the tiling |

### c. History walk memory at scale (minor)

`history`'s unbounded walk accumulates a `HashSet`/`HashMap` entry per parcel
(`crates/forklift/src/commands/history.rs`) — tens of MB at 1M commits. Not measured
as a *speed* problem, and irrelevant for the common case (`-n`/pager-bounded runs,
which only touch the frontier). Recorded as a note for whenever an unbounded
full-history walk at kernel scale is exercised for real, not something to act on now.

### d. Diff head-blob delta decode

**Update (FORK-183, 2026-10-17).** The decodes now fan out across the cores (see the FORK-183
section). The per-chain cost that remains is mostly verifying every intermediate base, which is
FORK-188.

~5.5–5.9 ms per 20 files reconstructing a blob from its delta record against the
densified pack's stored base. This is genuine reconstruction cost — the store trades
size for exactly this at compaction time (see `OBJECT_STORE_SCALING.md` §A) — not
overhead to eliminate. Only worth revisiting if `diff` is still hot after (a) lands;
until then this is the store working as designed, not a bug.

### e. Durability-barrier audit (write-batching sweep)

**Status: audit complete (2026-07-15, on main @ 18a04f9); findings 1 and 3 shipped
(PR A, 2026-07-16), then hardened by a second, multi-agent review of PR A itself
(2026-07-16 — 10 findings, all resolved: #1 data-loss fixed, #2 dedupe fixed, #3
docs corrected against a verified `gc` mitigation, #4 invariant comment fixed, #5
`park`'s read-phase parallelism restored, #6 mtime-anchoring fixed, #7 blob/shard
ordering fixed for `load` (refuted for `park`, verified separate barrier), #8 dead
`Mutex` traffic removed, #9 dead code deleted, #10 barrier-count now asserted by a
test); findings 2 and 4 (the shared multi-mutation join-point primitive) still
queued for PR B.**
Three unrelated pieces of work had converged on the same cost in short succession —
quick-win #3's `stack` fsync batching, the rollup-hash work's two rounds of
shard-write batching, and PR #59's benchmark verification surfacing unbatched-barrier
costs in `park`/`park` pop — so the audit sized the remaining problem before any
further patching.

**Method + validation.** Every barrier site was swept: `write_file_atomically` call
sites, `WriteBatch` construction, every raw `fsync`. None exist outside
`file_utils`'s primitives — no hand-rolled temp-file-plus-rename anywhere in the
tree. `F_FULLFSYNC` measured ~4 ms/call on the dev box, giving a ~8–9 ms
per-unbatched-atomic-write model (file fsync + directory fsync). Every measured
delta below matches the N × ~9 ms model within noise, which validates the model
itself as well as the findings.

**Findings, ranked** (measured on synthetic corpora, release builds):

1. **FIXED (PR A, hardened by review round 2). `load`'s per-file blob store**
   (`inventory_utils::build_inventory`) — ~7–8 ms per changed file (N=50 `load`:
   ~376 ms total, ~326 ms of it this). The rollup-hash join point batched shard
   writes but never the blob stores; fixed by staging each changed or brand-new
   file's blob (`LooseObject::store_deferred`) into a batch — durable no later than
   the shard that references it. Re-measured on this box (N=50, synthetic corpus,
   7-run median, release build): **413 ms → 37 ms** (≈11×), unchanged after review
   round 2's fixes below. Highest value, lowest risk — `load` is the most-run
   mutating command.
   * **Round 2, finding #7 (ordering).** The blob batch and the join point's shard-content
     batch turned out *not* to be safe to merge into one `WriteBatch`/one `run_write_barrier`
     call the way PR A shipped it: `touched_parents` there is a `BTreeSet<PathBuf>`, so
     `.forklift/inventory/` directories sort (and fsync) before `.forklift/objects/` ones —
     a crash between the two could durably publish a shard naming a blob whose own rename
     never became durable. Fixed by giving the blob batch (`InventoryBuilderContext::blob_batch`)
     its own barrier, finished strictly before the join point even stages shard content — three
     barriers total per `load` now (blob, ancestor-clear, shard-content) instead of two; SIGKILL
     tests structurally cannot catch this ordering hazard (a kill leaves all renames
     kernel-visible regardless of fsync order), so this was verified by code inspection, not a
     crash test.
   * **Round 2, finding #2 (dedupe).** Batching defeated `store_deferred`'s existence-based
     dedupe: `does_object_exist` cannot see a write staged earlier in the *same*, not-yet-finished
     batch, so N occurrences of identical content each independently decided "not on disk yet"
     and staged their own full compressed temp — a load with heavy content duplication could
     turn ~1 MB of real data into tens of GB of staged temps, and fail with ENOSPC on a load that
     used to succeed. Fixed with `WriteBatch::reserve_final_path` — an atomic check-and-reserve
     on the batch's final-path set, consulted (and populated) before compressing — so only the
     first occurrence of a given hash in a batch ever stages anything; a 50-occurrence regression
     test (`store_deferred_dedupes_repeated_identical_content_within_one_batch`) failed with 50
     staged temps before the fix, passes with exactly 1 after.
2. **FIXED (FORK-196, FORK-112 slice 3). `consolidate`/cherry-pick's per-merge-action
   funnel** (`apply_merge_action`) — measured ~9.1 ms/action; a 50-action merge was ≈570 ms,
   confirming this doc's original ~100-barrier prediction. Every merge action now stages
   through one `ShardMutationBatch` (see item 4), published before the caller records the
   merge as in progress; the barrier count no longer grows with the action count. bench112,
   50 actions over 5 directories: `consolidate` 882 → 85 ms; 30-action `cherry-pick`
   556 → 78 ms. A `Merged` action's blob goes into a write batch the shard batch owns, which
   its one publish finishes in one barrier before any shard naming those blobs is published. A
   failed action stages nothing (conflict entries staged with no merge in progress could be
   neither completed nor aborted); either failure records nothing as in progress and says that
   some or all of the applied changes are unstaged.
3. **FIXED (PR A, hardened by review round 2). `park` push** — ~8.9 ms/changed file
   across three unbatched layers: the per-file blob store inside
   `refresh_tracked_entries`, the per-directory tree-object store inside
   `tree_utils::build_tree_from_inventory` (the non-deferred variant `park` used
   to call), and the parcel object's own store. All three are now staged into one
   shared `WriteBatch` — `refresh_tracked_entries` batches its own blob stores into
   an internal barrier (finished before any shard rewrite, so a rewritten shard's
   entries never outrun the blobs they reference), and `park` was switched to
   `build_tree_from_inventory_deferred` (which also gives `park` the rollup-hash
   skip it previously lacked) and now stages the parcel object into the same batch,
   finished once before the signature sidecar and the parked-list record.
   Re-measured on this box after review round 2's fixes (fixed 8-directory corpus,
   7-run median, release build): **N=50 → 153 ms → 89 ms**, a further ≈1.7× on top
   of PR A's own ≈4.1× (main's 632 ms) — see finding #5 below, which is what moved
   this number again.
   * **Round 2, finding #1 (data loss, CONFIRMED).** `refresh_tracked_entries`'s pass-1/pass-2
     split read every tracked shard's rollup up front (pass 1), then published each in pass 2 in
     metadata order. A directory sorting *before* the root's `"./"` metadata entry (any name
     starting with a byte < `0x2E`) whose own real content change ran through the
     ancestor-clearing funnel in pass 2 correctly cleared the root's rollup on disk — but the
     root's *own*, later pass-2 write then restamped the *stale* pass-1-decided rollup right back
     over that clear, since it never re-checked whether it had just become an ancestor of some
     other change decided in the same pass. `park` then took the whole-tree rollup-skip fast path
     (the restamped rollup matched head) and refused with "nothing to park" — the real edit was
     never captured, and a subsequent `stack` would have taken the same skip and silently dropped
     it from the parcel. Fixed by reusing `load`'s join-point machinery instead of a bespoke
     second implementation: `refresh_tracked_entries` now decides every shard through the same
     `ShardOutcome`/`publish_shard_outcomes` primitive `load` does, so a carried rollup that turns
     out to be an ancestor of another shard's real change (computed from *every* decision in the
     batch, not just the ones already processed) is dropped before anything is published. A CLI
     regression test translating the review's exact repro (a `(marketing)/` directory, a real edit
     inside it, a same-content root-level rewrite) failed with the predicted "nothing to park"
     error on the pre-fix branch and passes now.
   * **Round 2, finding #6 (mtime widening, PLAUSIBLE, confirmed real).** The same pass-1/pass-2
     split published a stat-only shard rewrite (no real content change, just stale stat data) with
     "now" — the moment the *whole* refresh finished deciding every tracked shard — instead of the
     instant *that* shard was actually verified. `is_entry_unchanged`'s "racily clean" guard trusts
     a cached entry only when its own mtime predates the shard's published mtime; publishing later
     than the true verification instant widens that trust window, so a file edited in the gap
     could be silently missed forever after. Fixed by the same join-point reuse as finding #1 —
     `publish_shard_outcomes` anchors every shard's published mtime to its own decision-time
     `verified_at`, exactly like `load`'s join point already does. A dedicated (cwd-isolated)
     integration test manufactures a measurable gap by deciding 1,000 shards and checking the
     first-decided one's published mtime: unfixed, it landed **~41 ms** after the call started
     (≈ the time to decide all 1,000 shards, i.e. "now" at pass-2 time); fixed, **~0.2 ms**
     (≈ the time to decide just that one shard). *Superseded by FORK-208 (2026-10-19): entries
     now carry their own proof against a clock-probe anchor, the shard's mtime is no longer an
     anchor, and every shard publishes with "now".*
   * **Round 2, finding #5 (park's read-phase parallelism, CONFIRMED).** `park` traded a parallel
     per-directory shard read (its pre-PR-A tree build read+parsed each shard from inside its own
     `TaskExecutor` task) for a single serial `prepare_stack_inventory()` pass — an N-core-to-1-core
     regression on park's read phase in a PR whose stated purpose was making park faster. Fixed by
     fanning `prepare_stack_inventory`'s read+parse pass out across every core
     (`fanout_utils::fanout_map`), while still reproducing the exact "first conflict, or first
     parse error, in sorted key order wins" priority the serial loop guaranteed (verified by an
     existing regression test, `prepare_stack_inventory_reports_a_conflict_found_before_a_later_corrupt_shard`,
     which still passes). This is what took the N=50 synthetic number from 153 ms to 89 ms above,
     and git.git-scale park push from 166 ms (PR A) to ~156 ms (see the git.git sanity numbers
     below — the remaining cost there is the unrelated O(all-tracked-files) stat scan
     `refresh_tracked_entries` always pays inside itself, not the read parallelism this fixed).
   * **Round 2, finding #7 (blob/object ordering — verified, refuted for `park` specifically).**
     Unlike `load` (see finding #1 in item 1's entry above), `park`'s own tree/parcel object batch
     does *not* have the analogous hazard: the parked-list record (what makes a parcel *reachable*)
     is written by `park_utils::write_parked`, a separate, immediate `write_file_atomically` call
     strictly after `batch.finish()` returns — never batched with the tree/parcel objects
     themselves. So even in the crash window where some of those objects might not all be durable,
     nothing durable ever references them yet (confirmed by code inspection: `write_parked` and
     `sign_utils::store_parcel_signature` are both outside `batch`).
   * The per-*shard* (not per-object) mutation funnel inside `refresh_tracked_entries` still pays
     one barrier per touched-directory-that-needs-an-ancestor-clear — that collapse needs the same
     shared multi-mutation join-point primitive as findings 2/4, so a change that touches many
     distinct directories still costs more than one that touches a few; queued for PR B.
4. **`restore <dir>` (plain) and `park pop`** — ~9.2–9.6 ms/file; same join-point
   primitive as #2, bundles with it.
   * **`restore <dir>`: done (FORK-112, slice 1).** `inventory_utils::ShardMutationBatch` is
     the shared primitive: per-file shard edits collect in memory and publish once through
     `publish_shard_outcomes`, with a shard that only had its stat data refreshed carried as
     `Carry`. `restore .` now pays 1 barrier for 10 or 50 files (it paid 11 and 51), and no
     longer strips the rollups above the files it rewrites, so the next `stocktake` still
     skips them. Synthetic, 48 dirtied files: 721 → 20 ms. git.git, `restore Documentation`
     (982 tracked files, 200 of them dirtied): 18.6 s → 0.55 s (34×), 982 barriers → 1. Each shard is published at the moment of its last change. A caller must stat each file
     right after writing it, then call `update`; writing many files first and statting them
     later would trust an edit made in between.
   * **`park pop`: done (FORK-195, slice 2).** Its Write and Remove ops stage through one
     batch (`ShardMutationBatch::stage_file_from_stat` and `update`), published before the
     parcel leaves the parked list; the barrier count no longer grows with the file count.
     Synthetic, 48 files: 694 → 28.8 ms. A publish that fails after the working directory was
     rewritten leaves the parcel parked and says that some or all of the changes are unstaged.
   * **`consolidate`/`cherry-pick`: done (FORK-196, slice 3)** — see item 2.
5. **`remove <dir>`'s per-shard loop** — untimed, small, rare; demand-gated. (The
   commit-graph's multi-shard writes, once listed here, pay no barrier at all since
   FORK-162 — see "Best-effort state" below.)

**Confirmed already-batched (class a).** The `load` join point, `cleanup_after_stack_with`,
`replace_all`/`subtree_inventories`, and `stack`'s tree+parcel batch. `restore --staged`
measured flat (30→36 ms, N=1→20), confirming the shipped batching already works.

**No-go list** — batching may share barrier *cost*, it must never drop a
*guarantee*: signature durable before ref move; object batch durable before ref
move; ancestor clears durable before own content (phases may be shared across
callers but never merged with each other); durable-before-destructive dedup;
loose-object fsync stays; working-directory writes stay deliberately unfsynced.
PR A's changes stay inside this list: blobs join the *same* barrier as the shard
content that references them (at least as strong as "no later than"), and `park`'s
object batch (blobs/trees/parcel) is finished strictly before the signature sidecar
and the parked-list record, exactly like `stack`'s identical ordering.

**Best-effort state (FORK-162, 2026-10-17).** A second sweep looked for state that pays full
barriers although losing it misreads nothing. It found two candidates, and only one of them
qualified.

* **The commit-graph moved.** It is a cache keyed by immutable parcel hash, and it was paying
  two barriers per parcel on a cold build. A cold `blame` over 1,000 parcels paid 1,999 barriers and
  took 16–19 s with fsync, against 0.24–0.95 s without, across runs on the dev box. Shards are now published with
  `file_utils::replace_file_unsynced` (rename, no fsync), and the format went to v2 with a
  trailing blake3 checksum. The crash contract: after a power loss a shard may be old, torn,
  empty or missing. Every one of those reads as absent, or as a subset of correct records, and
  is rebuilt. A torn shard can no longer be misparsed into wrong ancestry, which is the case the
  checksum exists for. `crash_consistency.rs` pins the barrier count of a cold build at zero.
  Because a shard is only a cache, a shard that cannot be read or written (a full or read-only
  disk, a squatted path, a transient EMFILE) never fails the query. A missing or damaged shard
  reads as empty and is rebuilt over. One that fails to read for any other reason is answered
  from the object store and never written over, since the file may still be valid. A failed
  write leaves the records resident and dirty for a later retry. Shard writes run no directory sync, so they
  never consult or record a durability taint. `compact` walks through already-recorded parcels
  rather than pruning at them, so a lost ancestor shard behind a surviving head is rebuilt.
* **The undo journal stays durable.** Its push is best-effort by contract and costs about 4 ms
  of a ~47 ms `stack`, but a lost unsynced push leaves the journal *behind* the refs. `undo`
  would then try to reverse an older operation than the user's last. Since FORK-186 the gate
  compares each entry's recorded after-state (current pallet, its head, the consolidation and
  cherry-pick in progress) with the bay. An operation is journaled only when it changes one of
  those fields, so a lost push leaves the bay past the surviving entry and `undo` now refuses
  rather than misreads. Dropping the
  barrier would be defensible on that basis; it is not done here.
* **Everything else is authoritative.** That covers refs, the current pallet, consolidation and
  cherry-pick state, inventory, markers, scope, signatures, office anchors, bay records,
  config, the parked-parcel list and loose objects. The store side is covered by the no-go list above.
* **Left over:** a cold graph still rewrites a whole shard on every `persist_records` call, which
  is cheap without fsync but quadratic per shard. Measure it at git.git scale before batching.

**Implementation note.** Several loops call `update_shard` per file against the
same shard; the batching work needs to collapse same-shard actions into one
read-modify-write, not merely share one fsync across N rewrites of the same file.
PR A's `refresh_tracked_entries` rewrite does this implicitly for its blob layer (one
shared blob batch across every shard it touches); the shard-content layer itself
(one `write_shard_mutation`/`save_inventory` call per shard) is unchanged. That shared
primitive is now `ShardMutationBatch` (FORK-112): it applies every same-shard action to one
in-memory copy and publishes each shard once. `restore <dir>`, `park pop`,
`consolidate` and `cherry-pick` use it.

**git.git sanity (PR A, re-verified after review round 2).** A full first-time `load` of
git.git's working tree (4,775 files, every file brand-new — the same per-file blob-store
barrier finding 1 fixed) went **35.8 s → 0.66 s** (≈54×) under PR A; unchanged by round
2 (that fix only reordered *when* the same work becomes durable, not how much of it there
is). A git.git-scale incremental script (5 real files touched, `import-git`'d history,
release build) re-measured after round 2's fixes, median of 5 runs (1 for `stack`, which
consumes its staged input, so 6 independent touch-load-stack cycles were used instead):
`stocktake` **28.8 ms**, `diff --staged` **20.2 ms**, `stack` **55.9 ms** — no regression
on any of the three read/write paths PR A's own table claimed 22/16/54 ms for (`diff
--staged` in particular came in faster, 20.2 ms vs. the original 15-16 ms measurement's
*touch count* being different — not a like-for-like comparison, so not claimed as a further
win, just noted as no regression). `park` push at the same git.git scale: **155.7 ms**
(PR A: 166 ms) — a small further improvement from finding #5's read-phase parallelism fix,
smaller than the synthetic-corpus jump below because the dominant cost here is
`refresh_tracked_entries`'s own O(all-4,775-tracked-files) stat scan, which finding #5 did
not touch (it fixed the *tree-build* read phase, a separate step).

**Synthetic-corpus re-verification (release build, 7-run median, same fixed 8-directory
corpus PR A used).** `load` N=50: **37.9 ms** (PR A: 37 ms) — unchanged, as expected:
finding #7's extra barrier (blob barrier now strictly separate from, and before, the
shard-content barrier) did not show a measurable cost on this box for this corpus shape,
within run-to-run noise. `park` push N=50: **89.3 ms** (PR A: 153 ms, main: 632 ms) — a
further ≈1.7× from finding #5's fix restoring `park`'s read-phase parallelism, ≈7.1×
total against main.

**Barrier-count sanity.** A process-wide debug counter (`file_utils::barrier_count`,
`FORKLIFT_DEBUG_BARRIER_COUNT=1`) now backs a real assertion instead of only a manual
measurement (finding #10) —
`crates/forklift/tests/crash_consistency.rs::load_pays_a_constant_number_of_barriers_regardless_of_changed_file_count`
asserts `load`'s barrier count is identical whether 1 or 10 files change in each of 5
already-tracked directories. The count itself moved from PR A's constant 2 (phase A +
phase B) to a constant **3** (blob barrier + phase A + phase B — finding #7's fix, see
item 1 above), gated by `fsync_enabled()` like the fsync work it counts (also finding
#10). `park` push's barrier count still tracks the number of *directories* touched (not
files), unchanged by review round 2 — that residual per-shard funnel is explicitly left
for PR B (see findings 2/4).

**Correctness.** No missing-barrier bug was found anywhere in the original sweep,
but review round 2 (a second, independent multi-agent pass specifically over PR A's own
diff) found and fixed a real data-loss bug (finding #1) and a real staleness-widening bug
(finding #6) in the code that sweep produced — see the round-2 findings under item 3 above,
and "Review findings, round 2" below for the full list, including the two findings verified
and refuted. PR A's own crash-consistency coverage stands unchanged:
`crates/forklift/tests/crash_consistency.rs` gained a `load`-targeted SIGKILL spread
(`killing_load_midway_never_leaves_a_shard_referencing_a_missing_or_torn_blob`)
proving a killed `load` never leaves a shard durably referencing a blob that isn't, and a
`park`-targeted spread (`killing_park_midway_never_leaves_a_parked_parcel_referencing_a_missing_or_torn_object`)
covering the same property for `park`'s own object batch — every kill that lands after the
parked-list record becomes durable is popped right back, a real read of every tree and blob
the parcel references. Review round 2's own finding #7 note: those SIGKILL tests structurally
cannot catch a directory-fsync-*ordering* hazard (a kill leaves every completed rename
kernel-visible regardless of fsync order) — the finding #7 fix for `load` was verified by
code inspection instead, and `park`'s analogous hazard was verified absent the same way.

**Review findings, round 1 (fixed before PR A merged).** Two independent review passes over
PR A's own draft caught: (1) `refresh_tracked_entries`'s pass-1/pass-2 split had silently
dropped the keep-whatever-was-decided resilience `create_inventory_for_directory` gives
`load` on a mid-scan failure — fixed by deciding each shard through a fallible closure so
every shard decided *before* a failure still reaches pass 2; (2) `park` built a
`PreparedInventory` (whose `shards` map is silently incomplete past the first conflict entry
it finds) without the paired `has_conflict_entries_in` guard `stack_parcel` always checks
immediately after building its own — safe in practice only because the function's earlier,
decoupled `has_conflict_entries()` call already refuses before any conflict can exist, but
not a guarantee at the point `prepared` is actually consumed; fixed by adding the same guard
`stack` has. Doc comments that overclaimed the blob-staging exception covered a
chunked/large file's recipe and chunks too (it doesn't — those still store immediately,
unchanged, out of scope for this finding) were also corrected.

**Review findings, round 2 (a second, independent multi-agent review of PR A itself, once
it was posted as PR #61 — 10 findings, all resolved).** #1 CONFIRMED, data loss (see item 3 above) —
fixed by reusing `load`'s join-point machinery in `refresh_tracked_entries`. #2 CONFIRMED,
`store_deferred` dedupe defeated by batching (see item 1 above) — fixed with
`WriteBatch::reserve_final_path`. #3 CONFIRMED, the stranded-temp window widened from one
file to a whole batch on a hard kill — verified (`gc_utils`'s ordinary reachability sweep,
not a name-pattern sweeper, already reclaims a stranded `WriteBatch` temp past the grace
period exactly like any other unreferenced loose file; new tests
`gc_sweeps_a_stranded_write_batch_temp_past_the_grace_period` and
`gc_protects_a_stranded_write_batch_temp_within_the_grace_period` pin this) and the
misleading "no stale-temp sweeper" doc language this finding flagged as self-contradictory
was corrected in `file_utils.rs` and `bulk_store_session.rs`'s tests, rather than inventing
a new sweep mechanism — finding #2 also shrinks the *worst case* here from O(occurrences)
to O(distinct hashes). #4 CONFIRMED, a false all-or-nothing invariant in a comment (`finish()`
actually publishes every rename before the first failure, not none) — the comment was
rewritten to state the true, still-safe, "drop everything on any failure" behavior and why
it is conservative rather than incorrect; carried into `publish_shard_outcomes`'s own doc
comment for the same reasoning PR B will build on. #5 CONFIRMED, `park`'s read-phase
N-core-to-1-core regression (see item 3 above) — fixed by parallelizing
`prepare_stack_inventory`. #6 PLAUSIBLE, confirmed real, mtime widening (see item 3 above)
— fixed by the same join-point reuse as #1. #7 PLAUSIBLE, confirmed real for `load` (see
item 1 above; fixed with a dedicated blob barrier), refuted for `park` specifically (see
item 3 above; verified the parked-list record is a genuinely separate, later barrier). #8
CONFIRMED, cleanup — `park`'s tree build paid `Mutex` traffic for a `tree_hashes` map it
immediately discarded; `track_tree_hashes` is now a parameter `build_tree_from_inventory_deferred`
takes explicitly (`stack` passes `true`, `park` passes `false`). #9 CONFIRMED, cleanup —
`build_tree_from_inventory` (the plain, disk-reading, immediately-writing form) had zero
callers left; deleted, along with the `ShardSource`/`ObjectSink` enums it was the sole
`Disk`/`Immediate` variant of (both collapsed to their single remaining case). #10
CONFIRMED, cleanup — `barrier_count` existed but nothing asserted on it; now backed by
`load_pays_a_constant_number_of_barriers_regardless_of_changed_file_count` (see
"Barrier-count sanity" above) and gated by `fsync_enabled()` to match the work it counts.

**Recommended implementation packaging.** PR A (shipped) = findings 1+3 (low risk).
PR B (queued) = findings 2+4 (one shared multi-mutation join-point primitive +
adversarial review), plus the residual per-shard funnel finding 3 left behind.
Finding 5 is demand-gated.

---

## Deliberately NOT doing

- **Disabling or reducing loose-object fsync.** The crash-safety guarantee this
  protects: writers dedup by existence (an object that exists at its content
  address is assumed complete and correct), so a torn object write that survives a
  crash without an fsync barrier becomes **permanently invisible poison** — no
  future write will ever repair it, because nothing ever writes to an
  already-existing hash again. *Batching* fsyncs into fewer barriers (quick-win #3
  above) is fine and already shipped; *dropping* them is a durability regression,
  not a performance one, and is out of scope for a docs-only backlog like this.
- **Removing blake3 read-verification.** Roughly 4–5% of `history`'s post-fix time.
  This is a deliberate integrity guarantee — every object read is re-hashed and
  checked against the address it was fetched by (`object_utils::verify_object_bytes`)
  — that git does not pay for and does not offer. Trading it away for a few percent
  is not on the table. FORK-188 kept it whole: an intermediate delta base, used only as
  the dictionary its target is decoded against, is no longer hashed on its own, but every
  object a caller receives still is, and a base read directly is hashed then.
- **Sharing zstd dictionaries across parcel frames.** Parcels are intentionally
  stored as independent, full (never delta'd) frames — see `OBJECT_STORE_SCALING.md`'s
  "Parcels are stored full, never delta'd" note, which records that delta-compressing
  parcels was tried and made `history` *worse* (1.7 s → 27.7 s on git.git). Sharing a
  dictionary across frames reintroduces the same reconstruction-chain cost this
  design deliberately avoided.
- **Single-index inventory.** Sharding the inventory by directory is a deliberate
  RAM-scaling decision, not an oversight — see the per-directory-inventory rationale.
  A single global index would cap warehouse size by available RAM; the sharded design
  does not.

---

## Kernel-scale extrapolation (unverified — flag before quoting)

`history`/`log` was measured as linear up to 80k commits (git.git). Extrapolating
that line, post-fix forklift projects to roughly **5–6 s** at 1M commits against
git's roughly **10–11 s** — but this is an extrapolation, not a measurement: nothing
in this investigation ran a corpus anywhere near that size. Before quoting a
1M-commit number anywhere (a blog post, a sales conversation, a competitive
comparison), run it on a real ≥500k-commit corpus (e.g. `torvalds/linux`, per the
kernel-scale disk-budget note in `BENCHMARK.md`) and replace the projection with a
measurement.
