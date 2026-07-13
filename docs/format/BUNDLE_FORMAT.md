# Bundle format — version `2026-07-13`

A whole-warehouse bundle is a transport envelope around Forklift's native indexed packs. It is a
clone-time optimization, never a source of truth: every incoming object is reconstructed and
content-hash verified in quarantine, and the ordinary history walk fetches anything the bundle did
not contain.

Version `2026-07-13` removes the old clone-time impedance mismatch. Earlier bundles were one outer
zstd record stream; importing one decompressed every object and created one loose file per object.
With durable writes enabled, that meant one file sync and directory sync per object. The current
format carries the native `.pack`/`.idx` pairs directly, so a client verifies once and publishes a
small number of aggregate files. Pack records are already individually zstd-compressed, so there is
no redundant outer compression stream.

## Transport envelope

All integer fields in the envelope are unsigned and big-endian.

```text
forklift-bundle 2026-07-13\n
pack_count       u32
pack table       pack_count × (data_length u64, index_length u64)
pack sections    for each table row: data bytes, then index bytes
signature_count  u64
signatures       signature_count × (parcel_hash[64 ASCII hex], length u64, bytes[length])
EOF
```

The table is separate from the sections so the reader can validate every declared count and length
before it copies a byte into quarantine. The current limits are 4,096 packs, approximately 578 MiB
per data section (the 512 MiB rollover plus one maximum-size object and framing), and 4.8 MiB per
index. A header newline must occur within 128 bytes. Trailing bytes are rejected.

Parcel signature sidecars remain a separate envelope section because they are addressed by the
parcel they sign rather than by their own content hash. They retain their existing sidecar storage
and audit semantics; the native-pack change applies to objects.

## Native pack pair

The data section is byte-for-byte a Forklift pack data file:

```text
magic       8 bytes   FORKPACK
version     u32 LE    currently 2
records     concatenated native records
```

Version-2 native records are:

```text
full:   kind 0x00 | zstd(object bytes)
delta:  kind 0x01 | base hash[32 raw bytes] | target length[VLQ] |
        zstd frame compressed with the base object as its dictionary
```

The accompanying index section is byte-for-byte a Forklift pack index:

```text
magic       8 bytes   FORKPIDX
version     u32 LE    must match the data version
count       u32 LE
entries     count × (object hash[32 raw bytes], offset u64 LE, length u64 LE)
```

Index entries are strictly sorted by hash for binary search. Their record ranges must be non-empty,
in bounds, non-overlapping, and together cover every byte after the data header exactly once. Pack
filenames are derived from the sorted `(hash, offset, length)` layout, the same as packs produced by
local `compact`.

Only blobs may be deltas. The builder considers the previous blob at the same path, keeps a delta
only when it is smaller than the full compressed record, caps delta targets at 16 MiB, and stores a
full version at least every 50 links. Parcels and trees are full records. A delta base may live in
another pack from the same envelope; import verifies the complete quarantined pack set together.

## Import and publication

Import is deliberately two-phase:

1. Copy each exact-length pack and index section into hidden temporary files in the destination's
   real pack directory. Normal pack discovery sees only `.idx`, never these `.tmp` files.
2. Validate both native headers and the complete index structure.
3. Resolve every indexed full or delta record, verify its Blake3 address, enforce the 64 MiB object
   ceiling and the chunk-specific ceiling, and reject duplicate hashes or delta cycles. A bounded
   reconstruction cache accelerates chains without retaining the whole warehouse in memory.
4. Sync the handful of verified aggregate files, then publish data first and index last. The index
   is the reader-visible commit point. Finally sync the pack directory and invalidate the in-process
   pack cache.
5. Import the signature sidecar section and require EOF.

Any failure before pack publication removes the temporary files. A process crash during
publication leaves at worst a valid subset of verified packs: an index is never published before
its data, and the normal history walk fetches anything still missing. Re-import is idempotent; if
all indexed hashes already exist, the incoming duplicate pack is discarded.

This does **not** skip cryptographic work. Like Git's receive/index-pack path, Forklift still parses
the native framing, reconstructs deltas, and hashes every untrusted object. The speedup comes from
avoiding thousands of decompressed loose-file writes and durability barriers, not from trusting the
sender.

## Builder and server behavior

`forklift-server bundle` walks every user and meta pallet head in deterministic oldest-first order,
including all reachable parcels, trees, blobs, and parcel signature sidecars. It builds bounded
native packs in a temporary directory, writes the envelope to a sibling temporary file, syncs it,
then atomically renames it to `.forklift/bundles/latest`. The server exposes that file at
`GET /v1/bundles/latest`.

Chunked-file transport has not shipped. A reachable chunked file therefore refuses a whole bundle
instead of producing an incomplete artifact. Likewise, a grandfathered object above the 64 MiB
whole-object ceiling remains readable locally but cannot be put into a new bundle or sent by the
incremental object path; changing its representation would change signed history.

## Partial bundles and compatibility

The incremental `POST /v1/objects/batch` and subtree responses still use the `2026-07-11` legacy
record stream. Those small responses add selected objects to an existing store and cannot generally
be installed as a self-contained pack, because requested deltas may not have their bases. The same
importer accepts both layouts.

Whole-bundle readers remain backward compatible with:

- `2026-07-11`: one outer zstd stream of `O`/`D`/`S` records with the current object ceiling;
- `2026-07-06`: the first delta-record stream;
- `2026-07-03`: full objects and signature records only.

A current client that sees an unknown future header deletes the downloaded optimization and falls
back to incremental verified fetches. A known-format integrity failure remains fatal. Because the
wire endpoint did not historically negotiate bundle versions, clients older than `2026-07-13` may
need upgrading before cloning from a server that publishes the current envelope.

## Future extensions

- A native signature pack/index, so a warehouse with very many signed parcels also avoids loose
  sidecar durability barriers.
- Generational bundles (`base` plus changes since it), so server rebuilds and repeat syncs are
  incremental.
- Range negotiation over native indexes, allowing a client to fetch selected pack sections.
- Tree deltas where real repositories show enough benefit; today only blobs are delta-compressed.
