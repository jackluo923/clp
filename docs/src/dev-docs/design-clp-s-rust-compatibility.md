# CLP-S Rust rewrite compatibility contract

:::{important}
This document covers only the `clp-s` executable and the library that will replace its runtime.
It does not authorize changes to the `clp`, `glt`, or web UI products.
:::

This document defines the compatibility boundary for replacing the C++ implementation of `clp-s`
with a library-first Rust implementation. It is intentionally stricter than a feature list: it
separates behavior that downstream systems already rely on from incidental behavior that must be
characterized before deciding whether to preserve it.

The reference implementation is the C++ `clp-s` binary built from a pinned commit. It remains the
executable oracle until the Rust implementation passes the cross-implementation matrix below. The
target design is a Rust library with a thin CLI. Command-line parsing, process-global setup,
logging, and concrete service destinations must not become requirements of the core library.

## Compatibility terminology

The following labels are used throughout this document:

* **Hard contract**: Preserve this behavior unless a separately reviewed compatibility decision
  changes it. Add a regression test before implementing it in Rust.
* **Characterize**: Capture the reference binary's behavior in a test before porting the relevant
  code. Characterization does not automatically make an accident permanent.
* **Revisable quirk**: Preserve it during differential testing, or record an explicit decision and
  migration note explaining why the Rust CLI differs.
* **Out of scope**: Do not port it as part of this rewrite. An internal algorithm may still be in
  scope when `clp-s` depends on it, even if its current source lives outside `src/clp_s`.

Compatibility means observable equivalence, not byte-for-byte equality of newly written archives.
Archive UUIDs, zstd output, and other non-semantic bytes can vary. A Rust archive must nevertheless
be readable by the pinned C++ implementation, and all represented records, metadata, query results,
and ordering guarantees must agree.

## Baseline identity

Every characterization, golden-generation, interoperability, and performance run must record:

| Field | Requirement |
| --- | --- |
| Source | Full Git commit and whether the tree was dirty |
| Build image | Image name and immutable digest, not only a mutable tag |
| Target | OS, architecture, target triple, and endianness |
| C++ build | Compiler version, CMake options, optimization level, IPO/LTO state, and binary SHA-256 |
| Rust build | Pinned toolchain, Cargo feature set, profile, code-generation options, and binary SHA-256 |
| Native formats | zstd, MessagePack implementation, libarchive, and other format-affecting versions |
| Invocation | Argument vector, non-secret environment names, working directory, and input manifest |

The initially pinned source commit is selected when the first baseline is generated. Updating it is
a reviewed operation: regenerate or revalidate every affected characterization and state why the
oracle changed.

### Container-only rule

Do not compile C++ or Rust `clp-s` code on the host. Do not run a host-built `clp-s` in tests,
fixture generation, or benchmarks. These operations run in the image defined by
[`components/core/tools/docker-images/clp-env-base-manylinux_2_28`][manylinux-image], with the
workspace bind-mounted into the container.

If that image does not contain Rust, derive the build image from it and install a pinned Rust
toolchain there. Record the derived image's digest. Documentation-only checks and read-only source
inspection may run on the host; commands that can transitively build `clp-s` may not.

Rust changes follow the repository lint tasks rather than a crate-local style fork. The required
gate runs `cargo +nightly fmt --all -- --check` and locked, offline Clippy with `-D warnings`, all
targets, and all features for `clp-s`, `clp-s-container`, and `clp-s-ffi` from the workspace root in
the same image. Those three packages are the authoritative lint and test scope for this rewrite. A
whole-workspace gate may still run in repository CI, but unrelated workspace packages do not block
the `clp-s` replacement.

Distributable and performance-comparison builds use
`cargo +nightly build --locked --profile clp-s-release -p clp-s --bin clp-s`. The named root
profile inherits Cargo release settings and fixes fat LTO with one code-generation unit. Keeping
these settings in an opt-in profile prevents the `clp-s` performance contract from silently
changing other Rust binaries in the workspace.

[manylinux-image]: https://github.com/y-scope/clp/tree/DOCS_VAR_CLP_GIT_REF/components/core/tools/docker-images/clp-env-base-manylinux_2_28

## Product boundary

### In scope

The Rust replacement must cover behavior reachable from the three commands declared by
[`CommandLineArguments::Command`][command-line-arguments]:

:::{list-table}
:header-rows: 1

* - Surface
  - In-scope behavior
* - `clp-s c`
  - NDJSON and KV-IR ingestion; local paths and supported network sources; nested gzip, zstd, and
    general-purpose archive handling; JSON type inference; timestamp recognition; schema,
    dictionary, range-index, and log-order construction; lossless float formatting; structured and
    unstructured arrays; archive splitting; directory archives and SFA
* - `clp-s x`
  - Directory and SFA reading; local and supported network sources; unordered extraction; ordered
    merge and chunking; chunk statistics; optional decompression metadata output
* - `clp-s s`
  - KQL parsing and semantics; timestamp and range-index pruning; schema matching; projections;
    case-sensitive and case-insensitive matching; archive table scans; direct KV-IR search; count,
    count-by-time, min, max, and unique aggregations; stdout, file, network, results cache, and
    reducer destinations; optional search telemetry
* - Library
  - The data transformation, archive, query, source, sink, cancellation, resource-limit, progress,
    metrics, and structured-error APIs needed to implement the commands without calling CLI code
:::

The input and archive implementations currently reuse code under `src/clp`, including readers,
zstd wrappers, encoded CLP strings, wildcard matching, and KV-IR. Only the portions transitively
required by `clp-s` are in scope. Their source directory does not make the `clp` binary part of the
rewrite.

Service integrations may be optional Cargo features or adapter crates, but optional packaging is
not permission to remove behavior from the replacement executable.

### Out of scope

The following are not linked into the current `clp-s` runtime target and are excluded:

* The `clp`, `glt`, and web UI products.
* `src/clp_s/filter` and its standalone tooling.
* `src/clp_s/ffi`, including the current SFA FFI wrapper. New bindings are a separate consumer of
  the Rust library rather than a port of that wrapper.
* `src/clp_s/indexer`.
* `src/clp_s/log_converter`. Its KV-IR output remains an interoperability input.
* The SQL parser and SQL search syntax.

Shared build-system cleanup, unrelated package refactors, and changing other binaries' archive
formats are also out of scope. A later project can adopt the Rust library without widening this
rewrite.

## Source of truth

When sources disagree, use this precedence:

1. An explicit compatibility policy accepted in this document.
2. A production consumer of a machine-readable `clp-s` protocol.
3. An accepted golden or differential test tied to the pinned baseline.
4. Observable behavior of the pinned C++ executable and, for wire-format details that cannot be
   observed independently, the pinned implementation.
5. Existing unit and integration tests.
6. User and developer documentation.

Lower-precedence material is still evidence. A disagreement must be recorded; it must not be
silently resolved by selecting the easier behavior. For example, the user documentation currently
shows `date(...)` while the grammar accepts `timestamp(...)`, and it says order is not retained
while the executable records order by default. The grammar, executable, and tests govern migration
until the documentation is corrected.

Once a Rust behavior is accepted into the compatibility suite, that suite becomes the executable
contract. Tests should describe semantic intent so that an incidental formatting snapshot does not
quietly become a hard contract.

## CLI contract

### Accepted command surface

The pinned surface to characterize is:

* `clp-s c [OPTIONS] ARCHIVES_DIR [FILE/DIR ...]`, with `--compression-level`,
  `--target-encoded-size`, `--min-table-size`, `--max-document-size`, `--timestamp-key`,
  `--files-from`/`-f`, `--print-archive-stats`, `--no-retain-float-format`, `--normalize-paths`,
  `--remove-path-prefix`, `--remove-leading-slash`, `--single-file-archive`,
  `--structurize-arrays`, `--disable-log-order`, and `--auth`.
* `clp-s x [OPTIONS] ARCHIVES_PATH OUTPUT_DIR`, with `--ordered`,
  `--target-ordered-chunk-size`, `--print-ordered-chunk-stats`, `--archive-id`, `--auth`,
  `--mongodb-uri`, and `--mongodb-collection`. Chunk and metadata options require ordered mode, and
  the two MongoDB options must be provided together.
* `clp-s s [OPTIONS] ARCHIVES_PATH KQL_QUERY [OUTPUT_HANDLER [OUTPUT_HANDLER_OPTIONS]]`, where the
  query can also be supplied by `--query`/`-q`. Match controls are `--tge`, `--tle`,
  `--ignore-case`/`-i`, `--enable-telemetry`, `--archive-id`, `--projection`, and `--auth`.
  Aggregations are the mutually exclusive `--count`, `--count-by-time`, `--min`, `--max`, and
  `--unique` options. Min/max/unique require a nonempty field without an unescaped wildcard.
* Search handlers are `stdout` (the default), `file --path`, `network --host --port`,
  `results-cache --uri --collection [--batch-size] [--max-num-results] [--dataset]`, and
  `reducer --host --port --job-id`. File and network handlers reject aggregations. Reducer requires
  count or count-by-time; stdout and results-cache support the current aggregation set.
* `--help`/`-h` applies globally and to each command.

The reference defaults are:

:::{list-table}
:header-rows: 1

* - Area
  - Defaults
* - Compression sizes
  - Compression level 3; target encoded size 8 GiB; minimum table size 1 MiB; maximum document size
    512 MiB
* - Compression representation
  - Preserve float formatting; directory archives; unstructured arrays; record log order and range
    index
