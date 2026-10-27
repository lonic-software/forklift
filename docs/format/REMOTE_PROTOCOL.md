# Remote protocol — version `2026-07-05`

The protocol between a forklift client (`lift` / `lower` / `franchise`) and a forklift
remote. Two server heads implement it, and both are first-class ways to host a
warehouse: `forklift-server` (the self-hostable server head, in this repository) and
the AWS serverless head behind the hosted service (API Gateway + Lambda, DESIGN.html
§4). The spec is written so that a serverless implementation can hand the byte transfer
off to presigned storage URLs without the client caring.

## Transport

HTTP/1.1 or later. All control endpoints exchange JSON (`application/json`); object
bodies are raw bytes (`application/octet-stream`). Every endpoint lives under a version
prefix: `/v1/…`. Errors use conventional status codes with a JSON body
`{"error": "<human-readable message>"}`.

**Classified refusals** (the error taxonomy in `MACHINE_INTERFACE.md`) add two **optional**
fields to that body: `code` — the stable refusal code string — and `next_step` — its
machine-actionable recovery step. When a server-side operation fails with a classified refusal,
the head sets them so a client classifies it with the same code and exit code as a local refusal;
`error` is the human message. Both are **additive**: an older head omits them, and a client that
does not understand them ignores them (a plain error simply has no `code`). Example:
`{"error":"…","code":"out_of_scope","next_step":"…"}`.

**Base URLs and multi-warehouse serving.** The `/v1/…` paths hang off whatever base
URL the client was configured with (`remote.url`); the base may carry a path prefix
that names the warehouse, and clients treat it as opaque. The server head serves a
single warehouse at `/v1/…` (`--root`) or a folder of warehouses at
`/warehouses/{id}/v1/…` (`--warehouses`); the hosted service uses the same
`/warehouses/{id}` shape. A request addressed to a warehouse that does not exist is
`404` — creation is explicit (below), never a side effect of a lift.

**Warehouse creation** (`PUT /warehouses/{id}`, multi-warehouse servers only) is an
administrative operation outside the `/v1` protocol surface: idempotent (`201`
created, `200` already present), refused with `403` on servers without authentication
configured (an open server must not be a junk farm), `422` for invalid ids (a single
safe path component of at most 100 characters: lowercase ASCII letters, digits, `.`, `_`, `-`;
no leading `.` or `-`, no trailing `.`, and no Windows device name as its stem, the same
portable rules a pallet-name component follows), and `409`
when something that is not a warehouse already holds the id. Of concurrent creations of one new
id, exactly one answers `201`.

**Authentication** is optional and out of protocol scope beyond the carrier: when a
remote requires it, the client sends `Authorization: Bearer <token>` on every request.
A remote rejects unauthenticated requests with `401` and authenticated-but-unauthorized
ones with `403`.

The server head accepts a static token (full access) and per-operator tokens
(`--tokens`, a server-side file mapping token → office identifier — tokens are
transport secrets and never enter the tracked metadata). What an operator token may
*do* derives from the operator's role in the target warehouse's office (FORK-10):
readers read, writers upload and move their granted working pallets (the grant does
not scope `@`-qualified pallets — see `docs/SERVER.md`), admins move anything;
office lifts additionally verify per parcel that the *signer* stayed within their
privileges (non-admins may only touch their own keys), a content invariant that holds
no matter which token transported the chain. On a warehouse without trust there are no
roles; the transport token is the whole gate.

**Transport compression:** servers may negotiate `Content-Encoding` (the server head
offers zstd/gzip; clients send `Accept-Encoding`). The hash always covers the
uncompressed object (invariant 3), so verification is untouched; responses that are
already zstd streams (bundles, batches) are marked `identity` and never re-wrapped.

**Redirects:** `GET`/`PUT` on object endpoints may answer `307` with a `Location`
pointing at a storage URL (a presigned S3 URL in the hosted service). Clients must
follow redirects for object bodies. The server head serves bytes directly.

## The invariants (non-negotiable)

