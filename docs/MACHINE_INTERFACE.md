# The machine-first interface

Forklift's command surface is built to be driven by programs — scripts, CI, and AI
coding agents — as well as people. Four things make that work:
a `--json` mode with a versioned envelope, a stable error and exit-code taxonomy, an
MCP server that exposes every command as a schema-typed tool, and a progress-events
channel that keeps a long operation observable without touching its output.

## `--json`

`--json` is a global flag: add it to any command and stdout becomes exactly **one**
JSON document (nothing else prints there) on every exit Forklift controls itself —
usage errors included. Human prose is unchanged without the flag.

The envelope is the one every Lonic tool shares. Success:

```json
{
  "lonic_json": 1,
  "tool": "forklift",
  "schema": 9,
  "command": "stocktake",
  "ok": true,
  "data": { "…command-specific…" }
}
```

Failure keeps the same header, with `"ok": false` and an `error` in place of `data`
(it also sets the exit code below):

```json
{
  "lonic_json": 1,
  "tool": "forklift",
  "schema": 9,
  "command": "stocktake",
  "ok": false,
  "error": {
    "code": "not_a_warehouse",
    "message": "…",
    "next_step": "Run \"forklift prepare\" to create a warehouse here, or change into one."
  }
}
```

- `lonic_json` versions the envelope itself — the header, and the stderr warning shape
  below. A consumer detects the contract with this one key.
- `tool` names the producer, so a document read after `pult <command>` says whether it
  came from Pult or from the tool Pult ran.
- `schema` versions Forklift's payloads: every command's `data` and the error-code
  registry. It changes whenever either does, an added field or error code included (schemas
  4 to 6 each only added), so a consumer can pin it — and it *is* the capability-detection
  mechanism: check it before relying on a field, rather
  than sniffing for the field's presence.
- `command` is the command as parsed — the canonical name, never the alias typed
  (`st` reports `"stocktake"`) — on success and failure alike. It is `null` only when
  the command line names no known command: a usage error without one, or top-level
  `--help` / `--version`.
- `ok` says whether `data` is the command's result. It does **not** report the exit
  status: a command whose result is a negative outcome may exit non-zero with
  `ok: true`. Parse stdout first and consult the exit status second.
- `error.next_step` is present where there is a clear recovery; `error.details` is
  optional, code-specific structured detail.

A command's `data` shape is documented by the struct it emits (in
`crates/forklift/src/commands/`) — the generated, exhaustive reference for every one of
them is [`generated/json-schemas.md`](generated/json-schemas.md). Those schemas describe
what Forklift writes: a field under `required` is always present (possibly as `null`), and
any other field is left out when it has nothing to say.

**Usage errors** (an unknown command, a missing or invalid argument) are `usage`
failures with exit 2. `message` is the parser's one-line complaint, and
`details.usage` keeps its full text — the usage line and any "a similar command exists"
tip. `--help`, `--version` and the `help` command under `--json` are results, not
failures: a success document whose `data.message` is the text, exit 0. The keys of every
document appear in the order shown above, header first.

Only the bare `--json` token selects JSON when the command line fails to parse:
`--json=true` is itself a usage error, reported as prose on stderr with an empty stdout.

**Death by signal** (exit 128+n, including 130 from Ctrl-C) is outside the process's
control and may leave stdout empty. A consumer that finds no document classifies by the
exit status alone. Forklift has no prompt that handles a cancellation itself, so it never
exits 130 on its own: Ctrl-C anywhere, the passphrase prompt included, is a death by signal.

**Schema 10** (current): the `cache_miss` error code exists.

**Schema 9**: `batch_lane` may also be `out_of_time`, when the remote ran out of time for
a batch download and the rest of the command fetched one object at a time.

**Schema 8**: the `pallet_deleted_on_remote` error code exists; `palletize --remote` lists the
remote's deleted pallets as `deleted`, and `palletize` lists this served warehouse's own as
`deleted`; `palletize --delete`/`--restore` with `--remote` report `remote`.

**Schema 7**: the `mirror` command exists; `lift` carries `mirror` when it
mirrored the pallet it lifted (or tried to, and says why it did not); and a mirror run that leaves
a pallet out of step with git fails with `mirror_incomplete` (exit 33), whose `details` carry the
run's report, the same `{url, pallets}` a successful run returns.

**Schema 6**: `franchise`, `lower`, `expand` and `lift` carry `batch_lane`
(`degraded` or `endpoint_absent`) when the remote's batch download stopped being used and the
rest of the command fetched one object at a time; absent when it stayed open. `office
accept-regenesis` says so in its `message` instead. An error document does not carry it.

**Schema 5**: the `staged_upload_discarded` error code exists (exit 32), and a lift
whose commit the remote keeps answering "not ready" fails at a deadline instead of after a fixed
attempt count.

**Schema 4**: a franchise that fails after claiming its directory keeps it and exits
`franchise_incomplete` (exit 30, with `details`), which every other command run there also
refuses with until `franchise` is run again; and a sparse franchise's `--only` path that is not a
directory in the pallet refuses with `only_prefix_not_found` (exit 31).

**Schema 3**: `lower` and `franchise` report a first-contact adoption as `adopted_genesis`, and
the `trust_changed` and `remote_protocol_violation` error codes exist.

**Schema 2**: `history` entries carry `parents` (every parcel's parents, in
stored order, always present — `[]` for a root); the `empty_history` error code exists
(`history` on an unborn pallet); `palletize` list entries carry `head`; the `show`
command reads a file's content at a revision in one call; `diff` accepts the reserved
`:empty` token (either revision) for the empty tree; and `peek` on a binary blob reports
`binary: true` instead of silently mangling the bytes.

