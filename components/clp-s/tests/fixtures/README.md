# CLP-S cross-language fixtures

Every generator and Rust validation command in this document must run in the image defined by
`components/core/tools/docker-images/clp-env-base-manylinux_2_28`. Do not compile or execute CLP-S
fixture tooling on the host.

## Compression CLI interoperability oracle

`compression-cli-v1-input.json` is the 66-byte parse-many stream used to characterize the first
thin Rust `c` adapter. `compression-cli-v1-sfa.hex` is the exact 387-byte SFA emitted by the pinned
C++ binary with log order disabled, default compression level and table threshold, and SFA output.
The input SHA-256 is `991ae9aeb79bfc687cf9c0935ff37da8e1b1c31212976b59ec4d925d783da3cb`;
the decoded archive SHA-256 is
`fc54cd82086f7f9b85808b5dbb7583d02be26aa31268be8d6c9417563a055840`.

Regenerate the binary oracle inside the required image with:

```sh
oracle_root=$(mktemp -d build/cpp-compression-cli-oracle.XXXXXX)
mkdir "$oracle_root/archives"
build/clp-s-reference/core/clp-s c \
  --disable-log-order \
  --single-file-archive \
  "$oracle_root/archives" \
  components/clp-s/tests/fixtures/compression-cli-v1-input.json
oracle=$(find "$oracle_root/archives" -mindepth 1 -maxdepth 1 -type f)
sha256sum "$oracle"
od -An -v -tx1 "$oracle" | tr -d ' \n' \
  > "$oracle_root/compression-cli-v1-sfa.hex"
printf '\n' >> "$oracle_root/compression-cli-v1-sfa.hex"
cmp "$oracle_root/compression-cli-v1-sfa.hex" \
  components/clp-s/tests/fixtures/compression-cli-v1-sfa.hex
```

The Rust CLI integration test decodes the hex without a second binary fixture, requires both the
directory-member concatenation and SFA to match byte-for-byte, validates the archive-stat line,
and extracts both layouts through the Rust reader. It also pins post-record target-zero rotation
and the final empty archive's trailing source-byte accounting.

## KV-IR decoder interoperability oracles

`kv-ir-v0.1.0-four-byte-cpp.hex` and `kv-ir-v0.1.0-eight-byte-cpp.hex` are whitespace-only hex
representations of complete streams emitted by the adjacent C++ `Serializer`. The emitter writes
the required end marker explicitly. Both streams contain the same metadata, UTC-offset change,
two auto-generated schema nodes, five user-generated schema nodes, and one event spanning empty
object, null, boolean, integer, float, ordinary string, and CLP-encoded string semantics. Their raw
binary lengths and SHA-256 values are respectively:

- four-byte: 345 bytes,
  `48902f0f9eac863c3b3ba5e73210e046cb3bd2cf029a978dc2c2ea437d65299a`;
- eight-byte: 349 bytes,
  `dff4fa96cdde22f3743e786fbf77749d7f5fc12f24fc27146c57e2c19bac0425`.

After the pinned C++ dependencies have been built, regenerate and compare both encodings inside
the required image with:

```sh
oracle_dir=$(mktemp -d build/cpp-kv-ir-oracle.XXXXXX)
c++ -std=c++20 -O2 \
  -I components/core/src \
  -isystem build/deps/cpp/msgpack-cxx-install/include \
  -isystem build/deps/cpp/nlohmann_json-install/include \
  -isystem build/deps/cpp/boost-install/include \
  -isystem build/deps/cpp/ystdlib-install/include \
  components/clp-s/tests/fixtures/kv-ir-v0.1.0-cpp-emitter.cpp \
  build/clp-s-reference/core/src/clp_s/libclp_s_clp_dependencies.a \
  -o "$oracle_dir/emitter"
"$oracle_dir/emitter" "$oracle_dir/four.bin" "$oracle_dir/eight.bin"
sha256sum "$oracle_dir/four.bin" "$oracle_dir/eight.bin"
od -An -v -tx1 "$oracle_dir/four.bin" | tr -d ' \n' | fold -w 64 \
  > "$oracle_dir/four.hex"
od -An -v -tx1 "$oracle_dir/eight.bin" | tr -d ' \n' | fold -w 64 \
  > "$oracle_dir/eight.hex"
cmp "$oracle_dir/four.hex" \
  components/clp-s/tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex
cmp "$oracle_dir/eight.hex" \
  components/clp-s/tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex
```