1. **Nothing unverified is ever fetchable.** A remote must verify
   `Blake3(body) == {hash}` before an uploaded object becomes visible at its hash key
   (DESIGN.html §4.2 step 4 / §6.2). A head that lets clients upload *around* it (presigned
   PUTs straight to storage) must therefore take those bytes at a **staging key**, never at
   the hash key: a client with a presigned PUT to `objects/{hash}` could otherwise park
   arbitrary bytes at a valid hash and have them served before anything verified them. The
   staged bytes become fetchable only when the head copies them to the hash key, and it
   copies them only after the check — a **verify-and-promote**, the single write path into
   the canonical namespace.
2. **Clients verify every downloaded object** the same way before storing it.
3. **Objects travel uncompressed.** The hash covers the full uncompressed object
   (§4.4), so the wire format is the verifiable form. Transport-level compression
   (gzip, zstd content-encoding) is free to happen underneath.
4. **The ref update is the only mutation, and it is a CAS.** Everything else is
   immutable content addressed by hash.
5. **A trusted warehouse stays trusted.** Once a remote holds a trust anchor, every ref
   update is audited (office chain + signatures) before the CAS commits, and the anchor
   can never be replaced silently. The one sanctioned replacement is a **re-genesis**
   (§8.7 of the design): a new anchor naming the current genesis as `prior_genesis` and
   pinning the current office head as `adopts`, accepted only from the server
   operator's static token — a loud, total, visible reset, never a quiet edit.

## Endpoints

### `GET /v1/warehouse`

The one-round-trip handshake: protocol version, refs and trust.

```json
{
  "protocol": "2026-07-05",
  "default_pallet": "main",
  "pallets": { "main": "<parcel-hash>", "@office": "<parcel-hash>" },
  "trust": { "genesis": "<hash>", "enabled_at": 1780000000, "boundary": ["<hash>"],
             "prior_genesis": "<hash, re-genesis only>", "adopts": "<hash, re-genesis only>" }
}
```

`trust` is `null` when the warehouse has no trust anchor; a server never holds `@office`
without one, so a client refuses a handshake that has `@office` and no `trust` (the anchor was
stripped). Every hash — the `pallets` values and the anchor's — is an object hash in its one
canonical spelling, 64 lowercase hex characters; a client refuses a handshake with any other
spelling. `default_pallet` is what a
franchise (clone) checks out when the user does not choose; it is the remote's current
pallet. A pallet that exists but has nothing stacked (unborn) is simply absent from
`pallets`.

**Pallet reference form.** Keys in `pallets` (and the `{name}` of the ref-update
endpoint) are *qualified references*: a user pallet is bare (`main`), a **meta pallet** —
the office, and future tracked-metadata pallets — carries the `@` qualifier (`@office`).
The server recognizes the meta namespace by the qualifier, never by a hard-coded name, so
new meta pallets need no protocol change. Bare names never start with `@`.

Clients must refuse to talk to a remote whose `protocol` they do not know.

**Object hashes on the wire.** Every `{hash}` in an object or signature path, and every entry of
the `hashes` list of `missing`, `upload-targets` and `batch`, is an object hash in its canonical
spelling, 64 lowercase hex characters. A head refuses any other spelling with `422` and does not
repeat it back. The object store answers for that spelling alone.

### `POST /v1/objects/missing`

Body: `{"hashes": ["<hash>", …]}` (at most 10 000 per request — batch larger sets).
Response: `{"missing": ["<hash>", …]}` — the subset the remote does not have. Used by
`lift` to negotiate what to upload.

### `POST /v1/objects/upload-targets` (additive; storage-backed heads)

The **body-less upload negotiation**. Request:

```json
{ "session": "<lift session>", "hashes": ["<hash>", …] }
```

Response — one verdict per hash, and not a single object body sent to learn it:

```json
{
  "present": ["<hash>", …],
  "targets": { "<hash>": "<presigned PUT url>", … },
  "direct":  ["<hash>", …]
}
```

`present` are objects the remote already has (do not upload them; it is exactly the
complement of `missing`, so this call subsumes that one on the upload path). `targets` are
presigned `PUT`s into the session's **staging prefix** — the bytes go straight to storage,
bypassing the control plane, and are not fetchable until the session commit promotes them.
`direct` are objects to `PUT` to `/v1/objects/{hash}` as usual, for the head to verify
inline.

