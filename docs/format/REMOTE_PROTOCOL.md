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
safe path component: ASCII letters, digits, `.`, `_`, `-`; no leading `.` or `-`).

**Authentication** is optional and out of protocol scope beyond the carrier: when a
remote requires it, the client sends `Authorization: Bearer <token>` on every request.
A remote rejects unauthenticated requests with `401` and authenticated-but-unauthorized
ones with `403`.

The server head accepts a static token (full access) and per-operator tokens
(`--tokens`, a server-side file mapping token → office identifier — tokens are
transport secrets and never enter the tracked metadata). What an operator token may
*do* derives from the operator's role in the target warehouse's office (FORK-10):
readers read, writers upload and move their granted pallets, admins move anything;
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

`trust` is `null` when the warehouse has no trust anchor. `default_pallet` is what a
franchise (clone) checks out when the user does not choose; it is the remote's current
pallet. A pallet that exists but has nothing stacked (unborn) is simply absent from
`pallets`.

**Pallet reference form.** Keys in `pallets` (and the `{name}` of the ref-update
endpoint) are *qualified references*: a user pallet is bare (`main`), a **meta pallet** —
the office, and future tracked-metadata pallets — carries the `@` qualifier (`@office`).
The server recognizes the meta namespace by the qualifier, never by a hard-coded name, so
new meta pallets need no protocol change. Bare names never start with `@`.

Clients must refuse to talk to a remote whose `protocol` they do not know.

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

The bundle is the one response with no small upper bound — it is as large as the objects
asked for. A storage-backed head may therefore answer `307` with a `Location` of a
presigned `GET` for the bundle rather than streaming megabytes back through the control
plane (the same medicine as the upload path, in the other direction; a Lambda control plane
cannot return more than a few megabytes at all). The bytes live under an **ephemeral,
content-addressed response prefix**, never the `objects/` namespace, so nothing there is
reachable as an object at a hash key and invariant 1 is not in play — and the client
hash-verifies every record on import regardless.

### `GET /v1/objects/{hash}`

The raw (uncompressed) object bytes, or `404`. The client verifies the hash before
storing.

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
server serializes trust establishment with ref updates: two concurrent first contacts
can never both plant their anchor — exactly one wins, the other gets the `409`. The
office pallet ref must be lifted (with its objects) before or in the same sync — the
anchor without the chain verifies nothing.

**Re-genesis (trust reset).** An existing anchor is replaced only when the incoming one
names it as `prior_genesis` **and** `adopts` exactly the remote's current office head
(nothing of the old chain may be silently dropped) **and** the request authenticates
with the static token — per-operator tokens derive their authority from the chain being
replaced, so they cannot sanction its replacement (`403`). The replacement is logged
loudly. The subsequent office ref update is the one sanctioned non-fast-forward: allowed
exactly when the head being moved away from is the anchor's `adopts` pin. Clients that
pinned the old anchor refuse to sync until their holder consciously re-accepts
(`office accept-regenesis`).

A client that establishes trust locally (`office enroll`) while a remote is configured
includes the remote's pallet heads in the anchor's boundary (and refuses to enroll when
the remote is unreachable, or already has an anchor of its own): unsigned history that
only the remote has must stay inside the boundary, or the anchor's arrival would make
that pallet permanently un-liftable.

### `POST /v1/pallets/{name}`

The CAS ref update — the commit point of a `lift`. `{name}` is a qualified reference:
a user pallet bare (`main`), a meta pallet with the `@` qualifier (`@office`). The server
enforces meta-pallet rules by namespace, so the office lifts to `POST /v1/pallets/@office`.

```json
{ "old_head": "<hash-or-null>", "new_head": "<hash>" }
```

`old_head: null` means "the pallet must not exist yet". Checks, in order:

1. **CAS**: the current head equals `old_head`, else `409` (the client lowers/rebases
   and retries).
2. **Presence**: the `new_head` parcel and the full closure of every parcel between
   `new_head` and `old_head` (parents, trees, blobs) are present, else `422` — a ref
   must never point at missing history.
3. **Ancestry**: `old_head` is an ancestor of `new_head` (fast-forward only; there is
   no force push in protocol v1), else `409`.
4. **Trust** (only when the remote holds an anchor):
   * for the office pallet: the whole office chain from the genesis to `new_head`
     verifies forward (each parcel signed by a key active in the *previous* state);
   * for any other pallet: the full history from `new_head` is audited — every parcel
     signed by a tracked key, unsigned parcels tolerated only when reachable from the
     anchor boundary. Failure is `422`.

`forklift-server` audits incrementally: everything reachable from `old_head` was
verified when `old_head` was committed, so the signature walk stops there — a linear
lift audits O(new parcels). A creation (`old_head` absent) audits the full history.

Two honest caveats, recorded 2026-07-09 (DESIGN.html §5.0 B/R5, "no unnecessary walk"):
the signature walk stops at the single hash `old_head`, which is the exact frontier of a
*linear* lift but not of a **merge**, whose frontier is the merge-base set — so a merge
lift re-verifies signatures below the fork point. And the closure check builds its prune
set by walking `old_head`'s whole ancestry, so *every* ref update is O(history) parcel
reads regardless. Both err by doing more work than needed, never less; both are fixed by
the same generation-number-bounded frontier.

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
blob still sitting in staging simply reads as absent, and the client retries once the
verifier has caught up.

`200` when the session is ready to commit; `422` with the offending hash when an object is
missing, still unverified, or corrupt. Promotion is idempotent, so a retried commit is safe.
On success the session's staging prefix is swept.

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
simply absent:

```json
{ "names": { "<operator id>": "<display name>", "…": "…" } }
```

The server answers from its `resolution` hook (`docs/format/HOOK_PROTOCOL.md`),
forwarding the authenticated caller so the directory can tier its answer. A server
with no resolution hook returns an empty map. The endpoint is **additive**: servers
that predate it answer `404`, and clients treat any failure — `404`, unreachable,
malformed — as "show pseudonyms", so no version bump is required.

### `GET /v1/bundles/latest`

The most recent bundle (see `BUNDLE_FORMAT.md`), or `404` when none was built. A bundle
is an optimization, never a source of truth: clients verify every record's hash and
fall back to loose-object `GET`s for anything the bundle lacks.

## The flows

**lift (push)** — for the office pallet first (when trust is established), then the
working pallet:
1. `GET /v1/warehouse`; refuse when the remote head is unknown locally (lower first).
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
locally), fetch parcel signatures, then a local fast-forward. A diverged local pallet is
an error (consolidate locally, then lift) — protocol v1 has no remote-tracking refs.

**franchise (clone)** — prepare an empty warehouse, adopt the remote's trust anchor,
lower the office pallet and the default (or chosen) pallet, materialize the working
directory. When `GET /v1/bundles/latest` succeeds, the bundle is imported first and the
breadth-first walk only fetches what it lacked.
