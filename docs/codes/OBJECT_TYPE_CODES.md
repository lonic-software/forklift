# Object type codes
Each object type has a specific code.

## Codes
You can find the codes of individual object types in the table below.

| Code | Object type | Description                                       |
|------|-------------|---------------------------------------------------|
| `1`  | `Blob`      | Blob object (file contents).                      |
| `2`  | `Parcel`    | Parcel object (set of changes).                   |
| `3`  | `Tree`      | Tree object (directory contents).                 |
| `4`  | `Recipe`    | Recipe object (chunk index of a chunked file).    |
| `5`  | `Chunk`     | Chunk object (a leaf byte-range of a chunked file). |

Codes are frozen: the object type code is part of the bytes an object hashes to, so a code can
never be reused for a different type. A chunk (`5`) whose raw bytes equal a blob's (`1`) has a
different object hash, so the two never collide or cross-deduplicate.