A direct head (`forklift-server`) answers with every missing hash in `direct` and an empty
`targets`, so one client code path serves both heads. Without this call, a client uploading
to a storage-backed head must send each body to the control plane only to be answered `307`
— paying for the bytes twice, through a request-size limit the byte plane exists to avoid.
Servers that predate it answer `404`; the client falls back to `missing` + per-object `PUT`.

### `POST /v1/objects/batch`

Many objects in one round trip. Request: `{"hashes": […]}` (max 10 000). Response: a
**bundle-format stream** (`BUNDLE_FORMAT.md`) of the requested objects, served with
`Content-Encoding: identity` (the stream is already zstd inside). Objects the remote
lacks are simply absent — the client notices what did not land and falls back to loose
`GET`s. The endpoint is additive: servers that predate it answer `404` and clients
fall back entirely. Every imported record is hash-verified before it lands, exactly
like a bundle import.

A head builds the whole bundle before its first byte, so a large request can take a while to
start answering. A client that has had no status line within its own patience (the reference
client waits 45 s past connecting) abandons the batch and falls back to loose `GET`s, exactly as
for a `404`, and does not ask this endpoint again for the rest of the command. A failure after
the status line, or a `5xx`, is an error, not a fallback.

The bundle is the one response with no small upper bound — it is as large as the objects
asked for. A storage-backed head may therefore answer `303 See Other` with a `Location` of a
presigned `GET` for the bundle rather than streaming megabytes back through the control
plane (the same medicine as the upload path, in the other direction; a Lambda control plane
cannot return more than a few megabytes at all). This is `303`, not the `307` a `GET`/`PUT`
redirect uses: the original request here is a `POST`, but the target only ever accepts `GET`,
so the redirect must tell the client to switch methods rather than replay the `POST` — which
would fail signature verification against a presigned URL signed for `GET` only. Clients must
follow this redirect by issuing a plain `GET` (no request body, and no `Authorization` header —
the presigned URL is self-authorizing) rather than relying on generic HTTP redirect-following,
since a `307`/`308` from an older or non-conforming head must still be handled the same way.
The bytes live under an **ephemeral, content-addressed response prefix**, never the `objects/`
namespace, so nothing there is reachable as an object at a hash key and invariant 1 is not in
play — and the client hash-verifies every record on import regardless.

### `GET /v1/parcels/{parcel}/subtree/{path}` (additive; the path-addressed fetch)

The object closure of the subtree at `path` in `parcel` — the subtree root object and every
tree and blob beneath it — as a **bundle-format stream** (`BUNDLE_FORMAT.md`), served with
`Content-Encoding: identity`. `{path}` is a warehouse path key (`/`-separated, e.g. `src/api`);
the empty path is the whole tree. A parcel or path that does not resolve to a subtree is `404`.
Every record is hash-verified on import, exactly like a bundle.

This is the **path-addressed** counterpart of hash-addressed fetch. A sparse client already
fetches exactly a subtree by pruning its hash-addressed walk by path (the MVP, which needs no
new endpoint); this endpoint moves that resolution to the **remote**, which is what lets the
remote *authorize a fetch by path*. A hash-addressed `GET /v1/objects/{hash}` is path-blind — it
cannot enforce a per-path grant without a hash→path index — so this endpoint is the wire surface
**file-level path enforcement** (FORK-10) is designed to gate: the remote resolves the subtree,
checks the operator's path grants, and `403`s an out-of-scope path. The authorizer is a distinct
seam in the head, wired when file-level FORK-10 ships; **today the read is open to every
authenticated principal**, exactly as an object fetch already is, so the endpoint adds no
enforcement yet — it only positions the surface. Task-scoped sparse workspaces are not
confidentiality (hiding content): that is a separate feature (the private-tier locker), and this
endpoint must not be sold as it.

The endpoint is **additive**: a client whose remote predates the route gets a `404` and falls
back to the shipped hash-addressed scoped walk, so it needs no protocol bump. A storage-backed
head may answer `307` with a presigned `GET` of the bundle under the ephemeral response prefix,
the same medicine the batch endpoint uses for large responses.