### `history --json`

```json
{
  "data": {
    "entries": [
      {
        "parcel": "<hash>",
        "parents": ["<base-hash>", "<other-hash>"],
        "consolidates": ["<base-hash>", "<other-hash>"],
        "actions": [
          { "action": "author", "operator": "<id>", "timestamp": "2026-07-13T00:00:00+00:00" },
          { "action": "stack", "operator": "<id>", "timestamp": "2026-07-13T00:00:01+00:00" }
        ],
        "description": "…"
      },
      { "parcel": "<root-hash>", "parents": [], "actions": [ /* … */ ] }
    ],
    "next": null
  }
}
```

`parents` is always present, in the parcel's stored (canonical, base-first) order — a
root parcel's is `[]`. It is the general graph edge (every parcel, not only merges); the
older `consolidates` field is unchanged and kept for compatibility (present, non-empty,
only on a merge parcel).

`history` on an unborn pallet (nothing stacked yet) fails with `empty_history` (exit 19)
rather than the generic `error` code.

Every action also carries `"trust": "recorded"`: history reads the identity the parcel
itself records (and joins the office off it) without verifying the signature. The
verified counterpart is `query` — same `--class` flag word, different trust tier, and
both commands stamp theirs so a consumer can never mistake one for the other.

### `query --json`

`query` filters parcel history on its signed dimensions — identity class, supervisor,
role, signing key, recorded machine-authorship provenance (`provenance.model`,
`provenance.tool`, `provenance.session`), signed tags (`tag`) — plus parcel-local
facts (dates, description glob, merge-ness, parent count, hash prefix, touched paths).
Identity answers are **verified by default**: the walk prunes only on non-identity
predicates, then resolves the *verified* signer (a real signature check plus the key's
active/revoked status) for every survivor and filters on that — the parcel's own
recorded operator never decides what gets verified. `--recorded` opts into the cheap,
self-declared reading; every answer is then labeled.

Flags AND together; `--where <json>` (or `--where -` for stdin) takes the full
predicate tree for or/not/nesting: combinators `{"all": […]}`, `{"any": […]}`,
`{"not": …}` over leaves `{"field", "op", "value"}`. Operators: `eq`/`ne` (one scalar
value), `in` (an array of values), `matches` (glob `*`/`?` or literal substring — never
regex), `before`/`after` (one RFC 3339 timestamp), `between` (a two-element timestamp
array, inclusive), `touches` (a warehouse path, `path` field only). `null` is a valid
value only where absence is meaningful — `author.supervisor` (`"op": "eq", "value":
null` = "has no supervisor"); anywhere else it is refused. Bounds (refused past them,
exit 18): payload 64 KiB, depth 16, 128 leaves, `in` ≤ 256 values, glob ≤ 256 chars
(the glob bound also applies to `provenance.model`/`provenance.tool` patterns).

Provenance and tags are honest about absence, differently:

* **Provenance is opt-in evidence, not a claim.** A parcel with no provenance entry at
  all matches no `provenance.*` predicate in either direction — `{"not": {"field":
  "provenance.model", "op": "matches", "value": "claude-*"}}` never sweeps up a parcel
  that simply has no provenance recorded. A subject can carry more than one provenance
  entry (e.g. amended); a leaf matches if *any* entry matches, and the report shows the
  newest by record time. If the whole `@manifest` pallet has no head yet (never
  created, or a sparse clone that never fetched it), every `provenance.*` predicate
  reads as unknowable and `scope.provenance_source` says `"meta_pallet_absent"` (vs.
  `"present"`) — an honest empty answer, not an error.
* **A tag is membership.** An untagged parcel plainly does not carry a given tag
  (excluded by `tag eq …`, included by its negation) — only a warehouse with no
  `@tags` pallet at all makes `tag` predicates unknowable, and `scope.tags_source`
  says so the same way `provenance_source` does (`"present"` vs.
  `"meta_pallet_absent"`).

`touches` tests whether the parcel's tree differs from its **first parent's** at a
warehouse path (a file or a directory prefix) — the same diff `blame` already does for
its per-line attribution. A merge parcel is judged by the same rule: if the merge's
own result at the path already matches what its first parent had (e.g. both branches
added the identical content, or the resolution kept the first-parent side), the merge
does not match, even though its *other* parent touched the path on its own line. On a
sparse warehouse, a `touches` predicate whose confirming check needs a tree outside
the fetch scope degrades to unknowable for that parcel (counted in
`scope.out_of_scope`) rather than erroring; a tree missing that the scope says
*should* be present still errors, as tampering rather than sparseness.

```json
{
  "data": {
    "matches": [
      {
        "parcel": "<hash>",
        "author": { "operator": "<id>", "class": "agent", "supervisor": "<id>",
                    "trust": "verified" },
        "signer": { "key": "<key-id>", "operator": "<id>", "class": "agent" },
        "is_merge": false,
        "actions": [ { "action": "author", "operator": "<id>", "trust": "recorded",
                       "timestamp": "…" } ],
        "description": "…",
        "provenance": { "model": "claude-opus-4-8", "tool": "claude-code",
                        "session": "sess-1" },
        "tags": ["v1.2.0"]
      }
    ],
    "next": "<cursor>",
    "scope": { "trust": "verified", "office_asof": "current",
               "walked": 1234, "matched": 12, "out_of_scope": 0,
               "provenance_source": "present", "tags_source": "present" }
  }
}
```

