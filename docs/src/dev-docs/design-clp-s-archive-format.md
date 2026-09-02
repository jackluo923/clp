# CLP-S archive format 0.5.0

:::{warning}
This is a source-derived specification of the archive written by the C++ `clp-s` implementation.
It is a migration prerequisite for the Rust rewrite, not a proposal for an incompatible new format.
The C++ executable and committed golden archives remain the interoperability oracle.
:::

This document describes both CLP-S archive containers:

* a directory containing a header and one file per logical section; and
* the single-file archive (SFA), which concatenates the same logical sections.

The source baseline used for this audit is commit
`7636e26a98a9f78f18964af4d8d951b95602f712`, whose current writer emits archive version 0.5.0.
The format is private to `clp-s`; this document does not describe the `clp` streaming archive or
KV-IR stream formats.

## Status labels and notation

The following labels distinguish evidence from decisions that the rewrite still needs to make:

* **Confirmed** means the current C++ writer and reader directly establish the behavior. Unless a
  narrower version is named, it describes archives emitted as version 0.5.0.
* **Effective ABI** means the behavior follows from raw C++ object representation on the supported
  little-endian manylinux targets. It is real compatibility behavior, but it was not originally
  designed as a portable wire encoding.
* **Rust validation requirement** means the new reader must validate the condition before indexing,
  allocating, seeking, or exposing data. The C++ reader may currently perform a weaker check.
* **Unresolved** identifies a fact that cannot be made normative from the current source alone. It
  needs a golden archive, an older writer, or an explicit compatibility decision.

Integers named `u8`, `u16le`, `u32le`, `i32le`, `u64le`, `i64le`, and `f64le` have the indicated
width. Except inside MessagePack payloads, every multibyte number is written by copying its native
C++ representation. The supported build rejects big-endian targets, so the effective wire order is
little-endian. `f64le` is the raw 64-bit representation of the target's C++ `double`; the supported
targets use IEEE 754 binary64.

`bytes[n]` means exactly `n` uninterpreted bytes. A `zstd(...)` block is one standard zstd frame
unless stated otherwise. Sizes inside the decompressed form describe decompressed bytes, not zstd
frame sizes. The current writer uses zstd's streaming API and sets the selected compression level;
it supplies no zstd dictionary or CLP-S-specific inner framing. Exact compressed bytes may therefore
change with zstd versions and settings without changing the archive's semantics.

## Container overview

### Logical sections

**Confirmed:** Both containers represent the following logical sections. The leading slash is part
of each name in archive metadata; it is used as a path separator when constructing directory paths.

| Canonical order | Metadata name | Directory filename | Contents |
| ---: | --- | --- | --- |
| header | `/header` | `header` | 64-byte archive header followed by compressed archive metadata |
| 0 | `/schema_tree` | `schema_tree` | schema-tree nodes |
| 1 | `/schema_ids` | `schema_ids` | schema IDs and flattened schemas |
| 2 | `/table_metadata` | `table_metadata` | packed-stream and schema-table metadata |
| 3 | `/var.dict` | `var.dict` | variable dictionary |
| 4 | `/log.dict` | `log.dict` | CLP string logtype dictionary |
| 5 | `/array.dict` | `array.dict` | unstructured-array logtype dictionary |
| 6 | `/0` | `0` | concatenated zstd frames containing schema tables |

The header is not included in the archive file-information list. That list contains the other seven
sections in the order shown.

### Directory archive

**Confirmed:** A directory archive has this physical layout:

```text
<archive-id>/
    header
    schema_tree
    schema_ids
    table_metadata
    var.dict
    log.dict
    array.dict
    0
```

`header` contains the same 64-byte header and zstd metadata frame that begin an SFA. The section
offsets in its `ArchiveFileInfo` packet describe the hypothetical concatenation order above, but the
current directory reader opens each named file and does not use those offsets.

For a directory archive, `ArchiveHeader.compressed_size` is the sum of all eight physical file
sizes. It is not the size of the `header` file.

### Single-file archive

**Confirmed:** An SFA is laid out as follows:

```text
0                         64-byte ArchiveHeader
64                        zstd archive-metadata frame
64 + metadata_size        files_base: /schema_tree begins here
files_base + files[1].o   /schema_ids
files_base + files[2].o   /table_metadata
files_base + files[3].o   /var.dict
files_base + files[4].o   /log.dict
files_base + files[5].o   /array.dict
files_base + files[6].o   /0
compressed_size           logical end of archive
```

For entry `i`, `ArchiveFileInfo.o` is a `u64` offset relative to `files_base`. The first offset is
zero. The compressed size of a non-final section is `files[i + 1].o - files[i].o`; the final section
ends at `ArchiveHeader.compressed_size`. Section names, list order, physical order, and offsets must
therefore agree. The current streaming reader also requires callers to check sections out in
physical order because it refuses a backward seek.

`ArchiveHeader.compressed_size` equals the full SFA size at the time it is written. There is no
archive footer.

