# Proof Load: how Forklift is tested under faults

Proof Load is the fault-injection harness Forklift's durability claims are measured with. It breaks
things on purpose: damaged files, a process killed mid-write, power lost before a sync, a full
disk, a read-only warehouse, a volume that folds case, a network that drops a request. Then it
reads every record back and grades what the tool did. It runs the same faults against git, at
its defaults and tuned, so each result has something to compare against.

This page explains the method: what is faulted, how a result is graded, how to replay one, and
how to read the report. The harness lives in [`crates/proof-load`](../../crates/proof-load/README.md).
Its desktop window, which shows a report, lives in
[`tools/proof-load-desktop`](../../tools/proof-load-desktop/README.md).

## 1. What it is, and what it never does

Proof Load drives `forklift`, `forklift-server` and `git` only as programs, found by the paths you
give it. It never links Forklift's code. Its expectations come from a history it generates from a
seed (the *oracle*), and it judges each readback by comparing bytes with that history. An oracle
that shared code with the tool under test would share that code's bugs.

It only works on copies. Every run works inside the output folder you name, which must be empty
or new. Each tool starts from an empty environment plus `PATH`, with `HOME`, `XDG_CONFIG_HOME` and
`TMPDIR` pointed into that folder and every `FORKLIFT_*` variable unset unless a scenario sets one
on purpose; git reads configuration the harness wrote. The tools are pointed at the output folder both by path and
through the environment, so they do not find your own configuration. `--source` and
`--history-from` are only read; a history is mirrored into the output folder before anything runs
over it.

## 2. Running it

Build the binaries under test and the harness, then run it from the repository's root:

```sh
cargo build -p forklift -p forklift-server -p proof-load
target/debug/proof-load run --source . --out ../proof-load-out \
  --forklift target/debug/forklift --forklift-server target/debug/forklift-server \
  --git "$(command -v git)" --only history
```

- `--source` is the source tree of the version under test, read only. The harness reads its census
  of on-disk names and its server route table from it, so a check can fail when something new has
  no test.
- `--forklift`, `--forklift-server` and `--git` name the binaries under test, by path. The list of
  commands the write-op check covers comes from the `--forklift` binary (`forklift self schema`),
  so it follows that binary, not the source tree.
- `--out` receives the scratch areas, the report and the details files.
- `--only <scenario>` runs one scenario. Without it, every scenario runs, which takes much longer.
- Leave out a binary and the lanes that need it are reported as *not run*, with the reason, rather
  than skipped silently.

It exits 0 when the report passes, 1 when it fails, and 2 when the run itself could not go ahead,
such as over a non-empty output folder or an unknown scenario. Its progress goes to
stderr. The results are the files it writes.

The other flags add lanes; section 6 lists them.

## 3. Fault classes

Each fault class is a row of the report's coverage table, with a cell for Linux, macOS and Windows.
Each cell reads `run`, `sampled` (the lane ran over a sample of what it could reach), or
`not run: <reason>`. No cell is ever left blank.

| Class | What is done |
|---|---|
| damaged files | Each file and folder name in Forklift's on-disk census is truncated, has a byte flipped, is zeroed, or is deleted, and git's own layout gets the same treatment. A census name with neither a fault bound to it nor an exemption fails the run. |
| dead-PID lock | One of git's lock files is planted for a holder that no longer runs. Forklift's locks are kernel locks on files that stay in place, so a holder that dies leaves nothing to plant; deleting their files is a damaged-file fault. |
| kill points (client) | The command is killed at each call it makes that changes or syncs a file. Needs strace, so Linux only. |
| kill points (server) | The same, for the server of a hosted scenario. |
| power loss (client) | The command's traced calls are replayed over a model that keeps only what was synced. Each moment is graded as the disk could be left after a power cut. Linux only. |
| power loss (server) | The same, for the server of a hosted scenario. |
| disk full | The small volume given as `--capped-volume` is filled with a ballast file, leaving none, a quarter, half and three quarters of the most the command consumes. |
| read-only | The work folder's write bits are cleared. |
| case-folding volume | The work tree is cloned onto a volume that folds case, and a file is renamed by case only. |
| no-exec volume | The work tree is cloned onto a volume that keeps no executable bit, with an executable left untouched. |
| network proxy | Every hosted scenario's server is reached through the harness's proxy. In the scenarios `--network` names, the proxy delays, drops or cuts a request, cuts, resets or stalls a response, or kills the server, one fault of each kind. |
| round-trip fidelity | History goes from git to Forklift and back, and from Forklift to git and back, and each field that changed is listed. |
| clock skew | Not driven yet: each cell says so. |
| AWS head | Not driven yet: each cell says so. |

The lanes that need extra setup (strace, a small volume, a folding volume) report *not run*
whenever that setup is missing, on every OS.

## 4. Grades

Every faulted row gets one of five grades. Three of them are failures.

| Grade | Means | Fails the run |
|---|---|---|
| `healed` | Exit 0 with the expected bytes, either from the faulted command or from one plain retry of it. | no |
| `refused-working-remedy` | The tool refused and named a remedy, and running that remedy led to the expected bytes. | no |
| `refused-no-remedy` | The tool refused and named no remedy, or named one that does not work. Every earlier record still reads back. | yes |
| `data-lost` | Even after every named remedy and the tool's own remedy set, an acknowledged record cannot be read back, and the tool refuses rather than serve wrong bytes. | yes |
| `silent` | Some readback served wrong bytes with exit 0. This outranks every other grade. | yes |

