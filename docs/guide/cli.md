# The forklift CLI — user guide

Every command forklift ships, organized by what you're trying to do. If you know
git, the [terminology map](README.md#the-warehouse-model-in-one-minute) will get
you oriented fast.

Conventions in this guide:
- `forklift <command>` — most commands have a short alias, shown in the reference
  table at the end and in each command's `forklift help <command>`.
- Three global flags apply to every command: `-v`/`--verbose` (more detail),
  `--json` (machine output — see [Machine & agent output](#machine--agent-output)),
  and `--no-pager` (see [Paging](#paging) below).
- `forklift help` lists everything; `forklift help <command>` explains one command
  (subcommands work too: `forklift help office admit`).

Contents:
1. [Getting started](#1-getting-started)
2. [Tracking changes](#2-tracking-changes)
3. [Inspecting history](#3-inspecting-history)
4. [Branching and merging](#4-branching-and-merging)
5. [Parking work in progress](#5-parking-work-in-progress)
6. [Working with remotes](#6-working-with-remotes)
7. [Identity, signing, and agents](#7-identity-signing-and-agents)
8. [Machine & agent output](#8-machine--agent-output)
9. [Configuration reference](#9-configuration-reference)
10. [Command & alias reference](#10-command--alias-reference)
11. [Exit codes](#11-exit-codes)

---

## 1. Getting started

### `prepare` — create a warehouse

```sh
forklift prepare
```

Creates `.forklift/` in the current directory: the object store, the inventory,
the config folder, the pallets folder, the default pallet `main` (unborn until
your first `stack`), a warehouse config file, and an ignore file. Idempotent —
running it in an existing warehouse only creates what's missing. `-v` lists each
piece created.

### `import-git` — migrate a git repository

```sh
cd my-git-repo
forklift prepare
forklift import-git .
```

One-way migration of a git repo's history into the warehouse: git commits become
parcels, trees become trees, blobs become blobs, and each **local branch becomes a
pallet** (git's author/committer split maps onto forklift's author/stack). Git's HEAD
branch is checked out, and `.git/` is added to `.forkliftignore` so a colocated repo
reads clean. Requires `git` on your PATH. The imported history is **unsigned** (it
predates trust) — import into a fresh warehouse, then `office enroll` to anchor it as the
legacy boundary (a later `audit` then tolerates the imported parcels as legacy). Submodules
(gitlinks) are skipped with a warning. For agents, this means a project can be moved onto
forklift with one command.

A large history lands hundreds of thousands of objects at once — the case the loose store
is slowest and largest in — so the imported objects are written **straight into native
packs** ([`compact`](#compact--pack-the-object-store)'s format, without the detour through
one loose file per object), delta-compressing successive versions of files and directory
trees on the way in. The store arrives dense and you never have to remember to `compact`
it. Pass `--no-compact` to store loose objects instead (e.g. to inspect the raw store or
benchmark the loose baseline). Because that packing only ever sees one file's own history as
it streams in, `import-git` tips a one-line suggestion (also visible from `store`) that a
one-shot `compact --all --redelta` could shrink the result further by delta-compressing
across the whole store, including similarity a per-path pass cannot see.

Refuses in a scoped (sparse) bay (see `bay add --scope` below): importing builds
every pallet's history straight from the git tree, bypassing the sparse overlay entirely,
and materializes the whole imported HEAD — not a sensible operation to scope. Run it from
a full workspace.

### `export-git` — migrate back to git

```sh
forklift export-git ../my-repo-in-git
```

The reverse of `import-git`, and the **escape hatch**: parcels become commits, trees become
trees, blobs become blobs, and each **user pallet becomes a branch** (git's HEAD is set to
the current pallet, and its working tree is checked out). The point is that trying forklift is
reversible — your code history is never trapped. Requires `git` on your PATH; the target path
must be empty or new. **Lossy in this direction:** git has no home for the signed office, the
`@manifest` (approvals/provenance) or per-parcel signatures, so those are dropped. Author
identity round-trips as the on-chain **identifier** — forklift never stores display names
on-chain (zero-PII by design), so the git author name is that identifier. The meta pallets
(`@office`, `@manifest`) are not exported.

### `config` — who you are, and settings

Identity is zero-configuration: if you never set anything, an opaque operator id
(a UUID) is minted on first use and your history is pseudonymous. To be
recognizable, set a name:

```sh
forklift config operator.name "Ada Lovelace"
```

`config` has four forms:

```sh
forklift config                       # list every known key and its value
forklift config <key>                 # print one value
forklift config <key> <value>         # set a value (warehouse scope)
forklift config --unset <key>         # remove a value
```

- Add `--global` to target your per-user config (`~/.forkliftconfig`) instead of
  the warehouse. Reads consult the warehouse first, then fall back to global.
- Unknown keys are rejected (a typo can't silently do nothing).
- The known keys are listed in [Configuration reference](#9-configuration-reference).

### `.forkliftignore` — files forklift shouldn't track

`prepare` writes a `.forkliftignore` at the warehouse root. Each line is a
**regex** (not a glob) matched against warehouse-relative, `/`-separated paths.
The defaults ignore `target/`, `.idea/` and `.git/`; `.forklift/` itself is
always ignored. Example entries:

```
^target\/?.*$      # a folder called target
\.log$             # any .log file
```

### `alias` — the `fl` short name

Forklift ships a short `fl` alias (`f` and `l` are both left-hand home row —
quicker to type than `git`). It is a **shell-agnostic symlink** next to the
`forklift` binary (a `.cmd` shim on Windows, where a symlink needs elevated
privilege), not a per-shell alias — it works from scripts, non-interactive
shells and every shell dialect alike, and one `ls`/`rm` finds or removes it.

Every installer (the curl script, `pult install`, and self-update — see
[`self-update`](#self-update--check-for-and-apply-a-newer-release)) creates it
by default, calling the same command under the hood:

```sh
forklift alias install            # create "fl" next to this binary
forklift alias install fl2        # create a differently-named alias instead
forklift alias uninstall          # remove it
forklift alias                    # (or "alias status") report whether it's installed, and where
```

`install` is idempotent (an alias that already points at this binary succeeds
without change) and refuses — deliberately, with no `--force` — to overwrite
anything it did not create: a real file already named `fl`, or a symlink
pointing somewhere else. `uninstall` is a no-op if nothing is installed, and
also refuses a foreign real file, though it does remove a symlink/shim
pointing elsewhere (deleting a symlink can never lose data). Set
`FORKLIFT_NO_ALIAS=1` before installing to skip alias creation — there is no
interactive prompt (`curl … | sh` has no TTY to answer one).

---

## 2. Tracking changes

The cycle is: change files → **load** them into the inventory → **stack** a
parcel. `stocktake` shows you where you are; `diff` shows the actual lines;
`restore` throws changes away.

### `load` — stage changes (`l`)

```sh
forklift load .                # stage everything under the current directory
forklift load src/main.rs      # stage one file
forklift load src              # stage a directory
```

`load` reconciles each directory against its inventory shard: new files become
tracked, modified files are re-hashed, deleted files are staged as removals. It
is incremental — unchanged files are recognized by their stat data and never
re-read. `load` (like every mutating command) holds the warehouse lock while it
runs, so two forklift processes never interleave.

### `unload` — unstage (`ul`)

```sh
forklift unload src/main.rs
```

The inverse of `load`: resets the path's inventory entries to the pallet head,
so the next parcel won't record the staged change. The working directory is
**not** touched — your edits stay, they're just no longer staged. (`restore
--staged` does the same thing; `unload` is its natural name.) To stage a
removal instead, use `remove`.

### `remove` — stage a removal (`rm`)

```sh
forklift remove old.txt
```

Marks the path's inventory entries as deleted so the next parcel won't contain
them. The working directory is **not** touched (the file stays on disk). To
un-stage the removal, `load` the file again (if it's still there) or use
`unload`.

### `stocktake` — status (`st`)

```sh
forklift stocktake            # full report
forklift stocktake --summary  # just the counts (cheap; good for scripts/agents)
```

Reports the current pallet and its head, then two sets of changes:
- **Staged** — inventory vs the pallet head (what the next `stack` will record).
- **Not loaded** — working directory vs the inventory (what a `load` would stage).

Moves are detected and shown as `moved: old -> new` when a file's content is
unchanged but its path changed.

### `diff` — line-by-line changes (`d`)

```sh
forklift diff                       # working directory vs inventory (what load would stage)
forklift diff --staged              # inventory vs pallet head (what stack would record)
forklift diff main feature          # two revisions, tree vs tree
forklift diff main feature src/     # ...limited to a path
forklift diff --verbose             # include unchanged context lines
forklift diff :empty main           # main's root parcel vs nothing: every file is "added"
```

Binary files are reported, not printed. Whitespace-only changes are shown faint
(they're still real changes — the object store hashes them). With `--json`, diff
reports the changed-file set (path + kind) rather than every line.

A large file (stored in chunks — see below) is a binary as far as diff is concerned:
a changed one reports as `(binary contents; not shown line by line)`, never assembled
or line-diffed.

The two-revision form accepts anything `show` and `history` do — a pallet name, an
`@`-qualified meta pallet, or a parcel hash prefix — plus one reserved token,
`:empty`, meaning the empty tree. It exists for a root parcel, which has no real
"before": `diff :empty <revision>` compares it against a clean slate, so every file
it introduces lists as `Added` instead of `diff` refusing for want of a second
revision. `:empty` can never collide with a real revision — a pallet/meta name is
restricted to ASCII letters, digits, `.`, `_`, `-` and `/`, and a hash prefix is hex
digits only, so neither grammar can ever contain a `:`.

### `restore` — discard changes (`r`)

```sh
forklift restore path            # rewrite path from the inventory (drop unstaged edits)
forklift restore --staged path   # reset the inventory entry to the pallet head (unstage)
```

`restore path` overwrites the working file(s) from what's staged — it throws away
unstaged edits, so use it deliberately. `restore --staged path` leaves the
working directory alone and only un-stages (the inverse of `load`) — the same
operation as `unload`.

### `stack` — commit (`s`)

```sh
forklift stack "a clear description"
forklift stack                       # opens nothing; a description is optional but recommended
```

Builds tree objects from the inventory (bottom-up, staged removals excluded,
empty directories pruned), creates a parcel (recording you as the author with a
timestamp), advances the current pallet's head, and cleans up consumed staged
state. If a consolidation is in progress, `stack` completes it. On a warehouse
where trust is established, `stack` also signs the parcel (see
[Identity, signing, and agents](#7-identity-signing-and-agents)).

`stack` requires staged changes — it refuses to create an empty parcel.

### Large files

Large files are handled natively — no separate tool, no pointer files. A file at or above
8 MiB is split into content-defined chunks and stored as a small **recipe** (the ordered list
of chunk hashes) plus the chunks themselves; everything below stays a single object. This is
transparent: you `load`, `stack`, `shift`, `park` and `restore` large files exactly like any
other, and they round-trip byte-for-byte. The wins are streaming (a multi-GB file is never held
whole in memory), resumable transfer, and deduplication — an edit re-stores only the chunks it
actually touched, and two files (or two revisions) that share content share their chunks.

Chunking is content-addressed like everything else, so a chunked file's authorship is signed and
offline-verifiable down to every byte. Two commands treat a chunked file as the opaque binary it
is: [`diff`](#diff--line-by-line-changes-d) reports it changed without a line diff, and
[`blame`](#blame--who-wrote-each-line-bl-annotate) refuses it (there are no lines to attribute).

Over the wire, chunked files travel **per object**: a `lift` uploads the recipe and every chunk
(re-uploading only the chunks that changed — an appended-to file re-sends its new chunks and
nothing else), and a `franchise`/`lower`/`expand` fetches each in-scope chunk it doesn't already
have. A lift only advances the remote's ref once every chunk is confirmed present, so a chunked
file is never left half-transferred. Two limits are worth knowing: a `bundle` never carries a
chunked file (its chunks are reachable only through the recipe, which a bundle doesn't descend —
move that history over the wire instead), and lifting a chunked file to a remote that predates
chunking support is refused up front, naming the file (`chunked_transport_unsupported`, exit 14) —
upgrade the remote, or keep the file under the 8 MiB threshold.

### `undo` — reverse the last operation

```sh
forklift undo
```

Reverses the **last state-changing operation**, using an undo journal — not just the
last `stack`. Each `stack`, `consolidate` and `shift` snapshots its pre-operation state
(the pallet refs, current pallet and any in-progress consolidation — all tiny, since the
objects are content-addressed and already stored), so `undo` can walk them back one at a
time:

- Reversing a **`stack`** or **`consolidate`** is a *soft reset*: the head moves back while
  the working directory and inventory are kept, so the undone changes are staged again
  (like git's `reset --soft HEAD~1`). This is how a **merge is reversed** too — merge
  parcels are no longer refused.
- Reversing a **`shift`** moves back to the previous pallet and re-materializes its tree (it
  refuses on a dirty working directory, just like a forward `shift`).

Re-`stack` to redo (e.g. with a corrected message). When the journal is empty (a stack made
before this feature), `undo` falls back to soft-resetting the current pallet's head to its
first parent. Undoing a pallet's *very first* parcel, pure-staging (`load`/`remove`/`unload`)
and trust/remote commands are out of scope; `park` is reversed with `park pop`.

### `peek` — inspect an object (`pk`)

```sh
forklift peek <object-hash>          # print a blob / tree / parcel
forklift peek --inventory src        # print a folder's inventory entries
forklift peek --inventory src -v     # ...with full stat detail
```

A debugging/inspection tool: it decodes any object by hash (a blob's text, a
tree's entries, a parcel's tree/parents/actions/description) or dumps a folder's
inventory shard. With `--json`, a binary blob (a NUL byte anywhere, or bytes that
are not valid UTF-8) reports `"binary": true` and omits `content` instead of
mangling raw bytes through a lossy text conversion.

### `show` — a file's content at a revision

```sh
forklift show main:src/app.rs               # the file as of main's head
forklift show a1b2c3:src/app.rs             # ...at a parcel hash prefix instead
forklift show main:logs/build:latest.txt    # a path may itself contain ":" — only
                                             # the first ":" splits revision from path
```

The one-invocation equivalent of resolving a revision, walking its tree to a path
and peeking the blob by hash yourself — a single call in and out for a caller like
an editor's review panel. The argument is `<revision>:<path>`, split on the *first*
`:` — a revision (a pallet name, an `@`-qualified meta pallet, or a hash prefix)
can never contain `:`, so the split is unambiguous even when the path does.

A large file (stored in chunks) reports its metadata — content hash, total size,
chunk count — instead of assembling it; non-text content (a NUL byte anywhere, or
bytes that are not valid UTF-8) reports binary. Either way `content` is absent and
`binary` is `true`. In human mode a short notice prints instead of raw bytes; in
`--json` mode see [`docs/MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md) for the
exact shape.

---

## 3. Inspecting history

### `history` — the log (`hi`)

```sh
forklift history                 # from the current pallet's head, newest first
forklift history feature         # from another pallet
forklift history 1a2b3c          # from a parcel (hash prefix, ≥ 4 chars)
forklift history @office          # the office's audit trail (users & keys — a meta pallet)
forklift history --class agent    # only parcels an agent authored
forklift history -n 20            # only the 20 newest parcels (bounded walk)
forklift history --oneline        # one terse line per parcel: abbreviated hash + subject
forklift history -n 20 --json                  # a page + a `next` cursor (for agents)
forklift history --after <cursor> -n 20 --json # the following page
```

Walks the parcel graph newest-first (ordered by the latest action timestamp, so
merged lines interleave sensibly). `-n`/`--limit` bounds the walk to the newest N
parcels — on a large history that loads only those N and their frontier, not the
whole graph. Shows each parcel's hash, the parents it
consolidates (for merge parcels), its actions (author/stack with operator and
time), and the description. `--oneline` prints just the abbreviated hash and the
description's first line (git's `log --oneline`) — and, since it shows no author or
class, it skips the office read and the display-name resolution the full form does,
so it is the fastest way to scan history. **Machine authorship is legible:** an author that is an
agent, bot or service is shown as `[agent, supervised by <human>]`, read from the
signed office record — so authorship stays forge-proof, and `--class
<human|agent|bot|service>` filters the log to answer "which parcels did agents write,
under whose supervision".

With `--json`, each entry also carries `parents` — every parent of that parcel, in
their stored (canonical) order — base-first for a consolidation — `[]` for a root —
which is what a caller building a graph (rather than just reading a log) walks:

```json
{ "parcel": "<hash>", "parents": ["<base>", "<other>"], "consolidates": ["<base>", "<other>"], "actions": [/* … */] }
```

`history` on a pallet with nothing stacked on it yet fails with the `empty_history`
error code (exit 19) rather than a generic one.

One trust note: `history --class` reads the **recorded** (self-declared) identity —
each action in `--json` says so with `"trust": "recorded"`. `query --class` is the
verified counterpart.

### `query` — filter history on signed identity (`q`)

```sh
forklift query --class agent                    # parcels verifiably signed by an agent
forklift query office --class agent --unsupervised   # the compliance headline, scoped
forklift query --supervisor alice@corp          # work supervised by one human
forklift query --signer 91ff02                  # parcels signed by one key (prefix)
forklift query --grep 'fix*walk' --no-merges    # parcel-local facts compose in
forklift query --from v1-parcel feature         # scope: feature's line since a base
forklift query --recorded --class agent         # the cheap, labeled, unverified read
forklift query --model 'claude-*'               # recorded machine-authorship model
forklift query --tag v1.2.0                     # carries this signed tag
forklift query --touches src                    # changed this path (or under it)
forklift query --where '{"any":[…]}' --json     # full or/not/nesting as a JSON tree
```

The compliance question `history` cannot answer: *"all parcels authored by an agent,
unsupervised"* — answered **verified**. Where `history --class` trusts what the parcel
records, `query --class` checks the signature and joins the office off the *verified
signer*; a parcel that merely *claims* a human author but is signed with an agent's
key matches `--class agent`, and an unsigned parcel matches no identity predicate at
all (its class is unknowable, and a compliance filter must not guess). Answers carry a
trust label per match (`verified`, `signed-revoked`, `unsigned`, `unknown-key`, or
`recorded` under `--recorded`) and a `scope` block saying what the pass covered. A
`signed-revoked` match also carries `signer.boundary` — `"vouched"` when it sits inside
the revoking key's distrust boundary, or `"suspect"` when it sits outside it (a forged
backdate, or the key's holder kept signing after the revocation); `audit` refuses a
suspect parcel outright, but a read-only query labels it loudly instead. Filter on it
with `--where '{"field":"signer.boundary","op":"eq","value":"suspect"}'`.

`--model`/`--tool` read recorded machine-authorship provenance (a `manifest provenance`
entry): a parcel with *no* provenance recorded at all never matches, in either
direction — absence is not a claim to negate, so `--where '{"not": {…matches…}}'`
still excludes it. `--tag` is plain membership: an untagged parcel simply does not
carry the tag (excluded, and included by its negation) — only a warehouse with no
`@tags` pallet at all reads every `--tag` predicate as unknowable. `--touches <path>`
tests whether the parcel's tree differs from its **first parent's** at that path (a
file or a directory prefix); a merge parcel is judged the same way `blame` already is
for line attribution — if the merge's own result at the path already matches what its
first parent had (e.g. both branches added the identical content), it does not match,
even though its other parent touched the path.

Flags AND together; `--where` takes a JSON predicate tree (`all`/`any`/`not` over
`{"field","op","value"}` leaves) for anything flags can't express. Match patterns are
globs (`*`, `?`) or plain substrings — never regex. Pagination works like history
(`-n`, `--after`, `--oneline`).

The honest cost: `--limit` bounds *output*, not verification — a verified identity
query resolves every candidate its walk considers. Scope the walk (a pallet argument,
`--from`) to bound the work; unscoped over all of history costs about an `audit`.

### Paging

Forklift pages output the way git does. When stdout is a **terminal**, long human
output (`history`, a big `diff`, …) is piped through a pager so it is scrollable
instead of scrolling off the top:

- The pager is `$FORKLIFT_PAGER`, then `$PAGER`, then `less` (with git's `LESS=FRX`
  when unset: quit if it fits one screen, keep color, don't clear on exit).
- Short output that fits one screen prints inline; there is no pager to dismiss.
- `--no-pager` disables it, and `--json` or any **non-terminal** target (a pipe, a
  file) streams raw — never a pager. `history` streams as it walks, so
  `forklift history | head` (or quitting the pager early) stops the walk instead of
  reading the whole graph.

**For agents**, page `--json` output with a cursor rather than a pager:
`forklift history -n 20 --json` returns 20 parcels plus a `next` cursor; pass it as
`forklift history --after <cursor> -n 20 --json` to read the following page, until
`next` is absent (the history is exhausted). The cursor is opaque — treat it as a
token to hand back, not something to construct.

### `blame` — who wrote each line (`bl`, `annotate`)

```sh
forklift blame src/main.rs            # each line, attributed to the parcel that introduced it
forklift blame src/main.rs --rev v1   # blame as of another revision (a pallet or parcel hash)
forklift blame src/main.rs --json     # structured: line → parcel, parcel → signed author
```

Attributes every line of a file to the parcel that last changed it — and, because
authorship is signed and classed, to the author's **identity class and
supervisor**. That is blame git structurally cannot express: *"was this line written by a
human or an agent, under whose supervision"*, offline and forge-proof. An agent-authored
line reads `[agent, supervised by <human>]` in the gutter, exactly like `history`. The walk
follows the first-parent chain from the revision (git's `blame --first-parent`); a line a
merge brought in from a side line is attributed to the merge parcel. On a long history the walk
uses the commit-graph's **changed-path filters** (built by [`compact`](#compact--pack-the-object-store))
to skip the parcels that never touched the file, so it stays fast as history grows.

blame refuses a large binary file (one stored in chunks — see below): there are no lines to
attribute, so it fails with a clear message rather than assembling gigabytes to guess.

### `audit` — verify signed history offline (`a`)

```sh
forklift audit          # verify the office chain, then the current pallet
forklift audit feature  # verify a specific pallet
forklift audit @office  # verify just the office chain (the office is a meta pallet)
forklift audit --full   # also re-read every chunk and re-verify each large file's content hash
```

Requires established trust. Verifies the office chain from genesis (each office
parcel signed by a key that was active in the previous state), then the pallet's
parcels (each signed by a tracked key; parcels stacked before trust was
established are tolerated as "legacy"). Any tampering — stripped or corrupted
signatures, an unknown key, a chain that doesn't reach genesis — fails with a
non-zero exit. See [`trust-and-identity.md`](trust-and-identity.md).

A large file is stored as chunks indexed by a recipe. A normal audit **presence-checks**
those chunks (confirms each is present without re-reading its bytes) — bounded and fast.
`--full` is the stronger, slower level: it **re-reads every present chunk**, re-hashing it on
the content-addressed read (so on-disk bit-rot a presence check cannot see is caught), and
**re-assembles each fully-present chunked file** to confirm `Blake3(assembled) ==` the recipe's
recorded content hash. Streamed one chunk at a time — never the whole file in memory. On a
sparse warehouse, out-of-scope chunks stay sealed by hash under both levels; the output states
exactly what was re-hashed, presence-checked, and sealed.

**Run `--full` periodically as your own scrub against bit-rot.** Every `lift`/push used to
incidentally re-presence-check a pushed pallet's whole tree, chunked files included; a large
chunked file's subtree that a push leaves untouched is now pruned from that check (it is proven
present by the prior head it is byte-identical to, not re-walked — the cost fix behind this
release). That is sound for the commit gate — store durability between commits was never its job
— but it does mean push time alone no longer doubles as an incidental content scrub for
unchanged large files. A self-hosted `forklift-server` has no audit subcommand of its own, so
periodic `forklift audit --full` against a clone is the recommended way to catch on-disk bit-rot
that pushing quietly stopped surfacing for you.

### `manifest` — signed post-metadata on parcels (`mf`)

```sh
forklift manifest approve 1a2b3c -m "LGTM"   # a signed sign-off on a parcel
forklift manifest note 1a2b3c -m "add a test" # a signed review note
forklift manifest provenance 1a2b3c \         # how an AI produced the parcel
    --model claude-opus-4-8 --tool claude-code --session sess-42
forklift manifest show 1a2b3c                 # the entries attached to a parcel
forklift manifest                             # the whole manifest
```

The manifest is Forklift's "GitHub layer" living inside the warehouse: approvals, review
notes, and machine-authorship **provenance** (which model/tool/session produced a parcel) — recorded as **signed tracked metadata** that *references* a parcel without ever
changing it. Because provenance is signed, paired with an agent-class identity (`office
admit --agent --supervisor …`) it answers *"which model produced this change, under whose
supervision"* forge-proof and offline — the AI-traceability question git cannot. Entries live on the
`@manifest` meta pallet, so they are forge-proof, portable, and offline-verifiable:
`forklift audit @manifest` checks them like any signed history, `forklift history @manifest`
is their audit trail. **Authorship is the signature, not a field you type** — you record
as yourself, and no one can attribute an entry to another operator without their key.
Recording needs an enrolled signing key, exactly like an office change — an approval is
evidence, not an annotation. The subject is any revision (a pallet name or a parcel hash).

The manifest **syncs with the office and the working pallets**: `lift` pushes it, `lower`
and `franchise` pull it (never into the working directory — it is metadata). If two people
record entries concurrently the manifest diverges, and `lower` **merges it automatically**
— entries are independent, so the union is always conflict-free; you then `lift` the
merge. (The office, whose records interdepend, stays linear and is reconciled by hand.)

### `tag` — signed tags / releases

```sh
forklift tag create v1.2.0 main -m "the second release"   # an admin-signed release tag
forklift tag                                              # list every tag
forklift tag show v1.2.0                                  # one tag in full
```

A tag is a named, signed pointer at a parcel — a release or a milestone. Like the manifest,
who cut it is the parcel's **signature**, not a self-declared field, so it is verifiable
offline against the office chain (`forklift audit @tags`). The release convention:
a tag is signed by an **admin** key, so creating one requires an admin — an authoritative
act. Tag names are **immutable** (a name already in use is refused). Tags live on the
`@tags` meta pallet, reserving no user pallet name; without a revision, `tag create` tags
the current pallet's head.

### `haul` — pull requests (reviewable merge proposals)

```sh
forklift haul open --target main --source feature --title "Add X" -m "why"
forklift haul list [--state open|merged|closed|all]
forklift haul show <id>
forklift haul comment <id> -m "…"
forklift haul review <id> [--request-changes | --comment] -m "…"   # approves by default
forklift shift main && forklift haul merge <id>                    # merge lands on the target
forklift haul close <id> | reopen <id>
```

A **haul** proposes merging one pallet into another, with discussion and reviews — forklift's
pull request. Like the manifest, it lives on a meta pallet (`@haul`) as an **append-only log
of signed events** (Opened / Pushed / Comment / Review / Merged / Closed), folded into current
state; it lifts/lowers/franchises with your warehouse and a diverged `@haul` **auto-merges**
(the union of events). Because every event is signed, authorship — **including who approved** —
is forge-proof and carries the reviewer's identity class, so a human's approval is
distinguishable from an agent's (`haul show` tags automated reviewers). `merge` reuses
`consolidate` (a clean disjoint merge auto-stacks; a conflict leaves the consolidation in
progress — resolve, `stack`, then re-run `haul merge` to record it). A haul id is
content-addressed; use any unique prefix.

This release is **intra-warehouse** (pallet → pallet), and reviews are **recorded but not
enforced** — anyone with write access to the target can merge (approval-gating is a later
policy layer). Cross-warehouse forks come later. Requires an enrolled signing key.

---

## 4. Branching and merging

### `palletize` — create a pallet / list pallets (`pz`)

```sh
forklift palletize                 # list pallets (the current one marked *)
forklift palletize --all           # also list the meta pallets (@office, …) under a heading
forklift palletize feature         # create "feature" at the current head and shift to it
forklift palletize hotfix 1a2b3c   # create "hotfix" at a revision and shift to it
```

The list shows user pallets only by default; `--all` adds the meta pallets (the office
and other tracked metadata), each in its `@`-qualified form — the address you reach it
with. Pallet names may contain `/` (mapped to subfolders). Creating one at a revision
materializes that state (and refuses on a dirty warehouse); creating one at the
current head just moves refs.

With `--json`, each pallet in the list carries its `head` parcel hash (`null` for an
unborn one — the current pallet is listed even when unborn, so a caller never has to
special-case it):

```json
{ "current": "main", "current_unborn": false,
  "pallets": [
    { "name": "feature/x", "current": true, "head": "<hash>" },
    { "name": "main", "current": false, "head": "<hash>" }
  ] }
```

### `shift` — switch pallets (`sh`)

```sh
forklift shift main
```

Materializes the target pallet's head tree in the working directory, repopulates
the inventory from it, and makes it the current pallet. Refuses to run when you
have staged or unstaged changes (they'd be overwritten) — stack, restore, or park
them first. Untracked files are left alone unless the target needs to write over
one.

### `consolidate` — merge (`con`)

```sh
forklift consolidate feature
```

Merges another pallet's head into the current pallet:
- **Already up to date** when the current head already contains it — nothing happens.
- **Fast-forward** when the current head is an ancestor of the target — the ref just moves.
- Otherwise a **three-way merge** against the common ancestor. Clean merges stack
  a two-parent merge parcel immediately. Conflicts are written into the working
  files with diff3 markers, the entries enter a conflict state, and the
  consolidation stays in progress until you resolve, `load`, and `stack`.

Requires a clean warehouse before it starts.

### `deliver` — squash a draft onto a target, keeping the trail (`dv`)

```sh
forklift deliver main -m "add the feature"   # run from the draft pallet
```

The squash agents need without losing the trail. Agents checkpoint constantly;
humans want one reviewed parcel. `deliver` takes the current (draft) pallet's net change
and lands it on the target as a **single clean signed parcel** — the draft head's tree
with the target as its *only* parent, so the checkpoints stay out of the target's history.
The full trail is **kept** (the draft pallet is left intact, browsable with `history`) and
recorded as a signed **delivery** entry on the new parcel's manifest, so "what the agent
tried, in what order" stays discoverable (`manifest show <parcel>`). The delivered parcel
preserves the trail's authors; the deliverer is the stacker.

Needs an enrolled key (the trail is signed evidence). The current pallet becomes the
target, and because the delivered tree equals the draft's, the working directory does not
change. A second delivery of an already-delivered draft is refused.

### `cherry-pick` — apply one parcel's change onto the current pallet (`cp`)

```sh
forklift cherry-pick 1a2b3c            # apply that parcel's diff here, as a new parcel
forklift cherry-pick feature -m "..."  # pick a pallet's head, with a custom message
```

Applies a parcel's diff — its change against its first parent — onto the current pallet as
a new parcel. Unlike rebase, cherry-pick only **adds**: no rewrite, no
force-push, nothing an audit has to bless twice. The picked parcel's **authors are
preserved** and you are recorded as the stacker (the same author/stacker split as
`import-git`/`export-git` and `deliver`), and the new parcel is freshly signed. It is
applied by three-way merge, so a **clean** pick is stacked immediately as a single-parent
parcel; a **conflicting** one writes diff3 markers and leaves a cherry-pick in progress —
resolve, `load`, and `stack` to complete it (still single-parent, still author-preserving),
or remove `.forklift/cherry-pick` to abort. A pick whose changes are already present is
refused. Requires a clean warehouse.

### `conflicts` — list unresolved conflicts

```sh
forklift conflicts            # human list
forklift conflicts --json     # structured: each file's base/ours/theirs as content addresses
```

Lists the files a consolidation or cherry-pick left in conflict. With `--json`, each file's
three sides (base, ours, theirs) are reconstructed from the diff3 markers and
given as content-addressed blob hashes you can `peek` — designed for agents,
which resolve merges well given structure instead of marker soup. An empty list
is a valid answer (nothing to resolve).

**Resolving a conflict:** edit each conflicted file to remove the markers,
`forklift load` it, then `forklift stack` to complete the consolidation or cherry-pick.

### `bay` — parallel working directories

```sh
forklift bay add feature ../myrepo.feature   # a new working dir on a new pallet "feature"
forklift bay                                 # list the bays
forklift bay remove feature                  # de-register a bay (its pallet + files are kept)
```

A bay is an additional working directory bound to this warehouse — git's worktrees,
designed in. It **shares** the object store, the refs (pallets/meta) and trust, but keeps
its **own** working tree, inventory, current pallet and lock. So several agents (or you and
an agent) work one machine without cloning the objects N times or serializing through one
lock: N bays, N pallets, one warehouse. `bay add <name>` checks out a new pallet named
after the bay (branched from the current head) into a fresh directory (default: a sibling
of the warehouse); `cd` into it and it behaves like any warehouse — `load`, `stack`, `lift`
all operate on the bay's pallet against the shared object store. The bay's directory holds
a `.forklift` *file* (a redirect back to the warehouse), and its local state lives under
`.forklift/bays/<name>/`.

### `bay add --scope` — a scoped (sparse) bay

```sh
forklift bay add api ../myrepo.api --scope src/api          # materialize only src/api
forklift bay add api ../myrepo.api --scope src/api --scope docs   # several subtrees
forklift scope                                              # show the current scope
```

A **scoped bay** materializes and operates on only the subtree(s) you name, instead of the
whole working tree — handy for a large monorepo, or to hand an agent exactly the corner it
should touch. In this release the object store still holds **everything** (the sparseness is
materialization-only), so a scoped bay is a local view over a full warehouse; nothing changes
on the remote and nothing is fetched differently.

Inside a scoped bay:

- `load`, `stack`, `stocktake`, `diff`, `shift`, `park` all work on the in-scope subtree(s).
  A `stack` rebuilds the root tree as *"the head with your in-scope subtree swapped in"* —
  every out-of-scope sibling is carried forward by the exact hash the signed head already
  commits, so the parcel you stack is **byte-identical** to what a full workspace stacking the
  same change would produce. Out-of-scope content is cryptographically pinned, never guessed.
- `stocktake`/`diff` report only in-scope changes; an out-of-scope path that only exists in
  history is *sealed by hash*, not shown as removed.
- A path argument outside the scope (`load src/web`, `blame src/web/x.rs`, `diff a b src/web`)
  refuses with the `out_of_scope` code rather than doing something surprising.
- `consolidate` merges in a scoped bay. In-scope content merges normally; an out-of-scope
  sibling (a subtree, file or symlink) that changed on only one side is adopted **by hash** —
  never materialized, never fetched — so the merge parcel is byte-identical to what a full
  workspace merging the same two heads would commit. An out-of-scope entry that changed on
  **both** sides has no content here to reconcile, so the merge refuses with
  `out_of_scope_conflict`; widen the scope to include that path and retry, or resolve the merge
  in a full workspace.
- `export-git`, `import-git` and `cherry-pick` refuse in a scoped bay (`sparse_workspace`): the
  first two bypass the sparse overlay and would export/import a truncated or scope-inconsistent
  view, and a cherry-pick materializes a diff that may touch paths this bay never fetched. Run
  them from a full workspace.
- `park` produces a parcel that stacks over the head's spine exactly like `stack`, so a parked
  parcel from a scoped bay is byte-identical to what parking the same work in progress in a
  full workspace would commit.
- If a subtree you scoped to has since been replaced by a file (or vice versa) at the revision
  you move onto, the operation refuses with `scope_path_type_changed` rather than guess — the
  scope is no longer valid there, so re-scope or resolve in a full workspace.

Scope is a property of **this checkout**, not the project: it is recorded bay-locally and is
**never tracked**, so it is never pushed to the remote or imposed on collaborators.

Bay creation is exposed over MCP (`bay_add`, with the same `scope` argument), so an
orchestrator agent can open task-scoped sandboxes for its sub-agents directly, without
shelling out. The scope it sets is advisory local setup, not a security boundary — see
**[`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md)**.

### `scope` — show the sparse-workspace scope

```sh
forklift scope            # this bay's materialization scope + the warehouse fetch scope
forklift scope --json     # the same, as a machine envelope
```

Read-only. In a plain bay or the main tree it reports the full tree; in a scoped bay it lists
the in-scope subtree prefixes. The warehouse fetch scope is reported too — *full* for an
ordinary warehouse (the store holds everything), or the fetched prefixes for a sparse
(`franchise --only`) one. Widen the fetch scope with `expand`; shrink a checkout's
materialization scope with `narrow`.

---

## 5. Parking work in progress

### `park` — stash (`pa`)

```sh
forklift park            # park tracked changes and reset to the pallet head
forklift park list       # list parked parcels, newest first
forklift park pop         # re-apply the most recent parked changes (staged) and drop it
```

`park` saves your staged and unstaged changes to **tracked** files as a parked
parcel and resets the warehouse to the pallet head (untracked files are left
alone). `park pop` re-applies the most recent parked changes as a clean re-apply
— it must be popped onto the head it was parked on, or it reports a conflict.

---

## 6. Working with remotes

Forklift's remote is untrusted storage plus thin verifiers: bytes move in
parallel, and anything you download is verified locally by hash. Set a remote
once, then push and pull.

### `franchise` — clone (`fr`)

```sh
forklift franchise http://forklift.example.com:9418 my-project
forklift franchise <url> <dir> --pallet main --token <secret>
forklift franchise <url> <dir> --only src/api            # sparse: fetch one subtree
forklift franchise <url> <dir> --only src/api --only docs # several subtrees
```

Prepares a fresh warehouse in the target directory, remembers the remote, adopts
its trust anchor, downloads the history (using the remote's bundle when it has
one, then loose objects for the rest), and materializes the chosen pallet
(default: the remote's default pallet). The directory must be new or empty.

**Sparse franchise (`--only`).** With one or more `--only <path>`, franchise fetches the
whole signed history — every parcel, signature and the tree spine — but only the **content**
under the named subtree(s). Out-of-scope subtrees and files are never downloaded; they stay
pinned by the exact hash a signed parcel already commits, so nothing can be forged, it is
simply not fetched. The working tree materializes only the in-scope subtree(s), and the
remote's whole-store bundle is skipped. History, `audit`, `blame` and `diff` on in-scope paths
all work; office and every meta pallet are always fetched in full, so a sparse franchise of a
trusted warehouse still audits offline exactly as a full clone does. Widen later with `expand`.
A sparse franchise records its origin — see the origin-only lift rule under `lift`.

When the remote's bundle installs native packs straight into the new warehouse, `franchise`
tips the same densify suggestion `import-git` does: those packs are the far end's own bulk
ingest, unseen by a cross-store `compact --all --redelta` pass.

### Configuring a remote on an existing warehouse

```sh
forklift config remote.url http://forklift.example.com:9418
forklift config remote.token <secret>      # only if the server requires one
```

### `lift` — push (`li`)

```sh
forklift lift
```

Uploads the current pallet's new parcels to the remote and moves the remote's ref
with a compare-and-swap. When trust is established, the office pallet and trust
anchor are lifted first (the server verifies every signature before accepting).
The remote only accepts **fast-forward** updates — no force push.

**Optimistic lift:** if the remote moved and the two changes are cleanly
mergeable (they touch different files), `lift` **auto-lowers, consolidates and
retries** on its own — so a fleet of agents stacking to one pallet stops
serializing through a human. It reports how many times it auto-merged. Only a
clean warehouse qualifies (the merge materializes), and a **true overlap** (both
sides edited the same file) still stops with the diverged error, leaving the
warehouse untouched — resolve it with `lower` + `consolidate`.

**Origin-only lift from a sparse workspace.** A sparse (`franchise --only`) warehouse only
ever proved its out-of-scope content present on the remote it fetched from — its origin. Lifting
to a *different* remote could fail late at that remote's closure check, so `lift` refuses up
front with the `non_origin_lift` code, naming the origin. Point `remote.url` back at the origin,
or run a full (unscoped) franchise against the new remote. A full warehouse holds the whole
closure and can lift anywhere.

**Very large lifts and old remotes.** A lift touching more objects than fit in one commit batch
(tens of thousands of distinct objects — realistic for a single maximal chunked file, or simply a
lot of small tracked files at once) needs a remote that understands paginated commits. Against a
remote that doesn't, `lift` refuses up front, before uploading anything
(`commit_pagination_unsupported`, exit 16) — upgrade the remote, or lift in smaller stages.

### `lower` — pull (`lo`)

```sh
forklift lower
```

Fetches the remote's new parcels for the current pallet and fast-forwards to them
— working directory and inventory included. On first contact with a trusted
remote, its trust anchor is adopted (a one-way door, like enrolling). A diverged
pallet is never merged implicitly: `lower` fetches the parcels and reports the
divergence, and you consolidate deliberately (`palletize` the remote head into
its own pallet, `consolidate` it, then `lift` the merge). Requires a clean
warehouse. In a sparse warehouse, `lower` stays pruned — it fetches new in-scope
content and leaves out-of-scope changes sealed by hash, exactly as the franchise did.

### `expand` — widen a sparse warehouse's fetch scope

```sh
forklift expand src/web            # fetch a subtree the sparse franchise left sealed
forklift expand src/web docs       # several at once
```

Adds subtree path(s) to what the warehouse has fetched, and downloads their content across the
whole history from the remote. Incremental and precise — only the newly in-scope objects are
fetched; what is already present is skipped, and the content is hash-verified against the seals
the history commits (so widening is safe from any remote — only publishing is origin-bound).
After expanding, a bay can be scoped to the new path (`bay add --scope`). A full warehouse
already holds everything, so there is nothing to expand.

### `narrow` — shrink this checkout's materialization scope

```sh
forklift narrow docs               # stop materializing a subtree here
```

Drops subtree path(s) from what **this** checkout (a bay, or a sparse main tree) materializes,
and removes those files from the working directory. This **frees nothing** in the shared object
store — the dropped content is ordinary reachable history, not garbage — it only shrinks what
this checkout shows. A checkout must keep at least one in-scope path; to stop scoping entirely,
open a fresh full checkout. `narrow` is the counterpart of a bay's `--scope`, not of `expand`:
`expand` widens what the *warehouse* fetched, `narrow` shrinks what *this checkout* materializes.

### `scope-prune` — reclaim disk from a sparse warehouse

```sh
forklift scope-prune docs            # forget a fetched path and free its content
forklift scope-prune docs --dry-run  # show what it would free, change nothing
```

The deliberate, **destructive** counterpart of `narrow`. Where `narrow` is bay-local and frees
nothing, `scope-prune` forgets a path **warehouse-wide**: it drops the path from the shared
warehouse fetch scope and deletes the objects under it from the object store, reclaiming disk
reachability-`gc` never could (narrowed-away content is still reachable history, so `gc`
correctly keeps it). It is **multi-bay-aware** — it refuses (`scope_prune_blocked`, exit 13) to
free a path any checkout (the main tree or a bay) still materializes, so narrow that path away
everywhere first. Nothing is lost: the pruned content is sealed by hash, re-fetchable from the
origin with `expand`. A full (non-sparse) warehouse has nothing to prune. Use `--dry-run` before
you leap. (Objects already inside a pack are reported but not reclaimed yet — a scope-aware
repack is future work.)

Pruning a **large (chunked) file** frees its recipe **and every chunk** it names, not just the
recipe — chunks are content-addressed objects reachable only through the recipe, so a
recipe-only delete would orphan them. A chunk the pruned file shares with a still-fetched file
elsewhere is **kept** (freeing it would break that file), exactly as a shared blob is. The chunks
are freed before their recipe, so a killed prune resumes cleanly on the next run.

If a prune gets interrupted (killed, crashed) before it finishes freeing everything, the fetch
scope has already narrowed but some objects are left behind. Running `scope-prune` again on the
*same* path resumes rather than refusing "not a fetched path" — it finishes freeing whatever is
left, or reports there is nothing left to free.

---

## 7. Identity, signing, and agents

The `office` command manages who may contribute and how contributions are signed
— users, keys, roles, agents, and trust resets. This is a big topic with its own
guide: **[`trust-and-identity.md`](trust-and-identity.md)**. In brief:

```sh
forklift office enroll            # establish trust (one-way; every parcel is signed after this)
forklift office keygen            # generate a key and print your enrollment line
forklift office admit <id> <pub> <pop>   # an admin admits a newcomer (or an agent/bot)
forklift office list              # who's enrolled, their roles, keys, and classes
forklift office rotate            # replace your key
forklift office retire <key>      # revoke a key
```

`profile` lets one machine act as different operators in different warehouses:

```sh
forklift profile list
forklift profile create work --name "Work Me"
forklift profile use work
```

---

## 8. Machine & agent output

Every command takes `--json` and emits one structured envelope instead of prose;
errors carry a stable code and a deterministic exit code. Forklift also ships an
MCP server (`forklift mcp`) that exposes the command surface as native tools for
AI agents. Full details — the envelope schema, the error/exit-code taxonomy, the
`conflicts` content addresses, and wiring `forklift mcp` into a client — are in
**[`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md)**.

```sh
forklift stocktake --json
forklift stack "x" --json
forklift mcp --root /path/to/warehouse    # run the MCP server against a warehouse
```

### `self-update` — check for and apply a newer release

```sh
forklift self-update            # update in place if you installed via the script
forklift self-update --check    # only report; change nothing
```

Compares this binary against the latest published release. If you installed via the
install script (or a manual copy), it updates in place by re-running that verified script.
If you installed via a **package manager**, it never overwrites the binary — it prints the
right upgrade command instead (`cargo install … --force`, `brew upgrade forklift`), so it
can't fight your package manager. `--check` (and `--json`) just reports `{current, latest,
update_available, install_method, update_command}`. There is **no server self-update** by
design: a server is redeployed (new container / Lambda version / package), never
self-mutated. If the [`fl` alias](#alias--the-fl-short-name) was already installed, self-update
keeps it working — the binary is replaced at the same path, and self-update also restores the
alias if it ever goes missing.

### `compact` — pack the object store

```sh
forklift compact
```

Forklift stores every object as its own compressed file. That is simple and safe, but at
scale (say after importing a large git history) it becomes hundreds of thousands of tiny
files: each pays filesystem slack, and a full-history walk pays an open per object. `compact`
sweeps the loose objects into a few dense **pack** files — an append-only data file plus a
sorted index — so the store is a handful of large files and a read is a binary search plus one
seek. Each file's successive versions are stored as **deltas** — the change from the previous
version of the *same file* rather than a whole new copy — which is git's biggest space win.
`compact` finds that previous version the way git does, by walking history to associate each
blob with its path, so the deltas are as tight as git's: **git.git packs to 261 MB, smaller
than git's own 310 MB pack.** A loose object is removed only after the pack that holds it is
durably written, and a delta is content-verified on read, so compacting can never lose or
corrupt an object (interrupt it freely). Packs roll over at a size/count threshold, so no
single pack grows unbounded. `--json` reports
`{objects_packed, packs_written, loose_removed, deltas, bytes_packed}`.

`compact` also **builds the commit-graph** (`.forklift/graph/`) while it is here: a sharded,
self-healing cache that gives each parcel a generation number — so ancestry checks
(`consolidate`, `haul`) find a merge base without walking history to the roots — and a
changed-path filter — so `blame` skips parcels that did not touch the file. It is derived and
repairs itself, so you never manage it; building it during `compact` just means it is warm the
first time you need it.

**You rarely need to run this by hand.** `import-git` writes packs directly on the way in
(unless you pass `--no-compact`), and afterwards forklift **compacts automatically** in the background of a
mutating command once the store has accumulated enough loose objects or packs to warrant it
(git's `gc --auto`). It runs synchronously, under that command's lock, so it is correct and
never races — which means it can add a brief pause when it fires (rarely). Turn it off with
`config maintenance.auto false`, or tune the thresholds (`maintenance.loose`, `maintenance.packs`).

Compaction also takes a **shared object-store lock** (separate from the per-bay lock ordinary
commands hold), so two bays or two processes never compact the same store at once. If another
compaction is already running, an explicit `forklift compact` reports it and stops; the
automatic background compaction simply skips (the other run is doing the same work). Ordinary
mutating commands are never blocked by it.

Plain `compact` is **incremental**: it packs the loose objects into a *new* pack and leaves
existing packs alone (cheap, but packs accumulate over time). `compact --all` is a **full
repack**: it rewrites every pack too, **dropping unreachable objects** that were stuck in
packs (garbage from undone stacks, abandoned pushes) and **consolidating many packs into
few**. It **reuses** each object's existing delta (a byte-copy, not a re-compress), so it is
fast and never balloons the store — but heavier than incremental; run it occasionally (auto
does when packs pile up, or a server off-peak).

```sh
forklift compact          # incremental: pack the loose objects
forklift compact --all    # full repack: drop garbage, consolidate every pack
```

`compact --all --redelta` goes further: instead of reusing each object's existing delta, it
re-reads and re-compresses **every** live object (packed or loose, files and directories alike)
and re-runs delta selection on the whole store together. That matters because a repack's
copy-fast-path can only keep whatever delta an object was *originally* given — and an object
packed in isolation (say, on import, or by an earlier incremental compact whose delta window
only ever saw a few neighbors) has no way to know a much better match (a rename, a moved file,
anything similar outside its own path) sits elsewhere in the store. `--redelta` gives every
object one more shot at the best base across the *entire* live set. It costs a full re-encode —
one-shot and CPU-bound, not something to run routinely — and it is only valid together with
`--all`. Safe to repeat any number of times on the same store. `store` (and `import-git`'s and
`franchise`'s own output) suggest running it when the store was bulk-ingested and has not been
through a redelta pass yet; a successful run clears that suggestion.

> This step removes per-file slack and the open-per-object read cost. Delta compression
> between similar versions (the rest of the size gap vs git) is additional work on top —
> see [`../OBJECT_STORE_SCALING.md`](../OBJECT_STORE_SCALING.md).

### `store` — object-store health

```sh
forklift store
```

The read-only counterpart of `compact` (and the object-store analog of `stocktake`, which
reports the working tree). It takes an **exact** census of `.forklift/objects`: how many
objects are **loose** (unpacked) versus **packed**, how many pack files there are and how
**delta-dense** they are, the on-disk sizes, and — against the `maintenance.loose` /
`maintenance.packs` thresholds — whether an incremental compaction or a consolidating repack
is currently **due**. It answers "how much of the store is packed, and does it need
maintenance?" without doing any work.

```
Object store
  loose:   12 objects   (48.0 KiB)
  packed:  402118 objects in 1 pack   (236.0 MiB, 261000 deltas)   99% packed
  total:   236.0 MiB on disk

  maintenance: auto on
    compaction  not due — 12 / 6700 loose objects
    repack      not due — 1 / 20 packs
```

`--json` reports `{loose_objects, loose_bytes, packed_objects, pack_files, deltas, pack_bytes,
total_bytes, packs:[{id, objects, deltas, bytes}], maintenance:{auto, loose_threshold,
pack_threshold, compaction_due, repack_due}, densify_suggested}` — every size an exact byte
count. Nothing is written; run `compact` to act on what it reports. `densify_suggested` (and,
in the human view, a one-line tip) appears once the store was bulk-ingested (`import-git` or a
franchise's bundle install) and clears once a `compact --all --redelta` pass has run
(see [`--redelta`](#compact--pack-the-object-store)).

---

## 9. Configuration reference

Two scopes: **warehouse** (`.forklift/config/warehouse.toml`, default) and
**global** (`~/.forkliftconfig`, with `--global`). Reads prefer the warehouse
value. Known keys:

| Key | Meaning |
|-----|---------|
| `operator.name` | Your display name (local only — never written on-chain). |
| `operator.identifier` | Your on-chain operator id (opaque; a UUID is minted if unset). |
| `operator.profile` | The named profile this warehouse acts under (see `profile`). |
| `remote.url` | The remote warehouse URL (for `lift`/`lower`). Can be a Tor `.onion` — see [Reaching a remote over Tor](#reaching-a-remote-over-tor). |
| `remote.token` | The bearer token, when the remote requires one. |
| `remote.tor` | Reach the remote over Tor: `auto` (default — only `.onion` remotes), `on` (every remote), `off` (never). |
| `remote.torProxy` | The Tor SOCKS proxy (default `socks5h://127.0.0.1:9050`). |
| `maintenance.auto` | Auto-compact after mutating commands (`false`/`0`/`off`/`no` to disable; default on). |
| `maintenance.loose` | Loose-object count that triggers an auto incremental compact (default 6700). |
| `maintenance.packs` | Pack count that triggers an auto consolidating repack (default 20). |

Set / read / remove:

```sh
forklift config operator.name "Ada"
forklift config --global operator.identifier ada@example.com
forklift config remote.token
forklift config --unset remote.token
```

The `FORKLIFT_GLOBAL_CONFIG` and `FORKLIFT_KEYS_DIR` environment variables
relocate the global config file and the private-key directory (used by tests and
for isolating identities). `FORKLIFT_KEY_PASSPHRASE` supplies a protected key's
passphrase non-interactively (an automation escape hatch — see
[`trust-and-identity.md`](trust-and-identity.md#6-passphrase-protected-keys)).

`FORKLIFT_FSYNC` controls write durability. By default every object, ref and pack
write is fsynced (file **and** its directory) so an interruption — including power
loss — can never lose or truncate committed data. Set it to `0`, `off`, `false`,
or `no` to skip all fsyncing: markedly faster for **bulk, disposable** work (large
imports, CI, throwaway fixtures) where a mid-run crash just means re-running the
whole operation. Leave it on for any warehouse whose history you intend to keep.

### Reaching a remote over Tor

Forklift can reach a remote through Tor — the payoff is **NAT traversal**: collaborate with a
remote that has **no fixed IP and no port-forwarding**, because a Tor `.onion` address is
reachable from anywhere on the Tor network. Tor is the mechanism, not the point.

By default (`remote.tor = auto`) this is invisible and opt-in per remote: a `.onion` remote is
routed through your local Tor SOCKS proxy automatically, and every other remote is dialed
directly and unchanged. So an onion URL Just Works while a plain `http(s)` remote behaves
exactly as before.

```sh
forklift config remote.torProxy socks5h://127.0.0.1:9150   # Tor Browser's bundled SOCKS port
forklift config --global remote.tor on                     # route every remote through Tor
```

- `remote.tor` — `auto` (default, onion-only), `on` (every remote — e.g. reach a clearnet
  remote anonymously), or `off` (never, even a `.onion`).
- `remote.torProxy` — where your local `tor` listens; default `socks5h://127.0.0.1:9050`. The
  `socks5h` scheme resolves the host **at the proxy**, which is what lets an opaque `.onion`
  name (it has no DNS record) resolve inside the Tor network.

> **With `on`, use `https://` for any non-onion remote that carries a token.** Routing a plain
> `http://` clearnet remote through Tor sends its bearer token out through a Tor **exit relay** —
> a *worse* position than the direct network path, since hostile exits harvest plaintext
> credentials. Onion remotes are unaffected: traffic is end-to-end encrypted to the onion key, so
> `http://…​.onion` is fine.

You need a local `tor` daemon for this. The default `9050` is a stock system `tor`; the `9150`
example above is **Tor Browser's** bundled daemon — convenient, but it only listens while the
browser is open. Either way, nothing here needs the Tor Browser *UI*. The transport adds **no new
dependency** — it flips on the SOCKS5 client already vendored in the HTTP stack.

---

## 10. Command & alias reference

### Coming from git?

Forklift accepts the familiar git command names as hidden aliases, so your muscle memory
works on day one — while forklift's own vocabulary stays primary in `--help` (the aliases
nudge you toward the real names as you go):

| You type (git) | Runs (forklift) | | You type (git) | Runs (forklift) |
|---|---|---|---|---|
| `init` | `prepare` | | `checkout` / `switch` | `shift` |
| `add` | `load` | | `merge` | `consolidate` |
| `commit` | `stack` | | `clone` | `franchise` |
| `status` | `stocktake` | | `push` | `lift` |
| `log` | `history` | | `pull` | `lower` |
| `branch` | `palletize` | | `stash` | `park` |
| `annotate` | `blame` | | `rm --cached` | `remove` |
| `restore --staged` | `unload` | | | |

The mapping isn't always one-to-one in *behavior* (e.g. `commit` runs `stack`, which commits
what you've `load`ed — it doesn't stage for you). `diff`, `restore`, `tag`, `show` and
`cherry-pick` keep their git names outright. For the review workflow, git's "pull request" is
[`haul`](#haul--pull-requests-reviewable-merge-proposals).

| Command | Alias | Does |
|---------|-------|------|
| `prepare` | `p` | Create a warehouse here |
| `import-git` | | Migrate a git repo's history into the warehouse |
| `export-git` | | Export the warehouse's history back into a git repo |
| `alias` | | Manage the `fl` short alias next to this binary |
| `config` | `cfg` | Read/set/unset configuration |
| `profile` | | Manage named identity profiles |
| `load` | `l` | Stage changes into the inventory |
| `unload` | `ul` | Unstage (undo a `load`) |
| `remove` | `rm` | Stage a removal |
| `restore` | `r` | Restore from the inventory (discard changes) |
| `stocktake` | `st` | Show staged and unstaged changes |
| `diff` | `d` | Show line-by-line changes |
| `stack` | `s` | Record the inventory as a new parcel |
| `undo` | | Soft-reset the last stack |
| `peek` | `pk` | Inspect an object or inventory |
| `show` | | Print a file's content at a revision (`<revision>:<path>`) |
| `history` | `hi` | Walk the parcel graph |
| `blame` | `bl` | Attribute each line to its author (with identity class) |
| `audit` | `a` | Verify signed history offline |
| `palletize` | `pz` | Create a pallet / list pallets |
| `shift` | `sh` | Switch pallets |
| `consolidate` | `con` | Merge another pallet in |
| `cherry-pick` | `cp` | Apply a parcel's diff here, preserving authorship |
| `deliver` | `dv` | Squash a draft onto a target, keeping the trail |
| `bay` | | Parallel working directories (add / list / remove) |
| `compact` | | Pack the loose object store into dense pack files |
| `store` | | Report object-store health (loose vs packed, sizes, maintenance due) |
| `conflicts` | | List unresolved conflicts |
| `park` | `pa` | Stash / list / pop work in progress |
| `franchise` | `fr` | Clone a remote warehouse (`--only` for a sparse clone) |
| `lift` | `li` | Push to a remote |
| `lower` | `lo` | Pull from a remote |
| `expand` | | Widen a sparse warehouse's fetch scope and fetch the new subtree(s) |
| `narrow` | | Shrink this checkout's materialization scope (frees nothing) |
| `office` | `o` | Manage users, keys, roles, agents, trust |
| `manifest` | `mf` | Record/read signed post-metadata on parcels |
| `tag` | | Signed tags / releases (admin-signed, offline-verifiable) |
| `mcp` | | Serve the command surface as MCP tools |
| `help` | `h` | Show help |
| `version` | `v` | Print the version |

---

## 11. Exit codes

Errors set a deterministic exit code so scripts branch without parsing prose. `0` is
success and `2` is clap's own usage/argument error; every other code is one of forklift's
own classified failures.

The full, always-current table — generated from the same enum the binary itself
branches on, so it can never fall behind — is
[`../generated/errors.md`](../generated/errors.md).

With `--json`, the same classification appears as `error.code` in the output
envelope. See [`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md).