## The 64-byte archive header

**Effective ABI:** The writer serializes `ArchiveHeader` with one raw `write` of the C++ struct. On
the supported ABI its exact layout is:

| Offset | Size | Field | Version 0.5.0 meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic_number` | bytes `fd 2f c5 30` |
| 4 | 4 | `version` | `u32le`, currently `0x0005_0000` |
| 8 | 8 | `uncompressed_size` | `u64le` producer statistic for raw input bytes ingested |
| 16 | 8 | `compressed_size` | `u64le` total physical/logical archive bytes |
| 24 | 32 | `reserved_padding[4]` | four `u64le`, all zero in canonical output |
| 56 | 4 | `metadata_section_size` | `u32le` compressed metadata-frame length |
| 60 | 2 | `compression_type` | `u16le`; `0` means zstd |
| 62 | 2 | `padding` | `u16le`, zero in canonical output |

The packed version has major in bits 31..24, minor in bits 23..16, and patch in bits 15..0. Thus
0.5.0 is integer `0x0005_0000`, represented in the header as bytes `00 00 05 00`.

`uncompressed_size` is not the sum of decompressed archive sections and is not a safe allocation
bound. It is maintained by the ingestion layer from consumed input bytes. `compressed_size` includes
the header and metadata as well as all seven logical data sections.

The only defined compression value is:

| Value | Meaning |
| ---: | --- |
| 0 | zstd |

**Confirmed current-reader behavior:** The C++ reader checks the magic and compression value. It
does not reject an unknown archive version, validate the reserved fields, compare
`compressed_size` with the physical source length, or prove that `64 + metadata_section_size` is in
bounds before using it as a section boundary.

**Rust validation requirement:** Decode individual fields as little-endian rather than
transmuting a Rust struct. Check all additions and conversions, enforce configured input and
metadata limits, validate the supported-version policy, and bound both metadata and data sections
by the actual source length. Reserved and padding fields must be zero for new 0.5.0 output; whether
nonzero fields are accepted on input remains a version-policy decision.

## Archive metadata frame

**Confirmed:** The bytes immediately after the header form one zstd frame. Its compressed byte
length is `metadata_section_size`. Its decompressed stream is:

```text
u8 packet_count
repeat packet_count times:
    u8    packet_type
    u32le payload_size
    bytes[payload_size] payload
```

`payload_size` is the decompressed payload size. The canonical writer emits three required packets
in the order `ArchiveInfo`, `ArchiveFileInfo`, `TimestampDictionary`, followed by `RangeIndex` when
the range index is nonempty.

| Packet type | Value | Encoding |
| --- | ---: | --- |
| `ArchiveInfo` | 0 | MessagePack map |
| `ArchiveFileInfo` | 1 | MessagePack map |
| `TimestampDictionary` | 2 | CLP-S raw numeric/string encoding |
| `RangeIndex` | 3 | MessagePack |

Unknown packet types are length-skipped by the C++ reader, which is the intended forward-extension
mechanism.

### ArchiveInfo packet

**Confirmed:** msgpack-c's map adaptor serializes this semantic shape:

```text
{"num_segments": 1}
```

`num_segments` is an unsigned 64-bit C++ value represented using MessagePack's integer encoding.
The current reader supports exactly one segment and rejects any other value. There are no segment
records elsewhere in the CLP-S archive.

### ArchiveFileInfo packet

**Confirmed:** The writer serializes:

```text
{
  "files": [
    {"n": "/schema_tree",   "o": 0},
    {"n": "/schema_ids",    "o": <offset>},
    {"n": "/table_metadata", "o": <offset>},
    {"n": "/var.dict",      "o": <offset>},
    {"n": "/log.dict",      "o": <offset>},
    {"n": "/array.dict",    "o": <offset>},
    {"n": "/0",             "o": <offset>}
  ]
}
```

The values of `o` are unsigned 64-bit offsets in the logical files concatenation. Sizes are not
stored directly. MessagePack maps are semantically unordered; a compatible writer must preserve
the keys and values, not msgpack-c's exact choice of integer width or map byte order.

**Rust validation requirement:** Require a map with a `files` array; unique supported names; the
required section set; first offset zero; monotonic, in-range offsets; and a physical/list order that
is consistent with the derived section bounds. Do not reproduce the C++ reader's current shortcut
of consuming the first map value without checking that its key is `files`.

### Packet presence and framing

**Confirmed current-reader behavior:** Unknown packets are skipped, but the reader does not enforce
exactly one of each required packet or reject duplicates. The timestamp-dictionary reader is passed
its packet size but does not use it to bound or verify the timestamp payload. Several packet parsers
also accept trailing bytes inside a declared payload.

**Rust validation requirement:** Parse every packet through a bounded view of exactly
`payload_size`; require the three mandatory packets exactly once; allow at most one range index;
reject truncated or overlong known payloads; cap packet counts and sizes; and skip unknown packets
without allocating their entire payload.

## Timestamp dictionary packet

The timestamp dictionary is packet type 2. It contains archive-level timestamp ranges followed by
the raw patterns needed to reconstruct each timestamp value.

### Range entries

**Confirmed:** The first part is:

```text
u64le range_entry_count
repeat range_entry_count times:
    u64le key_length
    bytes[key_length] key
    u64le column_id_count
    i32le[column_id_count] column_ids
    u64le timestamp_encoding
    if timestamp_encoding == 1:
        i64le epoch_start
        i64le epoch_end
    else if timestamp_encoding == 2:
        f64le epoch_start
        f64le epoch_end