`data-lost` is decided by what can be read back: the record is lost as far as the tool can tell
the user. The bytes may still sit undecoded in the store.

After a fault, the harness takes these steps in order, reading the records back as it goes, and
goes no further once they read back as expected:

1. The faulted command itself, then a readback of every record.
2. One plain retry.
3. Each remedy the tool names, run in turn. Remedies are bounded, and none is run twice.
4. Only when a record the tool had acknowledged still does not read back, the tool's own remedy
   set:
   - for Forklift, `forklift heal`, then `forklift audit` (which only reports), then a fresh clone;
   - for git, `git fsck --full` (which only reports), then each ref restored from its reflog, then a
     fresh clone.

A remedy is only run when the tool names it in a form the harness can extract:

- one of the tool's own commands;
- a `forklift-server …` operator command;
- `remove "<path>"`.

Anything else is recorded as unextractable. The harness never guesses what a sentence meant.

Each grade carries a recoverability record:

- **commands to expected**: how many commands it took from the fault to the expected bytes, or
  `never`;
- **right remedy**: whether the tool named the remedy that worked;
- **rescued by**: `local repair` (`heal`, or a ref restored from a reflog) or `re-clone`, when the
  own remedy set was what rescued it;
- **outside knowledge**: steps that are not the tool's own commands, such as removing a file,
  freeing space, or acting as the server's operator.

## 5. Seeds, and replaying a row

`--seed` (default 1) decides the generated history and every seeded choice made from it:

- which byte is flipped;
- which request the network faults hit;
- the partial power-loss states `--power-variants` draws;
- over a real history (section 7), which positions are sampled and the order its rows run in.

The report's header records the seed, each tool's path and `--version`, the scenarios, the
platform, the OS, the file system, git's configurations and the environment the tools ran in.

To replay a row:

1. Take the seed from the header and the scenario from the row.
2. Build the same binaries.
3. Run `proof-load run` with `--seed <seed> --only <scenario>` and the same lane flags as the
   original run, into a new output folder. The report does not record the lane flags; for a CI
   run, they are in the workflow file (section 9).

A row over a real history cannot be replayed with `--only`, because `--history-from` refuses it.
That lane replays as a whole.

## 6. Lanes and the budget

The damaged-file faults and the completeness checks run whenever the binaries are given. The other
lanes run only when their flag names them:

| Flag | Lane |
|---|---|
| `--strace <path>` with `--kill <scenario>` / `--power <scenario>` | kill points and power loss of the named scenarios (Linux). With strace given and neither flag, every scenario's. `--power-variants <n>` adds seeded partial states. |
| `--capped-volume <dir>` with `--disk-full <scenario>` | disk full, on a directory on a small volume of its own |
| `--read-only <scenario>` | a read-only warehouse |
| `--folding-volume <dir>` | the case-folding and no-exec scenarios, on an exFAT or vfat volume |
| `--network <scenario>` | one network fault of each kind |
| `--history-from <git repo>` | fidelity and damaged-file rows over a real history (section 7) |

`--jobs` sets how many injected runs go at once. It defaults to the machine's parallelism.

`--budget <seconds>` is a wall-clock budget. A run that goes past it still runs every scenario, and
then fails, so running long shows up as a failure rather than as missing rows. The one exception is
the damage rows over a real history (section 7): they start only while the budget lasts, and for
two hours at most.

Besides the faults, every run checks its own completeness, and a gap fails the run:

- every census name has a fault bound to it, or an exemption;
- every command `forklift self schema` lists, and every `forklift-server` subcommand, is seen
  writing in some scenario, or is exempt. Seeing the writes needs Linux.
- every route in the server's route table is answered through the proxy;
- on Linux, every write the server makes falls in the write set declared for the route that was
  in flight;
- no command recorded in the details of a Forklift row crashed, that is, exited 101 (a Rust panic)
  with no failure document. This covers the steps, the readbacks and the harness's own probes. A
  crash is never graded as an answer: a retry that panics claims nothing, as a lock refusal does,
  so a row's grade alone would not show it. Each crash is listed with its row, the command and the
  line that says it panicked. A row whose details cannot be read is listed too.

The exemptions live in `crates/proof-load/src/bindings.rs`.

## 7. Comparing with git

The damaged-file faults, and the client's kill points and power loss, run for git too, as two
subjects: `git` at its defaults, and `git-tuned`:

- every fsync on;
- fsck on transfer;
- a commit-graph;
- an untracked cache;
- the fsmonitor daemon.

The other lanes (disk full, read-only, the two volumes, the network, and the server's kill points
and power loss) run for Forklift only. Git's rows are reported beside Forklift's, and they never
fail the run. In a run given git, where a git lane could not run, the report says
`git: not fully measured`. The network lane is always one of these, because git's host is a bare repository reached by its path,
with no server for the proxy to fault.

### The data plate

The data plate pairs rows that faulted the same thing. A line is either the same scenario with the
same damage to a file in the same role (a pack, a branch ref, a lock), or the same lane stopping
the same operation. The line
has a column per subject. A subject's cell is its worst row on that line, by grade and then by
rescue kind. A cell with no row reads `not measured`, or `no counterpart` when the other tool has
no file in that role.

**Git does better** lists each line where git or tuned git has a better grade than Forklift, or the
same grade with a better rescue kind. Each entry gets a ticket.

### A real history

With `--history-from <repo>`, the damaged-file rows also run over a real repository's history:

- git's rows over the repository itself;
- Forklift's rows over its import of that repository;
- git's rows again over Forklift's export of that import.

So the plate has a line for each direction, `from git` and `from Forklift`. The repository is
mirrored into the scratch area first, and a shallow repository is refused.

A whole history is too large to read back in full after every fault. Instead, each damage row reads
back a sample of up to eight positions, chosen by the seed among the refs both subjects hold.

**Damage not reached by the sample.** Damage can land where no sampled position reads, for example
a commit that only an unsampled branch holds. When every sampled position read back as expected,
the harness runs the subject's whole-store check: `git fsck --full`, or `forklift heal`. If that
check finds damage, the row is not graded `healed`. Instead, it is listed under that subject's
`damage not reached by the sample` completeness check. Under Forklift, the entry fails the run;
under git, it is one of the gaps behind `git: not fully measured`. Such a row has no grade and no
place on the plate, so it never counts toward "git does better".

### Fidelity

The round trips list each field a trip changed or dropped, compared commit by commit or record by
record, never by id. Findings never fail the run. Each class of loss gets a ticket.

## 8. Reading the report

`report.md` is for reading, and `report.json` holds the same report for programs. A reader refuses a
`report.json` of any format number other than its own. Read `report.md` in this order:

1. **Header**: the determinism record from section 5.
2. **Coverage**: the class × platform table from section 3.
3. **Completeness**: each check, with what it found missing.
4. **Data plate** and **Git does better**: section 7.
5. **Grades**: one row per faulted run, with its subject, scenario, fault class, binding, grade
   and recoverability record. Rows of scenarios git has no counterpart for are listed here too.
6. **Forklift-only grades**: a section the report keeps, which a run currently leaves empty.
7. **Fidelity**: the findings from the fixtures. Over a real history the findings are counted by
   field, and `report.json` lists each one.

Each row in `report.json` names its details file (`detail`: `<name>.json`, sometimes with a JSON
pointer after `#`) in the output folder, and carries a `note` where one applies. In `report.md`,
the data plate's cells link to them. Use the details file to find out *why* a row has its grade.
It holds:

- every command run after the fault, in order, with its arguments, exit status, stdout and stderr;
- the readbacks.

The output folder also holds the run's evidence: `write-ops.json`, `exchanges.json` (each proxied
HTTP exchange) and `server-writes.json`, and on Linux the strace traces.

## 9. How CI runs it

Four workflows run Proof Load, all outside `ci passed`. The jobs that run fault scenarios continue
on error, so for now they report grades without failing a pull request.

- **`proof load`** runs on every pull request, once per OS, against forklift and forklift-server
  built for that OS's release target. It sets `--budget 2700`, the read-only and network lanes, and
  on Linux strace with kill points and power loss for the `history` and `lifted` scenarios. Where
  CI could make one, it adds a capped volume (a tmpfs on Linux, a RAM disk on macOS) and a folding
  volume (vfat on Linux, exFAT on macOS). `report.md` is added to the job summary, and the JSON
  files, `report.md` and the traces are uploaded.
- **`proof load replay`**, a second job of the same workflow, runs `replay --ref ${{ github.sha }}`
  on Linux on every pull request, over Forklift's own history and every one of its merges. On a
  pull request, `github.sha` is the merge commit that would land it, so that merge is a row too: a
  difference the pull request introduces shows on that pull request. That merge's id changes on
  every push, so no path entry can name it. A divergence there is resolved by changing the branch,
  or by merging the base into it, which makes the merge a commit of the branch with an id an entry
  can name. Its run step does not continue on error, so a failed verdict fails the job.
  `replay.md` is added to the job summary, and `replay.md` and `replay.json` are uploaded. The job
  is only as sensitive as the history is to merges: of the 290 merges replayed when it was added,
  240 were fast-forwards for Forklift, 45 were real merges and 5 conflicted, and the replay took
  110 s on macOS release.
- **`proof load history`** runs weekly on Linux with `--history-from .`, over Forklift's own history.
- **`proof load replay git`**, a second job of the same workflow and on the same triggers, runs
  `replay --sample 200 --seed 7` over a bare clone of git.git pinned at
  `e1ec09b23c8e0deb21a0b95ac5996422277395d4`. Its run step does not continue on error, and
  `replay.md` and `replay.json` are added to the summary and uploaded as for the per-PR job. The pin
  and the seed keep the sample fixed, so every entry for its merges was filed from one run. The seed
  moves once, by hand, to the run number (`${{ github.run_number }}`), the first time
  `replay-tickets.json` holds no entry for a merge of the seed-7 sample. From then on each run
  takes a new sample, and a red names a merge no one has looked at. The runner's git is not pinned,
  so a red can also come from a change of git rather than of Forklift; the report's header records
  git's version. Either way it is resolved like any other failure: an entry and its ticket.