Two further additive endpoints are **designed but deferred**, both pure optimizations behind the
same `404`-fallback discipline (a client falls back to the hash walk / to recomputing locally):
a **scoped-bundle** endpoint for sparse cold-start bundle speed, and **served commit-graph
shards** so a sparse clone gets full changed-path filters without recomputing them. Neither
changes what is fetchable, only how fast.

### `GET /v1/objects/{hash}`

The raw (uncompressed) object bytes, or `404`. The client verifies the hash before
storing. A client that falls back from the batch endpoint sends many of these at once, so a
rate-limited head may answer `429`. The client retries after `Retry-After` (delta-seconds, never
sooner than its own backoff, plus a random share of it so a wave of refused fetches does not
return at once) for a bounded time, then fails with that answer. It treats a `429` on
`POST /v1/objects/batch` and on the subtree endpoint the same way.

### `PUT /v1/objects/{hash}[?session={id}]`

Body: the raw object bytes. The remote verifies `Blake3(body) == hash` **before** the
object becomes fetchable; a mismatch is `422` and nothing is stored. Uploading an
already-present hash is a no-op `200` (objects are immutable, so equal hash means equal
content). Success is `201`.

A head whose byte plane is object storage answers `307` instead, with a `Location` of a
presigned PUT under the **staging prefix of the lift session** — `staging/{session}/{hash}`,
never `objects/{hash}`. Nothing is fetchable at the hash key until
`POST /lift/{session}/commit` (small control-plane objects) or the staging verifier (large
blobs) has verified and promoted it. Such a head therefore needs the session: `?session=`
is **additive and head-specific** (the `forklift-server` head ignores it and verifies the
body inline), but a staging head answers `422` without one, because bytes staged under no
session could never be promoted.

A session id is one or more ASCII letters, digits and `-` (the client sends a hyphenated UUID),
wherever it appears: this query, the `upload-targets` body and the commit path. A staging head
refuses any other id with `422`, since a `/` would place that session's staging keys under
another session's prefix.

### `GET /v1/signatures/{hash}` · `PUT /v1/signatures/{hash}`

The signature sidecar of a parcel (the binary format of
`PARCEL_SIGNATURE_FORMAT.md`), addressed by the parcel hash. `PUT` validates the sidecar
structure (`422` when malformed) — whether the signature *verifies* is decided at ref
update time, when the office state is known. `GET` answers `404` for unsigned parcels.
A sidecar for an already-signed parcel is immutable — a conflicting re-upload is `409`.

### `PUT /v1/trust`

Body: the trust anchor `{"genesis": …, "enabled_at": …, "boundary": […]}` (plus
`prior_genesis` and `adopts` on a re-genesis anchor). Establishing trust on a remote is
the same one-way door it is locally: accepted only when the remote has no anchor yet
(`201`), idempotent when the anchor is identical (`200`), and `409` otherwise. The
install is a compare-and-set on the anchor the server read: two concurrent first contacts
can never both plant their anchor — exactly one wins, the other gets the `409` (or the
`200`, when its anchor is the identical one that won). The
anchor comes first and the office pallet ref (with its objects) follows in the same sync:
no head lifts `@office` while it holds no anchor (`422`), and the anchor without the chain
verifies nothing. A remote that holds `@office` but no anchor has lost its anchor, and
`PUT /v1/trust` there is `409` — a client cannot plant an anchor over a chain it never
verified; the operator must restore the lost one.

**Re-genesis (trust reset).** An existing anchor is replaced only when the incoming one
names it as `prior_genesis` **and** `adopts` exactly the remote's current office head
(nothing of the old chain may be silently dropped) **and** the request authenticates
with the static token — per-operator tokens derive their authority from the chain being
replaced, so they cannot sanction its replacement (`403`). The replacement is logged
loudly. The subsequent office ref update is the one sanctioned non-fast-forward: allowed
exactly when the head being moved away from is the anchor's `adopts` pin. Clients that
pinned the old anchor refuse to sync until their holder consciously re-accepts
(`office accept-regenesis --confirm <genesis>`, which accepts only the genesis the remote
declares at that moment).

