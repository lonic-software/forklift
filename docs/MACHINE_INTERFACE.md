# The machine-first interface

Forklift's command surface is built to be driven by programs — scripts, CI, and AI
coding agents — as well as people. Three things make that work:
a `--json` mode with a versioned envelope, a stable error and exit-code taxonomy, and
an MCP server that exposes every command as a schema-typed tool.

## `--json`

`--json` is a global flag: add it to any command and stdout becomes exactly **one**
JSON document (nothing else prints there). Human prose is unchanged without the flag.

Success envelope:

```json
{
  "forklift_json": "2",
  "command": "stocktake",
  "ok": true,
  "data": { "…command-specific…" }
}
```

Failure envelope (also sets the exit code below):

```json
{
  "forklift_json": "2",
  "ok": false,
  "error": {
    "code": "not_a_warehouse",
    "message": "…",
    "next_step": "Run \"forklift prepare\" to create a warehouse here, or change into one."
  }
}
```

`forklift_json` is the output schema version. It changes only when the envelope or a
command's `data` shape changes incompatibly, so a consumer can pin it — and it *is* the
capability-detection mechanism: check the version before relying on a field, rather than
sniffing for the field's presence. A command's `data` shape is documented by the struct
it emits (in `crates/forklift/src/commands/`) — the generated, exhaustive reference for
every one of them is [`generated/json-schemas.md`](generated/json-schemas.md).

**Version 2** (current): `history` entries carry `parents` (every parcel's parents, in
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
`"unresolved"`: at least one of the revocation's boundary heads is not present on *this*
store (a partial clone whose franchise only ever fetched reachable history — an orphaned
head never arrives), so the question cannot be answered here at all. `"unresolved"` must
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
through `current_unborn`).

Token-cheap by default: `stocktake --summary` reports counts only (no per-path lists),
and `diff --json` reports the changed-file set (path + kind) rather than every line —
a program reads specific content by hash when it needs it. `diff` also accepts the
reserved token `:empty` as either revision, meaning the empty tree — the base for
comparing a root parcel (which has no real "before") against a clean slate, so every
file it introduces lists as `Added`. `:empty` can never collide with a real revision:
a pallet/meta-pallet name is restricted to ASCII letters, digits, `.`, `_`, `-` and
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
with a deterministic status so a script can branch without parsing prose. `2`
is reserved for argument/usage errors (clap); `0` is success.

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

## `forklift mcp` — the MCP server

`forklift mcp` runs a Model Context Protocol server on stdin/stdout (newline-delimited
JSON-RPC 2.0). Point an MCP client — an AI coding tool — at it, with the warehouse as
the working directory. It implements `initialize`, `tools/list` and `tools/call`.

Each tool re-invokes `forklift … --json` and returns its envelope, so the tools speak
the exact structured output above (and inherit the warehouse lock and exit-code
taxonomy). A command that exits non-zero comes back as an MCP tool error (`isError:
true`) carrying the error envelope — the agent sees the stable `code`/`next_step`, not
a crashed session.

The tool surface **mirrors the CLI** — every CLI command is exposed as a tool (a
multi-subcommand command becomes `<command>_<subcommand>` tools) or is on a small
human-only allow-list; a unit test (`every_cli_command_is_an_mcp_tool_or_explicitly_human_only`)
fails CI if that ever drifts. Tools (arguments in parentheses):

- **Inspect:** `stocktake` (summary?), `history` (revision?, class?, limit?, after?),
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
- **Remote:** `lift`, `lower`.
- **Review & metadata:** `manifest_note` / `manifest_approve` / `manifest_provenance`
  (model, transcript?, message?) / `manifest_show`, `haul_open` / `haul_list` / `haul_show` /
  `haul_comment` / `haul_review` / `haul_merge` / `haul_close` / `haul_reopen`,
  `tag_create` / `tag_show` / `tag_list`, `office_list`.
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
* Human output is untouched by all of this — the same commands print prose without
  `--json`, byte-for-byte as before.