- **`proof load perf`** runs `perf --shape small --reps 3 --gate self` on Linux, with a
  forklift-server, on every push to `main` and on a pull request that touches the code the timed
  binaries or the harness are built from (the forklift, forklift-core, forklift-server and
  proof-load crates, the workspace's `Cargo.toml` and `Cargo.lock`, `.cargo/`). A verdict that fails anything fails the job, and `perf.md` and
  `perf.json` are uploaded. Weekly, and when started by hand, it runs the small and medium shapes
  under `--gate all`, warm and cold, with `bin/perf-drop-caches` as the `--drop-caches` program. Each
  shape's `perf.json` and `perf.md` are kept on the `proof-load-perf` branch, at
  `<shape>/<UTC time>-<commit>.json`, whatever the run came to, and each run is set beside that
  shape's last kept report with `--against`. A run started by hand on another branch than `main`
  keeps nothing. A verdict that fails anything fails the job, after the reports are kept.
- **`proof load desktop`** builds, tests and runs Clippy on the desktop window on each OS. It runs no
  faults, and a failing step fails it.

[`contributing.md`](contributing.md) has the full detail of each lane as CI runs it.

## 10. Looking at a report in the desktop window

`proof-load-desktop <report folder>` opens a report in a window. It has a **Results** screen, which
shows the grade tally and the rows (with filters for failures and by binding) and a row's steps;
a **Coverage** screen; and a **Run** screen that starts a run itself. See its
[README](../../tools/proof-load-desktop/README.md).

## 11. Measuring performance

`proof-load perf` times Forklift against git on a generated history or a real one, and judges the
rows under a gate. It is a separate command from `run` and faults nothing. It runs one timed command at a time, with nothing else from the
harness running, so no timing shares the machine with a faulted job.

```sh
cargo build --release -p forklift -p forklift-server -p proof-load
target/release/proof-load perf --out ../perf-out \
  --forklift target/release/forklift --git "$(command -v git)" \
  --forklift-server target/release/forklift-server
```

- `--out` receives the subjects and the report, and must be empty or new, as for `run`.
- `--forklift` and `--git` name the binaries to time, by path. Time a release build: a debug build
  mostly measures unoptimised code.
- `--shape` names the generated history, `small` unless you say otherwise. Each shape has a fixed
  seed, so every run of a shape times the same history, and the report's header spells it out.
  Every shape has a `perf-side` branch that leaves `main` a few commits before its tip:

  | Shape | Text files | Directory levels | Binary files | Commits on `main` |
  |---|---|---|---|---|
  | `small` | 400 | 3 | two of 2 MiB | 40 |
  | `medium` | 5,000 | 4 | four of 4 MiB | 400 |
  | `huge` | 50,000 | 4 | eight of 8 MiB | 2,000 |

  `huge` is a large monorepo's scale, and is not meant for a run on every change.
- `--history-from` names a git repository to time on instead of a generated shape, so it and
  `--shape` cannot both be given. It is read only: the harness mirrors it into its scratch area
  first, as `run --history-from` does, and refuses a shallow repository or a folder that is not a
  repository's top. The header names the history and how many text files its `HEAD` holds.
- `--side-branch` names the branch of that history which switch and merge time against. It needs
  `--history-from`, since a generated shape has `perf-side`, and the history must have a branch of
  that name. Without it, switch and merge are reported as not run, with the reason "no side branch
  given (--side-branch)".
- `--gate` says which checks fail the run: `self` or `all`. It defaults to `all` on a generated
  shape and to `self` with `--history-from`, which refuses `all`. See *The verdict* below.
- `--reps` sets how many timed repetitions each cell gets (5 unless you say otherwise). Each warm
  cell also gets one untimed run of its command first, so that the first repetition does not pay
  for loading what the later ones find cached.
- `--drop-caches` names a program, by path, that drops the operating system's file caches. The
  harness runs it with no arguments just before each repetition of a cold row, untimed, and gives it
  10 minutes. Without it,
  every cold row is reported as not run, with the reason "no cache-drop program given
  (--drop-caches)". Dropping the caches usually needs root, so the program is yours to provide: on
  Linux, one that runs `sync` and writes `3` to `/proc/sys/vm/drop_caches`, as
  `bin/perf-drop-caches` does through `sudo -n` (so it fails, rather than waits, where sudo would
  ask for a password); on macOS, one that runs `purge`. On Windows the harness has no cold runs, and every cold row reads "cold runs are not built
  on Windows in this harness".
- `--forklift-server` names the forklift-server binary, by path, that serves Forklift's remotes
  for the clone, push and pull rows. Without it, those rows are reported as not run, with the reason
  "no forklift-server given (--forklift-server)". On Windows they are never run, and read "remote
  rows are not built on Windows in this harness, which stops a forklift-server gracefully only on
  unix".
- `--against` names an earlier run's `perf.json`, by path, to set this run beside. See *Against an
  earlier run* below. It never fails the run. A path that names no readable file stops the run
  before anything is timed.

The history is written as a `git fast-import` stream into a bare repository. Each subject is built
from that repository the way `--history-from` builds one: git's subjects by copying it, Forklift's
by its own `git import`, and then enrolled with `office enroll`, so every record it makes is signed,
as a Forklift user's is. There are three subjects, as in section 7. `git` has the configuration
every repository the harness makes is given. `git-tuned` has every fsync on, fsck on transfer, the
untracked cache, the fsmonitor daemon where this git has one, and a commit-graph written untimed
after the build.

It times these operations:

| Row | Forklift | git |
|---|---|---|
| status | `stocktake --summary` | `status` |
| log | `history` | `log` |
| blame | `blame FILE` | `blame --first-parent FILE` |
| commit | `stack -m MESSAGE` | `commit -q -m MESSAGE` |
| switch | `shift perf-side` | `switch -q perf-side` |
| merge | `consolidate perf-side` | `merge -q --no-edit perf-side` |
| verify | `audit --full` | `fsck --full --strict` |
| pack | `store compact --all` | `gc -q` |
| clone | `clone URL WORK` | `clone -q --no-local URL WORK` |
| push | `lift` | `push -q -u origin HEAD` |
| pull | `lower` | `pull -q --ff-only` |
| diff | `diff` | `diff` |
| diff --staged | `diff --staged` | `diff --staged` |

Every row runs twice: once warm, and once cold, with the caches dropped before each repetition by the
`--drop-caches` program. A cold cell has no untimed run first.

Status, log, blame and verify run on the clean tree. Blame names the text file the history changes
most: for a real history, the one the last 1,000 first-parent commits of `HEAD` change most, the
first in path order on a tie. A file git takes for binary is never blamed, nor is a symbolic link.
Switch and merge name the side branch: `perf-side`, or the `--side-branch` of a real history.

Commit, switch, merge and pack change the subject, so each of their repetitions runs on a fresh copy of the
clean subject, made untimed. The subjects themselves are never changed. A copy keeps each file's
modification time, so that neither tool reads a settled file as possibly edited. Before anything
else the harness runs, untimed, the command that brings each tool's record of its files up to date:
Forklift's `load .` and git's `status`. A copy whose refresh fails makes its row **not
comparable**. Where tuned git runs
the fsmonitor daemon, its copy gets a daemon of its own. Before each commit, the harness appends a line to the same 20
tracked files and stages them untimed, with `load .` and `add -A`. Those 20 files are as new to each tool as
just after a user's edit, so a tool that looks again at a file changed moments ago pays for that
in the timed commit. Forklift's stack is signed and
git's commit is not, and the commit row's note says so. Switch moves from the current branch to
the side branch, and merge merges the side branch into it. On a generated shape that merge has no
conflict, since the two branches never change the same lines; on a real history, pick a side
branch that merges cleanly, since a merge that stops on a conflict is never timed as a merge. On unix, the harness
then writes the copy out to disk, untimed, so that a command which syncs its own writes is not also
timed writing out the copy: `sync` on Linux, and on macOS, whose `sync` can return early, an fsync
of every file in the copy and one flush of the drive's cache. On Windows it does neither. A copy
that cannot be removed afterwards stops the run, naming it.

Each write is judged before and after it runs. Before, the copy must report exactly the 20 staged
files for commit, and nothing at all for switch, merge and pack. After each write, the copy must be
clean, with nothing staged, changed or untracked. A copy that fails either check makes its row
**not comparable**, naming the repetition and the paths. Switch and merge are also not comparable
when the side branch is at the commit the current branch is at in any subject, since they would then
time a change of nothing. Merge is not comparable either when one of the two branches contains the
other in any subject, since it would then not join two lines of history. A warm cell's untimed run
of its command is made on a copy of its own, and is not judged.

These lines are the ones the fault scenarios run for their record, switch and merge steps, less the
`--json` the scenarios give Forklift's. A test holds the two to each other.

Clone, push and pull talk to a remote. Forklift's is a forklift-server root, served over HTTP on
loopback for each repetition and stopped after it. A server that ends on its own during a
repetition, or does not stop when asked, makes the row **not comparable**. git's is a bare
repository, reached by its path, whose `HEAD` names `main`. Before they are timed, the harness builds, untimed, two remotes for each subject:

- one holding `main` and the subject's current branch at the subject's head, which clone and push
  run against;
- one holding the current branch 3 commits past the head, which pull runs against.

Each of those commits appends a line to one of the 20 edited files. For Forklift they are signed,
like every other record it makes. Each repetition runs against a fresh copy of its remote, so no
repetition starts from what another one left behind. The copies are removed afterwards, like the
write pairs' copies, and the remotes themselves once the three rows are timed.

- Clone clones `main` into a new folder, named in the command as `WORK`. The new work tree must then
  be on `main` at the subject's head, by the same reads as below, and clean.
- Push runs in a fresh copy of the subject with the same 3 commits made on its current branch. git's
  remote is set as `origin`.
- Pull runs in a fresh copy of the subject itself.

Push and pull are judged by where the current branch stands, read before and after the timed
command: Forklift's pallet list and the remote's, and git's `rev-parse` and `ls-remote`. A push must
leave the remote's branch changed, and at the subject's head. A pull must leave the subject's head
changed, and at the remote's branch. A command that succeeded without carrying the head across, or a
clone that landed anywhere else, voids its cell (see below). Each copy must also be clean before and after, as for the write pairs.

Push makes the subject 3 commits ahead of its remote, rather than the remote 3 commits behind the
subject, because Forklift's subject is enrolled at its head. A remote behind that head would lack the
record that trust begins at, and forklift-server refuses an unsigned record it cannot place before
it. A remote that could not be built makes all three rows **not comparable**, naming what failed.

Then the harness appends a line to the same 20 tracked files in every subject, for diff. For diff
--staged, it stages them untimed with `load .` and `add -A`. Every Forklift line runs without
`--json`.

Before a pair is timed, the harness checks that every subject sees the same change, by the paths
each one reports: Forklift's `stocktake`, and git's `status --porcelain -z` with every untracked
file listed. Before the clean-tree rows, no subject may report any path, untracked ones included.
Before diff, each must report exactly the 20 edited files as changed and not staged. Before diff
--staged, each must report exactly those 20 as staged and nothing else. A count that agrees is not
enough: a subject that misses one edited file and reports one other path does not pass. A pair that
fails its check is reported as **not comparable**, with each subject's extra and missing paths, and
is not timed.

### The gate subject

Two checks compare Forklift with itself rather than with git, and they need a subject the paired
ones cannot give. An import packs Forklift's store as it goes, so a compact of the paired subject
has nothing loose to pack, and its imported history carries no signature for an audit to verify.
So the harness builds one more subject, Forklift's alone, through Forklift's own commands and with
no import: `prepare`, 240 text files and a first `stack`, then `office enroll`, then 299 more
stacks that each change 5 files. That leaves 300 parcels, 299 of them signed, in a loose store. The
harness checks that `audit main` reads 299 verified parcels and 1 unsigned one before timing
anything. Its rows have one cell, Forklift's, and run warm:

| Row | Forklift |
|---|---|
| fresh compact | `store compact` on a fresh copy of the loose store, each repetition |
| repack | `store compact --all` |
| audit main | `audit main` |
| audit main, one core | `audit main` pinned to one core with `taskset -c 0` |

One more compact, untimed, packs the subject itself, so the repack runs over a packed store. A
compact that packs nothing makes the fresh compact's row not comparable. Pinning a command to one core is
built only on Linux, with `taskset` on the `PATH`; elsewhere the last row is reported as not run.

### The verdict

`perf.md` ends with the verdict, and `perf.json` holds it too. Under either gate, a run fails on:

- a **gross regression**: on the status, diff, diff --staged, switch, commit, verify and pack rows,
  Forklift's median more than 10 times tuned git's in the same run. This is a net for an order of
  magnitude, such as an accidental quadratic loop, not a benchmark;
- a **broken ratio of Forklift's own**: the repack's median above 1.5 times the fresh compact's, or
  the audit's above twice the one-core audit's;
- any of those rows, or of the gate subject's, that **could not run**: not comparable, with a
  voided cell, or missing a cell it is judged by (Forklift's, and on a pair row tuned git's).

Each of these compares two timings taken on the same machine in the same run, so a shared, noisy
CI runner can hold them. `--gate self` stops there. CI runs it on the small shape in `proof-load-perf.yml`, on every push
to `main` and on a pull request that touches the code the timed binaries or the harness are built
from. The same workflow runs `--gate all` weekly (see section 9). `--gate all` also fails on any other row that could not run, and on a row that is **clearly slower** with
no ticket tracking it. Clearly slower means Forklift's median is more than 1.25 times tuned git's,
and its fastest run is slower than tuned git's slowest. The verdict lists every clearly slower row
under either gate, with its ticket when one tracks it. A row not run, for any reason above, never
fails the run.

The tickets live in `crates/proof-load/perf-tickets.json`, a list of entries that each name an
operation, a shape, a cache (`warm` or `cold`), a platform (`linux`, `macos` or `windows`) and a
`FORK-N` ticket. An entry tracks only the row it names on that shape and platform. When a row an
entry tracks was timed and is not clearly slower, the verdict lists the entry as possibly closable;
that never fails the run. A change that makes a row clearly slower on purpose adds its entry, and
the change that closes the ticket removes it. A test holds every entry to a row this build times
and a shape it has, and to one entry per row. Since an entry names a generated shape, no entry can
track a row of a real history, so `--history-from` is judged under `self` and refuses `--gate all`.

### Against an earlier run

With `--against`, `perf.md` has a section after the verdict, and `perf.json` a field, that set each
row beside the same row of the earlier run, by its operation and cache. For each row, it gives
Forklift's median as a percentage of tuned git's, rounded, in the earlier run and in this one. A
ratio is compared rather than a time, because the two runs may have run on two machines; the
section names the earlier run's forklift, git and hardware. A row where either run did not time
both Forklift and tuned git, or where tuned git's median was 0 µs, or that only one run has, is
listed as not compared, with why.

The earlier report is not compared at all when this build does not read it (another format, or not
UTF-8 text), or when it timed another history (another shape, the same shape with other numbers or
another seed, or another side branch), ran on another platform, or gave either git subject another
configuration. The section then says why. However far a ratio moved, the comparison never fails the
run: the verdict alone does.

### Before you quote a number

These matter as much as the numbers.

- **Disk.** A real history takes several times its own size: the mirror, three subjects built from
  it, and one fresh copy at a time for the write rows. git.git is a few hundred MB of history and
  fits in a few GB. A kernel-scale history such as `torvalds/linux` is about 5 GB of history, needs
  tens of GB, and takes minutes to import. Check `df -h` before you start one.
- **Log output differs.** `history` and `log` do not print the same text. The row compares the cost
  of each tool's default command, not byte-for-byte output.
- **Status is a summary.** `stocktake --summary` prints counts, and git's `status` a short list.
  Neither formats a long per-file report.
- **Durability and signing differ.** Every Forklift record is signed, and git's commit is not, as
  the commit row's note says. Tuned git runs with `core.fsync=all` and `core.fsyncMethod=fsync`,
  which plain git does not set; git's own default sync method on macOS is documented as possibly not
  durable over a crash. Read the tuned column when the question is like for like.
- **The git binary matters.** An old or emulated git slows git's side a great deal: a git running
  under Rosetta once more than halved the ratios of some rows against the same Forklift. Check `git
  --version` and the binary's architecture, which the header records with the tools.
- **One machine, one run.** Quote the header's hardware with any number. A ratio between two
  subjects of one run travels better than an absolute time.
- **Imported text may be coerced.** An imported history with author names that are not UTF-8 has
  them made valid, and shows U+FFFD where it did. The author's email is kept as it was.

### Reading perf.md

`perf.md` is for reading, and `perf.json` holds the same report for programs, under its own format
number. A reader refuses a `perf.json` of any other format. The header records what the graded
report's header records, plus the machine: its CPU model, logical cores and memory.

Each row is named by its operation and its cache, as in `### merge, cold`, and has one cell per
subject:

- **median**, **fastest** and **slowest**: the wall time over the repetitions, never the mean. The
  clock starts just before the command is started and stops when it exits (on Windows, within a
  millisecond of it, because the harness polls for the exit there).
- **peak memory**: the most memory the command held resident at once, as the median with its range.
- **cores**: CPU time over wall time, so a figure above 1 means the command ran on several cores at
  once. A cell where any repetition used under 20 ms of CPU time reads *too short to
  measure*.

On Windows only the wall time is measured, and the memory and cores cells say *not measured on
Windows*.

A repetition that exits non-zero, or dies of a signal, voids its cell, which then shows the exit or
the signal and the command's stderr instead of a timing. A run killed at its limit, the untimed one
included, voids its cell too, which then says so and shows no stderr. In a cold row, a cache-drop
program that exits non-zero, or dies of a signal, voids the cell before the command runs, and the
cell shows the program's exit or signal and its stderr. A cache-drop program that runs past its 10
minutes is killed and voids the cell too, and the cell says it was the cache-drop program. A push or
pull that exits 0 without carrying the head across, or a clone that exits 0 on another branch or
commit, voids its cell as well. The cell then says *did
not move the head*, with the repetition and the refs it read.

Read two medians together with their spreads. A difference where one subject's fastest run is
slower than the other's slowest is clear. One where the ranges overlap is likely to be noise.

`perf` exits 0 when the verdict failed nothing under its gate, and 1 when it failed a row. It exits
2 when the run could not go ahead, such as over a non-empty output folder, an unknown shape, both
`--shape` and `--history-from`, `--gate all` with `--history-from`, a history it cannot mirror or
read, a side branch the history does not have, or a subject that could not be built. A run that exits 2 writes no `perf.json`.

## 12. Replaying merges

`proof-load replay` replays the two-parent merges of a real git history in Forklift and sets each
beside git's merge of the same two commits. Like `perf`, it is a separate command from `run` and
faults nothing. `run` asks whether Forklift keeps what it wrote; `replay` asks whether Forklift's
merge of two real commits is the merge git would have made.

```sh
cargo build --release -p forklift -p proof-load
target/release/proof-load replay --source . --out ../replay-out \
  --forklift target/release/forklift --git "$(command -v git)"
```

- `--source` is the git repository whose merges are replayed. It is only read, and a shallow
  repository is refused. The harness mirrors it into the output folder first and works on the
  mirror.
- `--ref` is the commit whose history is replayed: any revision git resolves to a commit, such as a
  branch, a tag, a commit id or a detached `HEAD`. It defaults to `HEAD`. The mirror is left holding
  one branch at that commit, so the import reads that history and nothing else.
- `--out` is an empty or new folder for the scratch area and the report.
- `--forklift` and `--git` name the two binaries by path.
- `--sample N` replays the first N merges only. Without it, every merge is replayed.
- `--seed S` shuffles the merges with that seed before the sample is taken, using the same
  generator as the fault seeds. Without it, the merges keep `git rev-list --merges` order, newest
  first.

Each tool runs in the scrubbed environment of section 1. Merges of more than two parents are counted
in the header and not replayed.

### How a merge is replayed

Before any merge is replayed, the history is rewritten once, and the header counts what was dropped:

- **Gitlink entries** (submodules), which Forklift's import skips.
- **Changes to `.forkliftignore` files.** A history's own `.forkliftignore` files can predate the
  current ignore dialect, and Forklift cannot shift away from such a revision. Until that is fixed,
  the replay drops them, and the header names the ticket for it.

Forklift then imports the rewritten mirror once. Each merge M with parents P1 and P2 gets three
branches, `m-<M>-o`, `m-<M>-t` and `m-<M>-m`, at P1, P2 and M. To replay M, the harness shifts to
P1's branch and consolidates P2's. Before it compares anything it checks two things, and a row that
fails either is an `error`:

- the rewritten M still has P1 and P2 as its parents;
- the tree Forklift checks out for P1 is P1's git tree.

Forklift's tree is hashed with git through a fresh index each time, so a stat cache left by an
earlier hash never stands in for a file's content. The hash reads attributes from the empty tree
and turns line-ending conversion off, so a history's own `.gitattributes` does not rewrite the
files Forklift checked out before they are compared.

A consolidate that reports an outcome the harness does not know, or conflicts that are not a list
of paths, makes the row an `error`. Once a row is compared, a conflicted consolidate is aborted.
When Forklift refuses the abort, the harness puts the conflicted files back as P1 has them and
aborts again, and the row is an `error` that keeps the paths its comparison found. Where the
comparison itself failed, its failure is the row's reason, and a failed clean-up after it is
recorded apart, as `clearing` in both reports. After any `forklift-refused`, `both-refused` or
`error` row the harness shifts to the ref's branch; when that is refused, no later row could
replay, so the run stops with exit status 2, naming the row, its reason and its `clearing`.

### Comparing with git

Each row has three of git's merges to compare with, all `git merge-tree --write-tree` with
`merge.conflictStyle=diff3`. Like the hash, git's merges and the hunk diffs read attributes from
the empty tree, so a `.gitattributes` in the history, such as a marker size or a merge driver, does
not change git's merges, which Forklift's do not follow either:

- **G**, git's own merge;
- **G1**, the same with rename detection off (`-X no-renames`);
- **G0**, G1 on a single merge base, as Forklift merges. Where the two commits have several bases,
  git merges them into a virtual base first, while Forklift takes one. G0 is then G1 run against
  each base in turn, and the run that differs from Forklift's merge at the fewest paths is used,
  the earliest listed base on a tie. The harness cannot see which base Forklift took, so G0 is the
  single-base merge closest to Forklift's: a `multi-base-gap` shows that some single base accounts
  for Forklift's file, not that it is the base Forklift used.

A path differs between two merges when:

- it is conflicted in one and not the other;
- or it is clean in both, with different content or present in one only;
- or it is conflicted in both, and the files still differ once the labels after the conflict
  markers are taken out.

Each differing path comes to one kind:

| kind | Forklift against git | what it means |
|---|---|---|
| `divergence` | Forklift differs from G0 | Forklift's merge differs from git's merge of the same base with no renames. Its subtype says how. |
| `rename-gap` | Forklift equals G0, and G differs from G1 | The difference is rename detection, which Forklift does not do. |
| `multi-base-gap` | Forklift equals G0 and G equals G1, but G1 differs from G0 | The difference is the virtual base git builds from several bases. |

A divergence has a subtype:

| subtype | what it means |
|---|---|
| `silent-wrong` | Both merged clean, to different content. |
| `forklift-clean-git-conflicted` | Forklift merged clean where git conflicted. The path row's `equals_committed` says whether Forklift's file is the one the merge commit holds. |
| `forklift-extra-conflict` | Forklift conflicted where git merged clean. |
| `conflict-content` | Both conflicted, and the files differ outside the labels. |
| `record-mismatch` | Forklift merged or fast-forwarded, and a stocktake right after it reports changes. This checks Forklift against itself, once per row, with no path. |

A gap path also has a direction: `forklift-conflicted` or `forklift-clean`.

### Classes

Each row comes to one class:

| class | what it means |
|---|---|
| `agree` | No path differs, Forklift merged clean, and its tree is the merge commit's. |
| `hand-edited` | No path differs and Forklift merged clean, but the merge commit holds another tree: whoever made it changed it after merging. |
| `agree-conflicted` | No path differs, and both tools conflicted, on the same paths with the same content. |
| `gap` | At least one path differs, and every one is a gap. |
| `divergent` | At least one path is a divergence. |
| `both-refused` | The two commits have no merge base, and neither tool merged them. |
| `forklift-refused` | Forklift refused where git merged. |
| `error` | The row could not be replayed or compared: a self-check above failed, a tool failed, or git refused where Forklift merged. |

### Tickets and the verdict

The tickets live in `crates/proof-load/replay-tickets.json`, built into the harness. It holds three
kinds of entry:

- a **cause entry**, `{"cause": "rename" | "multi-base", "ticket": "FORK-N"}`, covers every gap of
  that cause, wherever it shows;
- a **path entry**, `{"merge", "path", "kind", "blob", "ticket"}`, covers one divergence: that merge
  (its full id), that path, that subtype, and that content of Forklift's file (its git blob id, or
  `absent`);
