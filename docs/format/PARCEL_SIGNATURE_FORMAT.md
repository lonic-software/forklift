# Parcel signature format
Once trust is established for a warehouse (see `TRACKED_METADATA.md`), every parcel is
signed. The signature covers the **full parcel hash** — never selected fields — so it
transitively covers the tree (all content), the parents (all history) and every metadata
byte. Signing selected fields would create malleability bugs; this rule is recorded in
the design document (§6, FORK-13 review).

The signature is never inside the parcel: the parcel hash covers the full uncompressed
object (design invariant §4.4), so a signature over that hash cannot be part of the hashed
content. A signature made or received one at a time (local signing, a lift's upload to a server)
is written as a **sidecar file**, named like the parcel's loose object file plus the `.sig` suffix:

```
.forklift/objects/<first-2-hash-chars>/<remaining-hash-chars>.sig
```

`compact` later moves sidecars into **signature tables** in the pack folder (see "Signature
tables" below). Signatures that arrive in bulk go straight into tables: a bundle import writes
its signature section as tables, and a fetch gathers the signatures of its whole history walk and
writes them as tables before the walk returns, so before any ref moves. A fetch that fails partway
writes none; the parcels it stored are not yet behind a ref, so the next fetch walks them again
and fetches their signatures. A read looks in the tables first, then for the sidecar, and returns the same
bytes from either. A parcel's signature never changes once stored: storing a different
signature for a parcel that already has one, in either place, is refused.

## Structure (V1, latest)
Each `[...]` represents a byte or a sequence of bytes.
```
[format_version_vlq][key_id_length_vlq][key_id][signature_length_vlq][signature]
```
Where:
- `format_version_vlq` is the signature format version (`1`), stored as a
variable-length quantity.
- `key_id_length_vlq` is the length of the key id in bytes, stored as a variable-length
quantity.
- `key_id` is the id of the signing key (as ASCII hex bytes): the Blake3 hash of the raw
public key bytes. The public key itself is tracked in the office records (see
`TRACKED_METADATA.md`).
- `signature_length_vlq` is the length of the signature in bytes, stored as a
variable-length quantity (64 for Ed25519).
- `signature` is the Ed25519 signature over the parcel hash's ASCII bytes.

## Signature tables
A signature table holds many signatures in one immutable file, independent of which pack (or
loose file) holds each parcel:

```
.forklift/objects/pack/<id>.sigs
```

All integers are little-endian `u32`. Each `[...]` represents a byte or a sequence of bytes.
```
[magic "FORKSIGT"][version][entry_count][entries_length][header_crc]
[fanout: 256 × ([bucket_end][bucket_crc])]
[records: entry_count × ([parcel_hash: 32 bytes][offset][length][entry_crc])]
[entries: entries_length bytes]
```
Where:
- `version` is the table format version (`1`).
- `header_crc` is the crc32 of the header's first 20 bytes followed by the whole fanout.
- Bucket `b` holds the records whose parcel hash starts with byte `b`. `bucket_end` is the
cumulative record count up to and including the bucket; `bucket_crc` is the crc32 of the
bucket's record bytes.
- Records are sorted by parcel hash, with no hash twice. `offset` and `length` locate the
entry in the entries region; `entry_crc` is the crc32 of the entry bytes.
- Each entry is byte-for-byte a V1 sidecar (above).
- `<id>` is the Blake3 hash of the sorted entries, each as the 32-byte parcel hash, the entry
length as a `u32` and the entry bytes, so two writers publishing the same set publish the same
file under the same name.

A table holds at most 100,000 entries. The file length must be exactly what the header and
records imply. A reader checks the header and fanout on open, a bucket the first time a lookup
reads it, and an entry's crc on every read.

A table that cannot be read never reads as "no signature". While one is unreadable, a lookup
that finds the signature in no other table or sidecar fails and names it, and a full repack
refuses. What to do depends on the cause:
- A table in another format version was written by another build: do not move it aside; use a
build that reads it.
- A corrupt table (a failed crc, a truncated file) can be moved out of the pack folder. The
signatures it alone held are then lost locally: those parcels read as unsigned until they are
restored by franchising again.

`compact` merges every sidecar and every table below the 100,000-entry bound into new tables,
leaving at most one below it, and removes a sidecar or an older table only once a durably
written table holds the same bytes. A sidecar
whose parcel already has a different signature in a table, or one that is malformed, is left
loose and named; the tabled signature is the one reads return. `compact --all` rewrites the
tables and drops each signature whose parcel is no longer in the store.

## Verification
A parcel verifies when its stored signature verifies against the public key of
`key_id` as tracked in the office records. The `audit` command additionally checks the
office chain itself: every office parcel must be signed by a key that was active in the
*previous* office state — introducing a key and signing with it in the same parcel is
only valid for the genesis parcel (that self-signature is the trust-on-first-use anchor).
