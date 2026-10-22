# Column storage and search optimizations for CLP+

## Scope

This port brings separate column frames, adaptive numeric compression, selective column reads, scans of encoded numeric values, and adaptive dictionary-ID lookup to the C++ implementation. It builds on the [CLP+ wildcard optimizations](design-clpp-wildcard-search-implementation.md): encoded record table (ERT) summaries still reject entire ERTs first, and count queries can then avoid reading unrelated columns in surviving ERTs.

The source is [`jackluo923/separate-columns` at `f4b3ef36`](https://github.com/jackluo923/clp/tree/f4b3ef36). The implementation uses the Rust 0.6 numeric layouts while retaining the C++ query evaluator, parsing integration, and reconstruction behavior.

| Optimization | Rust commits | C++ treatment |
|---|---|---|
| Separate column frames and selective reads | `fbe2f9ce`, `3ce1b648` | Ported for count queries with resolved field dependencies; ordinary output and metadata-producing queries read all columns |
| Choose numeric encodings by compressed size | `15a57e03` | Ported for integer and double columns |
| Construct candidates one at a time | `51bbd4e5` | Ported; reuse a Zstd context and candidate buffers rather than retaining all encodings |
| Value-sorted float dictionaries with byte-wide codes | `267fc8d3` | Ported as an additional lossless candidate |
| Choose an exact dictionary-ID membership representation | `14344bfc` | Ported to variable-string column scans |
| Scan numeric columns in their encoded representation | `422a9f15`, `db272333`, `0933623e` | Ported for constants, frame-of-reference integers, dictionaries, and runs; includes packed dictionary scans and integer range checks |
| Reuse decompression buffers and resolve numeric operands before scanning | `a246881c`, `a0094f65` | The C++ reader and `ColumnScan` already provide these mechanisms |
| Skip archives outside timestamp bounds | `ae71fdf6` | Already handled by `EvaluateTimestampIndex` |
| Decimal floats, per-field dictionaries, compact CLP descriptors, binary UUIDs, and timestamp chunk indexes | Other changes on the Rust branch | Further porting work; these formats and execution paths are not introduced here |

## Writing smaller numeric columns

A raw numeric column stores eight bytes per value before Zstd compression. An alternative representation can expose repetition that Zstd cannot easily find in those interleaved bytes.

For each eligible column, `AdaptiveColumn` compresses the applicable candidates with the same Zstd context and level, then retains the smallest frame. Ties keep the earlier candidate. The plain representation remains a candidate, so choosing a numeric codec cannot increase that column's compressed size relative to its tagged plain representation. This does not guarantee a smaller whole archive: separate frames and their metadata add overhead and remove compression opportunities across columns.

| Column type | Candidates |
|---|---|
| Integer | Plain, constant, frame-of-reference offsets from the minimum, first differences, dictionary IDs, and run-length encoding |
| Double | Plain IEEE-754 bits, constant, byte-stream split, dictionary IDs, and run-length encoding |

Dictionary candidates stop tracking distinct values after 65,536 entries. Float dictionaries also try sorting values and using whole-byte codes, which can make slowly changing sequences easier for Zstd to compress. Float encoding preserves every bit, including signed zero and NaN payloads. Decimal conversion is not used.

The codec operates on `Integer` and `Float` columns. Existing `FormattedFloat`, `DictionaryFloat`, timestamp, and CLP-string representations retain their bytes. In particular, the CLI's default float-format retention remains unchanged; `--no-retain-float-format` permits ordinary `Float` columns but retains the existing CLI rendering behavior of that mode.

## Reading fewer columns

Separate ERTs store one Zstd frame per physical column. Table metadata gives each frame's compressed and uncompressed size, allowing the reader to seek past frames that the query does not need.

After schema resolution and constant propagation, `QueryRunner` gathers the field IDs still used by the expression. For a count query without output metadata, `ArchiveReader` selects the corresponding physical columns, including repeated occurrences of a field. `PackedStreamReader` decompresses only those frames, and `SchemaReader` prepares their column readers before evaluation.

Field wildcards with unresolved dependencies retain the complete read path. Queries that reconstruct strings or return timestamp/order metadata also read all columns.

The reader validates frame extents, column counts, codec tags, dictionary indices, run totals, and integer reconstruction bounds. Separate streams must contain exactly one ERT at offset zero. Rereading the current separate stream can load columns omitted by an earlier selection.

## Searching numeric columns without expanding them

Zstd decompression produces the numeric codec's payload. Expanding that payload into eight bytes per row would add allocation and copying before any predicate runs. `NumericColumnView` instead retains validated views of encodings that support direct access. This changes the reader and evaluator; the archive format and writer are unchanged.

| Representation | How the predicate runs |
|---|---|
| Constant | Compare once and fill the result bitmap when it matches |
| Dictionary | Compare each dictionary value once, then scan the packed row codes using those results |
| Run-length | Compare once per run and mark its matching rows |
| Frame-of-reference integer | Use the base and bit width to prove all or no rows match when possible; otherwise compare values reconstructed from offsets during the scan |

Dictionary codes of 1, 2, 4, or 8 bits use byte lookup tables on sufficiently large columns. Sixteen-bit codes use direct loads. Other widths use a sequential bit unpacker. Resolved numeric wildcard predicates use the same paths, so a repeated dictionary value is converted to decimal and wildcard-matched once rather than once per row. Ordinary numeric comparisons preserve the existing signed integer and IEEE-754 behavior, including comparisons involving NaNs and signed zero.

Tagged plain columns borrow their values directly from the decompressed frame. Delta integers and byte-stream-split floats still expand before evaluation. Record output and predicates that need individual values can read them through the existing column-reader interface. This avoids forcing every consumer to understand the numeric encoding.

Validation occurs before any value is accessed: dictionary codes must reference valid entries, run lengths must account for every row, packed padding must be zero, and integer reconstruction must remain in range. A fast all-or-none predicate cannot bypass these checks. When several physical columns represent the same field, each scan preserves matches already contributed by another column.

## Faster dictionary-ID predicates

Variable-string wildcard matching resolves to a set of matching dictionary IDs. `DictionaryIdSet` chooses the lookup representation once per distinct matching set, and `QueryRunner` caches it across ERTs:

* Up to four IDs use direct comparisons.
* Dense ID ranges use a bitmap, capped at 128 KiB.
* Sparse or large ranges retain the existing hash set.

All representations implement exact membership. The scan still applies the existing equality, inequality, and expression-inversion behavior.

## Options and compatibility

The new storage paths are opt-in:

```sh
clp-s --experimental c \
    --parsing-specification parsing.spec.cached.txt \
    --adaptive-numeric-columns \
    --separate-columns-min-size 1048576 \
    archives input.jsonl
```

`--separate-columns-min-size N` separates ERTs of at least `N` uncompressed bytes. With a nonzero threshold, smaller ERTs remain packed even when adaptive encoding is enabled. With only `--adaptive-numeric-columns`, every ERT containing an eligible numeric column is separated. Without either option, the writer retains the existing packed layout.

| Configuration | Version and behavior |
|---|---|
| Existing options only | Archive 0.5.1; existing packed columns |
| Separate frames only | Archive 0.5.1's reserved separate-column metadata section; numeric values remain plain |
| Adaptive numeric encoding | Archive 0.6.0; numeric columns in separate frames carry a scheme tag; packed ERTs retain their existing bytes |

The new reader accepts legacy archives. Older C++ readers reject the nonempty separate-column metadata section; they cannot read archives that use the new column layout. The new reader rejects unsupported newer minor versions rather than interpreting their bytes as this format. Versions 0.7 and later from the Rust branch require additional readers and are not supported by this port.

## Validation and measurement

Build and test inside the project's `clp-core-dependencies-x86-manylinux_2_28:dev` image with the C++ dependencies mounted. Configure `CLP_BUILD_CLPP_DECOMPOSITION=ON` and `CLP_BUILD_TESTING=ON`, then build `clp-s` and `unitTest`.

```sh
unitTest '[AdaptiveColumn],[NumericColumnView],[DictionaryIdSet],[SeparateColumns]'

python3 components/core/tools/benchmarks/clpp-column-port.py \
    --baseline /path/to/baseline/clp-s \
    --candidate /path/to/modified/clp-s \
    --output /path/to/new-results-directory \
    --clpp-input /path/to/hive-sample.jsonl \
    --parsing-spec /path/to/parsing.spec.cached.txt
```

The benchmark harness generates a deterministic numeric fixture, compares baseline, unchanged storage, separate-frame storage, and adaptive storage, and checks ordered extraction and query counts. It also verifies that the new binary reads baseline archives. Query timings include process startup and use a warmup followed by three interleaved runs per configuration. Ingest timings are medians of three runs; extraction is checked once per configuration. Measurements use warm caches on a shared host, so small timing differences should not be treated as stable gains.

Unit coverage includes every numeric wire scheme, signed integer limits, floating-point bit patterns, truncated payloads, invalid frame metadata, exact dictionary membership, and archive round trips with both archive containers and array layouts. Codec mutation checks also run under AddressSanitizer and UndefinedBehaviorSanitizer.

### Compression results

The column-frame comparison uses C++ commit `a994dc71` as the baseline and `3aa39f93` for the adaptive writer and reader, built with the same manylinux image and dependencies. The numeric fixture contains 100,000 records. The CLP+ workload contains 274,581 Hive records, sampled every 25th record from a 24-hour input, with string decomposition enabled. The adaptive configuration uses a 1 MiB ERT threshold; the numeric fixture also uses `--no-retain-float-format`.

| Workload | Baseline archive | Adaptive archive | Size reduction | Baseline ingest | Adaptive ingest |
|---|---:|---:|---:|---:|---:|
| Numeric fixture | 1,350,286 bytes | 994,164 bytes | 26.4% | 159 ms | 189 ms |
| Hive sample | 4,648,520 bytes | 4,624,248 bytes | 0.5% | 2.71 s | 2.51 s |

Trying several numeric encodings costs CPU during ingestion. It pays off substantially for the numeric fixture, while the mostly textual Hive sample changes little. Separating every ERT without adaptive encoding increases the numeric archive to 1,426,138 bytes, illustrating why independent frames alone do not guarantee better compression. A threshold also avoids paying frame overhead for tiny ERTs.

### Search results

These count queries use the same archives and adaptive configuration as the compression comparison.

| Workload | Query | Matching records | Baseline | Adaptive |
|---|---|---:|---:|---:|
| Numeric | `seq: 999` | 1 | 24.9 ms | 13.7 ms |
| Numeric | `state: b*` | 33,333 | 27.2 ms | 15.2 ms |
| Numeric | `x > 50.05` | 24,891 | 25.7 ms | 16.5 ms |
| Numeric | `*: busy` | 33,333 | 35.7 ms | 37.4 ms |
| Hive | `message: *blk_1073746491_5667*` | 3 | 88.7 ms | 91.0 ms |
| Hive | `message: *a*b*` | 117,705 | 170.5 ms | 164.4 ms |
| Hive | `message.blockID.blockNum: 1073746491` | 2 | 83.4 ms | 83.7 ms |

The numeric equality and string wildcard queries benefit from skipping unrelated column frames. `*: busy` retains the complete read path and pays numeric decoding cost without that saving. Hive queries change little: the existing wildcard planning and ERT pruning still account for much of the work, and only larger ERTs use separate frames at this threshold. The port therefore improves particular storage and scan workloads rather than every wildcard query.

### Effect of scanning encoded numeric values

This comparison isolates the reader change: both configurations use adaptive storage with a 1 MiB ERT threshold, and the baseline is `3aa39f93`. The numeric fixture contains 1,000,000 records. Reproduce it with the benchmark harness by adding `--baseline-adaptive --rows 1000000`.

| Query | Matching records | Expand before scanning | Scan encoded values | Speedup |
|---|---:|---:|---:|---:|
| `constant: 42` | 1,000,000 | 23.5 ms | 14.4 ms | 1.63× |
| `constant: 99` | 0 | 22.8 ms | 13.3 ms | 1.71× |
| `x > 50.05` | 249,841 | 42.8 ms | 22.2 ms | 1.93× |
| `sensor: *67*` | 20,011 | 45.4 ms | 37.3 ms | 1.22× |
| `mixed: -9223372036854775808` | 10,000 | 42.1 ms | 32.5 ms | 1.30× |
| `*: busy` | 333,333 | 262.1 ms | 205.6 ms | 1.27× |
| `seq: 999` | 1 | 24.9 ms | 26.7 ms | 0.93× |

Constants avoid materialization and repeated comparisons; dictionaries avoid comparing the same value for every row. Numeric wildcards also save repeated decimal conversion and pattern matching. Even a query such as `*: busy`, which retains all columns, can avoid expanding unrelated numeric columns. Delta-encoded sequences still use the ordinary decoder, so `seq: 999` receives no direct scan benefit.

The writer and archive layout are unchanged. Ordered extraction and query counts agree with the baseline. Six interleaved extraction measurements on identical archives give medians of 806 ms versus 798 ms for the numeric fixture and 335 ms versus 337 ms for the Hive sample; these small differences do not establish an extraction speedup.
