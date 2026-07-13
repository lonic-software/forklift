# Benchmarking Forklift against Git

A runnable guide for comparing **Forklift** to **Git** on real, large repositories
(Git's own source, or the Linux kernel). It ships with a one-command harness and,
below that, the manual steps the harness automates — so you can reproduce every
number by hand and add your own.

> **Read this first — the honest framing.** Git is a 20-year-old C codebase tuned
> against the exact repos below; Forklift is **v0.1.x**. Expect Git to win some raw
> single-command timings and Forklift to reach parity or better on others (status,
> commit, and — on a packed store — the history walk and on-disk size). The point of
> benchmarking here is not to "beat git" — it
> is to (a) see where Forklift is already in the same ballpark, (b) find the
> operations that are *unexpectedly* slow so they become optimization targets, and
> (c) measure the things Forklift does that Git doesn't have a column for (signed
> history, per-directory inventory). Treat a result that's within a small multiple
> of git as a good outcome for a system this young.

---

## 1. What you need

| Requirement | Why | Check |
|-------------|-----|-------|
| `git` on PATH | the baseline, and the source of history to import | `git --version` |
| `forklift` on PATH | the system under test | `forklift version` |
| A timer: GNU `date`, `python3`, or `perl` | wall-clock measurement | any of `python3 --version` |
| Disk: ~2× the repo's size | the harness keeps two working copies | `df -h .` |
| *(optional)* [`hyperfine`](https://github.com/sharkdp/hyperfine) | tighter statistics, warmup, outlier detection | `hyperfine --version` |

Install hyperfine if you want publication-grade numbers: `brew install hyperfine`
(macOS) or `cargo install hyperfine`. The bundled harness does **not** require it.

Disk budget, concretely:
- **git.git** — ~230 MB of history, ~4.7k files. Two copies ≈ 0.6 GB. Fast.
- **torvalds/linux** — ~5 GB of history, ~90k files. Two copies + import ≈ **20+ GB**
  and a multi-minute import. Only run this when you specifically want kernel-scale.

---

## 2. Quick start — the one-command harness

From the repo root:

```sh
# Git's own source (the good default: large but not enormous)
bin/benchmark --repo git

# The Linux kernel (huge — read the disk note above first)
bin/benchmark --repo linux

# Any repo you like — a URL or a local path
bin/benchmark --repo https://github.com/rust-lang/cargo.git
bin/benchmark --repo /path/to/some/local/repo

# Save the results table to a file, and keep the working copies to poke at
bin/benchmark --repo git --out results.md --keep
```

Options: `--runs N` (iterations for the fast read-only ops, default 5),
`--work DIR` (where to build the copies; default a fresh temp dir),
`--keep` (don't delete the work dir), `--out FILE` (append the results table).

### What it measures

The harness fetches the target once, then builds two working copies of the **same
tree** so each comparison is apples-to-apples — `git` runs in copy `A`, `forklift`
in copy `B`:

| Operation | git | forklift |
|-----------|-----|----------|
| **status** (clean tree) | `git status` | `forklift stocktake --summary` |
| **log** (whole history) | `git log` | `forklift history` |
| **diff** (dirty tree) | `git diff` | `forklift diff --staged` |
| **commit** (1 file) | `git add` + `git commit` | `forklift load` + `forklift stack` |
| **onboard** *(separate)* | `git clone --local` | `forklift import-git .` |

`import-git` writes straight into native packs as it imports — there is no loose store
and no separate compaction pass. So the forklift copy is already packed the moment the
import finishes, and the four comparison rows run on the *packed* store — the same shape
git is always measured in (`clone --local` copies packfiles).

Output is an **aligned text table** (pass `--markdown` for a pasteable markdown one):
git time, forklift time, the **forklift/git ratio** (below 1.0 means forklift was
faster), and a per-row note. Example shape:

```
  Repo:      git — 81470 commits, 4775 tracked files
  On-disk:   git .git = 316M   forklift .forklift = 236M  (forklift 1.3x smaller)

  Operation               git        forklift   forklift/git   Notes
  ----------------------  ---------  ---------  -------------  ----------------------------------
  status (clean tree)     172 ms     22 ms      0.13x          git status vs forklift stocktake …
  log (whole history)     929 ms     944 ms     1.02x          81470 commits walked
  diff (20 files changed) 25 ms      43 ms      1.72x          git diff vs forklift diff --staged
  commit (1 file)         41 ms      38 ms      0.93x          git add+commit vs load+stack

  Onboarding — measured separately; NOT a ratio (different operations):
    git clone --local     462 ms     (copies an existing packfile)
    forklift import-git   130.91 s   (re-encodes every commit/tree/blob straight into packs)
    Packed the imported store: 402118 object(s) into 5 pack(s), 187332 delta-compressed.
```

**The store arrives packed — no separate compaction pass.** `import-git` writes straight
into native packs as it imports (delta-compressing successive file/tree versions on the
way in), so the comparison table above runs on the **packed store** — the state a real
user operates in (git ships packed too: `clone --local` copies packfiles). Packed vs
packed, forklift lands smaller on disk than git's own pack and roughly at parity on the
whole-history walk. Packing removes per-file slack and the open-per-object cost of a
walk, and delta-compresses similar objects. See
[`OBJECT_STORE_SCALING.md`](OBJECT_STORE_SCALING.md). *(Numbers illustrative — run it.)*

**Onboarding is deliberately kept out of the ratio table.** `git clone --local` and
`forklift import-git` do fundamentally different work (see §3), so pitting them in a
ratio ("290× slower!") is misleading — the harness reports the two timings side by
side and lets you read them as what they are. *(Illustrative numbers — run it to get
your own.)*

---

## 3. Fairness caveats — read before you quote a number

These matter. A benchmark that hides them is a sales pitch, not a measurement.

- **Onboard is not a like-for-like race.** `git clone --local` hardlinks/copies an
  existing pack; `forklift import-git` *re-encodes* every git commit, tree and blob
  into Forklift objects and signs nothing (imported history is legacy/unsigned). So
  onboard measures the *one-time migration cost*, and git will look dramatically
  faster because it is doing dramatically less work. The honest question it answers
  is "how long to bring this history under Forklift", not "who clones faster".
  Two things to expect from `import-git` today:
  - **Import speed.** The importer streams every object through one long-lived
    `git cat-file --batch` pipe rather than forking git per object, so it stays fast
    at scale, and it writes straight into native packs as it goes — delta-compressing
    successive versions of files and directory trees on the way in. It still re-encodes
    and stores every object, so it is not instant like a local clone — expect a couple
    of minutes for kernel-scale history, not seconds. (Before batching + pack-direct
    writes, this ran far slower: one `git` fork+exec per object, then a loose object
    per file with an fsync each.)
  - **Non-UTF-8 history is tolerated.** Real repos carry commits with Latin-1 author
    names (git.git has several); the importer coerces such display text lossily
    rather than aborting. If you see U+FFFD (`�`) in an imported name, that's why —
    the author's email (the stable id) is preserved exactly.
  - **The table is measured packed, because git is.** `import-git` writes straight into
    native packs — there's no loose store and no separate compaction step to run before
    the comparison table. Every `status`/`log`/`diff`/`commit` row is packed-vs-packed
    from the moment the import finishes, the same way git is always measured packed
    (`clone --local` copies packfiles). A `--no-compact` flag exists but stores loose
    objects instead as a debug/inspection opt-out — it is not part of the normal
    onboarding path and the harness doesn't use it.
- **`log` output differs.** `git log` and `forklift history` don't print identical
  text (history density, merge interleaving). The harness compares the *graph-walk
  cost* of the default command each ships, not byte-for-byte output. If you want a
  stricter comparison, pin both to a fixed format (e.g. `git log --format=%H`).
- **Warm vs cold cache.** The harness runs warm (the OS has already cached the tree
  from building the copies). First-run/cold numbers are a different, also-valid
  experiment — drop the caches between runs (`sync` + platform-specific cache drop)
  if that's what you care about.
- **`--summary` vs full status.** The harness uses `forklift stocktake --summary`
  (counts only) against `git status`. That's the closest fair pairing (neither
  formats a long per-file list); use plain `forklift stocktake` if you want the
  full-report cost instead.
- **One machine, few runs.** The bundled timer reports a mean over a handful of
  runs, not a distribution. For anything you plan to publish, re-run the same
  commands under `hyperfine` (see §5) — it does warmup, many runs, and outlier
  detection.
- **Forklift signs; git doesn't (by default).** If you `office enroll` the imported
  warehouse, every `stack` afterwards signs the parcel — real work git isn't doing
  in its `commit`. Benchmark signed vs unsigned deliberately; don't conflate them.

---

## 4. Doing it by hand

The harness just automates this. Run it yourself to understand or extend it.

```sh
# 0. Get a repo to test on, once.
git clone https://github.com/git/git.git ~/bench/git-src

# 1. Two working copies of the same tree.
git clone --local ~/bench/git-src ~/bench/A          # git measured here
cp -a ~/bench/A ~/bench/B                             # forklift measured here

# 2. Import history into the forklift copy (this is the 'onboard' number).
#    import-git writes straight into native packs as it goes — the store is
#    already packed the moment this finishes, same as git is always packed.
cd ~/bench/B
forklift prepare
time forklift import-git .
du -sh .forklift                             # packed — this is the state the table compares

# 3. Compare, running each tool in its own copy.
# status:
( cd ~/bench/A && time git status )
( cd ~/bench/B && time forklift stocktake --summary )

# log (whole-history walk):
( cd ~/bench/A && time git log            >/dev/null )
( cd ~/bench/B && time forklift history   >/dev/null )

# diff (dirty the same files in both, then diff):
for f in README.md Makefile; do printf '\n// edit\n' >> ~/bench/A/$f ~/bench/B/$f; done
( cd ~/bench/A && time git diff           >/dev/null )
( cd ~/bench/B && forklift load . && time forklift diff --staged >/dev/null )

# commit (stage a change, then commit/stack):
( cd ~/bench/A && echo x >> README.md && git add README.md   && time git commit -q -m bench )
( cd ~/bench/B && echo x >> README.md && forklift load README.md && time forklift stack bench )
```

### Extend it — ops the harness doesn't cover

Good candidates to add for a deeper picture:

- **Branch switch / tree materialization** — `git checkout <old-branch>` vs
  `forklift shift <pallet>`. Forklift rewrites the working tree and repopulates the
  inventory on a shift, so this exercises a very different code path than status.
  (Both refuse or overwrite based on a dirty tree — start clean.)
- **Branch create** — `git branch x` vs `forklift palletize x`.
- **Stash** — `git stash` / `git stash pop` vs `forklift park` / `forklift park pop`.
- **Signed history** — `forklift office enroll`, then time a signed `forklift stack`
  and an `forklift audit` (verifying the whole chain offline). Git has no direct
  equivalent; the closest is `git commit -S` + `git log --show-signature`.
- **Cold cache** — repeat any read-only op after dropping OS file caches.

---

## 5. Going deeper with hyperfine

For real statistics, run the same commands under hyperfine. It handles warmup,
many runs, and — crucially — a `--prepare` step for mutating commands:

```sh
# read-only, straightforward:
hyperfine --warmup 3 \
  --command-name 'git status'    'git -C ~/bench/A status' \
  --command-name 'forklift st'   'forklift --json stocktake --summary'   # run from ~/bench/B

# whole-history log:
hyperfine --warmup 2 \
  'git -C ~/bench/A log' \
  'sh -c "cd ~/bench/B && forklift history"'

# commit needs a fresh staged change each run — use --prepare:
hyperfine --prepare 'echo $RANDOM >> ~/bench/A/README.md && git -C ~/bench/A add README.md' \
  'git -C ~/bench/A commit -q -m bench'
```

Use `--export-markdown out.md` to capture a table, or `--export-json` for plots.

---

## 6. Interpreting results — what "good" looks like

- **Same order of magnitude as git** on status/diff/commit is a genuine win for a
  v0.1 system. A large multiple on a *fast* op (tens of ms) is often fixed overhead
  (process start, lock acquisition, inventory open) that amortizes away on big
  operations — note the absolute time, not just the ratio.
- **`log`/`history`** stresses graph-walk and object decode, measured on the **packed**
  store (where forklift is ~at parity with git) — the store `import-git` always produces
  now, since it packs on the way in. A regression here is the real object-store
  read-path signal.
- **`onboard`** will always favor git (see §3). Track it over releases to catch
  import regressions, not to compare against clone.
- **A regression across releases matters more than the absolute gap to git.**
  Re-run `bin/benchmark --repo git --out history.md` after changes and diff the
  tables — that's the highest-signal use of this harness.

If a number looks wrong, re-run with `--keep` and inspect the two working copies
(`A/` = git, `B/` = forklift) by hand.