`author.trust` vocabulary: `verified` (live key, signature checks out) |
`signed-revoked` (signature checks out, key revoked — never flattened to verified; the
`signer` block then carries `revocation_reason` and `boundary`) | `unsigned` |
`unknown-key` (signed, key not in the office) | `recorded` (only under `--recorded`). A
parcel without a forge-proof identity (unsigned / unknown key) never matches an
identity predicate in either direction — three-valued honesty, not a guess.

A `signed-revoked` match's `signer.boundary` is `"vouched"` when the parcel sits inside
the revoking key's distrust boundary (the history the revoker vouched for at
revocation time — e.g. `{ "signer": { "key": "<key-id>", "revocation_reason":
"compromise", "boundary": "vouched" } }`), or `"suspect"` when it sits outside that
boundary: a forged backdate, or the key's holder kept signing after the revocation.
`audit` refuses a suspect parcel outright; a read-only query cannot refuse a signed
history it was only asked to read, so this is the loud label instead. It can also read
`"unresolved"`: the parcel is not reachable from any part of the boundary *this* store
holds, and the boundary walk crossed a gap (an absent boundary head, or an absent ancestor
behind one, as in a partial clone whose franchise only ever fetched reachable history) that
could have been what vouches for it, so the question cannot be answered here at all. A gap
alone never makes a parcel that a present boundary head vouches for `"unresolved"`; it
stays `"vouched"`. `"unresolved"` must
never be read as `"suspect"` — a parcel that is plainly vouched on the origin would
otherwise look suspicious purely because this clone is incomplete; run the query against
the origin, or fetch the full history, for a definitive answer. Filter on it with a
`where` leaf on `signer.boundary` (`eq`/`in`, values `"vouched"` or `"suspect"` only —
`"unresolved"` is an answer this leaf can produce, never a value it accepts as input).

`provenance` and `tags` are always computed and attached to a match (even when the
predicate never tested them); both are omitted entirely — not an empty object/array —
when the subject carries no provenance entry, or no tags. An omitted `tags` is proof
of "genuinely untagged" only when `scope.tags_source` reads `"present"`; when it reads
`"meta_pallet_absent"` the omission is unknowable, exactly like a provenance omission
under `provenance_source: "meta_pallet_absent"`.

The `scope` block is always present so a partial or unverified pass can never read as a
complete, verified one. `office_asof` is always `"current"`: class/supervisor answers
are as the office records them *today*, not as of each parcel's authoring time. A
missing parcel body errors the whole query (like `audit`) — the parcel spine is never
sparse, so a gap there is an incomplete fetch or tampering.

Cost note: `--limit` bounds the output page, never the verification work — a verified
identity query resolves every phase-1 survivor in its walked scope. Scope the walk (a
pallet argument, `--from <rev>`) to bound work; an unscoped verified identity query
over all of history costs about what `audit` does.

### `palletize --json` (listing)

```json
{
  "data": {
    "current": "main",
    "current_unborn": false,
    "pallets": [
      { "name": "feature/x", "current": true, "head": "<hash>" },
      { "name": "main", "current": false, "head": "<hash>" }
    ],
    "meta": []
  }
}
```

Every pallet in `pallets` carries its `head` parcel hash, `null` for an unborn one (the
current pallet is included in `pallets` even when unborn, rather than only signaled
through `current_unborn`). In a warehouse a server has served, `deleted` lists the pallets
deleted here whose tombstones still stand, each `{ "name", "head" }`; it is omitted when there are
none, as in every warehouse never served.

Token-cheap by default: `stocktake --summary` reports counts only (no per-path lists),
and `diff --json` reports the changed-file set (path + kind) rather than every line —
a program reads specific content by hash when it needs it. `diff` also accepts the
reserved token `:empty` as either revision, meaning the empty tree — the base for
comparing a root parcel (which has no real "before") against a clean slate, so every
file it introduces lists as `Added`. `:empty` can never collide with a real revision:
a pallet/meta-pallet name is restricted to lowercase ASCII letters, digits, `.`, `_`, `-` and
`/`, and a hash prefix is hex digits only — neither grammar can contain `:`.

### `show --json`

`show <revision>:<path>` reads a file's content at a revision in one call — a
program's alternative to resolving a revision, walking its tree and peeking the blob
by hash itself. The argument splits on the *first* `:` (a revision can never contain
one, so the split is unambiguous even when the path does). Its `data`:

```json
{
  "revision": "<the resolved parcel hash>",
  "path": "src/app.rs",
  "hash": "<the tree entry's own object hash: a blob hash, or a recipe hash>",
  "binary": false,
  "content": "…file text…",
  "size": 1234
}
```

`binary: true` means `content` is absent — either the bytes are not text (a NUL byte
anywhere, or the bytes are not valid UTF-8 — both count, and either alone is enough),
or the path is a chunked large file, reported by its recipe metadata instead of being
assembled:

```json
{
  "revision": "<parcel hash>",
  "path": "big.bin",
  "hash": "<recipe hash>",
  "binary": true,
  "size": 104857600,
  "content_hash": "<the assembled file's whole-content hash>",
  "chunk_count": 13
}
```

`peek <hash>` on a blob carries the same `binary` signal, with the same definition —
a NUL byte anywhere, or bytes that are not valid UTF-8, either one enough on its own:
`"binary": true` and `content` is omitted, instead of the pre-fix behavior of silently
mangling the raw bytes through a lossy UTF-8 conversion with no signal that it
had happened.

## Error codes and exit codes

Every failure carries a stable `code` an agent can branch on, and the process exits
with a deterministic status so a script can branch without parsing prose. Five statuses
mean the same for every Lonic tool: `0` success, `1` unclassified failure, `2` usage error,
`130` cancelled by the user, and `128`+n killed by signal n. Every other number is
Forklift's own, and only a consumer that ran `forklift` directly may branch on it; a consumer
classifies a failure by the pair (`tool`, `error.code`).

