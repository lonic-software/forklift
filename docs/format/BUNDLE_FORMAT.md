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
signatures       signature_count × (parcel_hash[64 lowercase ASCII hex], length u64, bytes[length])
EOF
```

The table is separate from the sections so the reader can validate every declared count and length
before it copies a byte into quarantine. The current limits are 4,096 packs, approximately 578 MiB
per data section (the 512 MiB rollover plus one maximum-size object and framing), and 4,832,792
bytes per index (the index of a 100,000-object pack, the object-count rollover). A header newline must occur within 128 bytes. Trailing bytes are rejected.

Parcel signature sidecars remain a separate envelope section because they are addressed by the
parcel they sign rather than by their own content hash. They retain their existing sidecar storage
and audit semantics; the native-pack change applies to objects.

## Native pack pair

The data section is byte-for-byte a Forklift pack data file:

```text
magic       8 bytes   FORKPACK
version     u32 LE    currently 3
records     concatenated native records
```

Native records are:

```text
full:   kind 0x00 | zstd(object bytes)
delta:  kind 0x01 | base hash[32 raw bytes] | target length[VLQ] |
        zstd frame compressed with the base object as its dictionary
```

The accompanying index section is byte-for-byte a Forklift pack index:

```text
magic        8 bytes   FORKPIDX
version      u32 LE    3, and must match the data version
count        u32 LE
fanout_bits  u8        at most 20
reserved     3 bytes   zero
header_crc   u32 LE    crc32 over the 20 bytes above, then the whole fanout table
fanout       2^fanout_bits × (end u32 LE, crc u32 LE)
entries      count × (object hash[32 raw bytes], offset u64 LE, length u64 LE)
```

The file is exactly `24 + 8 × 2^fanout_bits + 48 × count` bytes. Fanout bucket `b` holds the entries
whose hash's top `fanout_bits` bits equal `b`; its `end` is the number of entries in buckets `0..=b`
(so the ends never decrease and the last equals `count`), and its `crc` is the crc32 of those
entries' bytes. Writers pick the smallest `fanout_bits` that keeps the average bucket at 32 entries
or fewer. Readers refuse any other version by name.

Index entries are strictly sorted by hash for binary search. Their record ranges must be non-empty,
in bounds, non-overlapping, and together cover every byte after the data header exactly once. A
local reader checks a bucket's crc, order, prefix and bounds the first time it reads it; import
checks every bucket and the whole tiling before it trusts any entry. Pack
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
5. Import the signature sidecar section and require EOF. Each entry becomes its own sidecar file,
   so it is admitted only when its hash names a parcel present in the store once the packs have
   landed, and only once per bundle. An entry naming a blob, a tree, an absent hash, or a parcel an
   earlier entry already signed rejects the bundle before its bytes are read. (The importer runs
   only against a freshly prepared warehouse, so "present in the store" is "carried by the packs".)

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

## Partial bundles

The incremental `POST /v1/objects/batch` and subtree responses use the `2026-07-11` record stream:
a header line, then one outer zstd stream of `O` records, each `kind[1] | hash[64 ASCII hex] |
length u64 | object bytes`. Those small responses add selected objects to an existing store and
cannot generally be installed as a self-contained pack, because requested deltas may not have their
bases. They are read only from a remote's response, never from a whole-bundle file.

Every record is admitted before a byte of its payload is read, so a remote can make the client
store only what it asked for:

- **Batch.** A record must name a hash the request asked for, and no hash may appear twice. The
  client sends each hash once, and the builder writes a repeated hash once.
- **Subtree.** The client names the subtree's hash from the spine tree it already holds. The first
  admissible record is that tree; each admitted tree makes its files and subtrees admissible,
  loaded from the store even when the record was skipped because the client already held it. No
  hash may appear twice, and at most 10,000 records are admitted, the server's own cap on one
  response.

Any other record kind, an unrequested hash, or a repeat rejects the response.

## Version handling

A whole-bundle file is read only in the `2026-07-13` envelope. Any other header — a future version,
or one of the record-stream versions whole bundles once used (`2026-07-11`, `2026-07-06`,
`2026-07-03`) — is reported as unsupported: the client deletes the downloaded optimization and falls
back to incremental verified fetches. A known-format integrity failure remains fatal.

## Future extensions

- A native signature pack/index, so a warehouse with very many signed parcels also avoids loose
  sidecar durability barriers.
- Generational bundles (`base` plus changes since it), so server rebuilds and repeat syncs are
  incremental.
- Range negotiation over native indexes, allowing a client to fetch selected pack sections.
- Tree deltas where real repositories show enough benefit; today only blobs are delta-compressed.