The Rust tests decode both files through deliberately short and interrupted reads, preserve raw
value packets, and verify typed callback values. They also test direct stream concatenation,
schema reset, exact offsets, every truncated prefix, malformed protocol units, and configured
limits. Legacy `0.0.x` unstructured-log IR is detected but intentionally deferred to a separate
compatibility adapter.

`kv-ir-search-v0.1.0-nested-cpp.hex` is a hand-framed protocol `0.1.0` stream searched through the
pinned C++ `clp-s` binary. It contains two events, nested objects, deliberately non-lexicographic
schema insertion, repeated key names at different depths, null, empty object, Boolean, integer,
and ordinary-string values. The 116 decoded bytes have SHA-256
`4a844f7b2abb39d05d23bdc87e98d3a387382794a7422b5d7c18b80e7815a11c`. C++ emits matching
objects in lexical key order rather than schema insertion order; direct Rust KV-IR search uses it
as the exact multi-event search and JSON-framing oracle.

## KV-IR timestamp-promotion interoperability oracles

`kv-ir-v0.1.0-timestamps-four-cpp.hex` and
`kv-ir-v0.1.0-timestamps-eight-cpp.hex` are complete streams emitted by the adjacent specialized
C++ serializer. Each contains integer, binary64, ordinary-string, CLP-encoded-string, and missing
`ts` cases. In particular, binary64 promotion formats the decoded value with fixed nine fractional
digits before timestamp recognition. The input lengths and SHA-256 values are:

- four-byte: 377 bytes,
  `d13c67f4a37f430432636686c780f2925883007cc18910b40f6b96549d6c3812`;
- eight-byte: 389 bytes,
  `5aef59ae6f6930cae100e89cbf294a94675e08dd4baa28c9eb98034f184d3b8f`.

The paired `sfa-v0.5.0-kv-ir-timestamps-*-cpp.hex` files are the pinned C++ timestamp archives,
both 551 bytes. Their SHA-256 values are respectively
`8d982cef8f283be258d6d8748f570b1c84a8053061863bec2fe2d30edf024371` and
`f02e861de726e21e4aa89dddbc9a83a4e93f1b0579943d338f9166caa0675996`.
They establish the exact extracted values, millisecond range `[1700000000123,
1700000000126]`, and four timestamp patterns. Whole-archive byte equality is deliberately not a
Rust compatibility requirement here: the pinned C++ adapter lazily creates archive nodes in
libstdc++ `std::unordered_map` iteration order, whereas Rust traverses the validated wire schema in
a deterministic order. The committed tests compare extracted JSON semantics and timestamp
pattern/range metadata, and both readers can consume the other writer's archive.

`kv-ir-v0.1.0-timestamp-split-four-cpp.hex` is a 251-byte four-byte stream containing the
binary64 timestamps `-1.000000001` and `2.000000001`; its SHA-256 is
`15043677e77c4463283013de5d7fda75b51ac7f635886ff99a6ee088af39e488`. With a zero encoded-size
target, the pinned C++ CLI reports archive-local timestamp/source tuples
`(-1001,-1000,true,239)`, `(2000,2001,true,11)`, and `(0,0,false,1)`. The last tuple belongs to the
required final empty archive. Compressed size is deliberately excluded from the cross-language
assertion because deterministic Rust schema traversal can differ from libstdc++ unordered-map
iteration.

Regenerate and compare the streams and C++ archives in the required image with:

```sh
oracle_dir=$(mktemp -d build/cpp-kv-ir-timestamp-oracle.XXXXXX)
c++ -std=c++20 -O2 \
  -I components/core/src \
  -isystem build/deps/cpp/msgpack-cxx-install/include \
  -isystem build/deps/cpp/nlohmann_json-install/include \
  -isystem build/deps/cpp/boost-install/include \
  -isystem build/deps/cpp/ystdlib-install/include \
  components/clp-s/tests/fixtures/kv-ir-v0.1.0-timestamps-cpp-emitter.cpp \
  build/clp-s-reference/core/src/clp_s/libclp_s_clp_dependencies.a \
  -o "$oracle_dir/emitter"
"$oracle_dir/emitter" "$oracle_dir/four.bin" "$oracle_dir/eight.bin"
for width in four eight; do
  od -An -v -tx1 "$oracle_dir/$width.bin" | tr -d ' \n' | fold -w 64 \
    > "$oracle_dir/$width.hex"
  cmp "$oracle_dir/$width.hex" \
    "components/clp-s/tests/fixtures/kv-ir-v0.1.0-timestamps-$width-cpp.hex"
  mkdir "$oracle_dir/$width-archive"
  build/clp-s-reference/core/clp-s c \
    --single-file-archive --disable-log-order --timestamp-key ts \
    "$oracle_dir/$width-archive" "$oracle_dir/$width.bin"
  archive=$(find "$oracle_dir/$width-archive" -mindepth 1 -maxdepth 1 -type f)
  od -An -v -tx1 "$archive" | tr -d ' \n' | fold -w 64 \
    > "$oracle_dir/$width-sfa.hex"
  cmp "$oracle_dir/$width-sfa.hex" \
    "components/clp-s/tests/fixtures/sfa-v0.5.0-kv-ir-timestamps-$width-cpp.hex"
done
sha256sum "$oracle_dir"/*.bin "$oracle_dir"/*-archive/*
```

Namespace characterization uses the same emitter with separate auto- and user-generated maps.
Unprefixed descriptors select the user tree; `@` selects the auto tree; `$`, `!`, and `#` are valid
but have no KV-IR tree. A JSON input under an `@` CLI descriptor is an intentional no-op, including
in mixed JSON/KV input, matching the pinned C++ adapter. Rust deliberately fixes two C++ defects:
matching booleans, nulls, objects, and arrays remain ordinary values instead of triggering a
wrong-variant exception, and validated UTC-offset-change units remain source-accounted no-ops
instead of silently truncating the KV-IR stream.

## Directory-writer interoperability oracle

`sfa-v0.5.0-unstructured-arrays-cpp-dir/` contains the eight canonical members emitted by the
default C++ directory writer for the adjacent unstructured-array corpus with log order disabled:
`header`, `schema_tree`, `schema_ids`, `table_metadata`, `var.dict`, `log.dict`, `array.dict`, and
`0`. Two C++ runs produced identical bytes for every member. Concatenating them in that order is
exactly the 618-byte `sfa-v0.5.0-unstructured-arrays-cpp.bin` oracle, proving that directory and SFA
output are physical layouts of the same encoded sections.

The Rust core finalizes to reusable [`EncodedDirectoryArchive`](../../src/writer/directory.rs)
member buffers and can drive a caller-owned transactional member sink. The thin filesystem adapter
requires explicit target and staging paths; it stages all eight files before publishing with a
directory rename and deliberately leaves failed staging state under caller control.

Inside the required image, regenerate and verify the oracle with:

```sh
oracle_root=$(mktemp -d build/cpp-m8-directory-oracle.XXXXXX)
mkdir -p "$oracle_root/archive"
build/clp-s-reference/core/clp-s c \
  --disable-log-order \
  "$oracle_root/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl
oracle_dir=$(find "$oracle_root/archive" -mindepth 1 -maxdepth 1 -type d)

cargo +nightly run -p clp-s --example write_unstructured_array_directory -- \
  build/rust-m8-directory build/rust-m8-directory.staging
for member in header schema_tree schema_ids table_metadata var.dict log.dict array.dict 0; do
  cmp "$oracle_dir/$member" "build/rust-m8-directory/$member"
done

mkdir -p build/m8-cpp-x/rust-output build/m8-cpp-x/cpp-output
build/clp-s-reference/core/clp-s x \
  build/rust-m8-directory build/m8-cpp-x/rust-output
build/clp-s-reference/core/clp-s x \
  "$oracle_dir" build/m8-cpp-x/cpp-output
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl \
  build/m8-cpp-x/rust-output/original
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl \
  build/m8-cpp-x/cpp-output/original
```

The committed tests compare every member to the C++ fixture, reconstruct the SFA concatenation,
round-trip through the public directory reader/extractor, and prove encoding errors happen before
sink calls while member failures never invoke commit.

## Unstructured-array writer interoperability oracle