* - Compression I/O
  - No timestamp key, archive-stat output, path normalization/removal, files-from file, or network
    authentication
* - Extraction
  - Unordered; no chunking, chunk-stat output, archive-ID filter, MongoDB metadata, or network
    authentication
* - Search
  - Case-sensitive; no timestamp bounds, projection, aggregation, archive-ID filter, telemetry, or
    network authentication; stdout handler
* - Results cache
  - Batch size 1000 and maximum results 1000; dataset empty unless supplied
:::

### Hard contracts

The following are compatibility requirements:

* The executable name remains `clp-s`, with commands `c`, `x`, and `s`. Existing long options,
  short aliases, positional forms, defaults, and option validation remain accepted unless a
  deprecation decision says otherwise.
* Help and informational requests exit successfully. Invalid arguments and runtime failures exit
  nonzero. Successful processing exits zero. Characterization tests must pin the current values;
  the supported public exit-code contract is `0` versus nonzero unless a consumer is found to
  require a particular nonzero value.
* Data written to stdout is never mixed with diagnostic logging. Diagnostics remain on stderr.
* With `c --print-archive-stats`, stdout is NDJSON containing exactly one valid JSON object per
  completed archive. At minimum the fields `id`, `begin_timestamp`, `end_timestamp`, `size`, and
  `uncompressed_size` retain their names and compatible JSON types. Existing fields `is_split` and
  `range_index` remain available. Additional fields must not invalidate existing consumers.
* The [task compression worker][task-compression-worker] parses each archive-stat line as it
  arrives. Buffering all stats until process exit, emitting banners on stdout, or emitting a
  partial JSON document is incompatible.
* With `x --print-ordered-chunk-stats`, stdout is NDJSON with one object per completed chunk and a
  `path` field. The referenced file must already be finalized when the line is emitted.
* Search stdout and file results preserve their existing record representation, record boundaries,
  archive metadata behavior, and aggregation schemas. Network, MongoDB/results-cache, and reducer
  payloads retain their existing framing and field meanings.
* Compression and extraction preserve JSON values according to the current options. In particular,
  retained float lexemes, integer boundaries, null/boolean distinctions, object structure, array
  mode, authoritative timestamps, input metadata, and log order require dedicated tests.
* Search retains the current KQL grammar and matching semantics, including namespaces, escaping,
  wildcard rules, numeric comparisons, arrays, boolean precedence, timestamps, projections, and
  aggregation validation.
* Local, URL, and S3-authenticated inputs retain the supported wrapper/container combinations.
  Secrets are passed by the current environment-variable interface and must never be printed or
  stored in fixture metadata.
* Processing multiple input archives or files remains streaming from the caller's perspective. The
  library may not require all input or all output records to fit in memory.

The complete option surface is captured from `clp-s {c,x,s} --help` by characterization tests. The
[source declaration][command-line-source] remains useful for review, but a hand-copied option list
in this document is not the canonical parser test.

### Behavior to characterize before porting

Create black-box cases for each item below. Record stdout bytes, stderr after normalizing the logger
timestamp, exit status, and the output tree with file hashes where applicable.

* No command, unknown command, `--help` in each accepted position, missing positionals, unknown
  options, repeated options, and values at numeric boundaries.
* Which options may appear before or after positional arguments, especially multi-token
  `--projection` and search output-handler subcommands.
* Recursive input and archive discovery order, symlink behavior, duplicate input paths, empty
  files/directories, canonical path metadata, `--files-from`, `--normalize-paths`,
  `--remove-path-prefix`, and `--remove-leading-slash`.
* Output-directory creation, collisions, append versus truncate behavior, partially written output
  after failure, and behavior when a later item in a multi-input invocation fails.
* Unordered extraction's `original` filename and ordered names of the form
  `<archive-id>_<begin-index>_<end-index>.jsonl`, including empty archives and exact chunk threshold
  behavior.
* Duplicate JSON keys, very deep records, invalid UTF-8, malformed/trailing JSON, values near signed
  and unsigned integer limits, non-finite numbers, very long keys/strings, and documents around
  `--max-document-size`.
* Timestamp pattern selection, timezone normalization, precision, missing authoritative fields,
  mixed timestamp types, and the inclusive `--tge`/`--tle` boundaries.
* Search record ordering across schemas and archives; projection order; missing fields; ambiguous
  string/number literals; escaping; ignore-case Unicode behavior; and aggregation output for no
  matches.
* Error and retry behavior for truncated inputs, corrupt archives, unsupported compression, network
  interruption, invalid credentials, MongoDB failure, reducer failure, and telemetry failure.
* Direct KV-IR search selection and fallback. The current executable treats a path containing the
  KV-IR extension as an IR candidate, tolerates an incomplete stream for real-time search, and may
  fall back to archive search when an advanced feature is unsupported.

### Revisable quirks

Exact help wrapping, capitalization and punctuation of human diagnostics, logger timestamps, and
the wording of warnings are not hard contracts unless a consumer is discovered. Snapshot them so a
reviewer can see changes, but normalize unstable portions in the primary compatibility assertion.

The following implementation behaviors require an explicit decision instead of accidental
bug-for-bug compatibility:

* Detecting a KV-IR candidate by substring rather than an exact file suffix.
* Returning success after warning about a truncated direct-search KV-IR stream.
* Falling back to unordered extraction when ordered extraction is requested from an archive with no
  ordering metadata.
* Emitting a zero-schema archive for empty input even though the pinned C++ extractor rejects the
  pinned C++ writer's own empty output.
* Native-endian/raw-structure serialization and permissive handling of archive versions.
* Integer casts or overflows at the edges of JSON and archive numeric types.
* Filesystem traversal order and search output order where no ordering promise is documented.
* Retaining partial archives, extraction files, or search output after a later failure.

A changed quirk needs a regression test for the intended behavior and, when users can observe it, a
release note. Safety fixes may deliberately reject previously accepted malformed input; they still
need a compatibility decision.

### Initial search characterization results

The pinned black-box suite now confirms equal, left-associative `AND`/`OR` precedence; typed quoted
literals; bare default-namespace searches; key/value wildcard escaping; missing-key EXISTS/NEXISTS
behavior; independent existential predicates over structured-array elements; inclusive timestamp
bounds; projection membership and schema-derived output order; physical schema-table result order;
and empty output for zero-match `--count`. Under `C.UTF-8`, ignore-case matching folds ASCII but did
not equate `É` and `é` in the fixture.

Two unstructured-array observations are marked as suspected C++ bugs rather than accepted Rust
semantics: numeric `>`, `<`, `>=`, and `<=` all produced the same rows as `!=`, and
`NOT arr.missing:*` produced no rows. The Rust evaluator must keep these behind explicit
compatibility decisions and differential tests.

## Archive compatibility

CLP-S has two physical archive layouts with the same logical sections:

* A directory containing `header`, `schema_ids`, `schema_tree`, `table_metadata`, `0`,
  `array.dict`, `log.dict`, and `var.dict` as applicable.
* A single-file archive (SFA) containing a header, compressed metadata packets, and the logical
  files at recorded offsets.

The [current SFA definitions][sfa-definitions] specify magic `fd 2f c5 30`, version `0.5.0`, and a
C++ header occupying 64 bytes on the supported build targets. These facts are inputs to a separate
byte-level format specification, not permission to deserialize the C++ struct directly in Rust.
Rust readers and writers use explicit-width, bounds-checked little-endian operations. Unknown
node-type discriminants are errors; [known `NodeType` numeric values][node-types] never change.
Unknown metadata packet types are skipped only when their declared bounds are valid.

### Required matrix

Run every cell for both directory and SFA layouts, every supported archive version, structured and
unstructured arrays, ordered and unordered archives, and archives with and without range indexes.

| Producer | Consumer | Requirement |
| --- | --- | --- |
| Pinned C++ | Pinned C++ | Baseline oracle; fixtures must pass before they are admitted |
| Pinned C++ | Rust | Required before declaring the Rust reader, extraction, or search milestone complete |
| Rust | Pinned C++ | Required before declaring the Rust writer milestone complete |
| Rust | Rust | Required continuously; includes round trip, reopen, extraction, and search |

Compare more than reconstructed JSON. Validate archive ID and version, file metadata, authoritative
timestamp dictionary, schema/node types, record count, exact preserved numeric lexemes, log-event
indexes, ordered output, range-index contents, projections, query results, and aggregation values.
Use structural JSON comparison only where object-key order and insignificant whitespace are not
part of the contract.

The zero-record case has a characterized oracle exception: the pinned C++ writer emits an SFA but
the pinned C++ extractor rejects it because the schema count is zero. The Rust empty/no-log-order
writer is nevertheless byte-identical to pinned C++ output under `--disable-log-order`, and the
Rust reader opens both the no-log-order and default-log-order C++ variants. Rust-to-C++ extraction
acceptance therefore begins with the first nonempty table milestone; the explicit empty-input CLI
policy decision above remains.

### Current interoperability checkpoints

These checkpoints record implementation evidence, not completion of the full matrix. The C++
oracle is the release-with-IPO `clp-s` built in the required manylinux image from the pinned source
tree.

* The Rust no-log-order empty writer and pinned C++ `c --disable-log-order` produce the same 280
  bytes (SHA-256 `772eadf924b47e7e4f37f682db65ab60b6b9766bc54ca897ad5b81392473ce22`).