```

Timestamp encoding values are:

| Value | Name | Bounds payload |
| ---: | --- | --- |
| 0 | unknown | none |
| 1 | epoch | signed integer start and end |
| 2 | double epoch | binary64 start and end |

For current `Timestamp` columns, parsed values are stored in epoch nanoseconds in the table, while
the range entry stores outward-rounded millisecond bounds. A positive sub-millisecond remainder
rounds the upper bound up; a negative remainder rounds the lower bound down. The key is the
authoritative timestamp column descriptor, and the ID set identifies corresponding schema-tree
nodes.

The current writer only ingests one authoritative timestamp column and emits encoding 1. Encoding
2 and multiple range entries survive for compatibility. The current reader treats the first range
entry as authoritative.

### Pattern entries

**Confirmed:** The range entries are followed immediately by:

```text
u64le pattern_count
repeat pattern_count times:
    u64le pattern_id
    u64le pattern_length
    bytes[pattern_length] raw_pattern
```

Pattern IDs are explicit and need not equal serialization position. Both quoted-string and numeric
patterns share one ID space. The raw pattern uses the timestamp parser language documented in
`timestamp_parser/TimestampParser.hpp`; current patterns use backslash format specifiers and may
include the surrounding JSON quotes when the source timestamp was a JSON string literal.

Each value in a current `Timestamp` table column carries a `u64le pattern_id`. Decoding applies the
referenced pattern to the epoch-nanosecond value and produces a JSON literal.

**Unresolved:** Archives before 0.5.0 are interpreted with the older percent-directive
`clp_s::TimestampPattern` implementation. The current source does not provide old archive fixtures
that establish every historical unit, timestamp encoding, pattern dialect, and version boundary.
Deprecated timestamp support must not be declared complete until such fixtures exist.

## Range-index packet

**Confirmed:** Packet type 3 is the MessagePack representation of this semantic JSON shape:

```text
[
  {"s": <start>, "e": <end>, "f": {<metadata fields>}},
  ...
]
```

Each entry describes the half-open archive-local log-event range `[s, e)`. Empty ranges (`s == e`)
are legal. Canonical writers keep ranges in non-overlapping, monotonically increasing order. Values
inside `f` retain the JSON/MessagePack type supplied by the ingestion layer.

With default log-order recording, JSON and KV-IR inputs add:

| Field | Meaning |
| --- | --- |
| `_filename` | canonical input or archive-member filename |
| `_file_split_number` | zero-based split number for that input |
| `_archive_creator_id` | UUID shared by the parser invocation |

KV-IR user-defined metadata is added to the same `f` object. These fields are only populated by the
current ingestion path when log-order recording is enabled.

The C++ reader checks that the payload is an array, every entry has integer `s` and `e`, `f` is an
object, and `s <= e`. It does not fully enforce order, non-overlap, range bounds, unique keys, or
coverage of the archive's log-event index domain.

**Rust validation requirement:** Enforce configured nesting and payload limits, nonnegative
indices representable by the implementation, monotonic non-overlapping ranges, and bounds against
the archive's record/log-event domain once table metadata is known. Keep MessagePack maps semantic;
do not require byte-identical key order or integer-width choices from nlohmann JSON.

## Schema tree section

**Confirmed:** `/schema_tree` is one zstd frame containing:

```text
u64le node_count
repeat node_count times:
    i32le parent_node_id
    u64le key_length
    bytes[key_length] key
    u8 node_type