`sfa-v0.5.0-unstructured-arrays-cpp.bin` is a deterministic 618-byte C++ archive emitted with
the default unstructured-array representation and log order disabled. Its six records cover an
empty array, nested and mixed values, insignificant internal whitespace, integer and custom-float
variables, dictionary variables, escaped backslashes and marker bytes, and nested empty
containers. The exact array token is retained: the intentionally spaced fourth lexeme extracts
byte-for-byte rather than being normalized.

The binding-oriented Rust API accepts an [`UnstructuredArrayRef`](../../src/writer/array.rs) over
the exact borrowed UTF-8 JSON token. It validates one root array with caller-bounded token length
and iterative container depth before staging dictionary or table state. Structured-array schemas
remain outside this milestone. `/array.dict` stores the escaped templates, while array dictionary
variables share `/var.dict`, matching the C++ writer.

Inside the required image, regenerate and verify the oracle with:

```sh
oracle_dir=$(mktemp -d build/cpp-m7-array-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --single-file-archive \
  --disable-log-order \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl

cargo +nightly run -p clp-s --example write_unstructured_array_sfa -- \
  build/rust-m7-unstructured-arrays.sfa
cmp build/rust-m7-unstructured-arrays.sfa "$oracle_dir"/archive/*

mkdir -p build/m7-cpp-x/rust-output build/m7-cpp-x/cpp-output
build/clp-s-reference/core/clp-s x \
  build/rust-m7-unstructured-arrays.sfa build/m7-cpp-x/rust-output
build/clp-s-reference/core/clp-s x \
  "$oracle_dir"/archive/* build/m7-cpp-x/cpp-output
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl \
  build/m7-cpp-x/rust-output/original
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl \
  build/m7-cpp-x/cpp-output/original
```

The committed tests additionally inspect the array and shared variable dictionaries, 24/40-bit
descriptors, encoded variables, malformed-token errors, limit enforcement, and append atomicity.
The adjacent manifest pins generator inputs, source hashes, repeat determinism, and both-direction
extraction paths.

## Timestamp-writer interoperability oracle

`sfa-v0.5.0-timestamps-cpp.bin` is a deterministic 474-byte C++ archive emitted from the adjacent
JSONL with log order disabled and `ts` configured as the authoritative timestamp key. It
interleaves one quoted date-time pattern and one numeric epoch-millisecond pattern in a single
`Timestamp` column. The timestamp dictionary records node ID 1, outward-rounded millisecond
bounds, explicit pattern IDs `[0, 1]`, and the exact resolved patterns. The column stores signed
epoch-nanosecond deltas followed by unsigned pattern IDs.

The binding-oriented Rust API accepts a caller-resolved epoch, exact JSON lexeme, resolved CLP-S
pattern, and authoritative range key. It compiles the pattern and formats the epoch into a reused
scratch buffer, rejecting any mismatch before committing schemas, dictionaries, ranges, or table
bytes. JSON tokenization and timestamp-pattern discovery remain adapter responsibilities.

Inside the required image, regenerate and verify the oracle with:

```sh
oracle_dir=$(mktemp -d build/cpp-m6-timestamp-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --single-file-archive \
  --disable-log-order \
  --timestamp-key ts \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl

cargo +nightly run -p clp-s --example write_timestamp_sfa -- \
  build/rust-m6-timestamp.sfa
cmp build/rust-m6-timestamp.sfa "$oracle_dir"/archive/*

mkdir -p build/m6-cpp-x/rust-output build/m6-cpp-x/cpp-output
build/clp-s-reference/core/clp-s x \
  build/rust-m6-timestamp.sfa build/m6-cpp-x/rust-output
build/clp-s-reference/core/clp-s x \
  "$oracle_dir"/archive/* build/m6-cpp-x/cpp-output
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl \
  build/m6-cpp-x/rust-output/original
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl \
  build/m6-cpp-x/cpp-output/original
```

The committed tests require whole-archive byte identity, inspect the range, patterns, delta values,
and IDs through the Rust reader, and reproduce all timestamp lexemes exactly through extraction.
The adjacent manifest pins the image, generator, source hashes, input, archive, repeat run, and
differential extraction result.

## Log-order writer interoperability oracle

`sfa-v0.5.0-log-order-cpp.bin` is a pinned C++ default-order archive for six records interleaved
across three schemas. Its schema tree begins with the default metadata root and the
`_log_event_idx` `DeltaInteger` child. Each table contains that ordered column; its reconstructed
indexes are `[0, 2, 5]`, `[1, 4]`, and `[3]`, proving that table-local deltas retain archive-global
order across schema switches.