* The first nonempty writer checkpoint covers two schemas, nested objects, nulls, signed integers,
  finite binary64 values, and Booleans. Rust and pinned C++ produce the same 439-byte SFA
  (SHA-256 `07895b81df31cf7a999289e174d2c76110da55c08332e0e53758e43c9457947e`), and
  pinned C++ extracts all three Rust-written records byte-exactly.
* The next writer checkpoint adds variable strings, CLP logtypes, integer/custom-float/dictionary
  encoded variables, dictionary reuse, and escaped marker bytes. Rust and pinned C++ produce the
  same 504-byte SFA (SHA-256
  `5496a358b3ecb526e4cb2b4ccd13e510179d25b86d2946057ed9722f5d6a18fe`); pinned C++ extracts the
  Rust archive to the same 205-byte JSONL (SHA-256
  `9fbb1ef6327dcb09d77d16828d48045011f6cb7a50ea976639fa78e4a43f39ff`).
* The retained-float checkpoint validates each caller-supplied finite binary64 value against its
  exact JSON token, then selects C++-compatible formatted-float descriptors or dictionary-float
  fallback. Rust and two independent pinned C++ runs produce the same 504-byte SFA (SHA-256
  `b47781ecc24fcd06198602da0e491a9c2b2419f07880209706604a66e3c2aaf1`); pinned C++ extracts the
  Rust archive to the exact 381-byte source (SHA-256
  `a2cd123a7ce22dc7d19ba680d8ce1aa90aa1f9ce0f83ac1cf785eb1e2e047953`).
* The default-log-order writer checkpoint records archive-global indexes across interleaved schema
  tables and emits the canonical metadata root plus `_log_event_idx` delta-integer columns. The
  pinned C++ oracle is 593 bytes (SHA-256
  `60dc5e3dfdbec73575184017e9cb06dfeefce55b7a1ed38f52e4178347e266f2`). Given the committed
  creator UUID and canonical filename through `ArchiveSourceContext`, both the low-level archive
  set and the source-aware JSON adapter reproduce all 593 C++ bytes exactly, including the range-
  index packet. The source-agnostic `OpenArchive` remains a separate 451-byte checkpoint whose
  seven canonical section payloads are byte-identical. Pinned C++ extracts the Rust archive
  successfully, and Rust ordered extraction reproduces the original six-record interleaving
  exactly.
* The timestamp writer accepts a binding-friendly epoch-nanosecond value together with its exact
  lexeme, resolved pattern, and authoritative range key. It validates the lexeme, builds canonical
  timestamp ranges and pattern IDs, rounds millisecond range bounds outward, and emits delta-coded
  `Timestamp` columns transactionally. Rust and two independent pinned C++ runs produce the same
  474-byte SFA (SHA-256
  `9e41c1c854fc84a77ab587007e06ff6157a2d75fc6ca0c8fa29c23a16d0a3ffa`); pinned C++ extracts the
  Rust archive to the exact 144-byte input (SHA-256
  `6dc00287c52da04bbc307d52522b6371ce9426f7c0718f7579dcfcf523c82446`).
* The default-mode unstructured-array writer validates one exact borrowed UTF-8 JSON array with a
  bounded iterative stack, then uses canonical `/array.dict` logtypes and the shared `/var.dict`
  variable space. Rust and two independent pinned C++ runs produce the same 618-byte SFA (SHA-256
  `0e8608c850f5faf8c068afb92c75b2b01aef39ba4f112a1079f3912e9dd1b058`); pinned C++ extracts the
  Rust archive to the exact 284-byte input (SHA-256
  `68fbc4e1b8d1c838a6ddff7381229533c9d1600e2e564a8d8f2510058f16e827`).
* The library writer also accepts recursive borrowed `ValueRef::Array` values and emits the C++
  structured-array unordered schema region without materializing JSON. Its bounded iterative
  planner preserves heterogeneous encounter order, repeated physical occurrences of one schema
  node, empty-container bare nodes, nested array/object ancestry, and the C++ rule that only
  objects which are direct array elements receive `Object` delimiters. Ordinary columns remain in
  the sorted ordered prefix; unordered leaves remain in encounter order. A per-record schema-entry
  limit and the fixed 24-bit delimiter-body domain are enforced before commit, and all node,
  dictionary, table, and allocation failures retain the existing transactional append boundary.
  The nine-record Rust SFA is byte-identical to the committed 688-byte C++
  `--structurize-arrays --disable-log-order` oracle (SHA-256
  `2ef355f85ce0b4352d21216b1dcd673113db8d9787b54c7ae933ce5c62011a3c`) and reconstructs its exact
  354-byte physical-order JSONL.
* Canonical directory output now reuses the same encoded section buffers as SFA output and exposes
  all eight members through a binding-friendly in-memory result or caller-owned transactional sink.
  Every Rust member is byte-identical to two pinned C++ directory runs; their canonical aggregate
  is the same 618 bytes and SHA-256 as the array SFA above. The filesystem adapter stages all
  members and publishes by rename, and pinned C++ extracts a Rust-written directory to the exact
  284-byte source.
* Rust unordered reconstruction of the C++ no-floats, formatted-float, and timestamp corpora is
  byte-identical to C++ extraction. Their output SHA-256 values are, respectively,
  `78500ee1321a05e3d6edb18bc30c2340896599197614e9dbcec189a1b6b7d193`,
  `631b505f5cf775e3d22c13607c6f922b0047274c454d5de50b4368acf0ae83c5`, and
  `642b9ef2bc0dc6ca1fc248d32d07e5f23e5ee29edf6a6a9f5425967fc8a7084c`.
* Rust heap-merged reconstruction of the same C++ archives under ordered semantics is also
  byte-identical. The no-floats and formatted-float output SHA-256 values are
  `65d7b6bdad276c092cc1a40c4ffeedfb4f9762a378024bd13c3d8e75a548ac11` and
  `da8874ce3b992b45133733f6b0c6fb019c8dd935757eea8d8e5c761b59e438f5`; the timestamp
  corpus has one physical table and retains the hash above.
* SFA and directory archives now share the object-safe `ArchiveReader` extraction boundary. A
  filesystem directory integration test exposes the committed C++ SFA fixture as its eight exact
  physical members, accepts an unrelated sidecar like the C++ reader, and reconstructs the same
  JSONL through both primitive catalog/stream calls and the high-level record pipeline.
* The bounded Rust KQL lexer/parser produces a nonrecursive owned arena and pins the characterized
  precedence, quoted-type inference, namespaces, escapes, wildcard cleanup, nested prefixes, and
  compact list syntax. Archive compilation now resolves schema paths, namespaces, dictionaries,
  CLP strings, typed scalar comparisons, existence/missing/NOT behavior, ASCII-only case folding,
  scalar range-index predicates, timestamp columns and literals, compact list expressions,
  unstructured-array paths and values, and bounded physical-row bitmaps. Lists resolve their path
  once, compile values without DNF
  expansion, and implement the characterized any/all/none Boolean identities with at most two
  bounded row bitmaps. Array evaluation iteratively reconstructs exact dictionary values into
  reusable bounded scratch and preserves characterized C++ behavior, including non-equality
  numeric operators, unresolved nested `NEXISTS`, null transforms, and the current wildcard-number
  omission. Structured-array schema paths now retain repeated physical occurrences without
  materializing JSON, including C++'s empty-key element traversal, independent existential
  predicates across repeated objects, mixed null/non-null transforms, and the wildcard-resolution
  quirk that excludes named descendants below a structured array for non-pure wildcard paths.
  Schemas with unordered regions receive bounded container validation before matching, and the
  committed C++ structured-array oracle pins exact extraction, search, nested containers, empty
  values, comparisons, existence/missing, and projection behavior.
  Inclusive epoch-millisecond `tge`/`tle` bounds use the first authoritative timestamp range and
  fail explicitly when that range is absent, reversed, or unrepresentable. Deprecated date remains
  a separate incomplete milestone. Typed aggregation plans now stream count, count-by-time,
  minimum, maximum, and unique directly from matching archive columns. They retain bounded
  per-archive state and reproduce C++ mixed integer/float comparison, negative time buckets,
  scalar variant ordering, and compact JSON number formatting without serializing and reparsing
  every matched record. As in C++, aggregation field traversal cannot descend through a structured
  array.
  Archive-level orchestration now loads one format-independent catalog, compiles each parsed query
  once per archive, and scans one packed stream at a time in deterministic physical table order.
  A synchronous borrowed match sink receives the catalog, zero-copy decoded table, and table-local
  bitmap only for nonempty matches, so projection and reducers can consume typed columns without
  constructing JSON or retaining archive-sized result collections. Stream/table/schema context is
  preserved on decode, match, and sink failures. The first reusable projection adapter parses the
  C++ exact escaped-dot descriptors, rejects wildcards and duplicates, resolves selected nodes once
  per archive, and prunes one extraction program per matching schema. It advances unmatched
  stateful rows without formatting them and emits borrowed JSONL records in schema-derived key and
  physical table/row order; missing selected fields produce `{}` without affecting query matching.
  A conservative archive-compile fast path also skips packed-stream decompression when a direct
  root predicate has no possible schema/dictionary candidate, including the C++-pruned case of a
  wholly unresolved predicate immediately below root `NOT`; other negated and compound expressions
  remain on the ordinary three-valued table evaluator unless they can be proven safe later.