Both conditions the replacement is granted under are re-checked **at the write**, not just
when the request is read, so a re-genesis is refused if either moves underneath it. A second
re-genesis that lands first takes the incumbent the request validated against, and a loser
naming a **different** anchor gets `409` — of two concurrent re-geneses to different anchors,
exactly one wins. Two concurrent requests carrying the **same** anchor both succeed, one `201`
and one `200`: the second is refused nothing, because the state it asked for is the state that
now holds. That is the same idempotence a first contact has, where two clients planting the
identical anchor also get `201` and `200` rather than a conflict. An office ref update that
lands between the `adopts` check and the write gets `422`, the same status as an `adopts`
mismatch present from the start: the remedy is identical, and the client cannot usefully tell
the two apart.
A remote may also answer `503` when the write loses to unrelated concurrent traffic rather than
to either value actually moving. The AWS serverless head has already re-issued the write itself,
within a small bounded budget, before it answers `503`, so the answer means the write could not
be confirmed within that budget — and since an attempt may have timed out or still been in
progress, the anchor may already be the new one. A `503` therefore establishes nothing about the
anchor either way: read the trust anchor before repeating the request (repeating it unread is
harmless, since an identical anchor is answered `200`, but it reports nothing new). Clients do
not retry it automatically.

A client that establishes trust locally (`office enroll`) while a remote is configured
includes the remote's pallet heads in the anchor's boundary (and refuses to enroll when
the remote is unreachable, or already has an anchor of its own): unsigned history that
only the remote has must stay inside the boundary, or the anchor's arrival would make
that pallet permanently un-liftable.

### `POST /v1/pallets/{name}`

The CAS ref update — the commit point of a `lift`. `{name}` is a qualified reference:
a user pallet bare (`main`), a meta pallet with the `@` qualifier (`@office`), so the office
lifts to `POST /v1/pallets/%40office` (the reference `@office`, encoded as below). The meta
pallet names are a fixed list — `office`, `manifest`, `haul`, `tags`, `kv` — and any other
`@name` is refused with `422` before any other check.

```json
{ "old_head": "<hash-or-null>", "new_head": "<hash>" }
```

`{name}` is one path segment, here and on every route that takes a pallet. A client
percent-encodes the whole reference, `/` included, so `feature/x` travels as `feature%2Fx` and
`@office` as `%40office`. A server percent-decodes the segment back to the reference, and refuses
with a `4xx` a segment that does not decode to UTF-8. An unencoded `/` splits the name across
segments, so the request matches no route or the wrong one (a raw `feature/holds` reads as the
holds route of `feature`).

`old_head: null` means "the pallet must not exist yet". Checks, in order:

1. **CAS**: the current head equals `old_head`, else `409` with `code` `remote_ahead` and
   `next_step` `forklift lower` (the client lowers/rebases and retries).
2. **Presence**: `new_head` is an object hash in its canonical spelling (64 lowercase hex
   characters), and the `new_head` parcel and the full closure of every parcel between
   `new_head` and `old_head` (parents, trees, blobs) are present, else `422` — a ref
   must never point at missing history.
3. **Ancestry**: `old_head` is an ancestor of `new_head` (fast-forward only; there is
   no force push in protocol v1), else `409`.
4. **Trust** (only when the remote holds an anchor):
   * for the office pallet: the whole office chain from the genesis to `new_head`
     verifies forward. Each parcel is signed by a key active in the *previous* state,
     with every key it adds carrying valid proofs, else `422`. Each parcel also stays within
     its signer's privileges as of the previous state (only an admin changes user records
     or other users' keys, only an admin's key endorses a key for another user, and nobody
     removes, alters or un-revokes a key), else `403`. The refusal names the first failing
     parcel in chain order. When that overstepping parcel is already in the held office, the
     lift did not bring it: the refusal is `422` and says so;
   * for `@kv`: the office must be lifted (with no anchor or no office, `@kv` is refused
     with `422`), and every parcel from `old_head` to `new_head` passes the meta content
     check — one parent, signed by an office key, every cell it adds or changes stamped with
     that key, the pallet's format and layout kept;
   * for any other pallet: the full history from `new_head` is audited — every parcel
     signed by a tracked key, unsigned parcels tolerated only when reachable from the
     anchor boundary. Failure is `422`.

