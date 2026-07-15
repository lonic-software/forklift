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
the git binary measured is **x86_64 running under Rosetta** on this box, which
inflates every git measurement, most visibly on syscall-heavy operations (many small
`read`/`stat` calls each pay the translation tax). So the true native-vs-native gap
on fixed overhead likely favors forklift by even more than 7–8 vs 11–14 ms suggests.
Whatever is making `diff` and `commit` slower than git is not a fixed floor — it is
work that scales with the corpus, per command, addressed below.

---

## Fixed on the quick-wins branch

Four fixes landed on `perf-quick-wins`, none structural — this section exists so the
doc reads correctly once that branch merges (numbers below either measured on that
branch or expected, and `BENCHMARK.md` will get a full re-run once it does).

1. **`BufWriter` on `history`/`query`'s streaming stdout.** Both commands were
   writing to `stdout` one record at a time, unbuffered — a syscall per printed
   line at git.git's 81k-commit depth. Wrapping the writer in a `BufWriter` took
   `history` from **962 ms → ~0.42 s**, which is **~2× faster than git**, not merely
   at parity with it.
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
`BENCHMARK.md` gets its own full re-run once it merges.

**Mechanism.** Each inventory shard's header can now carry an optional rollup hash
(format `v2026_07_15`) — the tree hash `stack` would build for that shard's entire
staged subtree, directly comparable against the corresponding head subtree's hash.
`walk_directory_staged` (`stocktake`/`diff --staged`) and `stack`'s tree-build skip
plan (`compute_rollup_skip_plan`) both skip load+parse+descend for a subtree the
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

## Remaining items

With the rollup hash fixed (above), **(b) pack-index validation tax is now the top
remaining item** — a store-wide read-floor cost, not specific to one command.

### b. Pack-index validation tax

**Symptom.** ~6.4–6.9 ms warm (up to ~19 ms cold) added to **every** command that
touches a packed object — which, on a densified store, is nearly every command.

**Root cause.** `load_pack_pair`
(`crates/forklift-core/src/util/pack_utils.rs:563-606`) calls
`validate_index_records` — a linear scan over all ~403k index records on git.git —
the first time a process touches a given pack. This is a per-*process* cost (the
pack registry caches the loaded pack for the rest of that invocation, but a fresh
CLI process pays it again), so it recurs on every single-shot command a user or
script runs.

**Direction (all store-wide design, not command-specific — benefits everything).**
Several options, none evaluated in depth yet:

- A lazily-validated or self-checksummed index format, so a read only validates the
  record it actually touches rather than the whole index up front.
- An mmap'd index that is trusted after a one-time "seal" (e.g. a whole-index hash
  checked once when the pack is written or repacked, so a later open only needs to
  check that seal, not walk every record).
- Persistent warm state across invocations (a resident daemon or a cached
  validation result keyed by pack identity) — a bigger architectural change, and in
  tension with forklift's current all-state-on-disk, no-daemon design.

**Effect.** Not attributable to one command — it is a fixed tax on every packed-object
read, so fixing it moves the floor for `history`, `diff`, `stack`, `stocktake` and
everything else at once. Worth prioritizing behind (a) mainly because it needs its
own design pass on the index format, which is a bigger lift than a code-only fix.

### c. History walk memory at scale (minor)

`history`'s unbounded walk accumulates a `HashSet`/`HashMap` entry per parcel
(`crates/forklift/src/commands/history.rs`) — tens of MB at 1M commits. Not measured
as a *speed* problem, and irrelevant for the common case (`-n`/pager-bounded runs,
which only touch the frontier). Recorded as a note for whenever an unbounded
full-history walk at kernel scale is exercised for real, not something to act on now.

### d. Diff head-blob delta decode

~5.5–5.9 ms per 20 files reconstructing a blob from its delta record against the
densified pack's stored base. This is genuine reconstruction cost — the store trades
size for exactly this at compaction time (see `OBJECT_STORE_SCALING.md` §A) — not
overhead to eliminate. Only worth revisiting if `diff` is still hot after (a) lands;
until then this is the store working as designed, not a bug.

### e. Durability-barrier audit (write-batching sweep)

**What.** A systematic audit of every durability barrier in forklift-core and the
CLI head — `write_file_atomically` call sites, `WriteBatch` construction, every raw
`fsync` — counting barriers paid per logical operation, before any patching is
attempted. Audit first, patches second: the point is to size the remaining problem
rather than fix whatever is found first.

**Why now.** Three unrelated pieces of work converged on the same cost in short
succession. The quick-wins track already had to batch `stack`'s per-object fsyncs
into one barrier (quick-win #3, above). The rollup-hash work on this branch had to
batch shard-write barriers *twice* — once to bring stack cleanup's overhead down
from 70% to 18%, again to get `load .`'s join point faster than main rather than
merely neutral. And this branch's own benchmark verification (PR #59) surfaced
`park` costing roughly **+28 ms** and `park` pop roughly **+8 ms** on git.git,
tracing to unbatched barriers nobody had measured before. Three independent
investigations tripping over the same class of cost is a pattern, not a
coincidence.

**Known suspects to check first.**

- `consolidate`/cherry-pick's per-merge-action shard writes — each action pays the
  two-barrier mutation funnel individually; a 50-file merge is on the order of 100
  barriers, untimed so far.
- `park` pop's per-file replay.
- Multi-file `restore`.
- Journal writes.

**Method.** Classify each write site as: (a) already batched; (b) batchable with no
ordering constraint; (c) batchable with phased ordering — the `load .` join-point
pattern above, where ordering-constrained groups become ordered sub-barriers rather
than one barrier per file; (d) must remain individual for correctness.

**Standing rule.** Batching may reduce the *count* of barriers; it must never weaken
a barrier's *guarantee*. The durable-before-destructive contract and the ordering
invariants this doc already protects (ancestor-clears-before-content,
fsync-before-ref-move) are preserved exactly — only the granularity changes.

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
  is not on the table.
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
