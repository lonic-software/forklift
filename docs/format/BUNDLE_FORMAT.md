# Bundle format — version `2026-07-11`

A bundle is many objects in one file — forklift's packfile answer (DESIGN.html §4.3).
Cloning a large warehouse as one bundle `GET` plus a handful of loose-object `GET`s
replaces 100k+ individual requests. Bundles are a pure optimization: every record is
individually verified on import, a stale bundle just means more loose fetches, and a
missing bundle means none at all.

Version `2026-07-11` adds no new record kind. It marks the writer as one that never emits
a record above the whole-object ceiling (`object_utils::MAX_OBJECT_BYTES`, 64 MiB), so a
reader of a version-`2026-07-11` bundle may refuse an over-ceiling `'O'`/`'D'` record
*before reading a byte of its payload* — the ceiling as policy (see **Object ceiling and
streaming import**). Version `2026-07-06` added **delta records** (`'D'`, §9.1 #1): a blob
stored as its difference from an earlier version, so a large warehouse's transfer moves
each *change* rather than every whole file — Git's biggest transfer win. Readers of the
current version also accept `2026-07-06` and `2026-07-03` bundles; a reader that does
**not** understand a version refuses the whole bundle and falls back to loose objects (the
header contract below), so an older client degrades gracefully.

## Layout

```
forklift-bundle 2026-07-11\n     ← ASCII header line, uncompressed
<one zstd stream to end of file>
```

A reader looks for the header's terminating newline within a small fixed cap
(`bundle_utils::MAX_HEADER_BYTES`, 128 bytes — every header this build recognizes is under 30);
a hostile stream with no newline in that span is refused as "not a bundle" before anything else
is read, so an unbounded search for a newline can never grow the header buffer without limit.

The decompressed stream is a sequence of records:

```
kind      1 byte   'O' = object, 'S' = parcel signature sidecar, 'D' = delta object
hash      64 bytes ASCII hex (the object hash; for 'S', the signed parcel's hash)
length    8 bytes  big-endian u64, byte length of the payload
payload   <length> bytes
```

* `'O'` payloads are raw **uncompressed** object bytes — the reader must verify
  `Blake3(payload) == hash` and drop (error on) mismatching records. A large `'O'` payload
  is streamed to a temp file through an incremental hash, never buffered whole (see
  **Object ceiling and streaming import**).
* `'S'` payloads are signature sidecar bytes (`PARCEL_SIGNATURE_FORMAT.md`), stored
  next to their parcel on import. Their integrity is established at audit time, not by
  the bundle.
* `'D'` payloads carry a blob as a delta against a base object (see **Delta records**).
* Records may appear in any order **except** that a `'D'` record's base must be resolvable
  when the delta is read — the builder emits the base before the delta, and an incremental
  import may already hold it. A signature may precede its parcel.
* Duplicate hashes are legal; later records for an already-stored hash are skipped
  (objects are immutable).

## Delta records (`'D'`)

A `'D'` record's `hash` is the **target** object's hash. Its payload is:

```
base      64 bytes ASCII hex  (the base/dictionary object's hash)
declen    8 bytes  big-endian u64  (the target's decompressed length)
frame     <rest>   a zstd frame compressed with the base object as a dictionary
```

To import a `'D'` record the reader loads the base object's raw bytes, decompresses the
frame against them, then verifies `Blake3(result) == hash` exactly like an `'O'` record
before storing. A wrong base, a corrupt frame, or a missing base therefore **fails the
record**, never corrupts the store; a missing base means the bundle is corrupt (a correct
builder always emits the base first).

`declen` is **not** by itself a decompression-bomb guard: it is a number the sender chooses,
so a hostile `u64::MAX` would bound nothing. The reader therefore refuses any `'D'` record
whose `declen` exceeds **16 MiB** (`delta_utils::MAX_DELTA_TARGET_BYTES`) before it reads a
byte of the frame, decodes as a bounded stream that never pre-allocates `declen`, and
requires the frame to reconstruct to *exactly* `declen` bytes. The ceiling is sound because
no builder ever emits a delta above it: objects larger than 16 MiB are always stored in full
(`'O'`), since deltating huge blobs costs more RAM and CPU than it saves.

The delta is zstd-with-a-dictionary, **not** a bespoke diff format: unchanged regions of the
target are referenced from the base for near-free, and the entropy coding stays zstd's. The
builder only emits a `'D'` when the delta is smaller than the full object and the object is
at or under the 16 MiB ceiling, deltas each blob against the previous version at the same
path, and caps delta chains (a version every ~50 is stored in full) so reconstruction stays
cheap.

## Object ceiling and streaming import

A bundle can arrive from an untrusted remote (a `franchise` downloads one), so a record's
declared length is attacker-controlled. Two independent defenses bound the damage, and it is
worth being precise about which does what — because they are not the same defense.

* **Streaming is the unconditional memory defense.** The importer never buffers a whole large
  object. A record's declared length is never pre-allocated (a `u64::MAX` lie cannot
  capacity-panic), and an `'O'` payload above a small in-memory threshold
  (`object_utils::STREAM_STORE_THRESHOLD_BYTES`) is read in bounded blocks straight through an
  incremental Blake3 hash into a zstd-compressed temp file. The temp file is promoted into the
  store with an atomic rename **only** if the finished hash matches the record's claimed hash;
  any mismatch — a bomb, corruption, or truncation — discards it. Peak memory is one read block
  plus zstd's own window, regardless of the declared length **or the bundle version**. This is
  what actually closes the "expand until memory runs out" hole (DESIGN.html §5.0 row D item 7):
  a zstd frame whose declared length matches its real expansion can no longer exhaust memory,
  because the bytes go to disk incrementally, not to a growing `Vec`.