`forklift-server` audits incrementally: everything reachable from `old_head` was
verified when `old_head` was committed, so neither the signature walk nor the closure
check descends into it. A lift audits O(new parcels) — for a **merge** too, whose second
parent may fork below `old_head`: the boundary is computed from the commit-graph's
generation numbers rather than by stopping at a single hash, which is the exact frontier
of a linear lift but not of a merge (whose frontier is the merge-base set). A creation
(`old_head` absent) audits the full history.

On the AWS serverless head, the CAS in step 1 also conditions on the office head and the
trust anchor the audit above consumed, atomically with the pallet's own head — so a
concurrent office lift or re-genesis is caught at commit time even after the audit
already passed, and is reported `409` there too. That same head can additionally answer
**`503`**: its commit is a single backend transaction, and the backend's own concurrency
control can cancel it for a reason that establishes nothing about whether any input
actually changed (contention from another push touching a shared item, principally the
office pallet, rather than a value having moved). The head re-issues that transaction
itself before answering — the transaction alone, never the audit, under one idempotency token
per request, at most four attempts in all and only while its invocation window can afford
another — so a `503` reaches the client only when the transaction could not be completed within
that budget. Unlike every other status in this section, `503` makes no claim about server or
request state, including whether the update itself landed: an attempt that timed out or was
still in progress when the budget ran out may commit, and a retry of the identical request
would then be answered `409`, the pallet having moved to the client's own new head. So a client
fetches before retrying any `503` here: a pallet already at the pushed head means the push
succeeded, and a pallet still at `old_head` means the identical request can be sent again —
though an attempt still in progress can land just after that fetch, so a resend answered `409`
calls for one more fetch: a pallet at the pushed head then is a push that succeeded, not a
conflict, whatever the `409`'s message advises. The
message says which kind of `503` it is for a person reading it; nothing in the status or headers
distinguishes them, and a client must not parse the message to decide.

The consequence, stated plainly: a remote that has *lost* an object behind `old_head` no
longer fails at ref-update time. It never failed on a lost tree or blob behind it either —
that ancestry is trusted, by the same induction. The `audit` command is what re-proves a
whole history.

### `POST /v1/pallets/{name}/holds` (additive)

Body: `{"candidates": ["<parcel hash>", …]}` (at most 16 per request).
Response: `{"holds": {"<parcel hash>": "held" | "not_held" | "unknown", …}}` — one answer per
distinct candidate: whether the pallet still holds that parcel as its head or one of the head's
ancestors.

- `held` — the walk from the pallet's head reached the parcel.
- `not_held` — the walk read the pallet's whole history without reaching it, or the warehouse has
  no such pallet.
- `unknown` — the walk stopped before it could tell: it read its step bound of parcels, ran past
  its time bound, or could not read a parcel. Both bounds are fixed on every head.

A client may let a remote's `held` stand in for keeping that history itself; it must read
`unknown`, any answer it does not recognise, and a head without this endpoint (`404`) as
"not confirmed", which keeps everything. A candidate that is not an object hash, more than 16 of
them, or a name no pallet can carry is a `422`. Reading history is what fetching already allows,
so no permission beyond authentication is asked.

### `POST /lift/{session}/commit` (additive; serverless head)

The **session-commit** step, for a head where object bytes bypass the control plane —
the client `PUT`s them straight to storage via presigned URLs (the `307` redirect on the
object endpoints), so the head never sees them to verify inline. Before the ref update,
the client asks the head to confirm the session's uploads are ready:

```json
{ "control_plane": ["<parcel/tree/signature hash>", …], "blobs": ["<blob hash>", …] }
```