```

A node's ID is its zero-based position in this stream; no node ID is serialized in the node record.
There is no serialized synthetic root. Top-level namespace/subtree roots use parent ID `-1`. Empty
keys are valid and are used for the default object and metadata namespaces and for array elements.
Other parents refer to an earlier node in canonical output.

The key is length-delimited bytes with no encoding tag. Canonical JSON ingestion supplies decoded
UTF-8 keys. How a hardened reader exposes an invalid-UTF-8 key from a corrupt archive remains a
library API decision; it must not read beyond the declared length.

### Stable NodeType values

**Confirmed:** `NodeType` is stored as one byte. Its existing numeric assignments are declared
append-only in the C++ source:

| Value | Name | Table representation |
| ---: | --- | --- |
| 0 | `Integer` | `N` raw `i64le` values |
| 1 | `Float` | `N` raw `f64le` values |
| 2 | `ClpString` | CLP logtype descriptors and encoded variables |
| 3 | `VarString` | `N` variable-dictionary IDs |
| 4 | `Boolean` | `N` bytes |
| 5 | `Object` | structural only |
| 6 | `UnstructuredArray` | CLP encoding using `array.dict` |
| 7 | `NullValue` | structural only |
| 8 | `DeprecatedDateString` | legacy timestamp arrays; reader-only in 0.5.0 |
| 9 | `StructuredArray` | structural only |
| 10 | `Metadata` | structural namespace only |
| 11 | `DeltaInteger` | `N` delta-encoded `i64le` values |
| 12 | `FormattedFloat` | binary64 values plus 16-bit format descriptors |
| 13 | `DictionaryFloat` | `N` variable-dictionary IDs |
| 14 | `Timestamp` | delta epoch nanoseconds plus pattern IDs |
| 255 | `Unknown` | sentinel; never valid canonical archive data |

The default JSON subtree is normally an `Object` node with parent `-1` and key `""`. The metadata
subtree is a `Metadata` node with parent `-1` and key `""`. KV-IR may add other namespace roots such
as `@`. The `$` range-index namespace is virtual search metadata and is not a schema-tree subtree.

**Rust validation requirement:** Limit node/key counts and total bytes; reject unsupported node
types; require every non-root parent to exist and precede the child; reject invalid parent cycles by
construction; and reject duplicate `(parent, key, type)` nodes. The C++ reader reconstructs the tree
through a deduplicating API, so accepting duplicates would make later implicit IDs ambiguous.

## Schema map section and delimiter packing

**Confirmed:** `/schema_ids` is one zstd frame containing:

```text
u64le schema_count
repeat schema_count times:
    i32le schema_id
    u32le schema_entry_count
    u32le ordered_entry_count
    i32le[schema_entry_count] entries
```

Schema IDs are opaque signed 32-bit values. Do not assume they are serialized in numeric order,
dense, or zero-based. In particular, the current C++ `SchemaMap::clear` preserves its next-ID
counter when an ingestion writer splits archives, so later archives from one invocation can begin
with a nonzero schema ID. The canonical writer serializes this section in lexicographic order of the
flattened signed-32-bit schema-entry vectors because those vectors are keys in a `std::map`; this is
independent of both schema-ID order and table physical order.

The first `ordered_entry_count` entries form the ordered region. Remaining entries form the
unordered region used for structured arrays/objects and may repeat schema-tree node IDs.

Most entries are nonnegative schema-tree node IDs. An unordered-object delimiter instead packs a
node type and flattened body length into the same 32 bits:

```text
bits 31..24  NodeType
bits 23..0   number of immediately following schema entries in this object's flattened body
```

Equivalently, the writer begins a delimiter with `NodeType << 24` and ORs the body length into its
low 24 bits when the object closes. Canonical delimiters describe `Object` or `StructuredArray`
bodies. Nested delimiters count as entries within an outer flattened body. A schema entry is treated
as a delimiter whenever any high-byte bit is set.

For structured-array writing, every non-empty array has a `StructuredArray` delimiter and every
non-empty object that is a direct array element has an `Object` delimiter. Named objects nested
inside such an element do not add another delimiter: their schema-tree ancestry identifies their
shape and their leaves remain in the enclosing element body. Any empty array or object instead
contributes its bare structural node ID. Scalar and null array elements use empty-key children of
the array node. Repeated values or object shapes repeat the same node ID in the unordered region
and correspond to separate physical columns in encounter order.

This representation has two implicit limits that the C++ writer does not enforce:

* a normal schema-tree node ID must be below `0x0100_0000`, or it aliases a delimiter; and
* a delimiter body must fit in 24 bits, or its length corrupts the encoded type.

**Rust validation requirement:** Require `ordered_entry_count <= schema_entry_count`; unique schema
IDs; normal node IDs in the schema tree; delimiter types permitted by the format; body lengths
contained in the remaining schema; well-nested flattened bodies; and explicit enforcement of both
24-bit limits when writing. All count/byte-length multiplication must be checked before allocation.

## Table metadata and packed streams

### `/table_metadata`

**Confirmed:** This section is one zstd frame. Its decompressed form has two consecutive parts:

```text
# Packed-stream metadata
u64le packed_stream_count
repeat packed_stream_count times:
    u64le file_offset
    u64le uncompressed_size

# Separate-column streams (zero entries in default output)
u64le separate_column_stream_count
repeat separate_column_stream_count times:
    u64le stream_id
    u64le column_count
    repeat column_count times:
        u64le uncompressed_size
        u64le compressed_size

# Schema-table metadata
u64le schema_table_count
repeat schema_table_count times:
    u64le stream_id
    u64le stream_offset
    i32le schema_id
    u64le num_messages
