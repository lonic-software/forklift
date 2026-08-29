# Deploying the AWS serverless head

A from-source reference for standing up `forklift-aws-lambda` — the AWS "driver" behind
`docs/format/REMOTE_PROTOCOL.md` — on your own AWS account, in your own infrastructure-as-code.
It is a sibling of `forklift-server` (`docs/SERVER.md`): the same protocol, the same audit
code, a different transport. Nothing here is a managed offering; it is the open-source
building block anyone can deploy.

Every claim below is grounded in `crates/forklift-aws-lambda/src/{entrypoint.rs, head.rs,
aws/{s3.rs, dynamo.rs, config.rs}, bin/{control-plane.rs, verifier.rs}}` and
`crates/forklift-aws-lambda/tests/aws_integration.rs`, current as of this writing. Where the
code leaves an infrastructure choice open, it is called out as an **operator decision** with a
recommended default — this crate ships two Lambda binaries and the store code behind them, not
a Terraform/CDK stack.

## Architecture overview

```
 forklift CLI (lift / lower / franchise)
        │
        │ HTTPS, JSON control calls: /v1/…
        ▼
 ┌─────────────────┐
 │  API Gateway     │
 └────────┬─────────┘
          │ invokes
          ▼
 ┌────────────────────────────┐        ┌──────────────┐
 │ forklift-aws-control-plane │◄──────►│  DynamoDB     │  ref heads, trust anchor
 │        (Lambda)            │        │  table        │  (the CAS — see "DynamoDB table")
 └────────┬────────────────┬──┘        └──────────────┘
          │                │
          │ small objects   │ presigns GET/PUT URLs
          │ (verify+promote,│ (control endpoints only —
          │  signatures,    │  never carries object bytes)
          │  trust, refs)   │
          ▼                ▼
 ┌───────────────────────────────────┐
 │              S3 bucket             │
 │  objects/{hash}   staging/{sess}/  │
 │  signatures/{hash} responses/{h}   │
 └───────────────┬───────────────────┘
                  │ s3:ObjectCreated:* on staging/*
                  ▼
 ┌─────────────────────────┐
 │  forklift-aws-verifier   │  verify_and_promote → objects/{hash}
 │        (Lambda)          │  (the same function the control plane
 └─────────────────────────┘   calls synchronously for small objects)

 forklift CLI ═══════════════════════════════════════▶ S3 bucket
              presigned PUT (staging) / GET (objects, responses):
              object bytes travel directly, client ↔ S3.
              API Gateway and both Lambdas never see them.
```

The client never uploads or downloads an object body through API Gateway. `GET`/`PUT
/v1/objects/{hash}` and the `batch` bundle endpoint answer with a redirect (`307` for
`GET`/`PUT`, `303` for `batch` — see "API Gateway" below for why) to a presigned S3 URL; the
client follows it and talks to S3 directly. The one exception is the signature sidecar
endpoints, which do return raw bytes through the control-plane Lambda — see "Binary responses"
below.

A client `PUT`s straight to `staging/{session}/{hash}`, never to the canonical `objects/{hash}`
key — nothing lands at a hash key until it is hash-verified. Small control-plane objects
(parcels, trees, signatures) are verified and promoted **synchronously** by the control plane
when the client calls `POST /lift/{session}/commit`; large blobs and chunks are verified and
promoted **asynchronously** by the verifier Lambda, triggered by the S3 event the staging `PUT`
itself fires. Both call the identical `ObjectStore::verify_and_promote`, and it is idempotent,
so the two racing on one hash is safe — whichever gets there first wins, the other reads
`AlreadyPresent`.

## The two Lambda functions

| Binary | `[[bin]] name` | Source | Trigger |
|---|---|---|---|
| Control plane | `forklift-aws-control-plane` | `src/bin/control-plane.rs` | API Gateway (every `/v1/…` and `/lift/{session}/commit` request) |
| Staging verifier | `forklift-aws-verifier` | `src/bin/verifier.rs` | S3 event notification (`ObjectCreated` on `staging/`) |

**Control plane.** A thin adapter: builds the SDK clients once per cold start into a
process-global cell, converts the `lambda_http` request into the plain `http::Request` the
provider-agnostic router (`entrypoint::handle`) speaks, and runs it on a blocking thread
(`tokio::task::spawn_blocking`) — every `Head` method blocks on its store's futures, and tokio
refuses to let a runtime worker block. It builds `Head` with `Head::pooled`, which reuses a
process-global, on-disk audit-mirror scratch across warm invocations (keyed by warehouse id),
so the ref-update audit's mirroring cost is amortized rather than paid on every request.

