# Loose object format
This format is used to store objects in the objects store.
Objects are compressed using `zstd`.
The object consists of a header and the contents of the object.

## Header structure
Below is the structure of the loose object header (as of version `V2024_09_04`).
Each `[...]` represents a byte or a sequence of bytes.
```
[object_format_version_vlq][object_type_vlq][object_size_vlq][NULL]
```
Where:
- `object_format_version_vlq` is the code of the object format version, stored as a variable-length quantity.
See the list of version codes [here](../codes/LOOSE_OBJECT_FORMAT_VERSION_CODES.md).
- `object_type_vlq` is the code of the object type, stored as a variable-length quantity. See the list of object type
codes [here](../codes/OBJECT_TYPE_CODES.md).
- `object_size_vlq` is the size of the object (in bytes), stored as a variable-length quantity.
The header itself is not included in the size, so this is the size of the object contents.
- `NULL` is a null (zero) byte. It is used to separate the header from the contents.

Following bytes are the contents of the object. Documentation about the format of individual
object types can be found in the [format](.) directory.

## One spelling per object
An object is named by the hash of its bytes, so every value has exactly one encoding, and any other
spelling of it is refused when read and when stored, never accepted under a second name:

- Every variable-length quantity, in the header and in every object body, is in its shortest
  encoding: a multi-byte quantity whose last byte is zero is refused.
- The `NULL` comes right after `object_size_vlq`, and `object_size_vlq` is exactly the number of
  bytes after it. Padding before the `NULL`, or bytes the size does not cover, is refused.
- A tree, compact parcel or recipe body is exactly the bytes its own declared format version writes
  for the value it decodes to: entries out of their written order, a missing or extra terminator,
  or trailing bytes are refused. An object in an older body version stays readable, so what each
  version writes is fixed for as long as that version is read.