```

`file_offset` is relative to the beginning of `/0` and points to the logical stream start (the zstd
frame start for a nonempty stream). `stream_id` is a zero-based index into the packed-stream
metadata. `stream_offset` is a byte offset into that stream's decompressed contents.

There is no serialized table length. For a table followed by another table in the same stream, its
length is the difference between their stream offsets. The last table in a stream ends at that
stream's advertised `uncompressed_size`.

Canonical table metadata is in physical decompression order: entries are grouped by increasing
stream ID and have increasing offsets within a stream. The list also defines the order in which the
current reader exposes schema IDs so it can consume an SFA without seeking backwards. The writer
sorts schema tables by decreasing uncompressed table size before packing them. It closes the current
stream after its accumulated size becomes strictly greater than the configured minimum table size,
or after the final table.

The separate-column section is empty by default, which reproduces the C++ layout byte for byte.
With `--separate-columns-min-size N`, the Rust writer stores every schema table of at least `N`
uncompressed bytes (and more than one column) in its own packed stream, written as one zstd frame
per value-bearing column in flattened schema-entry order, empty columns included as zero-length
frames. The section lists those streams by increasing `stream_id` with the size of each frame, so a
reader can seek to and inflate only the columns a query needs. The frames of a stream are
contiguous and, taken in order, decompress to exactly the shared-frame bytes, so a reader that
ignores the section can still inflate the stream frame by frame. The reader requires each listed
stream to hold exactly one schema table at offset zero and the frame sizes to sum to the stream's
compressed and uncompressed extents. The pinned C++ reader rejects any nonzero count. The reader
rejects `schema_table_count == 0` whenever streams exist.

**Confirmed empty-input quirk:** Compressing `/dev/null` with the pinned C++ CLI succeeds. With
default log-order metadata it emits a 402-byte SFA; with `--disable-log-order` it emits 280 bytes.
Both have table metadata consisting of exactly three zero `u64le` counts and an empty `/0`. The
first archive additionally contains the two log-order schema-tree nodes and a zero-length source
range with filename, split number, and creator metadata. The Rust canonical empty/no-log-order
writer output is byte-identical to the 280-byte C++ archive (SHA-256
`772eadf924b47e7e4f37f682db65ab60b6b9766bc54ca897ad5b81392473ce22`).

The same pinned C++ `clp-s x` rejects both variants at `ArchiveReader.cpp:148` because it treats
zero schemas as unsupported. The Rust reader accepts only the coherent all-empty tuple: empty
schema map, zero packed streams, zero separate-column schemas, zero schema tables, and a zero-byte
`/0`. Any mixture of empty metadata and nonempty schemas, streams, tables, or `/0` remains corrupt.

### `/0`

**Confirmed:** `/0` is a direct concatenation of the independent zstd frames for nonempty packed
streams. It has no section-level count or header. Each frame decompresses to exactly its advertised
`uncompressed_size` and contains one or more schema tables concatenated without separators. A
logical packed stream whose tables have no value-bearing columns has uncompressed size zero; the
current compressor emits no zstd frame for it, so that stream occupies zero physical bytes.

Within a decompressed stream, table `T` begins at its `stream_offset`. It has no internal schema ID,
record count, or column count: all three are obtained from `/table_metadata`, `/schema_ids`, and
`/schema_tree`.

**Rust validation requirement:** Require stream file offsets to start at zero and be monotonic
within the `/0` section; stream IDs to be in range and physically ordered; table offsets to be
ordered and within advertised stream sizes; schema IDs to be unique and present in the schema map;
and inferred table sizes to be nonnegative. Bound zstd output to the advertised size and a
caller-configured limit, require exactly that many decompressed bytes, and reject unexpected
decompressed trailing data. Treat a zero-size logical stream explicitly rather than asking a zstd
decoder to parse an absent frame. Do not allocate from an unchecked archive-provided count or size.

## Schema-table column encoding

**Confirmed:** A schema table is the concatenation of its value-bearing columns. Column order is
the flattened schema-entry order after delimiter entries and structural-only nodes are removed.
Repeated node IDs in the unordered region represent repeated columns. `num_messages` (`N`) from
table metadata supplies the length of every fixed-width column.

There are no per-column type tags or byte lengths in the table. The reader must use the schema tree
and schema map to select each encoding and must consume exactly the inferred table length.

### Fixed-width columns

| Node type | Bytes in table | Interpretation |
| --- | --- | --- |
| `Integer` | `i64le[N]` | signed values |
| `DeltaInteger` | `i64le[N]` | first value minus zero, then value minus previous value |
| `Float` | `f64le[N]` | raw C++ `double` |
| `FormattedFloat` | `f64le[N]`, then `u16le[N]` | numeric values followed by lexeme descriptors |
| `DictionaryFloat` | `u64le[N]` | IDs in `var.dict`; dictionary bytes are the original number lexemes |
| `Boolean` | `u8[N]` | writer emits 0 or 1; C++ reader treats any nonzero byte as true |
| `VarString` | `u64le[N]` | IDs in `var.dict` |
| `DeprecatedDateString` | `i64le[N]`, then `i64le[N]` | legacy epoch values and legacy pattern IDs |
| `Timestamp` | `i64le[N]`, then `u64le[N]` | delta epoch nanoseconds and current pattern IDs |

`Object`, `StructuredArray`, `NullValue`, and `Metadata` have no bytes of their own. An
`UnstructuredArray` uses the variable-width CLP string layout below with `array.dict` instead of
`log.dict`; the decoded text is already a JSON array and is emitted without surrounding quotes.

For delta encoding, let the stored values be `d[0..N)`. Decoding is `v[0] = d[0]` and
`v[i] = v[i - 1] + d[i]`. A delta column is local to one schema table. Consequently the metadata
`log_event_idx` column can reconstruct globally increasing archive indices even though records of a
schema are stored together.

### Formatted-float descriptor

The `FormattedFloat` `u16le` descriptor has this confirmed layout:

```text
bits 15..14  notation: 00 decimal, 01 lowercase e, 11 uppercase E
bits 13..12  exponent sign: 00 absent, 01 plus, 10 minus
bits 11..10  exponent digit count minus 1 (1..4; relevant in scientific notation)
bits  9..5   significant digit count minus 1 (1..17)
bits  4..0   unused; zero in canonical output
```

Bit patterns `10` for notation and `11` for exponent sign are not emitted. When a JSON numeric
lexeme cannot be represented and exactly restored by this descriptor plus binary64 value, the
writer uses `DictionaryFloat` instead.

### Current Timestamp column

A `Timestamp` column first stores `N` signed deltas of epoch-nanosecond values, then `N` unsigned
pattern IDs. The pattern ID must exist in the timestamp dictionary. The timestamp dictionary's
search range is deliberately coarser: it stores outward-rounded millisecond bounds.

**Unresolved:** `DeprecatedDateString` is never emitted by the 0.5.0 writer. Its two-array layout is
confirmed from the current reader, but the exact epoch units and all legacy pattern-ID semantics
must be established from pre-0.5.0 fixtures.

## Dictionary sections

### Common framing

**Confirmed:** All three dictionary sections have an uncompressed eight-byte prefix followed by a
zstd frame:

```text
u64le entry_count                    # outside zstd
zstd(
    repeat entry_count times:
        u64le value_length
        bytes[value_length] value
)
```

When `entry_count == 0`, the current compressor writes no zstd frame, so the canonical section may
consist of only the eight-byte count. Entry IDs are implicit zero-based positions and are not stored
in entry records.

`var.dict` stores arbitrary variable strings and fallback numeric lexemes. `log.dict` and
`array.dict` store escaped CLP logtypes. The latter two share the same physical entry format but are
separate ID spaces.

### CLP logtype bytes and placeholders

**Confirmed:** A logtype is a byte string containing constants and one-byte placeholders:

| Byte | Meaning |
| ---: | --- |
| `0x11` | signed integer encoded variable |
| `0x12` | variable-dictionary ID encoded variable |
| `0x13` | custom encoded floating-point variable |
| `0x5c` (`\\`) | escape the following logtype byte as a literal |

There is one `i64le` encoded variable for each unescaped `0x11`, `0x12`, or `0x13`, in placeholder
order. Escaped placeholders do not consume an encoded variable.

The interpretation of the same 64 raw bits depends on the placeholder:

* Integer: ordinary signed two's-complement `i64`.
* Dictionary: bit-cast to `u64`, then use as an ID in `var.dict`.
* Float: the custom layout below.

The eight-byte encoded-float layout is:

```text
bit  63      negative flag
bit  62      unused
bits 61..8   54-bit integer containing all decimal digits with the point removed
bits  7..4   decimal digit count minus 1 (1..16)
bits  3..0   decimal-point position from the right minus 1 (1..16)
```

This encoded-variable float is distinct from a JSON `FormattedFloat` column descriptor.

### CLP string column

**Confirmed:** For a `ClpString` or `UnstructuredArray` column with `N` records:

```text
u64le[N] logtype_descriptors
u64le total_encoded_variable_count
i64le[total_encoded_variable_count] encoded_variables
```

Each descriptor packs:

```text
bits 23..0   logtype dictionary ID
bits 63..24  starting index in the column's encoded_variables array
```

The number of variables for a record is derived from the referenced logtype's number of unescaped
placeholders. There is no per-record variable count. `ClpString` uses `log.dict`;
`UnstructuredArray` uses `array.dict`.

The descriptor creates two more implicit limits not currently enforced by the C++ writer: logtype
IDs must fit in 24 bits and encoded-variable offsets must fit in 40 bits. The writer combines the
values with a shift and OR; the reader masks them, so overflow would silently alias valid data.

**Rust validation requirement:** Cap dictionary counts and entry lengths; verify entry IDs and every
column dictionary reference; validate logtype escaping and placeholder counts; ensure every CLP
descriptor's ID and offset are in range; check `offset + variable_count`; and enforce the 24/40-bit
limits before writing. Reject malformed Boolean values, invalid current timestamp pattern IDs,
invalid formatted-float flags, and arithmetic overflow while reconstructing deltas.

## Log-order representation

**Confirmed:** Log order is represented jointly in three places when enabled:

1. The schema tree contains a default-namespace `Metadata` subtree with a child named
   `log_event_idx` of type `DeltaInteger`.
2. Every record's schema includes that child in the ordered region, and the table stores the
   archive-local append index in its delta column.
3. The range index maps half-open index ranges back to source filename, split number, archive
   creator ID, and optional KV-IR metadata.

The append index starts at zero and increments once per record. It resets when a new split archive
is opened. Because tables are grouped by schema rather than record arrival, ordered extraction reads
the per-schema delta columns and merges their next `log_event_idx` values.

When log-order recording is disabled, the current writer omits the metadata column and does not
construct the source range index. Timestamp dictionary ranges are independent of this setting.

The archive's own ID is not stored in `ArchiveInfo`; readers derive it from the archive path or
receive it separately. `_archive_creator_id` in range metadata is a different UUID shared across
inputs handled by one parser invocation.

## Canonical writer invariants

The following are **confirmed properties of valid current output**, even where the C++ reader does
not fully enforce them:

* The header is exactly 64 bytes, magic/version/compression have the values above, and reserved
  fields are zero.
* The metadata frame is bounded by `metadata_section_size` and contains one each of archive info,
  file info, and timestamp dictionary, plus zero or one range index.
* `num_segments` is one.
* The seven file entries are unique, complete, in physical order, begin at offset zero, and have
  monotonically increasing offsets.
* Each non-dictionary metadata/schema section is a complete zstd frame; each dictionary begins with
  its raw count; `/0` is a concatenation of independently bounded frames for nonempty streams, while
  a zero-size logical stream may occupy no bytes.
* Schema-tree IDs are implicit stream positions, parents precede children, and enum values are the
  stable values above.
* Schema-table records point to existing schemas and streams; tables are ordered by physical stream
  position; every table created by normal ingestion has at least one message.
* A decompressed table is consumed exactly by the columns inferred from its schema and message
  count.
* Dictionary IDs and timestamp pattern IDs resolve, CLP variable spans remain in bounds, and
  range-index coordinates refer to archive-local append indices.

A Rust writer may choose different schema/table ordering, zstd frame bytes, MessagePack integer
widths, dictionary iteration order, or other non-semantic details only when both readers accept the
result and all represented records, metadata, timestamps, ordering, and query behavior agree.

## Required reader validation checklist

This list is normative for the Rust implementation even when it is stricter than the C++ reader:

1. Apply caller-configurable limits to physical archive bytes, metadata bytes, zstd output, packet
   count, sections, nodes, schemas, tables, messages, dictionaries, string lengths, and nesting.
2. Decode raw values explicitly as little-endian with checked conversions and checked
   addition/multiplication. Never transmute the header or cast an archive count directly to
   `usize`.
3. Validate magic, compression, supported version, header/metadata boundaries, physical length,
   section set/order/offsets, and zstd frame termination.
4. Parse each metadata packet through its declared bounded payload and enforce required/unique
   packet rules while retaining bounded skipping of unknown types.
5. Validate the schema tree before resolving implicit IDs, then validate every schema entry and
   delimiter against it.
6. Validate packed-stream metadata as one coherent physical layout before opening `/0`; validate
   every table range, schema reference, message-count-derived byte count, and exact table
   consumption.
7. Validate all dictionary and pattern references, CLP descriptor packing, variable spans,
   Boolean/float encodings, and delta arithmetic.
8. Validate range-index shape, types, order, non-overlap, and final bounds after the record domain is
   known.
9. Return a structured corruption/unsupported/resource-limit error with archive section and offset
   context. Do not panic, log, allocate without a limit, or expose partially validated objects.

## Known gaps and migration TODOs

1. **Golden-byte verification:** This document was derived without host compilation or archive
   execution. Generate the fixture corpus in the mandated manylinux container and check every field
   offset, zstd boundary, MessagePack shape, and table formula against annotated bytes.
2. **Version policy:** The current reader accepts any version after checking magic/compression and
   only uses `< 0.5.0` to select deprecated timestamp behavior. Define explicit minimum, maximum,
   and unknown-version handling before releasing the Rust reader.
3. **Legacy timestamps:** Obtain real pre-0.5.0 archives. Confirm `DeprecatedDateString` epoch units,
   two-array types, old timestamp-range encodings, pattern grammar, and mixed-version behavior.
4. **Native ABI:** Keep 0.5.0 interoperable by writing explicit little-endian bytes, then decide
   whether a future format version should become endian-neutral. Retain compile-time assertions in
   the C++ oracle for header size/offsets, and add guards for the remaining numeric assumptions while
   it remains supported.
5. **Section integrity:** There is no archive-level checksum or section digest. Decide whether a
   future version adds integrity metadata; do not add it incompatibly to 0.5.0.
6. **Weak C++ packet validation:** The C++ reader does not enforce required packet uniqueness and
   ignores the timestamp packet size. Add corrupt fixtures for missing, duplicate, truncated,
   oversized, unknown, and overlong packets.
7. **Implicit bit limits:** Add writer boundary tests for schema node IDs, delimiter lengths,
   logtype IDs, and encoded-variable offsets before implementing the Rust writer.
8. **Empty archives:** Characterization confirmed that C++ compression succeeds and writes an empty
   SFA that its own extractor rejects. The Rust low-level reader accepts the narrowly coherent
   all-empty representation. Decide separately whether the final Rust CLI preserves creation of
   this self-incompatible artifact, suppresses it, or reports an empty-input result without an
   archive.
9. **Nondeterministic bytes:** UUIDs, zstd versions, iteration of timestamp hash containers, and
   equal-sized table sorting can change bytes and physical order. Interoperability tests must assert
   semantics and separately gate archive size, not demand whole-archive byte identity.
10. **Schema IDs after splitting:** Commit a split-input fixture proving that schema IDs are opaque
    and may start above zero in later archives.
11. **Timestamp authority ordering:** The reader treats the first timestamp range as authoritative,
    while writer storage uses hash containers and only operationally supports one authoritative
    column. Specify a deterministic rule before supporting more than one.
12. **Trailing bytes and exact frames:** Add negative fixtures for trailing metadata payload bytes,
    trailing decompressed schema/table bytes, concatenated unexpected frames, section gaps, section
    overlap, and a mismatch between `compressed_size` and physical length.
13. **UTF-8 policy:** Valid JSON produces UTF-8 keys and dictionary strings, but the wire format
    only stores bytes. Decide whether the public Rust API rejects invalid UTF-8 archives or offers a
    byte-oriented lower-level representation.
14. **MessagePack canonicalization:** Treat MessagePack maps and integer widths semantically. Record
    the current msgpack-c/nlohmann output in goldens only as diagnostic evidence, not as a permanent
    byte-order requirement.
15. **32-bit length fields:** `metadata_section_size` and metadata packet payload sizes are `u32`,
    while the current writer narrows larger host sizes without an explicit overflow check. Rust must
    reject output that exceeds either limit and readers must check the widened values before offset
    arithmetic.

## Primary implementation evidence

* The header, version, magic, compression, metadata packet structs, and MessagePack map definitions
  are in [`SingleFileArchiveDefs.hpp`][sfa-definitions].
* Container construction, canonical file order, packet framing, packed-stream metadata, and section
  concatenation are in [`ArchiveWriter.cpp`][archive-writer].
* Header/packet parsing and SFA section-bound derivation are in
  [`ArchiveReaderAdaptor.cpp`][archive-reader-adaptor].
* Schema tree/map serialization and parsing are in [`SchemaTree.cpp`][schema-tree],
  [`SchemaMap.cpp`][schema-map], and [`ReaderUtils.cpp`][reader-utils].
* Table and stream metadata are in [`PackedStreamReader.cpp`][packed-stream-reader] and
  [`ArchiveReader.cpp`][archive-reader].
* Column layouts are paired in [`ColumnWriter.cpp`][column-writer] and
  [`ColumnReader.cpp`][column-reader].
* Dictionary framing and entry layouts are in [`DictionaryWriter.hpp`][dictionary-writer],
  [`DictionaryReader.hpp`][dictionary-reader], and [`DictionaryEntry.cpp`][dictionary-entry].
* Timestamp and range-index payloads are in
  [`TimestampDictionaryWriter.cpp`][timestamp-dictionary-writer],
  [`TimestampEntry.cpp`][timestamp-entry], and [`RangeIndexWriter.cpp`][range-index-writer].
* Log-order population and archive-local range coordinates are in [`JsonParser.cpp`][json-parser].

[archive-reader]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ArchiveReader.cpp
[archive-reader-adaptor]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ArchiveReaderAdaptor.cpp
[archive-writer]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ArchiveWriter.cpp
[column-reader]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ColumnReader.cpp
[column-writer]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ColumnWriter.cpp
[dictionary-entry]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/DictionaryEntry.cpp
[dictionary-reader]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/DictionaryReader.hpp
[dictionary-writer]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/DictionaryWriter.hpp
[json-parser]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/JsonParser.cpp
[packed-stream-reader]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/PackedStreamReader.cpp
[range-index-writer]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/RangeIndexWriter.cpp
[reader-utils]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/ReaderUtils.cpp
[schema-map]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/SchemaMap.cpp
[schema-tree]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/SchemaTree.cpp
[sfa-definitions]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/SingleFileArchiveDefs.hpp
[timestamp-dictionary-writer]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/TimestampDictionaryWriter.cpp
[timestamp-entry]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/TimestampEntry.cpp