The full table — generated from the `ErrorCode` enum the binary itself branches on, so
it can never fall behind as codes are added — is
[`generated/errors.md`](generated/errors.md).

The codes and exit numbers are a contract: they get added to, never repurposed. A single
`match` in the head (over `forklift-core`'s `RefusalCode`) maps a refusal to its exit code, so a
new code cannot ship without an exit code wired to it. `empty_history` is the one exception to
that match — a head-only condition `forklift-core` never raises, classified directly in the head
(`crates/forklift/src/output.rs`) rather than through a `RefusalCode`.

A refusal a **remote** raises carries the same code: the server tags its JSON error body with the
stable `code` (see `format/REMOTE_PROTOCOL.md`), and the client classifies it with the same code
and exit code as a local one — so a script branches identically whether the refusal was local or
server-side.

### Trust refusals: `trust_changed`

Every refusal about a remote's trust anchor is one code, `trust_changed` (exit 22), with
`error.details` saying what changed:

```json
"details": {
  "subject": "https://forklift.example.com",
  "current": "1ee9…",
  "previous": "712d…",
  "what_changed": "regenesis"
}
```

- `subject` — the remote, as its URL.
- `current` — the remote's genesis **now**: the value an override must name. `null` when the
  remote declares no anchor at all, or declares a malformed one (the value is not repeated).
- `previous` — the genesis this warehouse holds; absent when it holds none (a `franchise`).
- `what_changed` — a closed set, to branch on instead of the message:
  `regenesis` (the new anchor names the held one as its prior; the one override is
  `office accept-regenesis --confirm <current>`), `unrelated` (it does not: another warehouse,
  tampering, or a reset never seen; nothing overrides it), `unexpected` (the remote's anchor is
  not the `--expect` pin or the `--confirm` value), `malformed` (the remote declares an anchor
  whose values are not object hashes; every command against that remote refuses, and nothing
  overrides it — not a transport failure, so not worth retrying), `removed` (the remote holds an
  office but declares no anchor, which an honest server never does: its anchor was stripped.
  `lower` and `franchise` refuse before anything is fetched, whether or not an anchor is held,
  `lift` refuses before anything is uploaded, and nothing overrides it). A remote with neither an anchor nor an office is one trust was never
  established on, and is not refused; pin it with `--expect` to require an anchor.

`office accept-regenesis --confirm <value>` accepts only when `<value>` still equals the
remote's genesis; any other value refuses with fresh `details`, so an agent re-reads `current`
rather than retrying a stale one. With nothing pending, it succeeds only when `<value>` is the
genesis already held; otherwise it refuses (`unexpected`), so "nothing to accept" never reads as
the value being trusted. `franchise --expect` and `lower --expect` are checked against the
first response, before anything is fetched or created.

### A misbehaving remote: `remote_protocol_violation`

A remote that answers with something the protocol never produces — today, a handshake naming a
pallet head that is not an object hash in its canonical spelling (64 lowercase hex characters) —
is refused with `remote_protocol_violation` (exit 23). It is not a transport failure: retrying
gets the same answer, so an agent stops and reports it instead. The message does not repeat the
remote's value.

### A lift whose upload the remote lost: `staged_upload_discarded`

Against a remote where uploads go straight to storage, a lift commits its uploads before it moves
the pallet. When the remote no longer holds one of them — its verifier rejected the bytes, or they
never arrived — the lift is refused with `staged_upload_discarded` (exit 32). The remote's pallet
was not moved, and retrying the same commit can never succeed; running the lift again uploads the
object afresh. A remote that is merely slow to accept an upload is waited for instead, up to a
deadline; running out of it is an unclassified, retryable failure that also leaves the remote's
pallet unmoved.

A first-contact adoption is not a refusal: the command succeeds, reports the genesis as
`data.adopted_genesis`, and writes a `trust_adopted` warning (below) with the same `details`
shape, `what_changed: "first_contact"` and no `previous`.

### A mirror run left behind: `mirror_incomplete`

`forklift mirror` goes through every mirrored pallet even when one fails. When any was refused
(diverged, layout skew, a mirror repository it did not write), its push failed or raced, or
another run held the mirror, the command fails with `mirror_incomplete` (exit 33) after the rest
went through. `error.details` is the run's report, the same `{url, pallets}` shape a successful
run returns as `data`: each entry's `status` says what happened to that pallet and its
`message` names the step. A failure before any pallet is tried (no `mirror.url`, a busy or
damaged mirror) is not this code.

### History the anchor does not vouch for: `history_unverified`

A command that adopts a head another store served (`lower`, `franchise`, `lift`'s auto-merge,
`mirror`) or one already in the store (`palletize <name> <hash>`, `haul merge`, the `stack` that
completes a conflicted merge) refuses with `history_unverified` (exit 34) when the head's history
does not verify against the trust anchor and office it is judged against: the ones this warehouse
holds, or at first contact the ones the remote serves. Nothing was adopted: no ref moved, no
working file changed, and no anchor, office or meta pallet was installed. The message names the
pallet, the head and why; when a boundary parcel the verdict needed could not be fetched, it says
so too. A `franchise` reports it as `franchise_incomplete` with `details.cause_code:
"history_unverified"`; a `mirror` reports it as that pallet's `failed` entry.

### Catching up: `behind`, `remote_ahead`, `diverged`, `target_not_covered`

Four codes name what has to be caught up with before an operation can go through. The code names
the condition; `next_step` describes the remedy at this site, so an agent branches on the code
and follows the step.

| code | exit | condition | `next_step` |
|---|---|---|---|
| `behind` | 26 | this bay's base is behind its pallet's head: another bay or process moved the pallet. Raised by every command that moves the bay's pallet (`stack`, `deliver`, `consolidate`, `cherry-pick`, `haul merge`, `lower`, `lift`'s auto-merge, a consolidation's completion) | `forklift shift <pallet>`; a refusal mid-merge names aborting the merge first |
| `remote_ahead` | 28 | the remote's pallet holds parcels this warehouse has not lowered: its head is unknown here, descends from the local head, or moved since this client read it (the `409` of a ref update). It does not promise this side holds nothing new; `lower` then fast-forwards or reports `diverged` | `forklift lower` |
| `diverged` | 5 | the local pallet and the remote's each hold parcels the other lacks. `lower` never merges them; `lift` reports it when it did not auto-merge (an overlap, an unclean warehouse, or its retry bound) | for a working pallet: palletize the remote head (as `<pallet>-remote-<short hash>`, unique per remote head), shift back, consolidate it, lift; for a meta pallet with replay rules: `forklift lower`. The office and other meta pallets diverge as an unclassified error, reconciled by hand |
| `target_not_covered` | 27 | `deliver` would undo changes on the target that the draft never consolidated | `forklift consolidate <target>` |

`diverged` used to be described as a remote ref moving under a lift. That case is now
`remote_ahead`, and `diverged` means only a true divergence.

### A pallet this warehouse does not hold: `pallet_not_found`

`shift`, `consolidate`, `audit` and `haul open` (its target, and a source other than the current
pallet) refuse a pallet name this warehouse holds no ref for with `pallet_not_found` (exit 29). The
remedy depends on where the pallet might be: with a remote configured, `next_step` is
`forklift lower <name>`, which brings the remote's pallet in without moving this bay; without
one it is `forklift palletize <name>`. A missing meta pallet names `forklift lower` (which syncs
the remote's meta pallets) with a remote, `forklift palletize --all` without one. `lower <name>` for
a pallet the remote does not have refuses with the same code before writing anything, naming
`forklift palletize --remote`; a bare `lower` of a current pallet the remote does not have names
`forklift lift`.
`forklift palletize --remote` lists what the remote holds, so an agent can tell the two apart
before acting.
A pallet the remote deleted is the exception: `lower <name>` still refuses with
`pallet_not_found`, but says it was deleted and names `forklift palletize --restore <name> --remote`.

### A pallet the remote deleted: `pallet_deleted_on_remote`

A `lift` that would re-create a pallet the remote has only as a tombstone refuses with
`pallet_deleted_on_remote` (exit 35) before uploading anything. `next_step` is
`forklift lift --recreate` when the local head descends from the head the tombstone keeps, and
`forklift palletize --restore <name> --remote` otherwise. The remote raises the same code, with
the restore as `next_step`, when a creation of a deleted pallet does not acknowledge its tombstone
or does not descend from it, and when a ref update or delete finds its pallet deleted.

### Content a cache could not fetch: `cache_miss`

A warehouse franchised with `--cache` holds some content by hash alone, and a command that reads
such content fetches it from the warehouse's origin as it reads. When the origin cannot be reached,
the command refuses with `cache_miss` (exit 36), naming the object in `message`. `next_step` names
the origin: running the same command again once it can be reached is the way out. A command that
only reads, such as `show` or `diff`, changes nothing when it refuses. A command that changes the
bay, such as `shift`, does not yet fetch everything it needs before it starts, so it can refuse
partway through, with part of its change made.

A remote that answers but does not hand the object over, or a configured remote that is not the
origin, is a plain failure, not `cache_miss`: running the command again would not help.

### An unfinished franchise: `franchise_incomplete`

A `franchise` that fails after it has claimed its directory — a dropped connection, a full disk,
a path the working tree cannot hold, or a kill — keeps the directory with everything it fetched and
exits `franchise_incomplete` (exit 30). Every other command run in that directory, `heal` and
`audit` included, refuses with the same code before it takes any lock, so one code means one
thing: this directory holds a franchise that has not finished. Running the same `franchise` again
resumes it: the verified objects are kept, everything else is rebuilt, and `--pallet` and `--only`
may change. An omitted `--token` reuses the one the first attempt was given, and the trust anchor
the first attempt saw (or its `--expect` pin) stays enforced.

```json
"details": {
  "directory": "/work/clone",
  "remote": "https://forklift.example.com",
  "cause_code": "error",
  "retry": "unknown"
}
```

- `remote` — `null` when the franchise's record cannot be read.
- `cause_code` — the code the failure would have had on its own (`error` when it had none);
  `null` when no attempt has failed yet (one is still running, or was killed).
- `retry` — what to do next, `null` with `cause_code`:
  - `same` — a transient failure (a timeout, a server error): run the same command again.
  - `unknown` — anything else: run the same command again. When an attempt fails again in the
    same part of the franchise with the same code and stores nothing new, it is reported as
    `change` instead, so an agent never loops on a failure that makes no progress.
  - `change` — the same command fails the same way (`remote_protocol_violation`,
    `only_prefix_not_found`): change `--only` or `--pallet`, or delete the directory.

`next_step` spells out the command. A sparse franchise's `--only` path that names no directory in
the pallet is the cause `only_prefix_not_found` (exit 31 where it is raised on its own).

### Custom metadata that changed on both sides: `meta_conflict`

`lower` reconciles a diverged meta pallet (`@kv`, `@tags`, `@manifest` or `@haul`) path by
path: changes to different paths replay onto the remote's and are lifted. When the same path
changed differently here and on the remote, nothing is lifted for that pallet and `lower`
refuses with `meta_conflict` (exit 24). The working pallet
still lowers; only the conflicted meta pallets wait. `details` names every conflicted path and
both sides of it, and the working pallet that lowered with its new head:

```json
{
  "pallets": [
    { "pallet": "@kv",
      "conflicts": [
        { "path": "<subject>/<key>",
          "ours":   { "value": "amber", "signer": "<key id>" },
          "theirs": { "value": "red",   "signer": "<key id>" } }
      ] }
  ],
  "lowered": { "pallet": "main", "head": "<hash>" }
}
```

A side without `value` removed the key. Rerun `lower` with one `--keep @<pallet>:<path>=ours|theirs`
per path, both exactly as `details` names them (`--keep @kv:<subject>/<key>=theirs`). The pallet
keeps its `@`; a bare `kv:` is refused. `theirs` on a bare `<subject>` resolves every key under
it. A keep that names no conflict, or a pallet that did not diverge, is refused rather than
ignored, so a stale resolution never applies to a later conflict.

## Structured conflicts

`forklift conflicts` lists the files an unresolved consolidation or cherry-pick left in conflict.
With `--json`, each file's three sides are **content addresses** — blob hashes a
resolver fetches (`forklift peek <hash>`) and diffs, instead of parsing marker soup:

```json
{
  "data": {
    "conflicts": [
      { "path": "f.txt", "markers": true,
        "base": "<hash>", "ours": "<hash>", "theirs": "<hash>" }
    ]
  }
}
```

A whole-file or binary conflict has `markers: false` and no sides. An empty list is a
valid answer — nothing to resolve — not an error.

## Progress events

A program that runs a long Forklift operation can watch it on a channel of its own,
separate from stdout. Open a pipe (or a FIFO, or a socket), leave its write end open in
the child at some fd number above 2, and put that number in the environment:

```sh
PULT_EVENTS=7 forklift compact --json 7>events.fifo
```

Forklift writes one event per line to that fd. Two verbs appear:

| Line | Meaning |
|---|---|
| `status <phase>` | The operation entered a new phase. |
| `progress <0-100\|?> [text]` | How far the current phase has got, as a percentage, or `?` when the total is unknown. The optional text names the item or the count reached. |

Forklift never writes `step`: it always runs inside someone else's channel and leaves step
counting to whoever owns it. A reader should ignore any verb it does not know and any line it
cannot parse.

These commands report today, with these phases:

| Command | Phases |
|---|---|
| `compact` | `Collecting objects`, `Packing objects`, `Removing packed originals` |
| `lift` | `Fetching history` (only when it merges first), `Finding objects to lift`, `Uploading objects`, `Committing staged objects` (only against a staging remote), `Updating the remote` |
| `lower` | `Fetching history`, `Writing files`, `Rebuilding the inventory` |
| `import-git` | `Converting commits`, `Publishing objects`, `Building the inventory` |

Phase names are for display. Treat them as text, not as an API to branch on.

**The variable.** `PULT_EVENTS` must be bare decimal digits naming an fd above 2 that is open
for writing and is a pipe, FIFO or socket. Forklift checks this at startup, before it opens any
file of its own. Anything else is ignored without a message, and the command runs as if the
variable were unset: an empty or non-numeric value, a sign or whitespace, 0, 1 or 2, a closed
fd, a regular file, a terminal, or a read-only end. So an fd inherited by accident is never
written to, and stdout always stays the one `--json` document. Whatever a numeric value above 2
names, accepted or not, is made close-on-exec, so it is not passed on to anything Forklift runs.

**Ordering and loss.** Events arrive in the order they happened. Emitting never blocks, slows
or stops the operation: a small queue sits between the operation and a dedicated writer, and
when the reader falls behind the queue coalesces instead of waiting.

* A progress replaces the newest queued progress, if that one has not been written yet and
  its value is no greater than the new one. `?` replaces only `?`.
* A status replaces a queued status that has not been written yet, so a phase with nothing
  reported in it can be skipped.
* When the queue is still full, the oldest entry is dropped.

Within a phase, a percentage never goes down. It starts again from a lower value only after a
`status` naming the new phase, and even that ordering is best-effort if entries had to be
dropped. Repeats of the same percentage are thinned to at most ten a second. A report held
back this way is not lost: it goes out with the next report, before the next `status`, or
when the operation ends. Until then the text you are showing can lag behind the work, for
as long as the operation stays quiet.

**Lines.** Each line is at most 512 bytes, newline included: the smallest write POSIX
guarantees is atomic on a pipe, so a line never interleaves with another writer's. Longer text
is cut on a character boundary. Control characters, the Unicode line and paragraph separators,
and bidirectional formatting characters are replaced with spaces. Event text never
carries credentials. Per-item text such as a file path goes on `progress` lines, never on
`status`.

**A reader that goes away** costs the operation nothing. Writes that fail stop the channel,
and the operation finishes and exits normally. It is not killed by `SIGPIPE`, whatever the
parent's signal disposition. On macOS this takes a setting on the channel itself:
`F_SETNOSIGPIPE` on a pipe or FIFO, `SO_NOSIGPIPE` on a socket. Both settings belong to the
shared pipe or socket, not to Forklift's descriptor. Once you hand Forklift the channel, any
process writing to it, yours included, gets `EPIPE` instead of `SIGPIPE` when the reader is
gone, even after Forklift exits.

**The channel is Forklift's alone.** Forklift removes `PULT_EVENTS` from its environment and
marks the descriptor close-on-exec, so nothing it runs (the per-call `forklift` processes of
`forklift mcp`, `git` during `import-git`) writes to the channel or keeps it open. At exit, Forklift gives the writer up to 200 ms to deliver what
is still queued, then exits anyway.

The channel exists on macOS and Linux. On other platforms the variable is ignored.

## `forklift mcp` — the MCP server

`forklift mcp` runs a Model Context Protocol server on stdin/stdout (newline-delimited
JSON-RPC 2.0). Point an MCP client — an AI coding tool — at it, with the warehouse as
the working directory. It implements `initialize`, `tools/list` and `tools/call`.

Each tool re-invokes `forklift … --json` and returns its envelope, so the tools speak
the exact structured output above (and inherit the warehouse lock and exit-code
taxonomy). The tool result's content is:

* the envelope, as the first text item;
* then each stderr line that is a [warning document](#--json), one text item each, in
  order. Other stderr is not forwarded.

A failure document (`ok: false`) comes back as an MCP tool error (`isError: true`)
carrying the error envelope — the agent sees the stable `code`/`next_step`, not a crashed
session. `isError` follows `ok`, not the exit status. A run that left no document at all,
for example one killed by a signal, is always an error. Its first item names how it ended
(`forklift left no result document: it was killed by signal 9.`), then quotes the last
4 KiB of whatever it wrote to stdout and, warnings aside, to stderr, so a crash's own
account of itself is not lost.

The server ignores `SIGPIPE`: a client that stops reading ends the server once the call
in progress has finished, instead of killing it, and that call's `forklift`, mid-run. That is
the end of the connection, as stdin closing is: the server exits with status 0 and writes
nothing to stderr.

Requests are answered one at a time, in the order they arrive.

**Progress.** A `tools/call` whose request carries `_meta.progressToken`, a string or an
integer, gets `notifications/progress` for that token while it runs. Any other token is
treated as none. The server hands the call a
[progress-events](#progress-events) channel of its own and turns the events into
notifications:

* `total` is never sent. An operation can start a phase over, and a fraction sent before
  that would have reported the whole operation done.
* `progress` is 100 × the number of resets seen + the event's percentage, where a reset is
  a percentage lower than the previous one. Each new phase therefore raises the value
  instead of rewinding it. Read it as a strictly increasing tick, not a fraction.
* `progress` strictly increases. An event that would not raise it is still sent if it
  brings new text, at the last value plus 1/4096; otherwise it is dropped. Those steps
  stop short of the next whole value: at most 4095 of them follow one, so the value keeps
  meaning 100 × resets + percentage. Text that names no new phase may use only 2047 of
  them, so a new phase is still reported after a long run of text; past either limit, text
  is dropped until the value rises.
* `message` carries the phase, the percentage and the event's text
  (`Packing objects: 40% …`), and is sent only when the negotiated protocol is
  `2025-06-18` or later, the first version that has the field. A version that is not a
  `YYYY-MM-DD` date counts as earlier. Before that version, only events that raise the
  value are sent.
* At most one notification goes out every 100 ms while the call runs, plus one when it
  ends, which can follow the previous one sooner. What arrives in between is folded into
  the next one: the highest value and the newest text.
* Every notification for a call is written before that call's response, never after.

Only `compact`, `lift` and `lower` report progress. Other tools send no notifications,
and progress needs macOS or Linux, as the channel does.

The tool surface **mirrors the CLI** — every CLI command is exposed as a tool (a
multi-subcommand command becomes `<command>_<subcommand>` tools) or is on a small
human-only allow-list; a unit test (`every_cli_command_is_an_mcp_tool_or_explicitly_human_only`)
fails CI if that ever drifts. Each tool's `inputSchema` is closed
(`additionalProperties: false`), and the server enforces it: a `tools/call` whose
`arguments` are not an object, name an argument the tool does not declare, or give a
declared one the wrong type is refused with JSON-RPC `invalid_params` rather than run
with the argument dropped. A `null` value counts as the argument omitted. (`manifest_provenance`'s `tool` and `session` are the one
exception: the server sets them from the connection and discards an agent's value.)
Tools (arguments in parentheses):

- **Inspect:** `stocktake` (summary?), `history` (revision?, limit?, after?),
  `diff` (staged?, targets?), `peek` (object | inventory), `show` (target — a single
  "<revision>:<path>" string), `blame` (path, rev?), `audit` (pallet?), `conflicts`.
- **Change:** `load` (path), `remove` (path — stage a removal), `unload` (path — unstage),
  `stack` (description?), `restore` (path,
  staged?), `undo`, `park` / `park_list` / `park_pop`, `cherry_pick` (revision, message?),
  `deliver` (target, message?).
- **Maintain:** `compact` (all?) — pack the loose object store into a few dense pack files
  (safe to run anytime; worth running after a large import). `all=true` is a full repack:
  also rewrite existing packs, dropping unreachable garbage and consolidating.
- **Branch / merge:** `shift` (pallet), `consolidate` (pallet), `palletize` (name?,
  revision?, all?).
- **Remote:** `lift`, `lower` (expect?, keep?).
- **Review & metadata:** `manifest_note` / `manifest_approve` / `manifest_provenance`
  (model, transcript?, message?) / `manifest_show`, `haul_open` / `haul_list` / `haul_show` /
  `haul_comment` / `haul_review` / `haul_merge` / `haul_close` / `haul_reopen`,
  `tag_create` / `tag_show` / `tag_list`, `kv_set` (revision, key, value) / `kv_get` / `kv_unset` /
  `kv_log` (revision, key) / `kv_list` (revision?), `office_list`.
- **Sandboxing:** `bay_add` (name, path?, scope?), `bay_list`, `bay_remove`
  (name), `scope` — an orchestrator agent opens task-scoped (optionally sparse) sandboxes for
  its sub-agents directly. The scope a bay records is advisory local setup, not the agent's
  own security boundary; enforcement of what an identity may touch lives remote-side, on the
  server.

**Pagination:** `history` reads in pages — pass `limit`, and the result's `data.next`
cursor back as `after` for the following page (absent once exhausted). This is the
agent-facing counterpart of the CLI's pager (agents get a cursor, never a pager).

**Provenance is transport-derived, not self-reported.** For `manifest_provenance`
the server sets the `tool` from the connection's `clientInfo` (the harness that drove the
model) and mints the `session` itself — overriding anything the agent passes, so a model
cannot fabricate its own `tool`/`session` in the tool-call arguments. That is why those two
fields are **not** in the tool's schema. `model` stays the agent's attestation: MCP carries
no model identity, so nothing at the transport can supply or verify it. As always the entry
is *signed*, so who recorded it is forge-proof; the transport-derivation just removes the
model's own output from the `tool`/`session` it can't be trusted to report about itself.

**Not exposed** (deliberately human-only): warehouse/identity setup (`prepare`, `config`,
`profile`, `franchise`, `import-git`, `export-git`), the host-machine concerns `alias` and
`self-update`, and meta (`mcp`, `help`, `version`). `config` in particular can rewrite
`remote.url` / `remote.token`, which is not an agent-workflow action; the `office`
*mutations* (enrol/admit/rotate/…) are likewise held back — an agent operates within a
warehouse whose trust is already set up. (`office_list` is exposed, read-only.)

`bay` **is** exposed, despite also being a host working directory: an
orchestrator agent creating task-scoped sandboxes for sub-agents over MCP is how it uses
`bay`. Every bay operation is non-destructive — `bay_add` refuses onto a non-empty directory,
`bay_remove` only deletes forklift's own bookkeeping, never the materialized files.

Example session:

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"forklift","version":"…"}}}
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"stocktake","arguments":{"summary":true}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{…forklift envelope…}"}],"isError":false}}
```

## Notes for implementers

* Nothing but the envelope (or the MCP protocol messages) reaches stdout under
  `--json` / `mcp`. Progress chatter is suppressed; the result is a single document.
  Machine-readable progress goes on the `PULT_EVENTS` channel instead (see
  [Progress events](#progress-events)).
* Human output is untouched by all of this — the same commands print prose without
  `--json`, byte-for-byte as before.
* A mutating command that triggers background auto-maintenance (git's `gc --auto`
  equivalent — see `maintenance.auto`) can leave a durability taint standing if that
  maintenance's own directory sync fails, even though the command's own work already
  succeeded (`ok: true`, exit 0). That case is surfaced on **stderr** — never stdout, so the
  one-document invariant above still holds — as a single-line JSON object:
  `{ "lonic_json": 1, "tool": "forklift", "warning": "durability_taint", "message": "…", "next_step":
  "Run \"forklift heal\" …" }`. A script or agent that wants to catch this should also read
  stderr, not just check the exit code; the taint is enforced regardless on the very next
  command's entry-heal chokepoint (`durability_taint`, exit 21) until `forklift heal`
  resolves it.
* A `franchise` or `lower` that adopts a remote's trust anchor on first contact writes a
  `trust_adopted` warning to the same channel, with `details`
  `{ "subject": "<url>", "current": "<genesis>", "what_changed": "first_contact" }` — see
  [Trust refusals](#trust-refusals-trust_changed). `lower` writes it as soon as the anchor is
  adopted, since the adoption stands even if the pull then fails; `franchise` only on success,
  since a failed franchise removes the warehouse it adopted into.
* A `lower` that adopts a remote's trust anchor on first contact, or an `office accept-regenesis`,
  writes a `pallets_unvouched` warning to the same channel when the anchor does not vouch for the
  history of a pallet this warehouse already held, or that history could not be read to judge it, with `details` `{ "pallets": ["<name>", …] }`.
  Those pallets stay as they are; a take of their history into another pallet is refused
  (`history_unverified`). The command's own result stands and its exit code is unchanged.
* A `lift`, `lower` or `franchise` whose remote head could not be added to the warehouse's
  held-heads record (`.forklift/held-heads`, the remote heads it lifted to or was served by a lower,
  up to date or ahead included, or a franchise) writes a `held_head_unrecorded` warning to the same channel; `message` names the cause. The command's own
  result stands and its exit code is unchanged.
* A `scope-prune` whose remote could not answer which recorded heads of a pallet it still holds
  (a server too old to know the check answers `404`; a busy one answers `unknown`) writes a `holds_unanswered` warning to the
  same channel, with `details` `{ "pallet": "<name>" }`; `message` names the cause. The prune frees
  only what the remote's current heads allow; its result and exit code are its own.
* The same stderr channel also carries a maintenance warning, in the same shape:
  `{ "lonic_json": 1, "tool": "forklift", "warning": "maintenance_unavailable", "message": "…", "next_step":
  "Resolve the reported cause; …" }`. It means background auto-maintenance could not even decide
  whether it was **due**, so nothing was attempted. The commonest cause is a configuration file
  that does not parse, but the same decision also scans the object store, so
  an I/O error there reaches this code too — read `message` for which, and do not assume the
  condition is permanent. Unlike the taint above nothing enforces it later — the store just stops
  being packed — so a script that ignores stderr will never learn about it from any other
  surface. Every stderr warning carries these five fields (`lonic_json`, `tool`, `warning`,
  `message`, `next_step`), and may carry an optional `details`; `warning` is the code to switch
  on. Read stderr line by line and keep only the lines that parse.
