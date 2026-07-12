# Recipe object format
This format is used to store recipes in the object store. A recipe is the **chunk index** of a
large file stored chunked: a file at or above the chunk threshold (8 MiB) is stored as one recipe
object plus its chunk objects, and its tree entry (type `NormalChunked`/`ExecutableChunked`)
points at the recipe. The recipe's own object hash is what the signed tree commits.

## Structure
Below is the structure of the recipe object (as of version `V1`, `RECIPE_FORMAT_V1`).
Each `[...]` represents a byte or a sequence of bytes.
```
[recipe_format_version_vlq]
[content_hash]                     64 ASCII-hex bytes
[total_size_vlq]
[chunk_count_vlq]
( [chunk_hash] 64 ASCII-hex bytes [chunk_size_vlq] ) * chunk_count
```
Where:
- `recipe_format_version_vlq` is the code of the recipe object format version, stored as a
  variable-length quantity.
- `content_hash` is the Blake3 hash of the **assembled** (whole-file) bytes, as 64 ASCII-hex
  characters. It is *advisory until assembly*: it is verified only when the file is actually
  materialized (or by a full audit), never trusted at rest. The true content is defined solely by
  the individually content-addressed chunk list, so a wrong `content_hash` can only cause a
  checkout failure on that one file, never substitute bytes.
- `total_size_vlq` is the total assembled file size.
- `chunk_count_vlq` is the number of chunks.
- each chunk entry is its chunk object's `chunk_hash` (64 ASCII-hex characters) followed by the
  chunk's raw byte `chunk_size_vlq`. A chunk's offset in the assembled file is the running prefix
  sum of the sizes before it (derivable, so it is not stored).

## Structural checks at load
The parser enforces these before returning the recipe (cheap, `O(chunk_count)`, zero bytes
fetched):
- every hash is exactly 64 ASCII-hex characters;
- no chunk's declared size exceeds the per-chunk ceiling (`MAX_CHUNK_BYTES`, 4 MiB);
- `sum(chunk_size)` equals `total_size`;
- no bytes trail the last chunk.

A recipe that fails any of these is refused at load. `content_hash` correctness is **not** among
them — only an actual streaming assembly re-derives and checks it.

## Frozen parameters (RECIPE_FORMAT_V1)
The chunk boundaries, and therefore chunk and recipe hashes, are a format freeze tied to this
version: the vendored FastCDC-class gear table and boundary algorithm, and the constants
`CHUNK_THRESHOLD_BYTES` = 8 MiB, min/avg/max chunk = 256 KiB / 1 MiB / 4 MiB. Two clients that
chunk the same bytes must produce byte-identical chunks, or they would fork the signed tree hash
for identical content. A future recipe format version may change any of these; `V1`'s values stay
frozen forever.

## v1 hard limits
Hashes are ASCII-hex (consistent with every other object format), so a recipe object at the
whole-object ceiling (`object_utils::MAX_OBJECT_BYTES`, 64 MiB) lists roughly 987,000 chunks —
about a 964 GiB file at the 1 MiB average chunk size. A single file beyond that is not
representable by one `V1` recipe (hierarchical recipes are deferred).

That ceiling is enforced on the way *in*: a locally authored recipe that would exceed it is
refused on write, and one handed over in a bundle or a lift is refused on import. It gates
writes and imports only — a pre-existing over-ceiling object authored before this policy stays
readable and checkout-able locally, forever, so an old store never bricks — but it can never be
sent to a remote or into a bundle again: `docs/format/BUNDLE_FORMAT.md`'s "Grandfathered giants"
section explains why no migration exists (identity is pinned by the signed tree entry that
points at it) and how transport refuses it honestly, at the source, with the stable
`oversized_transport_unsupported` code.