* **The 64 MiB object ceiling is policy layered on top.** A version-`2026-07-11` bundle is
  written by a build that never emits a record above `object_utils::MAX_OBJECT_BYTES`, so its
  reader refuses an over-ceiling `'O'`/`'D'` record before reading a byte — cheap, and sound
  because no such writer exists. A `'D'` record is capped at the ceiling on **every** version
  (no writer of any version ever emitted a delta near 64 MiB, and a delta targets at most
  16 MiB), so a hostile delta frame is refused before it is read into memory regardless of
  header.

* **The residual, stated honestly.** An **older**-version bundle (`2026-07-06` / `2026-07-03`)
  is deliberately *not* hard-refused on an oversized `'O'` record — it may legitimately carry a
  *grandfathered giant*: a whole object authored before the ceiling existed, which must still
  import so an old store never bricks. Such a record streams in with memory bounded as above,
  but its declared length still bounds how many bytes are written to the temp file before the
  final hash check can fail. So a hostile old-version bundle retains a **disk-fill** exposure —
  bounded by available disk, self-cleaning (a failed import removes the temp file), and strictly
  smaller than the memory exposure it replaces. It is a residual on old-version bundles only,
  not a claim of zero attack surface.

## Grandfathered giants are readable forever, never transportable

A **grandfathered giant** — an object above `MAX_OBJECT_BYTES` that was authored (or, per the
residual above, imported via an old-version bundle) before the ceiling existed — is fully
readable and checkout-able on the warehouse that holds it, forever: the ceiling gates writes and
imports, never reads. But it can **never** be sent to a remote or into a bundle again, on
purpose, and there is no migration command that changes that: a blob's hash is pinned inside a
signed tree, so re-chunking it to shrink it would mint a different hash under a different,
unsigned tree — not a real migration, a silent history fork. Both bundle builders
(`build_bundle` and `build_partial_bundle`, the incremental `objects/batch` counterpart) refuse
loudly, at the source, the moment their walk reaches such an object — before a single byte of
the bundle is written — naming the object (and its path, when the walk knows one) with the
stable `oversized_transport_unsupported` code (`docs/MACHINE_INTERFACE.md`). A lift refuses the
same way, client-side, before the object's bytes ever reach the wire. This is deliberately not a
"ship it anyway and let the reader sort it out" situation: no reader would accept the record
regardless (a version-`2026-07-11` reader refuses its declared length before reading a byte, and
an older reader would only rediscover the same problem after streaming it in), so failing at the
source — loudly, honestly, before anything is written or sent — is strictly better than shipping
something nothing can finish importing.

## Building

The server head's builder (`forklift-server bundle`) walks every pallet head in
oldest-first (topological) order: all reachable parcels, their signature sidecars, and the
full tree/blob closure, each object emitted once. Blobs are delta-encoded against the
previous version at the same path (emitted earlier, so the base is available on import);
parcels and trees are always stored in full. The walk refuses loudly (see **Grandfathered
giants**, above) the moment it reaches an object above the whole-object ceiling, before writing
anything — a warehouse holding one anywhere in reachable history cannot produce a bundle at all
until it is gone from that history. The bundle is written atomically to
`.forklift/bundles/latest` and served at `GET /v1/bundles/latest`. In the hosted service the
same format is built by an ECS job and stored in S3. (The incremental
`POST /v1/objects/batch` bundle is full-only — its objects are requested individually, so a
delta base may not be in the set; it gives the same ceiling refusal for any requested object
above it, rather than silently omitting it the way it omits an absent one.)

## Future extensions (not in this version)

* A trailing index (hash → stream offset) enabling ranged partial fetches.
* Multiple/generation bundles (`latest` + deltas since), so rebuilding is incremental.
* Delta-encoding trees against their parent tree (only blobs are delta'd today).

A reader encountering an unknown header version must refuse the bundle and fall back to
loose objects.