A fresh C++ file is not deterministic because the JSON-file adapter emits a random
`_archive_creator_id` in its range-index packet. The committed archive records that generated UUID
and its canonical filename as fixture facts. Given those values through explicit
`ArchiveSourceContext` bracketing, `ArchiveSetWriter` reproduces all 593 bytes exactly. The adjacent
lower-level `OpenArchive` oracle remains source-agnostic and separately proves that all seven
canonical section byte streams match when no source context is supplied.

Inside the required image, regenerate and validate with:

```sh
oracle_dir=$(mktemp -d build/cpp-m5-order-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --single-file-archive \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl

cargo +nightly run -p clp-s --example write_log_order_sfa -- \
  build/rust-m5-log-order.sfa
cargo +nightly test -p clp-s \
  writer::tests::log_order_writer_matches_every_canonical_cpp_section -- --exact
cargo +nightly test -p clp-s \
  writer::archive_set::tests::source_context_makes_the_complete_log_order_archive_byte_identical_to_cpp \
  -- --exact

mkdir -p build/m5-cpp-x/rust-output build/m5-cpp-x/cpp-output
build/clp-s-reference/core/clp-s x \
  build/rust-m5-log-order.sfa build/m5-cpp-x/rust-output
build/clp-s-reference/core/clp-s x \
  "$oracle_dir"/archive/* build/m5-cpp-x/cpp-output
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-unordered.jsonl \
  build/m5-cpp-x/rust-output/original
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-unordered.jsonl \
  build/m5-cpp-x/cpp-output/original

cargo +nightly run -p clp-s --example extract_sfa_ordered -- \
  build/rust-m5-log-order.sfa build/m5-rust-ordered.jsonl
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl \
  build/m5-rust-ordered.jsonl
```

An empty C++ default-order invocation is also nondeterministic and was 403 bytes in the pinned
characterization, while C++ `--disable-log-order` is the canonical deterministic 280-byte empty
archive. The source-agnostic `OpenArchive` emits that same 280-byte archive for either option.
`ArchiveSetWriter` instead reproduces C++'s legal empty `[0, 0)` range when an empty source context
is explicitly opened and closed.

## Retained-float writer interoperability oracle

`sfa-v0.5.0-retained-floats-cpp.bin` is a deterministic 504-byte archive emitted from the
adjacent JSONL by the pinned C++ `clp-s c` binary with log-order disabled and retained-float
formatting enabled (the default). It covers negative zero, decimal leading and trailing zeroes,
uppercase and lowercase scientific markers, exponent signs and widths, minimum subnormal and
maximum finite binary64 values, repeated dictionary fallback values, precision overflow,
unsupported mantissa shapes, over-width exponents, and exact-rounding fallback.

Inside the required image, regenerate and verify it with:

```sh
oracle_dir=$(mktemp -d build/cpp-m4-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --single-file-archive \
  --disable-log-order \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-retained-floats-cpp-input.jsonl
sha256sum "$oracle_dir"/archive/*

cargo +nightly run -p clp-s --example write_retained_float_sfa -- \
  build/rust-m4-retained-floats.sfa
cmp build/rust-m4-retained-floats.sfa "$oracle_dir"/archive/*

mkdir -p build/m4-cpp-x/rust-output build/m4-cpp-x/cpp-output
build/clp-s-reference/core/clp-s x \
  build/rust-m4-retained-floats.sfa build/m4-cpp-x/rust-output
build/clp-s-reference/core/clp-s x \
  "$oracle_dir"/archive/* build/m4-cpp-x/cpp-output
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-retained-floats-cpp-input.jsonl \
  build/m4-cpp-x/rust-output/original
cmp components/clp-s/tests/fixtures/sfa-v0.5.0-retained-floats-cpp-input.jsonl \
  build/m4-cpp-x/cpp-output/original
```

The committed writer tests construct the same typed borrowed records, require byte identity with
the C++ archive, inspect formatted descriptors and dictionary IDs through the Rust reader, and
extract every original number token byte-for-byte. The typed API validates JSON number grammar,
finite parsing, and exact binary64 bit agreement before any archive state is committed.

## String-writer interoperability oracle