* The ingestion library now has a dependency-free, bounded physical-line NDJSON reader. It reuses
  its input, decoded-string, event, and explicit parser-stack buffers; presents each accepted record
  as borrowed balanced events with exact raw string/number tokens; and supports deterministic stop
  or skip policy for malformed and over-limit physical records. A separate `ParseManyReader`
  preserves that explicit NDJSON contract while accepting multi-line and directly adjacent (`}{`)
  root objects like C++ `iterate_many`. It bulk-scans/copies input chunks, carries string/escape and
  object-depth state across boundaries, reuses amortized parser buffers, retains exact document
  offsets, and latches after a malformed or over-limit stream because no safe generic
  resynchronization boundary exists. Strict library defaults report an incomplete suffix, while an
  explicit compatibility policy accounts and ignores only an incomplete final object at physical
  EOF. The CLI selects that policy and reports the ignored byte count, matching C++ behavior.
* The JSON-to-writer adapter consumes those borrowed traversals without constructing a recursive or
  self-referential record tree. Nested objects use a fallible flat writer-event API; array-start
  events carry backpatched exact `[...]` spans and collapse to unstructured-array values without a
  copy; decoded strings remain borrowed. Number classification matches `simdjson`: negative integer
  tokens must fit `i64`, nonnegative tokens may fill `u64` and are reinterpreted as signed bits, and
  fraction/exponent tokens must be finite binary64 values. Source and archive failures are located
  and transactional. Parsing the pinned six-row array source through this adapter produces the
  exact 618-byte C++ SFA above. Sibling duplicate detection uses scoped hash indexes, avoiding a
  quadratic wide-object path.
* Source-aware JSON ingestion opens an explicit caller-supplied `ArchiveSourceContext`, preserves
  absolute NDJSON/parse-many offsets across post-record rotation and callback retry, and atomically
  charges trailing bytes while closing the range. Its complete pinned log-order archive is byte-
  identical to the 593-byte C++ oracle. The KV-IR archive adapter uses the decoder's existing
  bounded metadata traversal instead of reparsing its preamble, maps the validated
  `USER_DEFINED_METADATA` object into typed range fields, opens one context per decoded stream, and
  closes it together with the explicit EOF byte. It also promotes an optional authoritative
  timestamp path directly from KV-IR schema/value events without constructing JSON. Exact-boundary
  rotation retains C++'s final empty range, and concatenated streams receive independent split-
  number sequences. Four- and eight-byte C++ fixtures pin exact archive interoperability,
  negative/sub-millisecond rounding, schema changes, and transactional failure after a valid
  prefix.
* Compression input is a bounded streaming pipeline rather than a whole-file staging step.
  It recursively probes and decodes gzip and zstd wrappers, enforces independent physical,
  per-layer decoded-byte, and nesting limits, preserves decoder/source context in typed errors, and
  feeds either the KV-IR or parse-many adapter after probing the decoded prefix. CLI source
  discovery, `--normalize-paths`, `--remove-path-prefix`, `--remove-leading-slash`, and
  `--files-from` are wired through the same source-context transform for filesystem sources.
  Existing filesystem paths take precedence over URL-looking spellings; otherwise HTTP(S)/S3
  schemes are recognized case-insensitively. HTTP(S) source metadata retains the exact caller URL
  and deliberately bypasses filesystem path transforms. Because that compatibility metadata can
  include caller query credentials, diagnostics redact URL userinfo, query, and fragment even
  though the persisted source name does not. Unknown inputs replay the bounded probe into the
  optional `clp-s-container` adapter,
  which streams regular libarchive members in physical order without extracting or buffering them,
  skips special and hardlink entries, and feeds each JSON, KV-IR, or empty member through the same
  public archive-set ingestion function. Real member names bypass outer path transforms;
  libarchive raw fallback uses the transformed outer filename or exact outer URL. Member probing is
  zstd-only like C++, nested containers are rejected, and opaque non-UTF8 member names currently
  produce a typed error rather than lossy range metadata. The CLI removes representable size/count
  caps for compatibility while retaining the finite no-progress guard. Remote compression keeps
  the successfully probed response instead of issuing a second GET. It makes one open/probe
  attempt, matching the pinned C++ executable: although the C++ source contains a 1/2/4-second
  retry helper, deterministic HTTP 503 characterization made one request and failed immediately.
  HTTP clients, HTTPS-only `CURL_CA_BUNDLE` loading, and per-open S3 environment lookup are lazy,
  so earlier local inputs are committed before a later remote authentication failure and a bad CA
  path cannot reject plaintext HTTP.
* The Rust executable remains a thin adapter over object-safe reader, writer, match, and
  record-sink APIs. Its initial `clp-s s` surface accepts a positional query or `-q`/`--query`,
  `-i`/`--ignore-case`, `--archive-id`, exact column projection, an optional explicit `stdout`
  handler, and default buffered stdout. It sorts discovered local archive paths and preserves each
  archive's physical schema-table/row order. Committed C++ fixtures are byte-identical for full,
  case-insensitive, and projected records. Inclusive timestamp bounds and all five stdout
  aggregations are implemented; aggregation output is byte-identical to the committed C++ oracle,
  including values above `2^53` and negative zero. The reducer destination supports count and
  count-by-time through a reusable `Read + Write` protocol adapter: it negotiates one connection
  per command, shares it across archives, and emits the C++ native `int64_t`/`size_t` framing and
  MessagePack record-group schema without stdout output. Results-cache search is implemented as a
  library-first generic batch adapter plus a thin optional MongoDB CLI sink. Ordinary search keeps
  the latest N results per archive, replaces the retained minimum only for a strictly newer
  timestamp, emits ascending timestamp order in batches, and preserves the C++ BSON fields and
  scalar types. Archive ID and dataset are shared while results are retained rather than copied
  into every heap entry, and rejected old matches advance column cursors without reconstructing
  JSON. Aggregations reuse the typed aggregation engine and support all five C++ operations with
  the same per-archive lifecycle and batch ordering; `max-num-results` remains intentionally
  inapplicable to aggregation output. MongoDB initialization is deferred to the archive sink's
  pre-output hook, and neither ordinary nor aggregate results-cache output writes to stdout.
  Telemetry remains recognized and rejected explicitly. The `file --path` handler
  writes the required metadata-bearing MessagePack 5-tuples with C++-compatible framing and
  lifecycle; it is not
  repurposed as a JSONL file mode. Extraction continues to append unordered output to `original`,
  perform record-aligned ordered chunk rotation, emit post-rename NDJSON chunk statistics, and fall
  back only for the exact missing-log-order error. HTTP(S) SFA search and extraction use the public
  forward-only streaming reader; direct remote KV-IR uses the same public searcher and reopens the
  URL only when its characterized deserializer fallback treats the source as an SFA. S3 signing
  consumes borrowed environment credentials without exposing the generated query, while source
  metadata retains the original URL. Protocol-visible remote archive IDs apply the C++ percent
  decoding and trailing-segment rules, but ordered extraction separately validates the decoded ID
  as one filesystem component and falls back to the encoded segment when a decoded slash or `..`
  would escape the output directory. Literal or percent-encoded path dot-segments are rejected
  before I/O because the HTTP stack would otherwise normalize them and could fetch a different
  object. Invalid UTF-8 becomes a typed error instead of reproducing the pinned C++ process abort.
  End-to-end tests pin exact C++ fixture output,
  the 20-byte threshold split `[0,2)`, `[2,5)`, `[5,6)`, archive-ID filtering, validation, and
  output-side-effect behavior.
* `clp-s-ffi` exposes a versioned C ABI over opaque archive and compiled-query handles. Every
  search creates fresh reader state, result bytes are copied into caller-provided buffers or
  delivered synchronously through callbacks, callback cancellation is distinct from failure, and
  Rust panics are caught before crossing the ABI. A C smoke test exercises version discovery,
  handle ownership, query reuse, exact newline-free callback records, cancellation, and stable
  status/error retrieval. The core crate remains free of C ABI layout and unsafe code.

An exploratory warm-cache search checkpoint on the existing one-million-record archive found the
Rust release path at 0.181 seconds mean wall time versus 0.237 seconds for C++ across eight
alternating runs of `level:ERROR` with a two-column projection. Both emitted the same 8,489,778
bytes (SHA-256 `1ff1525710e6420801513628b3d894a763e06504d6149cfe2e81f5830f00986f`). A
dictionary value absent from the archive took 0.002 seconds in Rust versus 0.008 seconds in C++
after the conservative no-match fast path. These are exploratory local measurements, not a
substitute for the complete cold-cache, schema-churn, array, and multi-archive matrix.

The fixture-level exact-output checks cover every stable current value representation present in
those corpora, including raw formatted-float lexemes and timestamp formatting. The final heldout
matrix below adds end-to-end compression, extraction, and search evidence. It does not claim
cold-cache, rotated multi-chunk, service-adapter, malformed-input performance, or other-architecture
performance coverage; failure handling is covered by the targeted limits and malformed-input tests.