The head **verifies and promotes the small control-plane objects synchronously** — for each
it reads the staged bytes, checks `Blake3(bytes) == hash`, and only then copies them to the
canonical hash key. A corrupt object staged straight to storage is *discarded* here and the
lift is refused (`422`); it never becomes fetchable, because it was never at the hash key to
begin with. Large blobs are **checked for presence at the canonical key only** — which is
itself the proof they were verified, since the staging verifier (an out-of-band worker
running the same verify-and-promote) is the only thing that could have put them there. A
blob absent there is looked up in the session's staging area: still staged means the
verifier has not caught up yet; in neither place means its upload was discarded (the
verifier rejected it) or never arrived. The head probes the canonical key once more before
calling a blob gone, so a blob promoted between the two probes is never refused.

The answers:

| Status | Meaning | Client |
|---|---|---|
| `200` | The session is ready to commit. On the final batch the staging prefix is swept. | Proceed to the ref update. |
| `503` with `Forklift-Commit: not-ready` and `Retry-After` | Some blob is still staged, not yet verified. | Retry the identical request. |
| `422` with code `staged_upload_discarded` | An object is in neither place, or a control-plane object was corrupt and has been discarded. Retrying can never succeed. | Stop; running the lift again uploads it afresh. |
| `429` | Rate limited. | Retry after `Retry-After` (delta-seconds). |
| other `5xx` | Says nothing about the session. | Retry. |

A refusal is always a `4xx`; no `5xx` from this endpoint is one. A client classifies the
answer from the **status line and headers alone**: the not-ready
signal is the header, never the body, so a `503` whose body is cut off is still a not-ready
answer. Only a terminal `4xx` body is read, for its framed code. Promotion is idempotent, so
every retry is safe, and the reference client bounds its retrying with one deadline per
commit batch rather than a fixed attempt count. It also retries a `408` (an intermediary gave
up before the remote answered), any connect that failed before sending anything (refused,
unreachable, a name that did not resolve) other than a TLS handshake it rejected, and a
connection reset or cut off after sending; it does not retry a rejected TLS handshake or an
answer it cannot parse, which would only repeat. The commit never moves a ref; whatever its
outcome, the remote pallet is untouched until the ref update that follows.

**Mismatched versions, both directions.** An older client retried only a `422` carrying its
not-ready marker in the body, so it fails at once on the `503` rather than waiting for the
verifier, and reads a discarded upload's `422` as an ordinary refusal. An older head answers
a still-staged blob with that marked `422`, which a newer client treats as terminal: the lift
again fails at once. Either way the remedy is running the lift again once the verifier has
caught up. Neither direction can move a ref or lose data; the head
and the client should still be deployed together.

The endpoint is **additive and head-specific**: the `forklift-server` head verifies every
`PUT` inline (a returned `PUT` means the object is present and verified), so it does not
need it, and a client talking to a head that predates it — or one that serves bytes
directly — simply skips it. The invariant it preserves is the same on both heads: nothing
unverified is ever fetchable at its hash key.

### `POST /v1/resolve`

Resolve pseudonymous operator identifiers to display names. The chain stores zero PII
(DESIGN.html §8.12), so names live only in the provider's directory; resolution is
**server-mediated on purpose** — the server authenticates the caller and applies the
resolution policy before answering, which a client talking to the directory directly
could not. This endpoint feeds display only (`history`, `office list`); it is never a
verification input.

```json
{ "identifiers": ["<operator id>", "…"] }
```

Response — the names the caller is permitted to see; anything withheld or unknown is
simply absent. Every key is an identifier the request named, spelled as it named it: a
server drops any other key its directory gives, and a client ignores any other key it
receives:

```json
{ "names": { "<operator id>": "<display name>", "…": "…" } }
```

The server answers from its `resolution` hook (`docs/format/HOOK_PROTOCOL.md`),
forwarding the authenticated caller so the directory can tier its answer. A server
with no resolution hook returns an empty map. A request names at most 10 000 distinct
identifiers (a repeated one counts once); a server refuses more with `422` — batch
larger sets. The endpoint is **additive**: servers
that predate it answer `404`, and clients treat any failure — `404`, unreachable,
malformed — as "show pseudonyms", so no version bump is required.

### `GET /v1/bundles/latest`