- a **row entry**, `{"merge", "kind", "reason", "ticket"}`, covers one `forklift-refused` or
  `error` row: that merge, that kind, and a reason that contains the entry's `reason` text. When
  the row fails for another reason, the entry stops matching. A failed clean-up recorded as
  `clearing` is not part of the reason.

A `record-mismatch` is about a row's whole tree, so its path entry gives an empty `path` and
`blob`.

A path entry covers a path only when all four fields match. When Forklift's merge of that path
changes, the entry stops matching and the run fails until someone looks.

The run fails on:

- a divergence no path entry covers;
- a gap whose cause has no entry;
- a `forklift-refused` or `error` row with no row entry.

An `error` or `forklift-refused` row's paths, where the comparison found any before the row
failed, are judged as on any other row.

A cause entry never covers a divergence. An entry that no row of the run matched is listed as
possibly closable and never fails the run, so in a sampled run the entries of merges outside the
sample are listed too. A test holds every entry to a `FORK-N` ticket and a full merge id, every path
entry to a divergence kind with a path and a blob id or `absent` (a `record-mismatch` to an empty
path and blob), and every row entry to a row kind and a non-empty reason.

A change that introduces a divergence on purpose adds its entry. The change that fixes it removes
the entry.

### Reading replay.md

`replay.md` is for reading, and `replay.json` holds the same report for programs, under its own
format number. `replay.md` has:

1. **Header**: the source, the ref and the commit it resolved to, the sample and the seed, the merge
   counts, what the rewrite dropped, the tools, the platform, and the import and replay times.
2. **Classes**: the number of rows in each class.
3. **Verdict**: every failure, with its merge, path, kind, blob and reason, then the possibly
   closable entries.
4. **Rows other than agree**: each row's merge-base count, what each tool came to, its differing
   paths, and up to 5 hunks.

A hunk is the first `-U3` hunk of the diff between Forklift's file and git's, cut at 40 lines.
Divergences come before gaps, and the row says how many paths it left out. A divergence is shown
against G0 and a gap against G. Where both files are conflicted, the diff is taken with the labels
removed, so the hunk shows the line they differ on. A `silent-wrong` path also gets a hunk against
the merge commit's file.

`replay` exits 0 when the verdict failed nothing, and 1 when it failed. It exits 2 when the run
could not go ahead: a non-empty output folder, a shallow or unreadable source, a ref that is not a
commit, `--sample 0`, a failed import, or a warehouse that cannot shift after a refused or `error`
row, for example. A run that exits 2 writes no `replay.json`.
