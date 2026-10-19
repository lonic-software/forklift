# Benchmarking Forklift against Git

A runnable guide for comparing **Forklift** to **Git** on real, large repositories
(Git's own source, or the Linux kernel). It ships with a one-command harness and,
below that, the manual steps the harness automates — so you can reproduce every
number by hand and add your own.

> **Read this first — the honest framing.** Git is a 20-year-old C codebase tuned
> against the exact repos below; Forklift is **v0.3.x**. On git.git against a native
> arm64 git (2026-10-18, hyperfine), `status` is faster than git (0.88x) and `log` is
> level with it (0.99x); `diff` is ~1.2–1.3x slower (2.2–2.4x before FORK-183); `commit`
> is 3.1x slower than git's defaults but 1.47x against a git that syncs as durably as
> forklift does (§3).
> On-disk size ends up smaller after the one-shot redelta pass; see §2. The slower
> rows are tracked as optimization targets in
> [`PERFORMANCE_BACKLOG.md`](PERFORMANCE_BACKLOG.md). Earlier versions of this page
> reported `status` and `log` *faster* than git (0.64x / 0.84x); those came from a
> harness with no warm-up run and a shell timer, and this re-run does not reproduce
> them; the 0.88x here comes from the FORK-183 fixes, recorded in the backlog. The point of
> benchmarking here is not to "beat git" — it
> is to (a) see where Forklift is already in the same ballpark or ahead, (b) find the
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
| *(recommended)* [`hyperfine`](https://github.com/sharkdp/hyperfine) | the harness times with it when it is on PATH: more runs, outlier detection | `hyperfine --version` |

Install hyperfine for publication-grade numbers: `brew install hyperfine` (macOS) or
`cargo install hyperfine`. The harness picks it up from PATH and names the timer it used
in its report; without it, or with a hyperfine older than 1.13, it falls back to its
own shell timer and fewer runs.

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

Options: `--runs N` (iterations for the fast read-only ops; default 20 under hyperfine,
5 with the shell timer — the commit rows default to 10 and 3, set `COMMIT_RUNS` in the
environment to change that),
`--work DIR` (where to build the copies; default a fresh temp dir),
`--keep` (don't delete the work dir), `--out FILE` (append the results table),
`--no-redelta` (skip the post-import densify pass and time the as-imported store —
redelta re-encodes every object, which on kernel-scale repos is minutes of CPU).

### What it measures

The harness fetches the target once, then builds two working copies of the **same
tree** so each comparison is apples-to-apples — `git` runs in copy `A`, `forklift`
in copy `B`:

| Operation | git | forklift |
|-----------|-----|----------|
| **status** (clean tree) | `git status` | `forklift stocktake --summary` |
| **log** (whole history) | `git log` | `forklift history` |
| **diff, unstaged** (20 edited files) | `git diff` | `forklift diff` |
| **diff, staged** (same 20, staged) | `git diff --staged` | `forklift diff --staged` |
| **commit** (1 file) | `git commit` | `forklift stack` |
| **commit, git fsync=all** | `git -c core.fsync=all -c core.fsyncMethod=fsync commit` | `forklift stack` |
| **onboard** *(separate)* | `git clone --local` | `forklift import-git .` |

`import-git` writes straight into native packs as it imports — there is no loose store
and no separate compaction pass, so the forklift copy is already packed the moment the
import finishes. The harness then records that as-imported size and runs the one-shot
**`compact --all --redelta`** densify pass import itself suggests (import's per-path
delta chains miss cross-path similarity, so the as-imported store is larger than it
needs to be). Both sizes appear in the report, the redelta time is reported alongside
the import as onboarding cost, and the comparison rows run on the *densified*
store — the steady state a user who followed the suggestion operates in, compared
against the shape git is always measured in (`clone --local` copies packfiles).
Pass `--no-redelta` to time the as-imported store instead.

Each read-only command gets one untimed warm-up run before its timed runs. The commit
rows time only the commit: the one-line edit and its `git add` / `forklift load` are
untimed setup (hyperfine's `--prepare`), so each timed commit records exactly one file.
The commit rows run before the diff rows, on the clean tree onboarding leaves, so no
reset step runs between them — a reset is a different operation in each tool and would
leave forklift's inventory in a different state from a fresh import's. The harness
checks that both trees are clean around the commit rows and that both tools saw the
same change set before each diff row, and it stops with an error if any timed or setup
command fails, rather than publishing the time of a command that failed early.

Output is an **aligned text table** (pass `--markdown` for a pasteable markdown one):
git time, forklift time, the **forklift/git ratio** (below 1.0 means forklift was
faster), and a per-row note. Example shape:

```
  Repo:      git — 82513 commits, 4857 tracked files
  On-disk:   git .git = 334M   forklift .forklift = 445M as imported, 240M after redelta  (forklift 1.4x smaller)
  Tools:     git version 2.55.0 · Forklift version 0.3.1
  Method:    hyperfine 2.0.0; read-only ops = mean of 20 runs after one warm-up run, commit = mean of 10; warm cache.
             A ratio below 1.00 means forklift is faster.
             git ships packed; forklift is packed + densified (one-shot compact --all --redelta after import).

  Operation                  git        forklift   forklift/git   Notes
  -------------------------  ---------  ---------  -------------  ----------------------------------
  status (clean tree)        16 ms      14 ms      0.88x          git status vs forklift stocktake --summary
  log (whole history)        560 ms     556 ms     0.99x          82513 commits walked
  commit (1 file)            18 ms      56 ms      3.11x          git commit vs forklift stack, staging untimed
  commit, git fsync=all      38 ms      56 ms      1.47x          same, git with core.fsync=all core.fsyncMethod=fsync
  diff, unstaged (20 files)  13 ms      16 ms      1.23x          git diff vs forklift diff
  diff, staged (20 files)    9 ms       12 ms      1.33x          git diff --staged vs forklift diff --staged

  Onboarding — measured separately; NOT a ratio (different operations):
    git clone --local     420 ms     (copies an existing packfile)
    forklift import-git   60.43 s    (re-encodes every commit/tree/blob straight into packs; store lands at 445M)
    Packed the imported store: 409297 object(s) into 5 pack(s), 315500 delta-compressed.
    compact --all --redelta  179.16 s (one-shot densify: 445M -> 240M)
```

**Two sizes, one store.** `import-git` writes straight into native packs as it imports
(delta-compressing successive file/tree versions on the way in), but its per-path delta
chains don't see the cross-path similarity (renames, moved files) that a global repack
ordering does — on git.git the as-imported store lands **~1.3x larger** than git's own
pack (445M vs 334M). The one-shot `compact --all --redelta` densify pass closes that
and more: **240M, ~1.4x smaller than git**, for ~179 s of one-time CPU on this corpus.
The harness reports both sizes and times the comparison rows on the densified store;
redelta itself did not measurably change the timing rows. On
repos whose `.git` carries unreachable refs or stale
remote-tracking branches, `.forklift` often lands far smaller still — `import-git` only
pulls branch-reachable history — but that reflects the source repo's ref clutter, not a
general "forklift packs tighter" result; git.git above (a repo with little such clutter)
is the representative case. See
[`OBJECT_STORE_SCALING.md`](OBJECT_STORE_SCALING.md). *(Measured on git.git on one
Apple-Silicon Mac, warm cache — re-run for your own repo/hardware.)*

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
  - **The table is measured packed (and densified), because git is packed.**
    `import-git` writes straight into native packs — there's no loose store — and the
    harness then runs the one-shot `compact --all --redelta` densify pass before the
    comparison table (skip it with `--no-redelta`). Every `status`/`log`/`diff`/`commit`
    row is packed-vs-packed, the same way git is always measured packed (`clone --local`
    copies packfiles). An import-time `--no-compact` flag exists but stores loose
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
- **One machine.** Under hyperfine the harness reports a mean over 20 runs (10 for
  commit); with the shell-timer fallback it is a mean over a handful of runs, not a
  distribution. Either way it is one machine on one day — quote the hardware with the
  number. For a distribution, run the commands under `hyperfine` yourself (see §5).
- **Forklift's timings depend on inventory state.** `restore --staged` leaves entries
  whose stat data is re-read on every later `stocktake`, so later `status`/`diff` timings
  are slower than on a fresh import. If you run the steps by hand, time on a freshly
  imported copy.
- **Durability differs by default.** Every `forklift stack` is durable when it
  returns: on macOS it ends with a device-cache flush (`F_FULLFSYNC`). git's defaults
  are weaker there: git-config(1) documents `core.fsyncMethod`'s macOS default as
  `writeout-only`, which "may not be durable in the event of a system crash". The
  harness therefore times git's commit twice, with its defaults and with
  `core.fsync=all` + `core.fsyncMethod=fsync`; the second row is the like-for-like
  durability comparison. Quote the one that matches the question you are asking.
- **Forklift signs; git doesn't (by default).** If you `office enroll` the imported
  warehouse, every `stack` afterwards signs the parcel — real work git isn't doing
  in its `commit`. Benchmark signed vs unsigned deliberately; don't conflate them.
- **Which `git` binary you measure against matters.** The numbers above are native
  arm64 git 2.55.0 vs native arm64 forklift — apples to apples. But an old or
  emulated git binary inflates git's side dramatically: on this same machine, the
  previous PATH git (2.33.0, x86_64 under Rosetta 2) measured `status` at 0.31x and
  `log` at 0.48x against the identical forklift binary — the git version/arch alone
  moved those ratios by more than 2x on some rows. See
  [`PERFORMANCE_BACKLOG.md`](PERFORMANCE_BACKLOG.md), which records the emulated
  run. Check `file $(command -v git)` and `git --version` before you quote a ratio.

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
du -sh .forklift                             # packed, as imported

# 2b. Densify — the one-shot pass import suggests. Import's per-path delta
#     chains miss cross-path similarity; this re-delta-compresses globally.
time forklift compact --all --redelta
du -sh .forklift                             # densified — the state the table compares

# 2c. import-git leaves an untracked .forkliftignore (the .git rule) and asks you to
#     record it; do, so that a later `load .` stages only your edits.
forklift load .forkliftignore && forklift stack "Keep the .git ignore rule"

# 3. Compare, running each tool in its own copy.
# status:
( cd ~/bench/A && time git status )
( cd ~/bench/B && time forklift stocktake --summary )

# log (whole-history walk):
( cd ~/bench/A && time git log            >/dev/null )
( cd ~/bench/B && time forklift history   >/dev/null )

# commit (stage a change, then commit/stack; only the commit is timed). Run this
# before the diff step, while both trees are still clean:
( cd ~/bench/A && echo x >> README.md && git add README.md   && time git commit -q -m bench )
( cd ~/bench/B && echo x >> README.md && forklift load README.md && time forklift stack bench )

# diff (dirty the same files in both, then diff unstaged and staged):
for f in README.md Makefile; do printf '\n// edit\n' >> ~/bench/A/$f; printf '\n// edit\n' >> ~/bench/B/$f; done
( cd ~/bench/A && time git diff           >/dev/null )
( cd ~/bench/B && time forklift diff      >/dev/null )
( cd ~/bench/A && git add -A    && time git diff --staged      >/dev/null )
( cd ~/bench/B && forklift load . && time forklift diff --staged >/dev/null )

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
  pre-1.0 system. A large multiple on a *fast* op (tens of ms) is often fixed overhead
  (process start, lock acquisition, inventory open) that amortizes away on big
  operations — note the absolute time, not just the ratio.
- **`log`/`history`** stresses graph-walk and object decode, measured on the **packed,
  densified** store (where forklift is level with git — 1.01x on the recorded
  run). A regression here is the real object-store read-path signal.
- **`onboard`** will always favor git (see §3). Track it over releases to catch
  import regressions, not to compare against clone.
- **A regression across releases matters more than the absolute gap to git.**
  Re-run `bin/benchmark --repo git --out history.md` after changes and diff the
  tables — that's the highest-signal use of this harness.

If a number looks wrong, re-run with `--keep` and inspect the two working copies
(`A/` = git, `B/` = forklift) by hand.
