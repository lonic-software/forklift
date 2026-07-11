# Chunk object format
This format is used to store chunks in the object store. A chunk is a leaf byte-range of a large
file stored chunked (see the [recipe object format](RECIPE_OBJECT_FORMAT.md)). Chunks are ordinary
content-addressed objects, so a chunk shared by two files or two revisions is stored once.

## Structure
A chunk object's content is the chunk's **raw bytes**, verbatim — there is no inner format
version. The recipe format version (`RECIPE_FORMAT_V1`) governs the whole chunking scheme,
including how chunks are encoded, so a chunk needs none of its own.
```
[raw_chunk_bytes]
```

The distinct `Chunk` object type (code `5`) in the loose-object header is what keeps a chunk from
ever colliding with a same-bytes blob: the type code is part of the bytes the object hashes to.

## Ceiling
A chunk's raw payload is never larger than `MAX_CHUNK_BYTES` (4 MiB) — the content-defined
chunker's maximum, and an **enforced** ceiling. A `Chunk`-typed object whose payload exceeds it is
refused on both store and read, even though a larger object would otherwise be a legal object.
This bounds the streaming-assembly memory to one chunk at a time regardless of a malicious recipe's
claims.

## Packing
Chunks are **never** packed or delta-compressed: each stays an individually addressable loose
object (a hosted head serves each chunk as its own presigned GET, and loose chunks give O(1)
ranged reads). Recipes, by contrast, are packed and delta-compressed like blobs.
