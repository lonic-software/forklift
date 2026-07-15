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

## Remaining items

### a. Per-shard rollup hash (the structural fix — highest value)

**Symptom.** `diff --staged` (and `stocktake`'s staged walk, the same code path) and
`stack`'s shard pass are **O(all tracked directories), not O(changed)**. On git.git
(224 tracked directories), this is ~18–19 ms of every `diff` and 4–12 ms of every
`stack` — confirmed linear on synthetic corpora at 0.08–0.12 ms per directory,
independent of how many of those directories actually changed.

**Root cause.** `walk_directory_staged`
(`crates/forklift-core/src/util/stocktake_utils.rs:151-267`) loads every inventory
shard and unconditionally calls `object_utils::load_tree` on every corresponding head
subtree, for every directory in the walk — there is no short-circuit for a subtree
that is unchanged since the head. This is the mirror image of a bug that does *not*
exist on the tree-vs-tree path: `diff`'s pallet-vs-pallet walk already skips a
subtree the moment the child and head subtree hashes are equal
(`crates/forklift/src/commands/diff.rs:584-586`, `if from_subtree.hash ==
to_subtree.hash { continue; }`). The staged walk has no equivalent test to skip on,
because an inventory shard has no single hash to compare against the head subtree's
hash — it is a set of per-file entries, not a rolled-up value.

**Direction.** Give each inventory shard an aggregate ("rollup") hash that is
comparable against the corresponding head subtree hash, and skip
load+parse+descend for that subtree the moment the two match — exactly the
short-circuit `diff` already has, extended to the staged/stack side. This was
already floated in the 2026-07-07 stocktake/diff scaling review (see
`OBJECT_STORE_SCALING.md`'s revision history and the internal review notes referenced
from `DESIGN.html` §5.0 milestone A) as a known gap, not a new finding.

**Correctness-critical, not a drop-in patch.** A stale rollup hash silently hides a
real staged change — the failure mode is not a crash but `diff --staged`/`stocktake`
under-reporting, which is worse. Every mutating path that can leave a shard's rollup
hash out of sync with its contents must maintain it: `load`, `unload` (formerly
`remove`), `restore`, sparse `narrow`/`expand`, `consolidate`/merge, `cherry-pick`,
and `park`/pop. This is very likely a shard format/version bump (the rollup hash has
to be *stored*, not recomputed on every read — recomputing it on every read is the
same O(all directories) cost this change exists to remove). Needs a design pass —
covering exactly which mutating paths must update the hash, whether it is
incrementally maintained or recomputed on write, and how a shard from an
old-format store degrades (presumably: treated as "unknown", falls back to the
current unconditional-load behavior) — not a quick patch.

**Expected effect.** Fixes both the staged diff walk directly and, combined with the
quick-wins scan collapse (item 2 above), removes most of `stack`'s remaining
shard-pass cost — the two paths share the same root cause.

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
