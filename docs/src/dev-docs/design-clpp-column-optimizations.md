# Column storage and search optimizations for CLP+

## Scope

This port reduces how much data C++ CLP+ stores, reads, and evaluates. It builds on the [CLP+ wildcard optimizations](design-clpp-wildcard-search-implementation.md): search first checks whether integer summaries rule out every candidate encoded record table (ERT), then loads the dictionaries needed to prepare any remaining query. It rejects ERTs that cannot match, counts an ERT directly when metadata proves every row matches, and reads the required columns from the remaining ERTs. Adaptive encodings reduce column sizes and let eligible predicates run without expanding every value.

The ports draw on [`jackluo923/separate-columns` through `75367842`](https://github.com/jackluo923/clp/tree/75367842). The implementation reuses the Rust numeric payload layouts and adapts its ERT range proofs to existing C++ integer summaries, while retaining the C++ query evaluator, parsing integration, and reconstruction behavior. Dictionary-ID codecs use a separately versioned C++ archive extension; this does not add support for current Rust archives.

| Optimization | Rust commits | C++ treatment |
|---|---|---|
| Separate column frames and selective reads | `fbe2f9ce`, `3ce1b648`, `f652e684` | Ported for predicates, projected records, and output metadata; output-only frames load after an ERT matches |
| Count proven matches from ERT metadata | `0d2b46ef` | Ported through the C++ evaluator's proof of the complete query; only count handlers accept a bulk count |
| Prove range predicates from ERT summaries and remove covered column dependencies | `54abc872`, `cc292f7a` | Adapted to existing CLP+ integer summaries; timestamp summaries and numeric aggregate pushdown remain separate work |
| Encode and scan dictionary-ID columns | `12828b82`, `a4c6b36c` | Ported for variable strings and dictionary-backed floats; legacy CLP-string descriptor buffers retain their layout |
| Load only the dictionaries a query needs | `2a6168a7`, `3ce1b648`, `ffe8e6aa` | Predicate dictionaries remain complete; output dictionaries load after a match, with resumable variable-dictionary prefixes for separate-column ERTs |
| Reject impossible archives before loading predicate dictionaries | `de53a8f2`, `75367842` | Integer proofs run before dictionary preparation; CLP+ string shape rejection already runs during schema matching |
| Prepare and reuse dictionary string predicates | `de53a8f2` | Exact-literal matching and query caches already exist; prepared case folding removes per-entry string allocations |
| Choose numeric encodings by compressed size | `15a57e03` | Ported for integer and double columns |
| Construct candidates one at a time | `51bbd4e5` | Ported; reuse a Zstd context and candidate buffers rather than retaining all encodings |
| Value-sorted float dictionaries with byte-wide codes | `267fc8d3` | Ported as an additional lossless candidate |
| Choose an exact dictionary-ID membership representation | `14344bfc` | Ported to variable-string column scans |
| Scan numeric columns in their encoded representation | `422a9f15`, `db272333`, `0933623e` | Ported for constants, frame-of-reference integers, dictionaries, and runs; includes packed dictionary scans and integer range checks |
| Reuse decompression buffers and resolve numeric operands before scanning | `a246881c`, `a0094f65` | The C++ reader and `ColumnScan` already provide these mechanisms |
| Skip archives outside timestamp bounds | `ae71fdf6` | Already handled by `EvaluateTimestampIndex` |
| Fuse legacy string-token scanning | `ff72ff6b` | Further porting work; this parser is separate from CLP+'s log-surgeon parsing path |
| Decimal floats, per-field dictionaries, compact CLP descriptors, binary UUIDs, and timestamp chunk indexes | Other changes on the Rust branch | Further porting work; these formats and execution paths are not introduced here |

## Writing smaller numeric columns

A raw numeric column stores eight bytes per value before Zstd compression. An alternative representation can expose repetition that Zstd cannot easily find in those interleaved bytes.

For each eligible column, `AdaptiveColumn` compresses the applicable candidates with the same Zstd context and level, then retains the smallest frame. Ties keep the earlier candidate. The plain representation remains a candidate, so choosing a numeric codec cannot increase that column's compressed size relative to its tagged plain representation. This does not guarantee a smaller whole archive: separate frames and their metadata add overhead and remove compression opportunities across columns.

| Column type | Candidates |
|---|---|
| Integer | Plain, constant, frame-of-reference offsets from the minimum, first differences, dictionary IDs, and run-length encoding |
| Double | Plain IEEE-754 bits, constant, byte-stream split, dictionary IDs, and run-length encoding |

Dictionary candidates stop tracking distinct values after 65,536 entries. Float dictionaries also try sorting values and using whole-byte codes, which can make slowly changing sequences easier for Zstd to compress. Float encoding preserves every bit, including signed zero and NaN payloads. Decimal conversion is not used.

The numeric option operates on `Integer` and `Float` columns. The dictionary option extends the integer codecs to `VarString` and `DictionaryFloat` ID columns, as described below. `FormattedFloat`, timestamp, and legacy CLP-string payloads retain their bytes. The CLI's default float-format retention remains unchanged; `--no-retain-float-format` permits ordinary `Float` columns but retains the existing CLI rendering behavior of that mode.

## Counting an ERT without reading its records

An ERT's metadata includes its record count. When the query evaluator proves that every record in the ERT matches the complete query, a count query can add that number directly. For example, `message: *` can count records in ERTs containing that field. A wildcard can also qualify when the fixed text in an ERT's string shape (template) guarantees a match, regardless of its variable values.

`QueryRunner::schema_init` simplifies the expression for each ERT to true, false, or unknown. `Output` skips false ERTs, adds the record count for true ERTs, and scans unknown ERTs. The proof must cover the entire expression, including AND, OR, NOT, and any time bounds. A match in a string shape alone is insufficient when another predicate still depends on record values. Existence checks inside unstructured arrays also require evaluation of those arrays.

Only output handlers that explicitly support bulk counting can take this path. Ordinary record output, output metadata, and time-bucketed counts retain record evaluation. Both local count aggregation and reducer count output accept bulk counts while preserving their result format and checking for overflow.

The reader opens the record-data section only when an ERT requires scanning. If every ERT is rejected or counted from metadata, that section remains unopened. Query preparation may still read dictionaries needed to establish the result. This optimization applies to existing packed archives as well as separate-column archives and requires no storage option or format change. Telemetry reports the ERTs and records counted from metadata, and uses the `metadata_count` termination stage when no ERT was scanned.

## Proving integer ranges before reading columns

CLP+ already stores the smallest and largest integer values for each field in an ERT's `ColumnValueFilter`. Equality and numeric-prefix wildcard queries use these summaries to reject impossible matches. Range predicates now use the same bounds to determine whether every value, no value, or only some values could match, before loading the column.

For an ERT whose `seq` values range from 100 through 199:

| Predicate | Proof | Effect on a count query |
|---|---|---|
| `seq < 100` | No value matches | Skip the ERT |
| `seq >= 100 AND seq < 200` | Every value matches the complete query | Add the ERT's record count from metadata |
| `seq >= 100 AND sensor > 1950` | The `seq` predicate is always true | Evaluate only `sensor > 1950`; separate-column archives also leave the `seq` frame unread |
| `seq >= 150` | The range crosses the boundary | Read and evaluate `seq` |

`QueryRunner` evaluates `<`, `<=`, `>`, and `>=` at both endpoints. If both results agree, the predicate has that result throughout the range; otherwise it remains unresolved. The proof uses the same operand conversion and comparison functions as row evaluation, including rounding fractional operands for integer comparisons. It compares endpoints directly without incrementing or decrementing signed integer limits.

The result feeds the existing AND, OR, and NOT simplification before column selection. A true predicate can disappear from an AND, and a false predicate can disappear from an OR. Counting still requires proof of the complete expression. Different ERTs can therefore be rejected, counted from metadata, or scanned within one query without counting a record twice. Record reconstruction still reads the columns needed to produce the output.

This adaptation requires an existing integer summary. Missing or empty summaries, unresolved field paths, and fields inside unstructured arrays retain their evaluation path. A summary for a repeated integer field covers all its physical occurrences; endpoint agreement remains sufficient to prove the result. Dictionary-ID bounds describe IDs rather than string ordering and are never used for string range proofs. Timestamp and floating-point columns have no applicable summary in this implementation. Ordinary CLP-S archives without CLP+ summaries also retain their current range scans.

The writer and archive bytes are unchanged. Packed ERTs can be skipped or counted in full; skipping an individual column frame requires separate-column storage. The Rust changes also use timestamp summaries and aggregate metadata, which this integer adaptation does not introduce.

## Rejecting archives before loading predicate dictionaries

Consider `seq > 100 AND state:busy` when every candidate ERT has `seq` values below 100. The integer summaries prove the result empty, so searching the variable dictionary for `busy` would be wasted work. `Output` now asks `QueryRunner::may_match_before_dictionaries` to check those summaries before loading predicate dictionaries or preparing string queries. Schema resolution and CLP+ string shape preparation still precede this check.

This pass reuses the existing integer equality, numeric-prefix wildcard, and range proofs. It evaluates each candidate ERT's resolved expression without changing that expression. Every other predicate remains unknown: an unloaded dictionary is never treated as empty. A false integer condition rules out an AND, but `seq > 100 OR state:busy` still requires the dictionary. NOT reverses proven truth values and leaves unknown values unknown.

Search finishes with an empty result only when every candidate ERT is proven impossible. If any ERT remains possible, dictionary preparation and the normal per-ERT evaluator continue. This pass does not remove dictionary dependencies from individual surviving branches. The output handler is finalized even for an empty result, and telemetry reports `metadata_pruning` with no scanned ERTs.

The check applies to both packed and separate-column CLP+ archives with existing column summaries. Forward-only inputs and archives without those summaries retain ordinary preparation. CLP+ string shape rejection already happens before predicate dictionary loading; the Rust precheck for legacy CLP-string templates is separate work. Archive bytes and compression ratios are unchanged.

## Reading fewer columns

Separate ERTs store one Zstd frame per physical column. Table metadata gives each frame's compressed and uncompressed size, allowing the reader to seek past frames that the query does not need.

After schema resolution and constant propagation, `QueryRunner` gathers the field IDs still used by the expression. `ArchiveReader` selects the corresponding physical columns, including repeated occurrences of a field. `PackedStreamReader` decompresses only those frames, and `SchemaReader` prepares their column readers before evaluation.

For record output, the first matching row triggers a second read of only the missing output columns. The existing JSON output plan supplies these dependencies: its column readers cover projected fields and repeated array elements, and its compiled string shapes identify the columns needed for CLP+ reconstruction. Timestamp and record-order columns are included when the output handler needs metadata. A shape-only projection can therefore omit variable columns that a reconstructed string would need. The initial column readers, decoded buffers, and filter bitmap remain valid throughout this preparation; neither predicate frames nor the filter need to be evaluated again.

An ERT with no matches never loads output-only frames. Counts need no output columns. Unresolved field wildcards conservatively read all potentially searched columns, and packed ERTs still require their shared frame. Forward-only inputs read output columns during their initial pass because returning to an earlier frame is unsupported.

The reader validates frame extents, column counts, codec tags, dictionary indices, run totals, and integer reconstruction bounds. Separate streams must contain exactly one ERT at offset zero. Rereading the current separate stream can load columns omitted by an earlier selection.

## Reading only needed dictionaries

Reading fewer column frames does not help if search first decompresses a large dictionary for an unrelated field. For example, counting `seq: 999` needs the integer `seq` column, even when the same ERT has a text column backed by a large string shape dictionary.

After resolving fields and matching ERTs, `SchemaMatch::prepare_dictionaries` derives dictionary dependencies from each ERT's query. An integer or ordinary double predicate needs no string dictionary. Variable strings and dictionary-backed floats need the variable dictionary. A CLP-string predicate needs both variable and logtype dictionaries; an unstructured-array predicate needs the variable and array dictionaries. Resolved field-existence predicates need no dictionary values, but existence checks inside unstructured arrays still need the array's dictionaries. Unresolved field wildcards conservatively retain all potentially searched dictionary dependencies.

Dictionaries used only for output load after an ERT matches, according to its output plan. Local single-file archives and memory buffers use bounded section cursors that retain independent positions, allowing dictionary reads between table reads. Directory archives use independent files. Forward-only inputs, including network streams and readers whose seek capabilities are unknown, retain ordered loading.

For a separate-column ERT, search can also load a prefix of the variable dictionary. It finds the highest dictionary ID referenced by the output columns, then decodes entries from ID zero through that ID. For example, references through ID 99 require the first 100 entries, even if the archive contains a million entries. Constants, dictionaries, and runs expose their highest ID without expanding every row. Legacy CLP-string and array columns use their placeholder types to distinguish dictionary references from encoded numbers.

The dictionary reader retains its Zstd decoder, compressed input buffer, and decoded entries. A later ERT can extend the prefix without rereading compressed bytes or copying the existing entries into a new snapshot. Predicate preparation always loads the complete variable dictionary, so an unloaded entry cannot be mistaken for an absent value during equality, wildcard, or negated searches.

Prefix discovery includes every row in the selected output columns, preserving whole-column validation of adaptive dictionary IDs. A high ID in a nonmatching row can therefore force a complete dictionary read. Packed ERTs and unsupported discovery retain full-dictionary loading after a match. The decoder validates consumed entries immediately and validates the frame ending, checksum, and trailing data when the complete prefix is requested; an unread tail remains unchecked. Decompression buffering may read beyond the last requested entry.

These changes use existing archive bytes and do not require a writer option or format version. The largest savings occur when the predicate needs no variable dictionary and the output references only early dictionary entries. Per-field dictionaries remain separate porting work.

## Preparing dictionary string matches

String predicates search the variable dictionary once, then reuse the matching dictionary IDs across ERTs. Exact literals already bypass wildcard matching. Case-insensitive matching now also avoids allocating and converting a string for every dictionary entry.

For exact literals, the reader builds a byte-to-uppercase table, converts the query once, and compares dictionary bytes through the table. Length mismatches and unequal bytes can reject an entry immediately. For wildcards, `PreparedWildcard` converts the pattern once and folds dictionary bytes as the existing matcher visits them; it does not copy the complete entry. The same wildcard algorithm handles escapes and backtracking in both case modes.

The two paths retain their existing case-conversion conventions: exact literals use the C++ locale's uppercase mapping, while wildcards use the C locale's lowercase mapping. Neither introduces Unicode case folding. Preparing the mapping once preserves NUL bytes, non-ASCII bytes, escaped wildcards, and the existing matching-ID caches. These reader changes apply to ordinary C++ CLP-S archives as well as CLP+ archives.

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

## Compact dictionary-ID columns

A `VarString` column stores an ID into the variable dictionary for each row. CLP+ uses these columns for extracted string variables. A `DictionaryFloat` column also stores IDs, but its dictionary entries preserve decimal text that cannot be represented by the ordinary formatted-float encoding. Compressing these IDs therefore benefits both string predicates and some numeric predicates without changing the stored text.

With `--adaptive-dictionary-columns`, separate frames for these two column types use the integer codec candidates: plain IDs, a constant ID, frame-of-reference offsets, first differences, a local dictionary of IDs, or runs. The writer chooses by compressed size after Zstd, just as it does for numeric columns. A local dictionary here maps compact row codes to IDs in the archive's variable dictionary; it does not replace that archive dictionary.

The reader retains constants, frame-of-reference offsets, local dictionaries, and runs in compact form. String predicates test those IDs against the existing matching-ID set. Float predicates resolve an ID to its decimal value using the existing conversion: once for a constant, once per local dictionary entry, or once per run. Numeric wildcards reuse the same result, avoiding repeated conversion and matching. Plain IDs remain directly readable, and delta IDs expand before scanning.

The reader validates encoded structure and verifies that referenced IDs exist before using the column. Reference validation is delayed until first use so loading an unrelated column does not force its dictionary to be read. Extraction still obtains the original dictionary text, including the original decimal spelling. Legacy `ClpString` descriptor and variable buffers are not encoded by this option; the Rust shortcuts for those buffers need a separate port.

## Options and compatibility

The new storage paths are opt-in:

```sh
clp-s --experimental c \
    --parsing-specification parsing.spec.cached.txt \
    --adaptive-dictionary-columns \
    --separate-columns-min-size 1048576 \
    archives input.jsonl
```

`--separate-columns-min-size N` separates ERTs of at least `N` uncompressed bytes. With a nonzero threshold, smaller ERTs remain packed even when adaptive encoding is enabled. With a zero threshold, `--adaptive-numeric-columns` separates ERTs containing eligible numeric columns; `--adaptive-dictionary-columns` also includes ERTs containing variable-string or dictionary-float columns and enables adaptive numeric encoding. Without any of these options, the writer retains the existing packed layout.

| Configuration | Version and behavior |
|---|---|
| Existing options only | Archive 0.5.1; existing packed columns |
| Separate frames only | Archive 0.5.1's reserved separate-column metadata section; numeric values remain plain |
| Adaptive numeric encoding | Archive 0.6.0; numeric columns in separate frames carry a scheme tag; packed ERTs retain their existing bytes |
| Adaptive dictionary-ID encoding | Archive 0.6.1; also tags eligible dictionary-ID columns in separate frames; packed ERTs retain their existing bytes |

The new reader accepts existing C++ 0.5.1 and 0.6.0 archives. Readers predating separate columns reject their nonempty metadata section. Readers supporting 0.6.0 also reject the 0.6.1 dictionary extension: its separate-column metadata begins with a `UINT64_MAX` marker and extension version `1`, before the usual stream count. The marker cannot be a valid stream count, so readers that ignore the header's patch version still reject the archive. The extension is emitted whenever the dictionary option is enabled, even if every ERT remains packed.

The new reader rejects unknown extension versions and archive versions newer than 0.6.1. Rust 0.7 and later require additional readers and are not supported by this port.

## Validation and measurement

Build and test inside the project's `clp-core-dependencies-x86-manylinux_2_28:dev` image with the C++ dependencies mounted. Configure `CLP_BUILD_CLPP_DECOMPOSITION=ON` and `CLP_BUILD_TESTING=ON`, then build `clp-s` and `unitTest`.

```sh
unitTest '[AdaptiveColumn],[NumericColumnView],[DictionaryIdSet],[DictionaryColumns],[SeparateColumns],[SelectiveDictionaries],[OutputColumns],[DictionaryPrefix],[MetadataCounts],[IntegerRangeProofs],[ColumnValueFilter],[MetadataPreflight],[DictionaryMatching],[wildcard]'

python3 components/core/tools/benchmarks/clpp-column-port.py \
    --baseline /path/to/baseline/clp-s \
    --candidate /path/to/modified/clp-s \
    --output /path/to/new-results-directory \
    --clpp-input /path/to/hive-sample.jsonl \
    --parsing-spec /path/to/parsing.spec.cached.txt
```

The benchmark harness generates a deterministic numeric fixture, compares baseline, unchanged storage, separate-frame storage, and adaptive storage, and checks ordered extraction and query counts. It also verifies that the new binary reads baseline archives. Query timings include process startup and use a warmup followed by three interleaved runs per configuration. Ingest timings are medians of three runs; extraction is checked once per configuration. Measurements use warm caches on a shared host, so small timing differences should not be treated as stable gains.

Unit coverage includes every numeric wire scheme, signed integer limits, floating-point bit patterns, truncated payloads, invalid frame metadata, exact dictionary membership, and archive round trips with both archive containers and array layouts. Selective-read tests verify that count queries leave damaged unused dictionaries unread while reconstruction rejects them, and cover forward-only inputs, memory buffers, reader reuse, and logical expressions. Metadata-count tests cover logical expressions, time bounds, array members, output requirements, and damaged record data that must remain unread for proven counts. Integer-range tests cover endpoint inclusivity, fractional operands, signed limits, repeated fields, missing fields, mixed types, and queries combining metadata counts with scans. They also damage one integer frame to verify that a proven predicate leaves it unread while another column is scanned. Dictionary-column tests compare compact scans against decoded scans and verify reference validation, reader reuse, and exact float-text extraction. Numeric codec mutation checks also run under AddressSanitizer and UndefinedBehaviorSanitizer.

Output-read tests make unused frames unreadable, check that required frames are read at most once, and verify incremental dictionary reads across interleaved sections. They cover exact float text, repeated array elements, output timestamps and record indices, CLP+ projections, and corrupted dictionary tails. The `clpp-output-reads.py` harness compares exact result bytes and interleaves baseline and candidate searches over the same archives, including low and high dictionary IDs, narrow projections, zero matches, wildcard predicates, full output, and count controls.

Preflight tests make variable dictionaries, array dictionaries, and record data unreadable for queries that summaries prove impossible. They cover multiple ERTs, AND/OR/NOT, integer limits, fractional comparisons, packed and separate storage, missing summaries, output finalization, and forward-only inputs. Dictionary-matching tests compare results with the previous conversion behavior, including a custom C++ locale. Prepared wildcard tests cover every byte value and randomized patterns with escapes and backtracking.

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

This comparison isolates the encoded scan change in `5139a614`: both configurations use adaptive storage with a 1 MiB ERT threshold, and the baseline is `3aa39f93`. The numeric fixture contains 1,000,000 records. Reproduce it with the benchmark harness by adding `--baseline-adaptive --rows 1000000`.

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

### Effect of selective dictionary reads

The baseline is `5139a614`, including encoded numeric scans. Both readers use the same adaptive archives with a 1 MiB ERT threshold. The text-heavy fixture contains 50,000 records with `seq`, a three-value `state`, and unique text in `detail`; its large logtype dictionary makes the cost of unrelated text visible. The numeric fixture contains 100,000 records, and the Hive sample contains 274,581 records.

Generate these workloads with the benchmark harness using `--baseline-adaptive --dictionary-heavy --repeats 7`. The comparison below uses a warmup followed by seven interleaved runs of both binaries on identical baseline archives. Peak resident memory is measured for each CLI process with Linux `wait4`; the reported values are medians.

| Workload | Query | Matching records | Baseline | Selective dictionaries | Speedup |
|---|---|---:|---:|---:|---:|
| Text-heavy | `seq: 999` | 1 | 61.1 ms | 8.3 ms | 7.39× |
| Text-heavy | `state: b*` | 16,667 | 62.9 ms | 8.8 ms | 7.15× |
| Text-heavy | `detail: *` | 50,000 | 61.6 ms | 8.1 ms | 7.57× |
| Text-heavy | `detail: *needle*` | 50 | 142.8 ms | 143.2 ms | 1.00× |
| Numeric | `x > 50.05` | 24,891 | 12.5 ms | 9.3 ms | 1.34× |
| Hive | `message.blockID.blockNum: 1073746491` | 2 | 78.5 ms | 13.2 ms | 5.93× |
| Hive | `message: *blk_1073746491_5667*` | 3 | 86.9 ms | 70.2 ms | 1.24× |
| Hive | `message: *zzzz*` | 0 | 59.6 ms | 28.4 ms | 2.10× |

The narrow text-fixture queries skip `detail`'s dictionary entirely, while `detail: *needle*` still needs it and receives no measurable benefit. The Hive BlockID query resolves directly to an integer column, avoiding string dictionaries and unrelated parsing and shape-index sections. Peak resident memory drops from 73.9 MiB to 18.0 MiB for the text-fixture integer lookup, and from 105.1 MiB to 20.3 MiB for the Hive BlockID lookup.

Query counts and ordered extraction agree across the readers. Extraction medians are 88 ms versus 90 ms for the numeric fixture, 177 ms versus 178 ms for the text-heavy fixture, and 327 ms versus 292 ms for Hive. Archive bytes and compression behavior are unchanged; this optimization affects what the reader loads.

### Effect of metadata-only counts

This comparison isolates bulk counting from the preceding selective-dictionary reader. Both binaries read identical archives, with a warmup followed by seven interleaved runs. The adaptive numeric fixture contains 1,000,000 records, the packed numeric fixture contains 100,000 records, and Hive contains 274,581 records.

| Workload | Query | Matching records | Per-row count | Metadata count | Speedup |
|---|---|---:|---:|---:|---:|
| Numeric, adaptive | `seq: *` | 1,000,000 | 14.0 ms | 7.3 ms | 1.93× |
| Numeric, adaptive | `seq: * AND state: *` | 1,000,000 | 10.7 ms | 7.5 ms | 1.43× |
| Numeric, packed | `seq: *` | 100,000 | 20.2 ms | 8.3 ms | 2.45× |
| Hive | `message: *` | 274,581 | 28.0 ms | 11.7 ms | 2.39× |
| Hive | `message: *PacketResponder*` | 758 | 31.8 ms | 29.7 ms | 1.07× |
| Hive | `message: *Exception*` | 4,927 | 65.7 ms | 66.0 ms | 0.99× |

The adaptive existence query already avoids decompressing unnecessary columns; bulk counting removes its row loop. Packed ERTs also avoid decompression, reducing peak resident memory from 26.6 MiB to 17.8 MiB for `seq: *`. Broad field existence benefits more than the tested Hive wildcards, whose preparation and remaining evaluation still dominate. A numeric equality control, `seq: 999`, takes 23.2 ms before and 22.8 ms after on the million-row archive, providing no evidence of a scan improvement from this change.

### Effect of compact dictionary-ID columns

The baseline includes metadata counts, selective dictionary reads, and encoded numeric scans. The candidate additionally enables `--adaptive-dictionary-columns`; both writers use a 1 MiB ERT threshold. The numeric and repeated-value fixtures contain 200,000 records each. The repeated-value fixture mixes a constant string, 256 randomly selected categories, strings repeated in blocks of 1,024 rows, and two dictionary-backed decimal spellings. Hive contains 274,581 records.

Reproduce the comparison with `clpp-column-port.py --baseline-adaptive --dictionary-columns --rows 200000`, supplying the baseline, candidate, output directory, and Hive input/specification as above. Compression and query timings use three runs per configuration.

| Workload | Numeric codecs only | With dictionary-ID codecs | Size reduction | Baseline ingest | Candidate ingest |
|---|---:|---:|---:|---:|---:|
| Repeated values | 225,707 bytes | 203,866 bytes | 9.7% | 126 ms | 136 ms |
| Numeric fixture | 1,870,847 bytes | 1,849,430 bytes | 1.1% | 295 ms | 294 ms |
| Hive sample | 4,624,247 bytes | 4,521,292 bytes | 2.2% | 2.19 s | 2.18 s |

| Workload | Query | Matching records | Plain dictionary IDs | Compact dictionary IDs | Speedup |
|---|---|---:|---:|---:|---:|
| Repeated values | `category: categorya*` | 20,089 | 11.0 ms | 8.5 ms | 1.29× |
| Repeated values | `state: busy` | 99,648 | 8.9 ms | 7.7 ms | 1.16× |
| Repeated values | `decimal > 0` | 100,352 | 19.6 ms | 7.2 ms | 2.73× |
| Repeated values | `decimal: *25*` | 200,000 | 28.1 ms | 8.5 ms | 3.30× |
| Repeated values | `state: busy AND decimal < 0` | 99,648 | 21.0 ms | 8.2 ms | 2.58× |
| Hive | `message: *a*b*` | 117,705 | 142.8 ms | 143.8 ms | 0.99× |

Repeated decimal IDs benefit most because compact scans avoid parsing and wildcard-matching the same decimal text for every row. String-ID scans save less work because ordinary membership checks are already cheap. The numeric fixture and Hive do not show a consistent search speedup from this option, although their archives shrink. Ordered extraction and query counts agree across configurations, and the older reader rejects every tested archive using the dictionary-ID extension. The repeated-value fixture's approximately 8% ingestion increase illustrates the extra codec-selection work; these results do not justify enabling the option by default.

### Effect of integer range proofs

The baseline includes compact dictionary-ID columns, metadata counts, and selective reads. Both readers use identical CLP+ archives. The numeric fixture contains 1,000,000 records with sequential `seq` values and randomly distributed `sensor` values from 0 through 1,999. It is stored once with packed ERTs and once with adaptive separate columns. Hive contains 274,581 records and uses the existing adaptive archive with a 1 MiB ERT threshold.

```sh
python3 components/core/tools/benchmarks/clpp-integer-ranges.py \
    --baseline /path/to/reader-before-range-proofs/clp-s \
    --candidate /path/to/modified/clp-s \
    --output /path/to/new-results-directory \
    --hive-archive /path/to/hive-archives
```

The harness checks every query count and records a warmup followed by seven interleaved runs. Timings include CLI startup and use warm caches; the build and regression tests finish before measurement.

| Workload | Query | Matching records | Baseline | Range proofs | Speedup |
|---|---|---:|---:|---:|---:|
| Numeric, packed | `seq >= 0 AND seq < 1000000` | 1,000,000 | 43.3 ms | 9.8 ms | 4.43× |
| Numeric, packed | `seq < 0` | 0 | 37.9 ms | 9.2 ms | 4.12× |
| Numeric, adaptive | `seq >= 0 AND seq < 1000000` | 1,000,000 | 28.2 ms | 9.4 ms | 3.00× |
| Numeric, adaptive | `seq >= 0 AND sensor > 1950` | 24,401 | 30.5 ms | 17.2 ms | 1.77× |
| Numeric, adaptive | `seq > 999990` | 9 | 24.9 ms | 24.9 ms | 1.00× |
| Hive | `message.blockID.blockNum >= 0` | 6,028 | 15.5 ms | 11.4 ms | 1.36× |
| Hive | `message.blockID.blockNum > 2000000000` | 0 | 15.3 ms | 11.0 ms | 1.39× |
| Hive | `message: *PacketResponder* AND message.blockID.blockNum >= 0` | 758 | 34.4 ms | 32.7 ms | 1.05× |

The fully covered and impossible numeric ranges avoid record data entirely. The mixed predicate still scans `sensor`, but separate-column storage lets it skip `seq` after proving `seq >= 0`. On packed storage, the same mixed predicate improves only from 41.6 ms to 39.7 ms because the shared frame still has to be decompressed. Partial ranges and the `seq: 999` equality control show no meaningful improvement. The Hive compound wildcard saves little because the remaining query preparation still dominates. These gains depend on what an ERT's bounds can prove; compression ratio and archive bytes are unchanged.

### Effect of projected output and dictionary prefixes

The output-read comparison uses C++ commit `44cab052` as the baseline. Both binaries read the same archives, created by that baseline with separate frames and adaptive dictionary-ID columns. The prefix fixture has 1,000 early-value records, two later matching ERTs, and 30,000 unrelated records that contribute distinct 256-character strings to the dictionary. The wide fixture has 30,000 records with two numeric fields and eight string fields. The Hive cases use the existing adaptive archive described above.

The harness checks exact output bytes, then reports medians from seven interleaved runs after a warmup. Times include CLI startup and captured output, with warm caches on a shared host. Builds and tests finish before measurement.

| Workload | Search and output | Baseline | Optimized | Speedup |
|---|---|---:|---:|---:|
| Prefix fixture | `id:1`, whole records | 18.6 ms | 7.7 ms | 2.42× |
| Prefix fixture | `id > 0 AND id < 3`, project `value` | 19.3 ms | 8.8 ms | 2.19× |
| Prefix fixture | `id:3`, references near the dictionary's end | 18.6 ms | 18.9 ms | 0.98× |
| Prefix fixture | `value:ear*`, requires the dictionary for filtering | 18.6 ms | 19.5 ms | 0.95× |
| Wide fixture | `seq:100`, project `seq` and `value` | 43.8 ms | 8.6 ms | 5.08× |
| Wide fixture | `seq:101`, no matching records | 44.3 ms | 8.3 ms | 5.31× |
| Wide fixture | `seq:100`, whole record | 44.9 ms | 43.8 ms | 1.03× |
| Hive | `message:*PacketResponder*`, project `shape(message)` | 64.3 ms | 32.5 ms | 1.98× |

Early dictionary IDs avoid decoding the large tail. Numeric projections avoid string frames and their dictionaries entirely, and zero matches avoid all output-only preparation. The Hive shape projection avoids variable columns needed for string reconstruction. High IDs and predicates that search dictionary values still need a complete dictionary; the latter shows a small slowdown in this run. Returning every wide record remains close at 66.0 ms versus 66.6 ms, as do numeric counts at 8.6 ms versus 8.5 ms. Hive output of reconstructed matching strings changes from 62.1 ms to 63.2 ms. These controls show why the output mode and dictionary dependencies matter more than the number of matching records alone.

```sh
python3 components/core/tools/benchmarks/clpp-output-reads.py \
    --baseline /path/to/baseline/clp-s \
    --candidate /path/to/modified/clp-s \
    --output /path/to/new-results-directory \
    --clpp-archive /path/to/hive-adaptive-archive \
    --repeats 7
```

### Effect of early pruning and prepared dictionary matching

The baseline is C++ commit `d3d01624`. It writes two archives of the same 30,000 records: one with packed ERTs and one with adaptive separate columns. Each record has a sequential integer and a distinct mixed-case string of 245 or 246 bytes; an optional field splits the records into two ERTs. Both readers consume identical archive bytes. The harness checks exact count-output bytes and reports medians from seven interleaved runs after a warmup, including CLI startup on a shared host with warm caches. Builds and tests finish before measurement.

These results use the adaptive separate-column archive:

| Query | Baseline | Optimized | Speedup |
|---|---:|---:|---:|
| Impossible integer range AND exact string | 19.5 ms | 8.3 ms | 2.34× |
| Impossible integer range AND string wildcard | 22.2 ms | 8.3 ms | 2.68× |
| Exact string, case-insensitive | 51.6 ms | 20.5 ms | 2.52× |
| Prefix wildcard, case-insensitive | 36.8 ms | 20.7 ms | 1.77× |
| Substring wildcard, case-insensitive | 40.3 ms | 24.1 ms | 1.67× |
| Impossible integer range OR matching string wildcard | 20.3 ms | 20.2 ms | 1.00× |
| Prefix wildcard, case-sensitive | 20.0 ms | 20.3 ms | 0.99× |

Early pruning avoids reading and searching the variable dictionary for impossible conjunctions, including a negated range proven false. OR retains dictionary work because its string condition can still match. Case-insensitive searches still read the dictionary but avoid converting every complete entry; exact and prefix comparisons can stop at the first unequal byte. Numeric equality and metadata-count controls remain close at 7.5 ms versus 7.3 ms and 7.3 ms versus 7.4 ms. Packed archives show similar gains: the impossible conjunction drops from 19.1 ms to 8.2 ms, and case-insensitive exact matching drops from 52.1 ms to 21.1 ms. These are search improvements; neither change affects archive size.

```sh
python3 components/core/tools/benchmarks/clpp-dictionary-preflight.py \
    --baseline /path/to/baseline/clp-s \
    --candidate /path/to/modified/clp-s \
    --output /path/to/new-results-directory \
    --repeats 7
```