`sfa-v0.5.0-strings-cpp.bin` is a deterministic 504-byte archive emitted from the adjacent JSONL
by the pinned C++ `clp-s c` binary with log-order and retained JSON-float formatting disabled. It
covers repeated whole-string dictionary values, a reused CLP logtype, canonical integer and custom
float variables, cross-mode dictionary reuse, zero-variable logtypes, and escaped backslash and
placeholder bytes. The manifest pins the image, generator and relevant source hashes, input, output,
and differential extraction result.

Inside the required image, regenerate and verify it with:

```sh
oracle_dir=$(mktemp -d build/cpp-m3-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --single-file-archive \
  --disable-log-order \
  --no-retain-float-format \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-strings-cpp-input.jsonl
sha256sum "$oracle_dir"/archive/*

cargo +nightly run -p clp-s --example write_string_sfa -- build/rust-m3-strings.sfa
cmp build/rust-m3-strings.sfa "$oracle_dir"/archive/*
```

The committed writer test also constructs the same borrowed records, requires byte identity with
the C++ archive, decodes both dictionary spaces and 24/40-bit descriptors through the Rust reader,
and extracts the original string bytes.

## Structured-array search oracle

`sfa-v0.5.0-structured-arrays-cpp.hex` is the whitespace-only hex encoding of a deterministic
688-byte C++ SFA generated from the adjacent JSONL with `--structurize-arrays` and log order
disabled. Its SHA-256 is
`2ef355f85ce0b4352d21216b1dcd673113db8d9787b54c7ae933ce5c62011a3c`. The records cover direct
scalar/null elements, empty arrays and objects, repeated object schemas, nested arrays and objects,
and a structured array below an ordinary object. The 354-byte `-search.jsonl` file is the exact
physical-schema-table-order output of C++ `s ARCHIVE "*:*"` (SHA-256
`cf3e104cc2f30b48ef89b6f5127082841bec99e831a0c4c4d21c6ff8e093161d`).

Inside the required image, regenerate and verify the fixture with:

```sh
oracle_dir=$(mktemp -d build/cpp-structured-array-oracle.XXXXXX)
build/clp-s-reference/core/clp-s c \
  --structurize-arrays \
  --disable-log-order \
  --single-file-archive \
  "$oracle_dir/archive" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-structured-arrays-cpp-input.jsonl
xxd -p -c 64 "$oracle_dir"/archive/* > "$oracle_dir/archive.hex"
build/clp-s-reference/core/clp-s s "$oracle_dir/archive" "*:*" \
  > "$oracle_dir/search.jsonl"
cmp "$oracle_dir/archive.hex" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex
cmp "$oracle_dir/search.jsonl" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-structured-arrays-cpp-search.jsonl
```

The Rust extraction and search tests decode the same archive, require byte-exact reconstruction,
and pin the C++ path wildcard, existence, missing, null, range, repeated-occurrence, and projection
semantics without requiring either CLI at test time.

`tests/structured_array_writer.rs` constructs the same nine records through borrowed recursive
`ValueRef::Array`/`ValueRef::Object` values, requires all 688 Rust bytes to equal this C++ fixture,
then reopens the Rust output and requires the exact 354-byte physical-order JSONL above. This pins
the writer's delimiter bodies, empty-container nodes, repeated columns, nested ancestry, schema
ordering, and transactional entry limit without invoking either CLI during tests. For a smaller
standalone library example:

```sh
cargo +nightly run -p clp-s --example write_structured_array_sfa -- \
  build/rust-structured-array.sfa
```

## Network search output oracle

`search-network-v0.5.0-minimal-cpp.hex` contains the exact TCP byte stream emitted by the pinned
C++ command when searching `sfa-v0.5.0-minimal-cpp.bin` with query `*:*` and the `network` output
handler. The decoded stream is 95 bytes with SHA-256
`3709412759fe743a7f2537d2941127f518e5d25e405cb7d117d16f0fd95d4fcd`. It is one independent
five-element MessagePack array:

1. timestamp `0`;
2. the reconstructed JSON record including its trailing newline;
3. an empty original-path string;
4. an empty archive-ID string; and
5. log-event index `0`.

This differs intentionally from the file handler, which populates the timestamp, archive ID, and
log-event index. The fixture was captured in the required manylinux image by starting a loopback
TCP listener and running:

```sh
build/clp-s-reference/core/clp-s s \
  components/clp-s/tests/fixtures/sfa-v0.5.0-minimal-cpp.bin \
  '*:*' network --host 127.0.0.1 --port 28766
```

The listener wrote received bytes without adding framing, then `xxd -p` produced the committed
hex. The reference binary SHA-256 was
`e5042953dedaaec7c95ac6746be6b3d4de6e5629ca9d86ecf45d7e59f0a21284`.

## Complete C++ oracle archive

`sfa-v0.5.0-minimal-cpp.bin` is a complete SFA emitted by the current C++ `clp-s c
--single-file-archive` path from `sfa-v0.5.0-minimal-cpp-input.jsonl`. It contains all four metadata
packet types currently emitted by the writer: archive info, archive file info, an authoritative
integer timestamp dictionary, and a log-order range index. The adjacent manifest pins the header,
packets, seven physical sections, schema tree, schema map, packed-stream/table metadata, archive
statistics, generation environment, and C++ extraction and search expectations.

The current CLI has intentional UUID nondeterminism. It draws one UUID for the output archive ID
and another `_archive_creator_id` that is embedded in the range-index metadata packet.
Consequently, a fresh invocation is semantically reproducible but not byte-identical; UUID content
also causes small differences in the metadata zstd frame size. The manifest records three
investigated runs and preserves the selected run's concrete UUIDs and checksum. Regenerating this
fixture is a reviewed manifest revision, not an expectation that the command below reproduces its
SHA-256.

After the C++ dependencies and the Release `clp-s` target have been built in the pinned image, run
the following from the repository root inside that image:

```sh
fixture_work_dir=build/clp-s-full-fixture-regeneration
mkdir -p "$fixture_work_dir/output"
build/clp-s-reference/core/clp-s c \
  --compression-level 3 \
  --timestamp-key ts \
  --print-archive-stats \
  --remove-path-prefix components/clp-s/tests/fixtures \
  --single-file-archive \
  "$fixture_work_dir/output" \
  components/clp-s/tests/fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl \
  > "$fixture_work_dir/archive-stats.jsonl"
find "$fixture_work_dir/output" -maxdepth 1 -type f -print -exec sha256sum {} \;
```

Before admitting regenerated bytes, run C++ search and extraction against the generated path,
check the extracted JSONL against the source input, and update every generated ID, size, offset,
and checksum in `sfa-v0.5.0-minimal-cpp.manifest.json`. The committed Rust integration test opens
the golden directly and validates its envelope, metadata packets, schemas, dictionaries,
table layout, and byte-exact decompressed packed stream. It then lazily decodes the zero-copy
columns, uses the precompiled timestamp pattern and CLP logtype reconstruction APIs, and reproduces
the canonical input record byte-for-byte.

## Header-layout fixture

`sfa-header-v0.5.0-x86_64.bin` is a 64-byte `ArchiveHeader` emitted by the current C++
`ArchiveHeader` constructor. It is deliberately not a complete archive. Its field values make
byte order, padding, field width, and field offsets visible to readers without coupling the test
to a hand-written Rust encoder.

The adjacent JSON manifest is the source of truth for provenance and expected decoded values.
Regenerate the file only in the image defined by
`components/core/tools/docker-images/clp-env-base-manylinux_2_28`, then update the manifest hashes
and toolchain identity in the same review. The emitter rejects non-little-endian targets and
unexpected C++ layouts at compile time.

From the repository root, after `task deps:core` has prepared C++ dependencies inside the image:

```sh
cmake -S components/core -B build/clp-s-reference/core -DCMAKE_BUILD_TYPE=Release
cmake --build build/clp-s-reference/core \
  --target clp-s-sfa-header-fixture-emitter --parallel
build/clp-s-reference/core/clp-s-sfa-header-fixture-emitter \
  components/clp-s/tests/fixtures/sfa-header-v0.5.0-x86_64.bin
sha256sum \
  components/clp-s/tests/fixtures/sfa-header-v0.5.0-x86_64.bin \
  components/core/src/clp_s/SingleFileArchiveDefs.hpp \
  components/core/src/clp_s/tests/sfa-header-fixture-emitter.cpp
```

Run all commands above through the pinned container. Do not compile or execute the emitter on the
host.
