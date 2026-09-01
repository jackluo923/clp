# CLP-S release workload matrix

The release benchmark manifest should cover the matrix below. Large datasets remain user-supplied
and checksum-pinned rather than committed to the repository. The committed smoke fixture exercises
the harness, not this performance matrix.

## Corpus classes

Use at least one representative production corpus plus deterministic synthetic corpora for:

| Corpus | Required characteristics |
| --- | --- |
| Stable schema | Large NDJSON input, few schemas, moderate-cardinality strings |
| Schema churn | Many object shapes, missing fields, mixed scalar types, nested objects |
| High cardinality | Unique and near-unique variable strings and CLP strings |
| Numeric/timestamp | Integers, exact formatted floats, timestamp patterns and wide time ranges |
| Arrays | Structured and unstructured arrays, nested arrays, empty/null values |
| Large records | Records around parser buffer boundaries and the configured document-size limit |
| Many files | Many small input files and archive splits |
| KV-IR | Valid four-byte and eight-byte encoded streams, including multiple concatenated streams |

Record the dataset license and acquisition procedure outside the harness and put immutable files,
manifest, and checksums in the managed benchmark-data store.

## Workloads

Run directory archives and single-file archives where applicable.

| Operation | Required variants | Primary metrics |
| --- | --- | --- |
| Compression | NDJSON and KV-IR; stable/churn/high-cardinality/arrays; log order on/off; float retention on/off; archive splitting | Input throughput, CPU, RSS, archive size |
| Extraction | Ordered and unordered; directory/SFA; one large and many small archives; chunked ordered output | Output throughput, CPU, RSS |
| Search | Cold and warm experiments kept separate; zero, rare, medium, and all-match selectivity | Wall p50/p95, CPU, RSS |
| Search types | Exact/wildcard/case-insensitive string, numeric, timestamp/range-index, arrays, existence, Boolean combinations | Wall p50/p95, archive bytes per second |
| Projection | Narrow, broad, and all fields at several selectivities | Wall time, result throughput, RSS |
| Aggregation | Count, count-by-time, min, max, and unique | Wall time, CPU, RSS |

Include both one-large-archive and many-small-archive search groups. They expose different startup,
metadata, dictionary, and pruning costs and must not be averaged into a single favorable score.

## Proposed initial gates

These are bootstrap non-inferiority thresholds, not established release policy. First collect at
least 10 repeated pairs per production workload on an isolated runner and quantify normal variance.

| Metric | Suggested gate |
| --- | --- |
| Throughput | paired p05 Rust/C++ >= 0.95 |
| Search wall time | paired p95 Rust/C++ <= 1.05 |
| CPU time | paired median Rust/C++ <= 1.05 |
| Peak RSS | paired median Rust/C++ <= 1.05 |
| Compressed archive size | paired median Rust/C++ <= 1.00 |

In addition to these gates, no individual workload should regress by more than 10% without an
explicitly reviewed explanation. Archive interoperability and result correctness are prerequisites;
a faster incorrect workload is a failure and should never enter this harness's performance summary.

Build C++ and Rust with comparable release, LTO, target-CPU, assertion, allocator, and zstd settings
inside the same pinned `clp-env-base-manylinux_2_28` image. Use the same thread count for both
implementations unless the experiment is explicitly a scaling study.