An exploratory warm-cache extraction checkpoint used a deterministic one-million-record,
294,452,056-byte stable-schema corpus (SHA-256
`6e1e7910bc665de5a831c56ded4b96127a87c3898f2762779bbab61f7cf3553d`) and seven randomized
exact-output pairs. With both implementations using their generic CPU targets, the
release-with-thin-LTO Rust extractor produced median throughput of 242.6 MB/s versus 210.3 MB/s for
the release-with-IPO C++ oracle, with median peak RSS of 128.1 MB versus 141.1 MB. The median paired
Rust/C++ ratios were 1.160 for throughput, 0.862 for wall time, 0.862 for CPU time, and 0.908 for
RSS. Every paired throughput ratio was between 1.143 and 1.172. The Rust and C++ binary SHA-256
values were `35faaff271b58462aad9d589baeb6cf9472da261661125271376acb2f0da2cce` and
`e5042953dedaaec7c95ac6746be6b3d4de6e5629ca9d86ecf45d7e59f0a21284`, respectively, in image
`sha256:404dc2526ba72b548e69b4b66da5c9f2cabd5cd47d856b7f6d51c0bedfd2fee5`. This accepts early
extraction non-inferiority on one corpus; it is not a substitute for the production, schema-churn,
high-cardinality, arrays, ordered, many-archive, cold-cache, compression, and search matrix.

After introducing the borrowed record-boundary sink, the same generic-target build and seven-pair
protocol produced 241.2 MB/s Rust median throughput versus 210.5 MB/s for C++, with 128.2 MB versus
141.2 MB median peak RSS. The Rust/C++ throughput ratio was 1.146 and every paired ratio remained
between 1.132 and 1.165; all outputs retained the exact corpus hash. This confirms that exposing
record coordinates and ordered event indexes did not materially regress this early hot path. The
repeat Rust binary SHA-256 was
`5c34757a85a90bba30c712031fa708eb786362f975bce9007cb69e63f8328ea3`, and the result JSON
SHA-256 was `1095b7ad4281e38cdc03d7469f0ed52312b1390e50e3a6f1b9cbcbb7b50e897c` in the same pinned
image.

The first real executable checkpoint then ran `clp-s x` on both sides, including command parsing,
archive discovery, output-directory creation, and each implementation's default buffered file
sink. Across the same seven randomized pairs, Rust produced median throughput of 291.6 MB/s versus
212.1 MB/s for C++, median wall time of 1.010 s versus 1.389 s, and median peak RSS of 128.7 MB
versus 141.2 MB. The ratio of medians was 1.375 for throughput, 0.727 for wall time, and 0.912 for
RSS; individual paired throughput ratios ranged from 1.326 to 1.402. Every output retained the
exact 294,452,056-byte corpus hash. The generic-target, thin-LTO Rust executable SHA-256 was
`1ea7a6ccfee0bcfb1d02b5a7ce74d2a1956306d495d3e3208214a50845f62399`; the result JSON SHA-256
was `faffd41823cc596224357cd20acd85c64faff77e3bc229eac2b2df292516fa95`. At that historical
checkpoint, writer/search linkage and the remaining workload matrix were still outstanding; the
heldout checkpoint below supersedes that performance caveat for the seven accepted workload
shapes.

The companion `clp-s x --ordered` checkpoint exercised each executable's log-order path without
chunk rotation. Rust produced median throughput of 285.1 MB/s versus 208.5 MB/s for C++, median
wall time of 1.033 s versus 1.412 s, and median peak RSS of 128.8 MB versus 141.2 MB. The ratio of
medians was 1.367 for throughput, 0.731 for wall time, and 0.912 for RSS; individual paired
throughput ratios ranged from 1.314 to 1.401. All seven pairs produced the same exact
294,452,056-byte output and corpus hash. This used the same pinned binaries and image as the
unordered executable checkpoint; the ordered result JSON SHA-256 was
`2116a7f4c029349f12b54bc676c86c7f743cf541977975352e7ebaf77a4f2768`. At that historical
checkpoint, schema-churn and rotated multi-chunk workloads were still outstanding. The final
heldout matrix supersedes it for the accepted ordered-extraction shape; rotated multi-chunk remains
separate coverage.

The direct training-corpus checkpoint compared the PGO Rust candidate with the pinned C++
executable across both compression modes, both extraction modes, and three search workloads. Each
workload ran in balanced order on CPU 14 with two warmup pairs followed by 20 measured pairs. The
294,452,056-byte corpus SHA-256 was
`6e1e7910bc665de5a831c56ded4b96127a87c3898f2762779bbab61f7cf3553d`, and the 4,959,069-byte C++
archive SHA-256 was `791e08c1dde055bc40a888bfcfcc066be61eb7147e466968078335ebcfa581b6`.
The Rust and C++ executable SHA-256 values were
`e979d95a0b70730de694621ec856d3d9d4f5d5844ce6ab2fe4aad7c3cd3bbf95` and
`e5042953dedaaec7c95ac6746be6b3d4de6e5629ca9d86ecf45d7e59f0a21284`, respectively.

| Workload | Paired throughput median | Paired throughput p05 | Paired CPU-time median | Paired RSS median | Paired RSS p95 | Output-size p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Compress with log order | 1.283044 | 1.215211 | 0.779372 | 0.848512 | 0.849854 | 1.000001 |
| Compress without log order | 1.334225 | 1.319392 | 0.749505 | 0.903450 | 0.905065 | 1.000000 |
| Extract unordered | 2.504341 | 2.470488 | 0.399195 | 0.923991 | 0.926371 | 1.000000 |
| Extract ordered | 2.538124 | 2.491283 | 0.393949 | 0.923585 | 0.925245 | 1.000000 |
| Search rare record | 1.043900 | 1.037334 | 0.957739 | 0.909562 | 0.910490 | -- |
| Search all count | 1.087203 | 1.075785 | 0.919785 | 0.921088 | 0.922020 | -- |
| Search error count | 1.037160 | 1.022874 | 0.963991 | 0.908541 | 0.910223 | -- |

All table entries are paired Rust/C++ ratios; throughput values above one are better, while CPU,
RSS, and output-size values below one are better. Raw median Rust RSS was also below raw median C++
RSS in every workload. Both compression outputs cross-read exactly to the source corpus, both
extraction outputs were byte-exact, and every repeated Rust and C++ search output matched. The
equal-workload geometric mean of the seven paired throughput ratios was
1.438283, with a one-sided 95% bootstrap lower confidence bound of 1.435543; every individual
workload's corresponding lower bound was above one. The frozen result JSON SHA-256 was
`66d773e5a0678aaefc762d2ebf488678b090f89bfb3a60c84aa1e1721aa46e7b`, and the independent audit
SHA-256 was `e7400b8be7f3846971f47fc14ecdb8576c99b11d8eb42e18b64ec1e09d1fdb11`.

Because the PGO profile used this same corpus and workload set, this checkpoint is a training-set
control rather than evidence of performance generalization. The separate heldout checkpoint below
is the acceptance result.

The current candidate packs dictionary values into byte arenas with end offsets, uses
collision-safe ID indexing, stages small serializer transactions inline, batches dictionary zstd
writes in 128 KiB chunks, and bulk-decodes dictionary sections with bounded scratch and capacity
growth. The end-to-end results below measure the combined candidate and do not attribute gains to
one optimization.

### Heldout CLI performance checkpoint

The final warm-cache heldout checkpoint used a separately generated 268,435,484-byte,
949,687-record corpus (SHA-256
`ff3aa37ea063ca38d00e95d00333d7af85296600172864fd0352959dfdd0bbff`) and a
23,063,329-byte C++ reference archive (SHA-256
`c1e740602360dfce93fb6110b8c1139ac6019cd1c06dfdd469ebc66618fe18ec`). The PGO profile was
trained only on the distinct stable corpus and archive with SHA-256 values
`6e1e7910bc665de5a831c56ded4b96127a87c3898f2762779bbab61f7cf3553d` and
`791e08c1dde055bc40a888bfcfcc066be61eb7147e466968078335ebcfa581b6`, respectively.

Each workload ran on CPU 14 with two warmup pairs followed by 20 measured pairs. Pair order was
balanced exactly ten-to-ten in every workload. All 308 invocations succeeded. The Rust and C++
binary SHA-256 values were
`df4b06edfe9594084089df52353eddaf76dd03d4d9fa59727bb135e970c54696` and
`e5042953dedaaec7c95ac6746be6b3d4de6e5629ca9d86ecf45d7e59f0a21284`.

| Workload | Throughput median | Throughput p05 | CPU-time median | RSS median | RSS p95 | Output-size p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Compress with log order | 1.024708 | 1.006024 | 0.975912 | 0.833995 | 0.834685 | 1.00000018 |
| Compress without log order | 1.042637 | 1.020665 | 0.959109 | 0.828265 | 0.828705 | 1.000000 |
| Extract unordered | 1.624460 | 1.614684 | 0.615451 | 0.633845 | 0.634441 | 1.000000 |
| Extract ordered | 1.770448 | 1.753979 | 0.564685 | 0.757110 | 0.757602 | 1.000000 |
| Search rare record | 1.314670 | 1.299704 | 0.760444 | 0.628526 | 0.629016 | -- |
| Search all count | 1.344905 | 1.334956 | 0.743274 | 0.629494 | 0.630510 | -- |
| Search error count | 1.273985 | 1.264857 | 0.784741 | 0.628274 | 0.628751 | -- |

All entries are paired Rust/C++ ratios. Every workload passed throughput at both the median and
p05. Median peak RSS was 16.6%--17.2% lower for compression, 24.3%--36.6% lower for extraction,
and 37.1%--37.2% lower for search. Both compression preflights exercised all four reader/writer
combinations; extracted bytes agreed between readers, all 949,687-record semantic multisets
matched, ordered cross-writer output was byte-identical, and archive sizes remained equivalent.
Every extraction pair was byte-identical and every repeated C++/Rust search result matched.