**Staging verifier.** Even thinner: for every S3 event record it parses the `staging/{session}
/{hash}` key (decoding the `+`/`%XX` escaping S3 event notifications apply, preferring the
event's own `url_decoded_key` field when present) and calls `verify_and_promote` on a blocking
thread. It builds only an `S3ObjectStore` — no `DynamoRefStore`, no warehouse id — because
promotion is content-addressed, not warehouse-scoped. A storage error propagates (so S3 retries
the event); a semantic outcome (missing, corrupt) is logged and the event is consumed, since
retrying a corrupt object would only rediscard it forever.

**Operator quirk:** both binaries call the same `config_from_env()`, which requires
`FORKLIFT_DYNAMODB_TABLE` to be set and non-empty — even for the verifier, which builds a
DynamoDB client it never calls a single operation on. Give the verifier some valid table name
(the same table is fine) or its cold start fails outright.

### Building the deployment artifacts

```sh
cargo build -p forklift-aws-lambda --features lambda --release
```

The two binaries are gated behind the `lambda` feature (`required-features = ["lambda"]` in
`Cargo.toml`) precisely so a plain `cargo build`/`cargo test` never needs the Lambda runtime
crates. `lambda_http`/`lambda_runtime` implement AWS's custom (`provided`) runtime contract
directly — there is no framework-specific packaging step in this repo (unlike
`forklift-server`, which ships a `Dockerfile`; this crate ships none). Concretely, you need to:

1. Cross-compile each binary for your target Lambda architecture.
2. Rename it to `bootstrap` and zip it (the `provided.al2023` runtime contract), or hand the
   binary to a tool that does this for you — [`cargo-lambda`](https://www.cargo-lambda.info/)
   (`cargo lambda build --release [--arm64]`) is the community-standard way to do this for
   `lambda_http`/`lambda_runtime`-based crates and needs no code changes here.
3. Upload one zip per function; the control plane deploys as
   `forklift-aws-control-plane`/`bootstrap`, the verifier as `forklift-aws-verifier`/`bootstrap`.

**Operator decision — packaging tool.** The code assumes nothing about how the binary reaches
Lambda beyond "a `bootstrap` executable behind the `provided.al2023` (or `provided.al2`)
runtime". `cargo-lambda` is a reasonable default; a hand-rolled cross-compile + zip works
identically.

### Architecture: arm64 or x86_64

Nothing in the code is architecture-specific — no inline assembly, no arch-gated `cfg`. The TLS
stack is deliberately routed through a **ring**-based rustls connector
(`aws-smithy-http-client/rustls-ring`, wired explicitly in `aws::config::build_clients`) instead
of the SDK's default `rustls-aws-lc`, specifically so `aws-lc-sys` — a C/cmake build — never
enters `Cargo.lock` (a standing constraint recorded in the crate's own dependency comments).
That has a direct, practical payoff here: cross-compiling to **arm64** (Graviton) needs no
native C toolchain for the crypto provider, only for whatever `cargo-lambda`/`cross` already
handle for a pure-Rust build.

**Operator decision — architecture.** Either works; arm64 (Graviton) Lambdas are generally
cheaper per GB-second and this crate's ring-based TLS choice removes the one dependency that
would have made cross-compiling to arm64 harder. Recommended default: **arm64**, unless your
build pipeline already standardizes on x86_64.

### Memory sizing

The two functions have different memory profiles, because they exercise different halves of
`verify_and_promote`'s two-tier promotion strategy (`aws::s3`, `STREAMING_THRESHOLD_BYTES = 8
MiB`): below the threshold, a staged object is buffered whole and hashed in one shot (bounded by
the threshold itself); at or above it, the object is **never** buffered — it is stream-hashed in
chunks and promoted with a server-side `CopyObject`, so memory use there is bounded by one read
chunk, not by the object's size (up to the hard `MAX_STAGED_OBJECT_BYTES` cap of 5 GiB, S3's own
single-`PUT` maximum).

* **Verifier Lambda:** only ever calls `verify_and_promote`, so its memory floor is small and
  flat regardless of blob size. **256 MB** is a reasonable default; there is no legitimate staged
  object whose promotion needs more.

* **Control-plane Lambda:** `verify_and_promote`'s own bound is the same 8 MiB/streaming split,
  but `Head::ref_update`'s commit-gate closure audit is a second, larger memory sink the
  verifier never touches. To presence-check a chunked file's chunks or prune an unchanged
  subtree, the audit reads a **recipe** or a **tree** object straight off `ObjectStore::get`
  (`Head::ref_update`'s `load_recipe_chunks`/`load_base_tree` closures, and `scratch.rs`'s
  `materialize`) — an unbounded, whole-object buffer, capped only by the write-time ceiling
  `MAX_OBJECT_BYTES` (64 MiB; `forklift-core`'s `object_utils.rs` — the one object type besides a
  very large tree that can legitimately approach it). One such buffer is held at a time (the
  walk is sequential), so the control plane's floor is "one 64 MiB buffer plus normal JSON/SDK
  overhead", not "one 8 MiB buffer".

  Recommended default: **512 MB**, which also buys proportionally more CPU (Lambda ties CPU
  allocation to configured memory) for the audit's own work. Bump to **1024 MB** if your
  warehouses routinely stack near-maximal (64 MiB) trees or recipes.

**Ephemeral storage (`/tmp`).** `Head::pooled`'s shared scratch (`scratch::Scratch::shared`)
persists on `/tmp`, keyed by warehouse, across warm invocations — so most invocations only add
the *new* parcels since the last one. The exception: the very first audit against a cold
container has no commit-graph yet, so it loads the **whole** parcel-body ancestry of the ref
being moved (bodies only, not trees/blobs below the bound — see `scratch.rs`'s module docs).
Lambda's default 512 MB of ephemeral storage is likely fine to start; **operator decision** —
raise it (configurable up to 10,240 MB) if a warehouse has a long history and cold starts are
common (low traffic, or aggressive Lambda recycling). There is no code-side cap here; running
out surfaces as an internal `500`.

### Timeout guidance

**This is the section to read before deciding your ref-update workload is safe.**

API Gateway enforces a **hard, non-configurable 29-second integration timeout** for a
synchronous Lambda proxy integration (a platform limit, not something this crate can adjust or
paginate around, and true of both REST and HTTP APIs). `POST /v1/pallets/{name}` — the ref
update — is exactly such a call, and it runs the whole commit-gate closure audit synchronously
inside it.

Most of that audit is cheap: the closure check is *O(new parcels)*, and an **unchanged**
subtree (its hash equal to the same path under the prior head) is pruned without loading its
tree or descending its recipe at all — the fix that made routine pushes affordable in the first
place. A **changed** chunked file is walked in full, and every one of its chunks is
presence-checked with an S3 `HeadObject`; but that walk is now a **bounded-concurrency batch**,
not a serial loop. The commit gate hands a recipe's whole chunk list to
`ObjectStore::objects_missing`, which the S3 store answers by running up to
`MISSING_PROBE_CONCURRENCY` (**64**) `HeadObject`s in flight at once (`aws::s3`, via `futures`'
`buffer_unordered` on the same blocking thread the head runs on — no task spawning), returning
the absent subset. The check stays non-tolerant: the ref is refused the moment any chunk comes
back missing.

**The realistic envelope.** At a typical in-region `HeadObject` latency of ~15–40 ms and 64
probes in flight, the head clears on the order of **1,500–4,000 chunk checks per second**. A
changed 1–2 GiB file (~1,000–2,000 chunks at the ~1 MiB average chunk size) therefore verifies in
**roughly one to two seconds** — comfortably inside the 29-second budget, where the old serial
walk took ~30–60 s and timed out. Even a changed ~10 GiB file (~10,000 chunks) lands around 3–7 s.
The headline case — pushing a large *changed* file through the serverless head — now fits.

**The ceiling that remains, and the operator posture for it.** Concurrency widens the envelope; it
does not make it infinite. A *maximal* 64 MiB recipe lists on the order of **987,000 chunks**, and
987k ÷ ~3,000/s is still ~5 minutes — which does fit inside Lambda's own 900-second ceiling but is
far past API Gateway's non-negotiable 29 s, so a single such file, *changed*, still cannot commit
through this head in one synchronous ref update. No concurrency cap short of thousands closes that
gap, and thousands would trip S3's per-prefix request-rate throttle (`503 SlowDown`) and the SDK's
connection pool — which is why 64 is the chosen width. This is now a **pathological-file** limit,
not an everyday-large-file one: it is reached only by content that chunks into ~10⁶ pieces
(roughly TiB-scale files at the average chunk size, or a hand-built maximal recipe). If your
warehouses genuinely carry such files, either (a) keep those objects off the trusted serverless
head's write path — lift them against a **self-host / server head**, which runs the *same* audit
without API Gateway's ceiling and whose presence probe is a microsecond filesystem lookup (serial
is instant there), or (b) split them upstream. Set the control-plane Lambda's own timeout to
roughly API Gateway's ceiling (**29–30 s** — anything higher just burns compute after the client
already has its `504`). The verifier Lambda has no such ceiling (it is not behind API Gateway);
its own cost is proportional to one object's size, so **60–120 s** is ample headroom. S3 invokes
it asynchronously, so Lambda's own asynchronous-invocation retry policy (a couple of automatic
attempts with backoff, before the event is dropped or sent to a configured on-failure destination)
covers transient failures — worth pairing with a dead-letter queue/on-failure destination on the
verifier for visibility into anything that exhausts its retries.

## The S3 bucket

### Key layout

One bucket, four prefixes (`aws::s3`'s module docs), and the boundary between them **is** the
protocol's "nothing unverified is fetchable" invariant:

| Prefix | Contents | Written by |
|---|---|---|
| `objects/{hash}` | Canonical, hash-verified objects. The **only** namespace a `GET` ever serves. | `put_verified` (direct write) / `verify_and_promote` (promotion) |
| `staging/{session}/{hash}` | Unverified bytes a client `PUT` straight into, via a presigned URL. Invisible to `exists`/`get`. | The client, via a presigned `PUT` |
| `signatures/{parcel_hash}` | Parcel signature sidecars — a distinct prefix so a sidecar never collides with its parcel's own object at `objects/{parcel_hash}`. | `put_signature` |
| `responses/{content_hash}` | Ephemeral, content-addressed `batch`-bundle bodies offloaded from the control plane. Never `objects/`. | `offload_response` |

No presigned `PUT` is ever issued to `objects/{hash}` — `presign_staging_put` hardcodes the
`staging/` prefix and is the only code path that mints a presigned `PUT` at all.

### Required: expire the `staging/` prefix

**You must configure a bucket lifecycle rule expiring `staging/*`.** Nothing in the code ever
revisits an abandoned lift session: `discard_session` only runs when a client's *final*
`commit_lift` batch (`more: false`) actually arrives, and a crashed, killed, or simply abandoned
client — or a paginating client that never sends a final batch — leaves its staged bytes
unswept forever. This is explicitly called out as an operational, not a code, gap in
`aws::s3`'s module docs.

* **Suggested age: 7 days** — comfortably longer than any legitimate lift (including a
  paginated one) is expected to take, short enough that abandoned sessions do not accumulate
  indefinitely.

### Recommended: expire the `responses/` prefix

Nothing in the code ever deletes a `responses/{content_hash}` object — `offload_response` is an
unconditional overwrite keyed by content hash, and there is no sweep anywhere in this crate.
Each presigned `GET` to one is only valid for the URL's own TTL (below), so the object itself
serves no purpose past that. **Operator decision, no MUST here** — a lifecycle rule expiring
`responses/*` after **1 day** is a reasonable default; there is no durability reason to keep them
longer, since clients hash-verify every bundle record on import regardless.

### Event notification wiring

Configure S3 Event Notifications (bucket-level, or EventBridge if you prefer) for
**`s3:ObjectCreated:*` on the `staging/` prefix**, targeting the verifier Lambda. The verifier
parses `record.s3.object.key`, decodes it, and calls `parse_staging_key` — anything that does
not split cleanly into `staging/{session}/{hash}` is logged and skipped rather than retried, so
a notification scoped to any other prefix is simply wasted invocations, not a correctness bug.
Do not wire `ObjectRemoved` or other event types — the verifier has no use for them, and a
`staging/`-shaped key from a delete event would just read back `Missing`.

### Versioning, encryption, CORS

All three are **operator decisions**; none is required or read by the code:

* **Versioning** — not referenced anywhere in `aws::s3`. Enable it if you want an audit trail of
  overwrites (there should be none in `objects/`/`signatures/`, since both are conditional
  writes to immutable keys — a `412` on a second write, not a silent overwrite).
* **Encryption** — SSE-S3 or SSE-KMS at rest, your call; the SDK calls carry no
  encryption-context parameters either way.
* **CORS — not needed.** Every client is the `forklift` CLI (a `reqwest`-based Rust binary)
  following a presigned redirect; there is no browser-facing surface anywhere in this crate (no
  `Access-Control-*` handling in `entrypoint.rs`, nothing in the protocol that expects a
  browser `fetch`). Do not add a CORS configuration unless you are building your own
  browser-facing client on top of this protocol.

## The DynamoDB table

### Schema

One table serves every warehouse. Exactly two attributes form the key (`aws::dynamo`'s module
docs, and the exact shape `tests/aws_integration.rs`'s `provision()` creates):

| Attribute | Type | Role |
|---|---|---|
| `wh` | `S` | Partition key — the warehouse id |
| `entity` | `S` | Sort key — `pallet#{qualified-ref}` (e.g. `pallet#main`, `pallet#@office`) or the literal string `trust` |

Item payload attributes (`head` for a pallet item, `anchor` — a JSON string — for the trust
item) are not part of the key schema and need no separate declaration when you create the
table; DynamoDB is schemaless beyond the key.

### The CAS

`compare_and_set_head` is a real conditional write, not a read-then-write — the atomicity that
lets this head scale horizontally where `forklift-server` needs an in-process mutex. As of
FORK-95 it is a `TransactWriteItems` of up to three actions rather than a single-item
`UpdateItem`: an `Update` on the target pallet's own item (`ConditionExpression` encoding the
caller's expected head, exactly the prior single-item shape), a `ConditionCheck` on the office
pallet's item at the office head a ref update's audit consumed (skipped when the target pallet
*is* the office pallet — the `Update` above already pins that item, and DynamoDB refuses two
actions on one item in the same transaction), and a `ConditionCheck` on the trust item at the
anchor's stored serialization. DynamoDB either applies the whole transaction with every
condition holding or applies nothing; on a condition failure
`ReturnValuesOnConditionCheckFailure=ALL_OLD` hands back the failed item, so the store reports
which precondition moved without a second round trip. `put_trust_if_absent` is still the
single-item shape (a conditional `PutItem`) for the one-way trust door — `replace_trust`'s own
conditional write is a later slice (FORK-95 claim C22). **No secondary index is used or
needed:** every action conditions on a single item's own attribute(s), and ref enumeration
(`list_refs`) is answered by a plain `Query` on the base table (`wh = … AND begins_with(entity,
"pallet#")`) — the partition key plus a sort-key prefix, which needs no GSI.

**There is no `dynamodb:TransactWriteItems` IAM action to grant.** AWS's own model —
verified against the "Using IAM with DynamoDB transactions" developer guide (none of its four
example policies grant it) and the Service Authorization Reference (whose `TransactWriteItems`
row maps the *operation* onto the IAM actions `ConditionCheckItem`/`DeleteItem`/`PutItem`/
`UpdateItem`, and lists no `TransactWriteItems` action at all) — is that permission to call
`TransactWriteItems` is governed entirely by the permissions for the underlying per-item action
types the transaction contains. This transaction always carries one `Update` and one or two
`ConditionCheck`s, so the deployed role needs `dynamodb:UpdateItem` (already granted, for the
pallet head write it always performed) and `dynamodb:ConditionCheckItem` (newly granted, for
the office/anchor checks) — see the derivation table below.

### Capacity mode

**Recommended: on-demand (`PAY_PER_REQUEST`)** — what `tests/aws_integration.rs`'s `provision()`
creates the test table with. Ref-update traffic is bursty and warehouse-sized rather than
steady, and on-demand avoids provisioning (and paying for) idle read/write capacity units per
warehouse. Switch to provisioned capacity only if you have enough steady traffic to make it
cheaper and are willing to manage auto-scaling.

**A ref-update commit moved from 1 write capacity unit to 4–6, once FORK-95's
`TransactWriteItems` replaced the single-item `UpdateItem`.** AWS's Developer Guide states that
DynamoDB performs two underlying reads or writes of every item in a transaction (one to
prepare, one to commit), consumed whether or not the transaction succeeds — that multiplier is
attested by the operator, not something this deployment can verify offline (no pinned SDK
source states it, and no execution against a real table has measured it here). Every figure
below is that attested `2×` multiplied by the transaction's item count, so a reader who doubts
the premise can see which number moves if it turns out wrong. A trusted lift to a non-office
pallet touches three items (the pallet `Update`, the office `ConditionCheck`, the anchor
`ConditionCheck`) for 6 WCU; a lift to `@office` itself or any push on an untrusted warehouse
touches two (no office `ConditionCheck` is built) for 4 WCU. On-demand billing absorbs this
without a capacity-planning step; on provisioned capacity, size write throughput against the
higher number.

### TTL

**None used.** No item this crate writes ever needs to expire — a pallet head and the trust
anchor are permanent state, not ephemeral data — so there is nothing to configure a DynamoDB TTL
attribute against.

## API Gateway

### HTTP API vs REST API

`lambda_http` is built with both the `apigw_http` and `apigw_rest` features (`Cargo.toml`), so
the control plane decodes events from either API Gateway flavor without any code change — this
is purely an **operator decision**. Recommended default: **HTTP API** (API Gateway v2) — it is
cheaper, has lower latency, and (see "Binary responses" below) handles binary Lambda-proxy
responses without any extra configuration, which a REST API does not.

A **single catch-all route** is sufficient either way (`ANY /{proxy+}` on an HTTP API, or a
greedy `{proxy+}` resource + `ANY` method on a REST API): `entrypoint::handle` does its own
method/path dispatch and answers `404` for anything it does not recognize, exactly like any
other head would for a stray path. There is no need to declare one API Gateway route per
protocol endpoint.

### Routes

Every endpoint `entrypoint::match_endpoint` recognizes, in single-warehouse mode (`/v1/…`
directly). In multi-warehouse mode every path below is additionally prefixed with
`/warehouses/{id}` (including the commit endpoint):

| Method | Path | Endpoint |
|---|---|---|
| `GET` | `/v1/warehouse` | Handshake |
| `POST` | `/v1/objects/missing` | Which hashes are missing |
| `POST` | `/v1/objects/upload-targets` | Body-less upload negotiation |
| `POST` | `/v1/objects/batch` | Many objects, one bundle stream |
| `GET` | `/v1/objects/{hash}` | Fetch an object |
| `PUT` | `/v1/objects/{hash}[?session={id}]` | Upload an object |
| `GET` | `/v1/signatures/{hash}` | Fetch a signature sidecar |
| `PUT` | `/v1/signatures/{hash}` | Store a signature sidecar |
| `PUT` | `/v1/trust` | Establish or re-genesis the trust anchor |
| `POST` | `/v1/pallets/{name}` | The ref-update CAS |
| `POST` | `/v1/resolve` | Operator id → display name (always an empty map on this head — no resolution hook) |
| `GET` | `/v1/bundles/latest` | Latest whole-warehouse bundle (always `404` today — no bundle builder exists for this head yet) |
| `POST` | `/lift/{session}/commit` or `/v1/lift/{session}/commit` | Verify-and-promote a lift session's staged uploads (both path forms are accepted — a documented spec/implementation mismatch the router tolerates rather than picks a side on) |

### Payload sizes

The protocol is shaped so this head never needs to negotiate around API Gateway's ~10 MB
request/response payload limit or a synchronous Lambda invocation's ~6 MB response limit:

* `POST /v1/objects/missing` caps a request at `MAX_MISSING_BATCH` (10,000 hashes) — a worst
  case of a few hundred KB of hex hashes, whichever direction.
* `POST /v1/objects/upload-targets` caps at the smaller `MAX_UPLOAD_TARGETS_BATCH` (1,000) —
  deliberately smaller, because each answer carries a presigned URL (~500 bytes) per hash, and
  10,000 of those would push a response toward the Lambda response ceiling. 1,000 keeps a
  worst-case response comfortably under 1 MB (`forklift-core`'s `remote.rs` spells out this
  arithmetic).
* Object bodies never transit API Gateway on this head at all — `PUT`/`GET
  /v1/objects/{hash}` and the `batch` bundle answer with a redirect (`307`/`303`) to a presigned
  S3 URL, empty body, `Location` header only, regardless of the underlying object or bundle
  size.
* `POST /lift/{session}/commit`'s combined `control_plane` + `blobs` hash lists are capped at
  `MAX_MISSING_BATCH` by the router, and a maximal chunked file's chunk list is paginated by the
  client across several `commit_lift` calls (`more: true`/`false`) precisely so one request never
  needs to name all of it.

These caps bound *payload* size; they used to also bound how long the head spent probing S3, one
`HeadObject` per hash. That is no longer serial: `missing`, `upload_targets`'s presence check, and
`commit_lift`'s blob-presence check all now resolve through the same bounded-concurrency
`ObjectStore::objects_missing` batch the ref-update chunk descent uses (see "Timeout guidance"
above), so budget for the *parallel* envelope described there, not for one round trip per hash in
the batch.

### Binary responses

Most responses are JSON or an empty body with a `Location` header. There is one exception worth
knowing about precisely: **`GET /v1/signatures/{hash}` returns raw signature bytes
(`application/octet-stream`) directly through the control-plane Lambda** — unlike object
fetches and `batch` bundles, `ObjectStore::get_signature` has no redirect variant, so signature
bytes are never offloaded to a presigned URL. (Object `GET`/`batch` *could* theoretically return
raw bytes too, per the trait's default — but on this S3-backed store, `access`/`offload_response`
always redirect, so in practice only the signature endpoint returns bytes on this head.)

Signature sidecars are small (a single parcel's signature, not file content), so size is not a
concern — but the response **is** binary. An **HTTP API** (API Gateway v2) handles a binary
Lambda-proxy response transparently (`isBase64Encoded`, no console configuration). A **REST
API** (v1) requires you to explicitly configure **Binary Media Types**
(`application/octet-stream`, or `*/*`) in the API's settings, or this one response type can be
mishandled. This is the concrete reason to prefer an HTTP API over a REST API here.

### Auth at the gateway

API Gateway authorizers (IAM, JWT/Cognito, or a Lambda authorizer) are an optional **additional**
layer in front of the head's own bearer-token check (below) — not a substitute for it, since the
head is also reachable directly if you ever expose the Lambda function URL, and because the
protocol's own bearer token is what `forklift-server` speaks too, so a single client code path
authenticates against either head. Layering a gateway authorizer in front is a legitimate
defense-in-depth choice, entirely orthogonal to what ships in the entrypoint.

## IAM — least privilege

One policy per Lambda, derived from the actual SDK calls each store method makes. The **canonical,
enforced** copies are `crates/forklift-aws-lambda/iam/control-plane.policy.json` and
`crates/forklift-aws-lambda/iam/verifier.policy.json` — `{bucket_arn}`, `{table_arn}`, and
`{log_group_arn}` are `templatefile()` placeholders for your resource ARNs. Two tests keep them
honest, so treat what follows as an explanation of *why* each statement exists, not a copy to
retype: `crates/forklift-aws-lambda/tests/iam_conformance.rs` (C11) asserts the code can only
reach the SDK operations these wrapper types expose, and that set is exactly what the JSON grants;
`infra/aws-serverless/tests/main.tftest.hcl`'s C6 asserts the Terraform-deployed policy equals an
independent `templatefile()` render of the same JSON. If you deploy by hand instead of via the
`infra/aws-serverless` module, attach the JSON files directly — do not retype a policy from this
page.

### `forklift-aws-control-plane`

Where each action in `iam/control-plane.policy.json` comes from: `HeadObject`/`GetObject`/presigned
`GET` → `s3:GetObject`; `PutObject` (conditional and unconditional)/presigned staging
`PUT`/`CopyObject` destination → `s3:PutObject`; `CopyObject` source read → `s3:GetObject` on
`staging/*`; `DeleteObject` (staged cleanup, session sweep) → `s3:DeleteObject`; `ListObjectsV2`
(`discard_session`'s sweep) and `HeadObject`/`GetObject` 404-vs-403 semantics (below) →
`s3:ListBucket` on the **bucket** resource (not `bucket/*`); `GetItem`/`Query` (ref reads,
enumeration) → the table ARN, no index ARN needed; `PutItem` (the one-way trust door) and the
ref-update commit's `TransactWriteItems` (below) → `UpdateItem` (the target pallet's own head)
and `ConditionCheckItem` (the office head and trust anchor `ConditionCheck`s the same
transaction carries) — **not** `dynamodb:TransactWriteItems`, which is not a real IAM action;
see "The CAS" above for why granting it would have been a no-op that left the transaction still
refused. The `Logs` statement (`logs:CreateLogGroup`/`logs:CreateLogStream`/
`logs:PutLogEvents`) is the standard Lambda execution-role grant every function needs to write to
its own CloudWatch log group.

### `forklift-aws-verifier`

The verifier calls only `verify_and_promote` — no DynamoDB permissions of any kind (it builds a
DynamoDB client via `config_from_env`/`build_clients`, but never issues a single operation against
it). `iam/verifier.policy.json`'s statements: `s3:GetObject`/`s3:DeleteObject` on `staging/*` (read
then clear the staged object once promoted), `s3:GetObject`/`s3:PutObject` on `objects/*` (write the
promoted copy), `s3:ListBucket` on the bucket (404-vs-403 semantics, below — `verify_and_promote`'s
`key_exists` check against the canonical `objects/{hash}` key, absent by definition for a genuinely
new object, is a `HeadObject` on a missing key), and the same `Logs` statement as the control plane.

### `s3:ListBucket` on both roles is unconditional, and has to be

This is a non-obvious operational fact, found against a real AWS account, not derived from the SDK
docs: **S3 returns `403 Forbidden` instead of `404 Not Found` for `HeadObject`/`GetObject` on a
missing key when the caller lacks `s3:ListBucket`** — without it, S3 refuses to confirm the key
doesn't exist rather than saying so. `is_head_object_not_found`
(`crates/forklift-aws-lambda/src/aws/sdk.rs`) only matches the SDK's modeled `HeadObjectError::
NotFound` variant, which a `403` is not, so a caller missing this grant sees every "object not
here yet" outcome surface as an opaque `500` instead of the expected absent/not-found result —
concretely, this **breaks lifting any object the head does not already have**, since a fresh lift
`HEAD`s a key that (correctly) doesn't exist yet.

The grant **cannot be prefix-conditioned**. `s3:prefix` is not part of the request context S3
authorizes a `HeadObject`/`GetObject` call against — it is a `ListObjectsV2`-specific condition key
— so a `ListBucket` statement scoped with `Condition: {StringLike: {s3:prefix: "..."}}` (as an
earlier version of this policy did, reasoning that `list_objects_v2`'s own callers only ever touch
one prefix) satisfies `ListObjectsV2` but does **nothing** for the 404-vs-403 problem above;
verified by toggling exactly this one variable against a live stack. The only grant that works is
an unconditional `s3:ListBucket` on the bucket resource (not `bucket/*`), and **both** execution
roles need it: the control plane's own `HeadObject`/`GetObject` calls need it directly, and the
verifier's async `verify_and_promote` calls `key_exists` on the canonical key — which does not
exist yet for a genuinely new object — so omitting it from the verifier fails promotion silently
and asynchronously, the same failure shape KMS omission produces (see "KMS" above).
`crates/forklift-aws-lambda/tests/iam_conformance.rs`'s module docs record this as a class of its
own: an IAM action a call site needs for its **error semantics**, not to perform the operation —
invisible to any op→action mapping, because the op genuinely only calls `GetObject`/`HeadObject`.

**What this actually grants, stated plainly:** `s3:ListBucket` on the bucket resource (not
`bucket/*`) lets its holder enumerate every key in the entire bucket via `ListObjectsV2` — both
roles can now do this over `objects/`, `signatures/`, `responses/`, and every session's `staging/`
prefix, not only the one they operate on. For the verifier in particular, this is a real widening:
its job is promoting the one key an S3 event handed it, and it now has bucket-wide enumeration it
has no operational need for beyond the 404-vs-403 semantics above. This is the trade-off this
section's title ("least privilege") is naming, not overriding — it is what least privilege
actually costs once `HeadObject`/`GetObject` correctness on a missing key is in scope.

### KMS — optional, and not in the two files above

Neither JSON file above grants any `kms:*` action, because SSE-S3 (no customer key) is the
default and needs none. If you bring your own customer-managed key, **both** execution roles need
`kms:Decrypt` and `kms:GenerateDataKey`, scoped to that key — not a narrower per-role split,
because both roles turn out to need both actions once you trace the actual calls (a presigned
`PUT`/`GET` and a `CopyObject` on each side). Withholding either role's grant fails in opposite
shapes: the control plane fails loudly and synchronously (every presigned `GET`/offloaded response
`403`s immediately), the verifier fails silently and asynchronously (the staging upload succeeds,
promotion dies, the object never becomes fetchable) — which is exactly the failure mode this
whole deployment's asynchronous verifier step exists to catch, and the harder of the two to debug
blind. If you deploy via `infra/aws-serverless`, setting `kms_key_arn` adds these statements for
you (see that module's `README.md`, "KMS" section, for the full per-operation trace and its
example staging stack); if you deploy by hand, add them yourself alongside the two JSON files.

### Presigned URLs inherit the signing role — not the caller's

Both `presign_get` and `presign_staging_put` sign with the control-plane Lambda's own S3 client
— there is no separate identity involved. This means the URL a client receives is only as
permissive as **the control plane's own execution role**, not the forklift CLI's (which has no
AWS credentials at all — it only ever holds a URL). Concretely: the control-plane role must be
allowed `s3:GetObject` on `objects/*`/`responses/*` and `s3:PutObject` on `staging/*` even though
the control plane itself never touches those bytes directly — every presigned download or
staged upload will `403` at the point the *client* tries to use the URL if that grant is
missing, which can be a confusing failure to debug without knowing this.

## Configuration

Every environment variable either binary reads, via `entrypoint::config_from_env` (both
binaries share it) or the AWS SDK's own default provider chain:

| Variable | Required | Read by | Meaning |
|---|---|---|---|
| `FORKLIFT_S3_BUCKET` | **Required** | Both | The object-plane bucket name. |
| `FORKLIFT_DYNAMODB_TABLE` | **Required** | Both (verifier builds but never calls the resulting client) | The ref/trust table name. |
| `FORKLIFT_WAREHOUSE_ID` | Optional | Control plane | Set (non-empty) → single-warehouse mode, serving fixed `/v1/…`. Unset or empty → multi-warehouse mode, `/warehouses/{id}/v1/…`. An empty string is treated as unset. |
| `FORKLIFT_AWS_ENDPOINT_URL` | Optional | Both | Endpoint override for LocalStack/MinIO; switches the S3 client to path-style addressing. An empty string is treated as unset. |
| `FORKLIFT_DEFAULT_PALLET` | Optional | Control plane | The pallet a franchise checks out by default. Defaults to `main`. An empty string is treated as unset. |
| `AWS_REGION` | Effectively required | The AWS SDK's provider chain (not this crate directly) | Lambda's own execution environment sets this automatically; no operator action needed in the ordinary case. |
| `FORKLIFT_TOKEN` | See below | Control plane only | The bearer token every request must present (`Authorization: Bearer <token>`). |
| `FORKLIFT_OPEN_ACCESS` | See below | Control plane only | Set to `1` to run with no token at all (LocalStack/local dev only). |

**Authentication.** `entrypoint::authenticate` resolves an `AuthConfig` once at cold start
(`auth_from_env`) and applies it to every request:

* **`FORKLIFT_TOKEN`** is the bearer token clients must present as
  `Authorization: Bearer <token>`, compared with `subtle`'s constant-time `ct_eq` — never a
  byte-wise `==`, which would leak timing information about how many leading bytes matched. An
  **empty value is treated exactly like an unset one** (refused), never as a valid empty token —
  the same "empty counts as unset" convention this crate already applies to
  `FORKLIFT_AWS_ENDPOINT_URL`, `FORKLIFT_WAREHOUSE_ID`, and `FORKLIFT_DEFAULT_PALLET`.
* **When `FORKLIFT_TOKEN` is unset (or empty), the head refuses every request with `401`** —
  fail closed by default, so a forgotten token fails loud rather than silently serving the world
  from a public endpoint.
* **`FORKLIFT_OPEN_ACCESS=1`** is the explicit, opt-in escape hatch from that default — every
  request passes untouched, mirroring `forklift-server`'s open-access mode. It only takes effect
  when `FORKLIFT_TOKEN` is unset (a configured token always wins); it exists for LocalStack and
  local development, and a real deployment should never set it.
* The `401`'s message is identical whether the header was missing, malformed, wrong, or no token
  was configured at all — a probing client must not be able to distinguish those cases, the same
  discipline that motivates the constant-time comparison.
* **You — the operator — are responsible for getting `FORKLIFT_TOKEN` into the Lambda's
  environment securely** (AWS Secrets Manager or SSM Parameter Store, wired to the function's
  environment variables by your own IaC/CI pipeline at deploy time). This crate never talks to
  Secrets Manager itself; it only ever reads an already-resolved environment variable.
* This check applies **only to the control-plane Lambda** — the verifier is never invoked by a
  bearer-carrying client (it is triggered by an S3 event), so it has no `AuthConfig` and needs
  none.

An API Gateway authorizer or resource policy (see "Auth at the gateway" above) is a legitimate
**additional** layer in front of this check, never a substitute for setting `FORKLIFT_TOKEN` —
the bearer check is what a client actually speaks, and what `forklift-server` speaks too.

## Operational notes

* **The staging lifecycle rule is not optional.** Repeating it here in an operational frame
  because it is easy to deploy the bucket/table/Lambdas and forget the one rule that is not
  enforced by any code path: without a lifecycle expiration on `staging/*`, an abandoned or
  perpetually-paginating lift session accumulates unswept bytes forever.

* **Run `forklift audit --full` periodically against a franchise as your own bit-rot scrub**
  (`docs/guide/cli.md`). The commit-gate closure check this head runs on every push is
  deliberately *not* a content scrub — an unchanged chunked file's subtree is now pruned by hash
  comparison rather than re-read (the fix that makes routine pushes affordable; see "Timeout
  guidance" above), which means push time no longer incidentally re-verifies untouched large
  files' bytes the way it once did. `forklift-server` has no `audit` subcommand of its own
  either, so a periodic `forklift audit --full` against a clone of this head is the way to catch
  on-disk bit-rot for a serverless deployment too.

* **What to alarm on:**
  * **Verifier Lambda errors or elevated duration.** A staged object that never gets promoted
    means a lift session can never commit its large blobs — the client's `commit_lift` retry
    keeps seeing the `LIFT_SESSION_BLOB_NOT_READY` marker and backs off, which reads as a hung
    lift from the outside. Verifier errors are the earliest signal something is wrong upstream
    of that symptom.
  * **Control-plane 5xx rate.** By design, only a genuine storage-layer failure maps to `500`
    (`error_response` in `entrypoint.rs`) — every protocol-legible failure (a bad hash, a stale
    ref, a missing closure) is a `4xx` the client is meant to read and act on. A `500` is always
    worth paging on.
  * Lambda throttling/concurrency limits on either function, and DynamoDB throttled requests if
    you provisioned capacity instead of on-demand.

* **LocalStack as a pre-deploy check.** `tests/aws_integration.rs` is the same protocol suite run
  against real S3 + DynamoDB APIs; it is gated on `FORKLIFT_AWS_TEST_ENDPOINT` and skips cleanly
  when unset, so it needs no AWS account to exist in CI. CI itself
  (`.github/workflows/aws-integration.yml`) pins `localstack/localstack:3` and runs:

  ```sh
  docker run --rm -d -p 4566:4566 -e SERVICES=s3,dynamodb localstack/localstack:3
  cargo build -p forklift   # the suite drives the real CLI binary; build it first
  FORKLIFT_AWS_TEST_ENDPOINT=http://localhost:4566 \
    AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 \
    cargo test -p forklift-aws-lambda --test aws_integration
  ```

  Run this before every deploy that touches `forklift-aws-lambda` — it exercises the same
  presigned-staging round trip, the chunked-file closure audit, and a full CLI
  lift/franchise/lower cycle that "Verification checklist" below asks you to re-prove against
  the real thing.

* **Version skew.** The protocol version check (`WarehouseInfo.protocol`, exact-string equality)
  only changes when the wire format itself changes — new capabilities are additive fields
  instead, so an old client and a new head (or vice versa) keep working. `chunking: true` is the
  one this head sets today: a chunk-aware client reads it to know it may lift chunked content
  here; an old client simply ignores the field. Expect future capability fields to follow the
  same pattern rather than a protocol-version bump.

* **A residual worth knowing about, even though it is a client/CLI-side concern, not a Lambda
  one:** the whole-object write ceiling (`MAX_OBJECT_BYTES`, 64 MiB) is enforced only on the way
  *in* — a pre-existing, over-ceiling object authored before this policy existed stays fully
  readable, and an old-version bundle can still carry such a "grandfathered giant" through the
  streaming import path, which deliberately does not enforce the ceiling (`forklift-core`'s
  `object_utils.rs`). This head's own promotion path (`verify_and_promote`) is type-blind and
  enforces only the coarse, size-only `MAX_STAGED_OBJECT_BYTES` (5 GiB) backstop regardless — so
  it will happily promote a grandfathered giant if one is ever staged. It is mentioned here only
  so you are not surprised by it; there is no operator action this document can recommend beyond
  awareness.

## Verification checklist

Run this against your **actual deployed infrastructure** (not LocalStack) once it is stood up:

1. `curl https://<your-api>/v1/warehouse` (or the multi-warehouse `/warehouses/{id}/v1/warehouse`
   form) answers `200` with `"chunking": true` in the body.
2. A small `forklift franchise <url> <dir>` round trip: `forklift prepare`, add a couple of
   small files, `load`/`stack`, `config remote.url <url>`, `lift`, then `franchise <url> other-dir`
   and diff the two directories byte-for-byte.
3. A **chunked** round trip: track a file at or above 8 MiB (the chunk threshold), `lift` it, and
   `franchise` it back down — confirms the presigned chunk `GET`s, the recipe descent, and
   `assemble_chunked_file`'s content-hash re-verification all work against your real bucket and
   table, not just LocalStack.
4. After a successful `lift`, list the bucket under `staging/{session}/` for the session that
   just committed — it should be **empty** (the final `commit_lift` batch swept it).
5. If you can drive it easily: stage every object of a lift **except one chunk**, attempt the ref
   update, and confirm it is refused (`422`) rather than silently committing over a missing
   chunk — then upload the withheld chunk and confirm the identical update now succeeds. (See
   `tests/aws_integration.rs`'s `ref_update_refuses_a_missing_chunk_and_commits_once_complete_
   over_s3_and_dynamodb` for the exact shape this proves against LocalStack; the point of running
   it again here is proving it against your real IAM policy and bucket, where a permissions gap
   could otherwise silently change the outcome.)
6. `forklift audit --full` against a fresh franchise of the warehouse comes back clean/green.

## Related documents

* `docs/format/REMOTE_PROTOCOL.md` — the wire protocol both heads implement.
* `docs/SERVER.md` — the self-hostable sibling of this head, same protocol.
* `docs/guide/cli.md` — the `lift`/`lower`/`franchise`/`audit` commands referenced above.
* `docs/format/BUNDLE_FORMAT.md` — the stream format `batch` and `bundles/latest` serve.