The most recent bundle (see `BUNDLE_FORMAT.md`), or `404` when none was built. A bundle
is an optimization, never a source of truth: clients verify every record's hash and
fall back to loose-object `GET`s for anything the bundle lacks.

A head serves only a bundle in the version its build writes. forklift-server answers `404`
while the file it holds is in another version (built before an upgrade), and logs a warning
naming the rebuild, so a client does not download a file it would only refuse. A client that is
served another version anyway skips it and fetches objects, as it does on a `404`.

A cut-off download can be resumed. Every answer states its length in `Content-Length`. A `200`
from forklift-server carries `Accept-Ranges: bytes` and a strong `ETag` naming the file served (it
sends no tag only on a platform that reports no modification time). A request with
`Range: bytes=N-`, either with no `If-Range` or with `If-Range: E` naming the current tag, gets
`206 Partial Content` when `N` is less than the bundle's length `L`. That response has
`Content-Range: bytes N-(L-1)/L`, the same `ETag`, a `Content-Length` of `L - N`, and exactly the
bytes from `N` on. An offset at or past the end gets `416` with `Content-Range: bytes */L`.
Everything else gets `200` with the whole current bundle under its own tag (RFC 9110 `If-Range`
semantics): an `If-Range` naming a stale or weak tag or a date, another range form, or a repeated
`Range` or `If-Range` line. A tag names one built file, never a path: a rebuild publishes a new
file and gets a new tag, and a download that started before the rebuild finishes the file it began
under that file's tag. Two builds may rarely share a tag. That is safe, because a resumed bundle is
checked against its own digest before any of it is stored (see `BUNDLE_FORMAT.md`), so a collision
costs one more full download, never a misread.

A head need not support ranges. One that ignores `Range`, or sends no `ETag` or no
`Content-Length`, stays correct: the client then downloads the bundle in full each time and keeps
nothing it could not resume. A client resumes only under `If-Range`, and accepts a `206` only when
its `Content-Range` starts at the offset it asked for, its `ETag` is the tag it sent and its
complete length is the one it recorded. It treats any other `206`, or a `416`, like a `200`: it
drops what it kept and downloads in full.

## The flows

**lift (push)** — for the office pallet first (when trust is established), then the
working pallet:
1. `GET /v1/warehouse`; refuse when the remote head is unknown locally (lower first), and
   refuse a remote that holds `@office` but no `trust` (its anchor was stripped).
2. Collect the closure of the parcels between the local head and the remote head;
   `POST /v1/objects/missing` in batches — or, against a storage-backed head,
   `POST /v1/objects/upload-targets`, which answers the same question *and* hands back the
   presigned `PUT`s in the same round trip.
3. `PUT` the missing objects in parallel — to the control plane, or straight to the
   presigned staging URLs; `PUT` the signature sidecars of the new parcels;
   `PUT /v1/trust` when the remote has no anchor and the client does.
4. Against a staging head only: `POST /lift/{session}/commit`, which verifies and promotes
   the staged control-plane objects before anything becomes fetchable.
5. `POST /v1/pallets/{name}` with `{old_head: remote head, new_head: local head}`.

**lower (pull)** — the mirror: `GET /v1/warehouse`, breadth-first fetch of the unknown
closure from the remote head (parallel `GET`s, skipping objects already present
locally), fetch parcel signatures, then a local fast-forward. The walk **stops at any
parcel already reachable from a local ref head**, whose closure was proven complete when
that ref moved — so a lower that brings one parcel walks one parcel, and no longer
re-probes the remote for the signature of every unsigned parcel in history. An interrupted
earlier sync leaves its ref unmoved, so its half-fetched objects sit above that bound and
are still re-walked and healed. A diverged local pallet is an error (consolidate locally,
then lift) — protocol v1 has no remote-tracking refs.

**franchise (clone)** — prepare an empty warehouse, adopt the remote's trust anchor,
lower the office pallet and the default (or chosen) pallet, materialize the working
directory. When `GET /v1/bundles/latest` succeeds, the bundle is imported first and the
breadth-first walk only fetches what it lacked. A franchise that fails partway keeps the bundle
bytes it synced, with their tag and total, and its next attempt resumes them as described under
`GET /v1/bundles/latest`.