The frozen evidence directory is
`build/final-perf-matrix-heldout-new-pgo-vs-cpp-20-v1`. Its `results.json` and
`raw-results.jsonl` SHA-256 values are
`8172edef89eabdd3e43120b899a3bde072b297d982ec6efab303bc85d25bdd06` and
`a2f653055490ddd2330890974fc4415adbdd09b5c89f4e2624d54068f9cc0258`. An independent
recomputation accepted every integrity, correctness, structure, and performance threshold; its
`audit.json` SHA-256 is
`54699cc2eb404830dc32b1d036d0d6c5daaec777454d648797bb4ea3086a7869`. The equal-workload
geometric mean of paired throughput was 1.318082 with a one-sided 95% bootstrap lower bound of
1.316648. The supplementary immutable-image, compiler, dependency, source, and PGO record is
`provenance.json` (SHA-256
`8e1c6ba4b83639813d9a51d988464fed1b47342fc65ece9c945308d66e18034e`); the directory checksum
manifest has SHA-256 `f9282403455cc0b79ad5bd788d85f8655e109ad9a49ed7fa5061c0c60eec5192`.
All five evidence files are mode `0444`.

The PGO bundle is `build/clp-s-pgo-dictionary-bulk-v1`. Its source manifest is
`build/clp-s-pgo-dictionary-bulk-source-v1.sha256` (SHA-256
`bec6b48561c2686fed48082fdb0e0565c04d20247cbce0d6dad95344010897ea`, 174 entries), and its
normalized source fingerprint is
`80c05f15d75a41f612626516b8e51418e3d0b80fbee6b3adca8ffffc6d44cdb5`. The merged profile
SHA-256 is `44b54c1a12ac20da5648c6ae5232af7d9b54901dfa01ba9522aa7a9c63d1ec1a`. The provenance,
validation summary, and checksum-manifest SHA-256 values are
`23fa3004ed9e35eaddad5ada9db381c2afbc159eb108db8ca4c714cce16ae34b`,
`5248c4c233c42bd3ae2e9b2827112cd4ae48b3af72137546e1c6c974dff6b061`, and
`5f560c8e5b23268f6b0a8be0ac2d736919a078a73e0ac67332c9a271a29e179c`. The build used image
ID `sha256:404dc2526ba72b548e69b4b66da5c9f2cabd5cd47d856b7f6d51c0bedfd2fee5`, nightly
`2026-08-30`, seven raw profiles, zero missing-function warnings, and eleven successful cross-read
validation commands. Because the local image is addressed by a mutable `:dev` reference plus its
inspected immutable ID, this is a local acceptance candidate rather than a registry-addressable
distributable release artifact.

After the final source changes, formatting, locked/offline strict all-feature and all-target Clippy,
693 non-doc tests, and the documentation tests passed for `clp-s`, `clp-s-container`, and
`clp-s-ffi` in the required image. Frozen logs are under
`build/final-gates-clp-s-dictionary-bulk-v1`; the Clippy, test, doc-test, and empty
successful-format log SHA-256 values are
`dea01309aac1bfc1652c7250d6be9f3f79660121ae5de77abed5e5a04c0466a4`,
`f0cd6d152737ab3cdab1996026f75b102613d9b7620d7bf9aa66a0eb76e31456`,
`9144c52671e07d23b458f4bdb2dd5cdcef4718d84f3186d017edf6f687b9a265`, and
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

New Rust archives target the current format until a version-change policy is accepted. Format
extensions require a version decision, a forward/backward-read matrix, fixtures for both sides of
the boundary, and an upgrade or rejection story. A Rust reader must reject out-of-bounds offsets,
impossible counts, truncated streams, and resource-limit violations without panicking or allocating
from an unchecked archive-provided size.

## KV-IR compatibility

`clp-s` consumes KV-IR during compression and can search a KV-IR stream directly. The Rust library
also provides the current KV-IR producer and an incremental decoder so bindings do not need the C++
runtime. The first downstream compatibility target is the existing `clp_ffi_py.ir.Serializer` and
`clp_ffi_py.ir.Deserializer` API; the CPython extension remains a thin adapter over the versioned C
ABI rather than owning a second implementation.

| KV-IR producer/input | Rust compression | Rust direct search | Existing Python deserializer |
| --- | --- | --- | --- |
| Pinned C++ log converter, complete stream | Required | Required | Required |
| Existing application/library fixtures, complete stream | Required | Required | Required |
| Valid stream split at every legal byte boundary | Required | Required | Required, including one-byte reads |
| Truncated stream accepted by the C++ real-time path | Characterize policy | Preserve current nonfatal behavior until policy changes | Preserve `allow_incomplete_stream` behavior |
| Malformed or unsupported-version stream | Deterministic typed error | Deterministic typed error | Preserve the public exception category |
| Rust KV-IR producer | Required cross-read | Required cross-read | Required cross-read |

The producer accepts borrowed typed values and the existing pair of MessagePack maps without first
building an unbounded generic value tree. Each event is transactional: validation, schema changes,
and encoded bytes either commit together or leave the serializer reusable. The decoder exposes both
the borrowed push path used by `clp-s` ingestion and a one-event-at-a-time path with explicit owned
lifetimes for bindings. Input, unit, schema, event, nesting, metadata, and output buffers all have
caller-configurable limits and reuse capacity without growing from an unchecked wire size.

The pinned Python serializer decodes the first complete MessagePack root map in each argument and
ignores trailing bytes. The Rust producer preserves this behavior without scanning or copying the
suffix; empty, truncated, and non-map first objects still produce structured errors and roll back
the whole event. Four- and eight-byte oracle tests require exact stream bytes for normal inputs and
for inputs with trailing `nil` or arbitrary data.

The incremental decoder commits exactly one stream header or IR unit per call. Once a complete item
has reached the synchronous callback, a callback error is resumable at the following item; reader,
protocol, and allocation errors are terminal. A borrowed event can resolve its node IDs against the
reader's retained schema without a consumer-owned schema copy. Bindings materialize only selected
paths into two DFS-preorder node arrays and one byte arena. The flat node is a fixed 32-byte POD, so
the C ABI can expose it without a projection allocation; encoded text is reconstructed once into the
owned arena. Returned events are independent of later reads and reader destruction.

The C++ event shares one mutable schema across retained events, so the self-contained Rust form is
not assumed to win every artificial retention pattern merely because it removes C++ unordered maps
and per-`to_dict` schema-sized bitmaps. Performance gates include consume-immediately, stable-schema
retention, and schema-churn retention separately, and require lower measured peak RSS rather than a
representation-only estimate.

The Python replacement preserves constructor signatures, four-byte serializer output, metadata,
byte accounting, flush/close/context-manager behavior, incomplete-stream policy, event lifetime,
and `to_dict(encoding, errors)`. Performance acceptance measures the unchanged public Python API,
including MessagePack ingestion for serialization and `to_dict` for deserialization. Across
randomized paired trials, the Rust-backed path must meet or exceed the pinned C++ throughput and
have lower peak RSS; native-only measurements are diagnostic and cannot substitute for that gate.

The latest audit-grade 100,000-event checkpoint passes that public-API gate for all six
comparisons. The C++ reference was rebuilt from detached `clp-ffi-py` commit
`08e0be37ec33ec1dcaf51209f12780035b579ca3` and its recursively pinned submodules; its native module
SHA-256 was `f81a60aaa744c83b7608e5e74fd472571aa2891c4ba4de0654ada7803b461236`. The
Rust-backed module SHA-256 was
`410fc6cdc055c60b83348785184c22f03a4b92e5cf82b2c972561b0e087ade12`. Each sample ran in a fresh
process on isolated CPU 15. The order of C++ and Rust samples was balanced inside each comparison,
with ten pairs in each order, and the gate used two warmups followed by 20 measured pairs. All 240
measured samples preserved the exact operation-specific stream or content, metadata, byte-count,
lifecycle, and repeated-EOF behavior. The gate also required at least equal paired CPU throughput,
strictly lower raw median peak RSS, and paired median and p95 RSS ratios below one.

| Public operation | Workload | Paired CPU throughput median | Median peak RSS, C++ -> Rust (KiB) | Paired RSS median | Paired RSS p95 |
| --- | --- | ---: | ---: | ---: | ---: |
| Serializer | Stable schema | 1.331654 | 31,990 -> 31,318 | 0.979086 | 0.988470 |
| Serializer | Sparse schema churn | 30.859057 | 35,240 -> 31,728 | 0.900833 | 0.906062 |
| Serializer | Encoded text and arrays | 1.873717 | 32,190 -> 31,350 | 0.973814 | 0.984652 |
| Deserializer plus `to_dict` | Stable schema | 1.275065 | 32,154 -> 31,476 | 0.977816 | 0.983690 |
| Deserializer plus `to_dict` | Sparse schema churn | 7.687228 | 35,428 -> 32,188 | 0.909618 | 0.915713 |
| Deserializer plus `to_dict` | Encoded text and arrays | 1.190863 | 32,140 -> 31,262 | 0.974568 | 0.983847 |

The result JSON SHA-256 is
`e369e122bb04871b47a46693fa5d64f634a23a3bc1709c467b51a80538e8bfd6`; the independently generated
acceptance report SHA-256 is
`4f864ca4184b2bf39d1ce2f485a190e60e5217709135c4a8d492710cba3225f5`, and the extended-metrics
report SHA-256 is `68620635cda8fd2a6ceab98eaf3f15ac69d2b085ed1c826c5e150dc11d17e292`.
The Rust raw median peak RSS was 672--3,512 KiB lower than C++ across the six comparisons. Both
implementations also produce the complete C++ behavior-oracle SHA-256
`32c6d7a9c9ffd364074ae40a872897a22128089b5ae01547d12571a48d05e242`.

