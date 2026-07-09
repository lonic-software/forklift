# Bundle format — version `2026-07-06`

A bundle is many objects in one file — forklift's packfile answer (DESIGN.html §4.3).
Cloning a large warehouse as one bundle `GET` plus a handful of loose-object `GET`s
replaces 100k+ individual requests. Bundles are a pure optimization: every record is
individually verified on import, a stale bundle just means more loose fetches, and a
missing bundle means none at all.

Version `2026-07-06` added **delta records** (`'D'`, §9.1 #1): a blob stored as its
difference from an earlier version, so a large warehouse's transfer moves each *change*
rather than every whole file — Git's biggest transfer win. Readers of this version also
accept version `2026-07-03` bundles (they simply have no `'D'` records); a reader that
does **not** understand this version refuses the whole bundle and falls back to loose
objects (the header contract below), so an older client degrades gracefully.

## Layout

```
forklift-bundle 2026-07-06\n     ← ASCII header line, uncompressed
<one zstd stream to end of file>
```

The decompressed stream is a sequence of records:

```
kind      1 byte   'O' = object, 'S' = parcel signature sidecar, 'D' = delta object
hash      64 bytes ASCII hex (the object hash; for 'S', the signed parcel's hash)
length    8 bytes  big-endian u64, byte length of the payload
payload   <length> bytes
```

* `'O'` payloads are raw **uncompressed** object bytes — the reader must verify
  `Blake3(payload) == hash` and drop (error on) mismatching records.
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

## Building

The server head's builder (`forklift-server bundle`) walks every pallet head in
oldest-first (topological) order: all reachable parcels, their signature sidecars, and the
full tree/blob closure, each object emitted once. Blobs are delta-encoded against the
previous version at the same path (emitted earlier, so the base is available on import);
parcels and trees are always stored in full. The bundle is written atomically to
`.forklift/bundles/latest` and served at `GET /v1/bundles/latest`. In the hosted service the
same format is built by an ECS job and stored in S3. (The incremental
`POST /v1/objects/batch` bundle is full-only — its objects are requested individually, so a
delta base may not be in the set.)

## Future extensions (not in this version)

* A trailing index (hash → stream offset) enabling ranged partial fetches.
* Multiple/generation bundles (`latest` + deltas since), so rebuilding is incremental.
* Delta-encoding trees against their parent tree (only blobs are delta'd today).

A reader encountering an unknown header version must refuse the bundle and fall back to
loose objects.