The frozen result set is
`<clp-ffi-py checkout>/build/kv-ir-efficient-combined-final-paired-20-cpu15-final-v1`. Its
`results.json`, `acceptance.json`, and `extended-metrics.json` files are mode `0444` and have the
SHA-256 values recorded above.

Local downstream validation sets `CLP_FFI_PY_CLP_S_RUST_WORKSPACE_DIR` to this workspace. The
current `clp-ffi-py` `src/clp` gitlink predates the Rust workspace and therefore cannot build the
Rust-backed wheel from its default submodule path. A distributable downstream change must advance
that gitlink to a commit containing both `components/clp-s` and `components/clp-s-ffi`; an external
workspace override is a development integration path, not a shipping layout.

As a diagnostic checkpoint, the exact 100,000-event streams currently decode in the Rust core at
2.331 million stable-schema events/s, 2.552 million schema-churn events/s, and 1.425 million
encoded-array events/s, with a 20.25 MiB peak RSS. Full byte coverage, leaf-kind counts, and
semantic digests are checked. These figures establish core headroom but deliberately exclude
Python object construction; they do not satisfy the public-API performance gate above.

The matching native producer checkpoint serializes 3.154 million stable-schema events/s, 2.762
million schema-churn events/s, and 0.986 million encoded-array events/s with about 19.3 MiB peak
RSS. It uses the same two borrowed MessagePack maps that cross the Python boundary and checks exact
C++ stream sizes and hashes. For the binding-oriented read path, incremental decode, compact event
ownership, complete node/span/scalar traversal, and semantic hashing sustain 0.968 million,
0.986 million, and 0.557 million events/s respectively, with about 20.0 MiB peak RSS. Packing
schema keys, compacting schema/materialization nodes, and retaining the hash-indexed hot path
improved those three rates by a further 4.0%, 3.7%, and 2.2% over the immediately preceding core
build. These remain native diagnostics: only paired runs through the unchanged Python serializer
and deserializer-plus-`to_dict` APIs can close the acceptance gate.

For each supported stream dialect and integer-width variant, fixtures must cover schema-tree nodes,
auto-generated fields, user fields, timestamps, CLP strings, arrays, empty values, stream metadata,
and multiple schema changes. Compare archive output semantically and compare direct-search results
with archive search for every query feature supported by both paths. Unsupported direct-search
features and their fallback behavior are part of CLI characterization, not an excuse to skip an
input silently.

The current Rust CLI now uses the public streaming KV-IR searcher for the C++ direct-search route.
It preserves the case-sensitive `.clp.zst` substring test over the complete resolved path, searches
only the first explicit stream, emits strict-UTF-8 C++-shaped JSONL, and ignores `--tge`/`--tle`
with a warning. Projection, aggregation, and non-stdout output warn and search the input as an
archive. A raw-reader open failure is fatal; only zstd or decoder failure before the first parsed
stream header falls back to archive search. Zstd decoder initialization and tagged physical-reader
failures remain fatal, matching the phases outside C++ `make_deserializer`. Once the header has been
accepted, decoder, protocol, query, and output failures are fatal except for the exact
schema-node-ID truncation category that the pinned C++ real-time path treats as warning/success.
Exhaustive prefix-cut library tests and focused CLI tests pin that distinction, including retention
of already committed JSONL records.

## Golden fixtures

Golden inputs and archives must be immutable, reviewable, and usable without network services. Give
each fixture a sidecar manifest containing:

* A stable fixture ID, short purpose, feature tags, and expected success or failure phase.
* The complete baseline identity described above.
* The shell-independent argument vector used to generate it and the names—not values—of relevant
  environment variables.
* Source input paths, byte lengths, SHA-256 hashes, provenance, and redistribution/license status.
* Archive layout and version; every archive member's logical name, byte length, and SHA-256; for a
  directory archive, the deterministic bundle hash as well.
* Actual generated archive IDs and all expected archive-stat fields.
* Expected extraction and search artifacts, specifying whether comparison is byte-exact,
  line-order-sensitive, structural JSON, or an unordered multiset.
* The required feature/configuration tuple: array mode, float-format retention, timestamp key,
  log-order recording, range index, archive splitting, compression level, and table thresholds.
* For a corrupt fixture, the parent fixture ID and a declarative description of each mutation,
  including logical section and byte offset where stable.

Never place credentials, authenticated URLs, machine-specific absolute paths, current timestamps,
or unseeded random source data in a fixture. Preserve generated UUIDs as fixture facts. Creating a
fixture with new generator versions creates a new fixture or a reviewed manifest revision; it does
not silently replace the oracle bytes.

The initial corpus must include every stable `NodeType`, both array representations, every float
representation, signed numeric boundaries, timestamp precisions and deprecated date columns if
legacy support is retained, empty and null values, high-cardinality dictionaries, schema churn,
split inputs, multiple archives, log-order deltas, range-index metadata, and unknown-but-skippable
metadata packets. Negative cases include bad magic, unsupported compression, invalid packet sizes,
bad offsets, truncated zstd/MessagePack data, invalid node IDs, excessive nesting/counts, and
trailing data.

## Library-specific compatibility

The Rust public API does not have to mimic the C++ class layout. It does have to make all CLI core
operations possible without global state or process I/O:

* Constructors validate configuration but do not create output directories, initialize service
  clients, configure logging, or write output.
* Record and search sinks support streaming borrowed data for synchronous consumption, with an
  owned convenience adapter for bindings and asynchronous destinations.
* Errors are typed and carry stable broad categories plus path/section/offset context. Library code
  does not log, terminate the process, or panic across an FFI boundary.
* Reader/search instances are independent. Thread safety and cancellation are explicit rather than
  hidden in globals.
* Resource limits are caller-configurable and have conservative defaults suitable for untrusted
  archives and binding callers.

The first binding surface should use opaque handles and callbacks over a documented C ABI. Rust
traits, lifetimes, concrete AST nodes, and archive struct layouts are not an ABI. Language bindings
may offer idiomatic wrappers while sharing the same semantic test corpus.

## Implementation architecture and sequence

The replacement is organized inward from format primitives rather than outward from the command
line. The CLI is an adapter over the same public operations that bindings and embedded users call;
it must not own archive algorithms, JSON reconstruction, search semantics, or split state.
The Cargo `cli` feature is enabled by default for ordinary package builds, but the executable and
its optional Clap dependency are gated behind it so embedding and binding builds can select
`--no-default-features` and compile only the archive library.

The reader path has four layers:

1. Checked header, metadata-packet, section-directory, schema, dictionary, table-metadata, and
   packed-frame decoders.
2. Borrowing typed column views. Fixed-width columns remain zero-copy and delta columns expose
   sequential cursors so a record scan is linear rather than repeatedly decoding each prefix.
3. A schema-specific extraction program compiled once. It records structural operations and stable
   table-local column indexes while omitting the metadata namespace from reconstructed JSON.
4. Record and archive iterators. A table iterator reuses escaped keys, timestamp programs, and
   formatting scratch; ordered archive extraction performs a heap merge on `_log_event_idx` across
   tables, matching the C++ ordered constructor without materializing every output record.

SFA and directory layouts converge below checked catalog and packed-stream operations. The SFA
source seeks to metadata-validated bounded ranges in one caller-owned stream; the directory source
opens the eight canonical physical members and verifies all seven data-member sizes against the
same directory metadata. Both implement the object-safe `ArchiveReader` trait used by high-level
extraction, so bindings can select a format at runtime without duplicating archive algorithms.
Catalog, table, extraction, and search code must not discover paths themselves. Filesystem, URL,
S3, retry, and authentication behavior remains in source adapters above it.

High-level extraction targets a synchronous borrowed `JsonlRecordSink`. Each callback receives one
complete newline-terminated record plus physical table/row coordinates and, in ordered mode, its
validated canonical log-event index. The `Write` convenience adapter preserves the ordinary
streaming JSONL API. Chunk rotation, FFI callbacks, and protocol framing can therefore retain
record boundaries without moving filesystem or service policy into the archive engine.

The search path keeps parsing, archive compilation, row matching, projection, aggregation, and
output protocols separate. A bounded owned `ParsedQuery` is archive-independent and reusable.
Compiling it against one `ArchiveCatalog` performs namespace and schema binding, range/timestamp
pruning, and dictionary presearch, producing table-local predicate programs. A bound table matcher
streams row coordinates; projection reuses extraction programs, while aggregators consume typed
columns directly instead of serializing and reparsing JSON. CLI handlers then translate the same
coordinates into NDJSON, MessagePack tuples, result-cache entries, or reducer messages.

The pinned parser has several compatibility traps that must be differential tests before the Rust
parser is accepted: `AND` and `OR` have equal precedence and associate left-to-right; quoting does
not prevent numeric/Boolean/null literal typing; bare values mean `*: value`; nested `key:{...}`
prefixes every inner path; and the weakly documented `key:(...)` list form is accepted. Search scans
physical table order rather than log-event order. Repeated structured values have existential
matching semantics, and two predicates may match different array elements. The current C++
unstructured-array implementation also appears to route every non-equality numeric comparison
through inequality; that behavior requires black-box characterization and an explicit quirk
decision rather than accidental reproduction.

The writer path uses the following ownership boundaries:

| Layer | Responsibility |
| --- | --- |
| Record input | Borrowed `RecordRef`/`ValueRef` trees or fallible flat `RecordEventRef` traversals and bounded reusable parsing scratch; no CLI ownership in the archive core |
| Archive set | Split policy, current archive, exact encoded/source-byte accounting, completed-archive publication, and statistics callbacks |
| Source context | `ArchiveSourceContext` brackets one input; the archive set owns split numbering, archive-local half-open ranges, exact optional range-index packets, and the same ranges in archive statistics |
| Open archive | Archive-local schema/tree registries, dictionaries, timestamp metadata, tables, and counters; finalization consumes this state |
| Schema and table builders | Schema interning, typed column lanes, exact encoded-size accounting, and stable wire serialization |
| Section spools | Seekable, bounded temporary storage for streaming dictionaries and column-major table data |
| SFA/directory sink | Canonical section ordering, metadata frame, header backpatch, flush, and filesystem atomic commit where the sink supports it |

The intended public compression lifecycle is:

```text
ArchiveSetWriter::new(archive_sink, statistics_callback, options)
    -> begin_source(ArchiveSourceContext)
    -> append_record_with_source_bytes(RecordRef, bytes)*
    -> end_source_with_uncompressed_bytes(trailing_bytes)
    -> finish(self)
```

`append_record` first prepares a bounded record plan: validate shape and numeric domains, identify
the schema, stage prospective dictionary/tree IDs, encode CLP variables into reusable scratch,
calculate deltas and size changes, and reserve destination capacity. Only then does it commit the
record. Validation and allocation failures leave the archive unchanged. An I/O error after bytes
have entered a streaming spool poisons the open archive, for which only abort or drop is valid.

Dictionary sections stream to seekable spools with their leading counts backpatched at finish.
Tables require column-major wire output despite record-major input, so column lanes use one bounded
spill arena rather than one unbounded buffer or file descriptor per column. Finalization finishes
dictionaries, serializes the schema sections, sorts and streams whole tables into packed zstd
frames, writes table metadata, computes canonical relative section offsets, writes the compressed
metadata frame, concatenates the sections, and backpatches the existing explicit-width header.

The C++ split threshold is checked after a complete committed record. The compatibility policy
therefore permits one-record overshoot and never divides a record or table. Rotation resets
archive-local dictionaries, tree, tables, timestamp/range state, UUID, and event index while
retaining the creator ID and incrementing the source split number. The encoded-size target remains
a rotation policy, not a memory-safety limit; independent limits cover record bytes, resident and
spilled column bytes, schemas/nodes/columns, dictionary entries and owned bytes, CLP variables,
timestamps, metadata packets, archive bytes, and temporary storage.

The implemented archive-set core now calculates the exact C++ dictionary-plus-encoded-column
rotation metric, atomically associates a record with its consumed source bytes, and offers one
encoded canonical member set to caller-owned publication and statistics callbacks. A source
context emits canonical filename, creator ID, split number, and caller-defined KV metadata as
sorted MessagePack fields in half-open archive-local ranges. Rotation closes the current range and
reopens it at zero only after pending publication and statistics callbacks succeed; the split
number then increments exactly once. This includes C++'s legal final `[0, 0)` range after an exact
boundary split. The packet is omitted when log order is disabled. `ArchiveSetStats` shares the
immutable range array through `Arc`, so callback retry does not clone arbitrary metadata or change
the encoded bytes.

Both callbacks are explicitly retryable, a statistics retry cannot republish an archive, and
dropping a session has no output side effect. `JsonArchiveSetSink::for_source` and
`finish_source` bracket one caller-identified parse-many/NDJSON input and preserve its offsets
across post-record rotation. `KvIrArchiveSetSink::for_source` brackets every validated stream,
including typed user metadata, and closes the context with EOF accounting. The thin local JSON CLI
publishes UUIDv4-named directory or SFA archives and matches the deterministic 387-byte C++ no-log-
order fixture in both layouts. Recursive gzip/zstd probing, source-path transformation, and
structured-array selection are now wired through the JSON adapters and CLI. General-purpose
containers use the public feature-gated streaming archive-set adapter described above. The remote
HTTP(S)/S3 reader, forward-only SFA layer, exact-URL source metadata, CLI authentication routing,
and observed single-attempt compression-probe policy are implemented without hidden path
inference.
Archive search also supports the C++ `network` destination through the reusable MessagePack
adapter: it emits one
transactional five-element tuple per match, deliberately zeroes the metadata fields used by that
handler, opens one TCP connection per archive, and matches a captured 95-byte C++ stream exactly.
The reducer destination likewise remains separate from archive matching: the CLI owns its single
TCP connection while the library adapter owns handshake and framing compatibility. Results-cache
remains a separate service-adapter milestone.

Implementation proceeds through independently interoperable milestones:

1. Read a C++ SFA through typed zero-copy columns and reconstruct committed C++ fixtures exactly.
2. Compile reusable extraction programs, reconstruct every stable current node type, and merge
   ordered rows across schemas.
3. Emit an empty canonical 0.5.0 SFA, then successively add primitive tables, dictionaries and CLP
   strings, formatted floats/timestamps/range metadata, multiple tables/frames, and archive
   rotation. Each step must open with both implementations before the next representation is added.
4. Add streaming JSON ingestion, followed by KV-IR ingestion. Parser selection is benchmark-driven;
   a full `serde_json::Value` tree is not the baseline design because it adds allocation and loses
   source-token information needed for float fidelity.
5. Add KQL/search and aggregation on the validated reader, then service/source/sink adapters.
6. Add the thin `clp-s` CLI, a versioned C ABI over opaque handles, malformed-input fuzzing, and the
   final differential and performance gates.

Hot loops avoid per-record schema traversal, hash lookup, key escaping, timestamp-pattern parsing,
locale-sensitive formatting, and avoidable allocation. Packed streams and large section copies use
reusable buffers. Benchmarks compare throughput, peak RSS, archive size, and extraction/search
latency using paired randomized C++/Rust runs in the pinned manylinux image. A milestone is not
performance-complete merely because its functional tests pass.

## Unresolved policy decisions

Resolve these before the affected milestone. Each decision should name an owner, date, rationale,
and tests changed by the decision.

:::{list-table}
:header-rows: 1

* - Decision
  - Recommended default until resolved
  - Needed by
* - Oldest readable archive version
  - Support every version for which a production or committed fixture can be obtained, including
    pre-0.5 deprecated date columns; explicitly reject the rest
  - Reader milestone
* - Unknown/new archive versions
  - Reject unsupported major/minor versions rather than inheriting the current permissive check;
    retain bounded skipping of unknown metadata packets
  - Format spec
* - Writer version and evolution
  - Write 0.5.0 until Rust output is bidirectionally compatible; version any incompatible layout
    change
  - Writer milestone
* - Byte identity versus semantics
  - Require bidirectional semantic interoperability and archive-size gates, not identical zstd
    frames or UUIDs
  - Golden harness
* - C++ in the final runtime
  - Use C++ only as an oracle or explicitly time-boxed bridge; final core has no C++ runtime
    dependency. Focused C libraries such as zstd/libarchive are allowed
  - Workspace design
* - CLI diagnostic fidelity
  - Preserve data protocols, flags, validation meaning, and success/failure; allow reviewed
    improvements to human-only text
  - CLI milestone
* - Quirk and malformed-input policy
  - Prefer deterministic safe errors over reproducing overflow, excessive allocation, undefined
    behavior, or crashes
  - Reader/ingest milestones
* - Ordering
  - Preserve ordered-mode guarantees; measure and document current default search and traversal
    order before promising determinism elsewhere
  - Reader/query milestones
* - Partial output on failure
  - Keep current behavior in the compatibility runner, then choose and document cleanup/atomicity
    semantics for the library and CLI
  - Public API design
* - Truncated KV-IR
  - Preserve the current direct-search warning-and-success behavior in the CLI; expose a structured
    incomplete-stream outcome in the library
  - KV-IR search milestone
* - Optional service adapters
  - Keep them in executable parity, isolate them from the core crate, and decide packaging/features
    before cutover
  - CLI milestone
* - First stable binding ABI
  - C ABI with opaque handles, explicit ownership, callbacks, cancellation, and version reporting
  - Binding milestone
* - Supported targets
  - Start with little-endian Linux targets supported by the selected manylinux image; make any
    broader portability claim only after cross-target fixtures pass
  - Format spec
:::

## Completion criteria

This contract is fulfilled when:

1. Every hard contract has a named automated test and every revisable discrepancy has an accepted
   decision.
2. The archive and KV-IR matrices pass inside the pinned container on every supported architecture.
3. The Rust CLI passes black-box protocol and filesystem comparisons against the pinned C++ oracle.
4. Malformed input tests and fuzzing demonstrate bounded failure without panics or uncontrolled
   allocation.
5. Performance acceptance passes under the separately recorded CLP-S benchmark protocol.
6. Downstream consumers use the Rust executable/library without requiring the C++ `clp-s` runtime.

Do not remove the C++ oracle or regenerate its goldens as Rust output until these conditions are
met.

[command-line-arguments]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/CommandLineArguments.hpp
[command-line-source]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/CommandLineArguments.cpp
[node-types]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/SchemaTree.hpp
[sfa-definitions]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/core/src/clp_s/SingleFileArchiveDefs.hpp
[task-compression-worker]: https://github.com/y-scope/clp/blob/DOCS_VAR_CLP_GIT_REF/components/clp-tdl-package/src/task/compression/compress.rs
