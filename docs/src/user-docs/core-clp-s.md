# CLP for JSON logs

For JSON logs, you can compress, decompress, and search them using the `clp-s` binary described
below.

## Compression

Usage:

```shell
./clp-s c [<options>] <archives-dir> <input-path> [<input-path> ...]
```

* `archives-dir` is the directory that archives should be written to.
* `input-path` is a filesystem path or URL to either:
  * a new-line-delimited JSON (ndjson) log file;
  * a KV-IR file;
  * a tarball containing JSON and/or KV-IR;
  * any of the above compressed with Gzip or Zstandard.
* `options` allow you to specify how data gets compressed into an archive. For example:
  * `--single-file-archive` specifies that single-file archives should be produced (i.e., each
    archive is a single file in `archives-dir`).
  * `--timestamp-key <field-path>` specifies which field should be treated as each log event's
    timestamp.
  * `--target-encoded-size <size>` specifies the threshold (in bytes) at which archives are split,
    where `size` is the total size of the dictionaries and encoded messages in an archive.
    * This option acts as a soft limit on memory usage for compression, decompression, and search.
    * This option significantly affects compression ratio.
  * `--structurize-arrays` specifies that arrays should be fully parsed and array entries should be
    encoded into dedicated columns.
  * `--encoding-effort fast|balanced|exhaustive` controls how many adaptive integer and float
    encodings are tried before Zstd. The default is `balanced`, independently of the Zstd
    `--compression-level` setting. All three efforts preserve numeric values exactly.
  * `--auth <s3|none>` specifies the authentication method that should be used for network requests
    if the input path is a URL.
    * When S3 authentication is enabled, we issue a GET request following the [AWS Signature Version
      4 specification][aws-signature-v4]. This request uses the environment variables
      `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and, optionally, `AWS_SESSION_TOKEN` if it
      exists.
    * For more information on usage with S3, see our
      [dedicated guide](guides-using-object-storage/index).

For a complete list of options, run `./clp-s c --help`.

### Examples

**Compress `/mnt/logs/log1.json` and output archives to `/mnt/data/archives1`:**

```shell
./clp-s c /mnt/data/archives1 /mnt/logs/log1.json
```

**Treat the field `{"d": {"@timestamp": "..."}}` as each log event's timestamp:**

```shell
./clp-s c --timestamp-key 'd.@timestamp' /mnt/data/archives1 /mnt/logs/log1.json
```

:::{tip}
Specifying the timestamp-key will create a range-index for the timestamp column which can increase
compression ratio and search performance.
:::

**Compress a KV-IR file stored on S3 into a single-file archive:**

```shell
AWS_ACCESS_KEY_ID='...' AWS_SECRET_ACCESS_KEY='...' \
  ./clp-s c --single-file-archive --auth s3 /mnt/data/archives \
  https://my-bucket.s3.us-east-2.amazonaws.com/kv-ir-log.clp
```

**Set the target encoded size to 1 GiB and the compression level to 6 (3 by default)**

```shell
./clp-s c \
    --target-encoded-size 1073741824 \
    --compression-level 6 \
    /mnt/data/archives1 \
    /mnt/logs/log1.json
```

### Rust numeric encoding effort

Use `--encoding-effort` to choose the amount of work spent finding numeric encodings. This setting applies to integer and float columns together. It does not change the Zstd compression level or timestamp precision. Final selection compares the candidates' compressed sizes after Zstd.

```shell
./clp-s c --single-file-archive --encoding-effort balanced --compression-level 3 archives input.ndjson
./clp-s c --single-file-archive --encoding-effort fast --compression-level 6 fast-archives input.ndjson
./clp-s c --single-file-archive --encoding-effort exhaustive --compression-level 6 exhaustive-archives input.ndjson
```

| Effort | Integer selection | Float selection |
|---|---|---|
| `fast` | Plain, frame of reference, dictionary, RLE, fixed-width deltas, and one packed-delta layout | Plain, byte-stream split, dictionary, sorted dictionary, RLE, and constant |
| `balanced` (default) | Also tries column-wide packed deltas alongside chunk-local packing for columns longer than 65,536 rows | Also tries sampled exact multiplication-decimal scales 0–6 with packed-delta or byte-plane integer storage |
| `exhaustive` | Also considers less frequent repeated runs for RLE | Also tries decimal scales through 15, more integer encodings, division reconstruction, and exact Float32-to-Float64 reconstruction |

Decimal encoding is lossless: every value must reconstruct to its original Float64 bits. Sampling can avoid an unpromising candidate, but never substitutes for checking every value. Unrepresentable columns retain another encoding. Higher effort may improve compression at the cost of additional compression work; it does not guarantee smaller archives or faster queries for every input.

These options require a rebuilt CLI. New multiplication-decimal archives require a matching format 0.15 reader, including the DuckDB extension. The hidden older `--decimal-float-columns` flag is an alias for shared `exhaustive` effort and conflicts with explicit `--encoding-effort`; use the shared option for new commands.

### Rust timestamp field options

**Build requirement:** these options require the rebuilt Rust CLI and DuckDB extension. The binaries currently published in `bin/oct-20-2026` have not been updated. New archives with timestamps or adaptive numeric/string columns use format **0.15**; use the matching rebuilt reader.

Declare `--timestamp-key` only for **absolute Unix epoch timestamps or text date/time values**. Device-relative clocks, counters, and durations remain ordinary numbers. The field name alone does not tell CLP that a number is a timestamp.

The public option is **`precision=`**. Repeat `--timestamp-key` for multiple fields; it is not a CSV list:

```bash
"$CLP" c --single-file-archive \
  --timestamp-key 'event_time:precision=ms' \
  --timestamp-key 'received_at:precision=100us:rounding=nearest' \
  archives input.ndjson
```

| Field option | Meaning | Default |
|---|---|---|
| `precision=DURATION` | Numeric input tick duration and native storage precision; text dates are parsed and quantized to this precision | `ns` |
| `precision=auto` | Infer each value's precision, then store the field at the finest encountered precision in that archive | Explicitly requested |
| `rounding=truncate\|nearest` | Reduce finer timestamp detail when an explicit precision is selected | `truncate` |

`ns`, `us`, `ms`, and `s` are equivalent to `1ns`, `1us`, `1ms`, and `1s`. Positive integer multiples such as `10us`, `100us`, `250us`, and `100ms` are accepted. Tick durations must fit `1..9223372036854775807` nanoseconds. Invalid, zero, negative, fractional, and overflowing durations are rejected. The interpreted instant must fit signed 64-bit epoch nanoseconds. Tick boundaries start at Unix epoch zero.

**Explicit precision:** integer input already counts ticks of that duration. With `precision=ms`, `1783328980204` means Unix epoch milliseconds, is stored in millisecond ticks, and returns the same integer. Changing the declaration to `precision=ns` reinterprets that number; it does not automatically convert milliseconds to nanoseconds. Convert numeric inputs yourself when deliberately changing their unit. Quoted integers follow the same rule and retain their string type.

**Text timestamps:** the parser reads the date/time and offset first, then quantizes the instant. With `precision=ms`, `"2026-07-06T09:09:40.204567Z"` becomes `"2026-07-06T09:09:40.204000Z"` with truncation or `"2026-07-06T09:09:40.205000Z"` with nearest rounding. Quoted fractional epoch seconds work similarly: `"1.234567890"` becomes `"1.234000000"` or `"1.235000000"`. Unquoted fractional numbers require `precision=s` or `precision=auto`. Explicit seconds quantize them to whole seconds. Truncation floors toward negative infinity; nearest rounds ties away from epoch zero.

**Automatic precision:** use `--timestamp-key 'event_time:precision=auto'`. Integer inputs, including quoted integers, infer s/ms/us/ns by magnitude. Text date/time values infer seconds with no fraction, milliseconds with 1–3 fractional digits, microseconds with 4–6, and nanoseconds with 7–9. Fractional epoch numbers are interpreted as seconds with fractional detail. `auto` never chooses custom ticks such as `100us` and does not reduce precision; `rounding` has no effect in this mode.

Numeric inference uses absolute magnitude: values greater than 31,536,000,000,000,000 are ns; otherwise values greater than 31,536,000,000,000 are us; otherwise values greater than 31,536,000,000 are ms; smaller values are seconds. This is a heuristic for epoch data. Use an explicit precision for known units, especially timestamps near 1970; do not use magnitude detection for relative device clocks.

Rows in one `auto` field may have different input precisions. For example, `1700000000`, `1700000000123`, and `1700000000123456` are parsed independently as seconds, milliseconds, and microseconds. That archive stores the field in microsecond ticks and records this resolved precision once; each row retains its source representation for decompression and `_source`. A later finer value promotes the field's storage precision without rounding earlier rows. Each rotated archive resolves its own precision. For DuckDB comparisons across mixed input units, use an explicit `TIMESTAMP_NS` mapping rather than comparing raw `_source` integers.

Global `--timestamp-precision` and `--timestamp-rounding` supply defaults; per-field options override them. Plain `--timestamp-key ts` still defaults to **nanoseconds**, without numeric inference. These declarations apply to JSON and KV-IR. Missing and null fields retain their normal behavior. Invalid timestamp lexemes and out-of-range instants currently fail compression; they are not silently converted to zero.

Each field stores native integer ticks and precision metadata. Supported timestamp predicates translate bounds to the stored ticks; readers convert projected instants when needed. Numeric summaries describe values exposed in `_source`; time indexes describe the stored instants. The first declared timestamp field present in an archive supplies time-only search bounds.

Paths use dotted syntax: `\.` is a literal dot, `\u003a` a literal colon, and `\@timestamp` the literal JSON key `@timestamp`. An unescaped leading `@` selects KV-IR's auto-generated namespace instead:

```bash
"$CLP" c --timestamp-key '\@timestamp:precision=auto' archives elasticsearch.ndjson
```

The old compression spellings `unit=` and `--timestamp-unit` are rejected. There is no separate input-unit, increment, or storage-factor option. The DuckDB query wrapper's `--timestamp-unit` remains a separate registration hint. CLI time-only `--tge`/`--tle` bounds remain epoch milliseconds; explicit numeric KQL bounds on indexed timestamp fields use epoch nanoseconds.

## Decompression

Usage:

```shell
./clp-s x [<options>] <archives-path> <output-dir>
```

* `archives-path` is a directory containing archives, a path to an archive, or a URL pointing to a
  single-file archive.
* `output-dir` is the directory that decompressed logs should be written to.
* `options` allow you to specify things like a specific archive (from within `archives-path`, if it
  is a directory) to decompress (`--archive-id <archive-id>`).
  * For a complete list, run `./clp-s x --help`

### Examples

**Decompress all logs from `/mnt/data/archives1` into `/mnt/data/archives1-decomp`:**

```shell
./clp-s x /mnt/data/archives1 /mnt/data/archives1-decomp
```

## Search

Usage:

```shell
./clp-s s [<options>] <archives-path> <kql-query>
```

* `archives-path` is a directory containing archives, a path to an archive, or a URL pointing to a
  single-file archive.
* `kql-query` is a [KQL](reference-json-search-syntax) query.
* `options` allow you to specify things like a specific archive (from within `archives-path`, if it
  is a directory) to search (`--archive-id <archive-id>`).
  * For a complete list, run `./clp-s s --help`

### Examples

**Find all log events within a time range:**

```shell
./clp-s s /mnt/data/archives1 'ts >= 1649923037 AND ts <= 1649923038'
```
or
```shell
./clp-s s /mnt/data/archives1 \
    'ts >= date("2022-04-14T07:57:17") AND ts <= date("2022-04-14T07:57:18")'
```

**Find log events with a given key-value pair:**

```shell
./clp-s s /mnt/data/archives1 'id: 22149'
```

**Find ERROR log events containing a substring:**

```shell
./clp-s s /mnt/data/archives1 'level: ERROR AND message: "job*"'
```

**Find FATAL or ERROR log events and ignore case distinctions between values in the query and the
compressed data:**

```shell
./clp-s s --ignore-case /mnt/data/archives1 'level: FATAL OR level: ERROR'
```

## Current limitations

* `clp-s` currently only supports *valid* JSON logs; it does not handle JSON logs with trailing
  commas or other JSON syntax errors.
* Time zone information is not preserved.
* The order of log events is not preserved.
* The input directory structure is not preserved and during decompression all files are written to
  the same file.
* In addition, there are a few limitations, related to querying arrays, described in the search
  syntax [reference](reference-json-search-syntax).

[aws-signature-v4]: https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-query-string-auth.html

### Delta restart points

Timestamp chunks contain 65,536 rows. New archives store an absolute timestamp tick value at each chunk boundary, followed by deltas within the chunk. For supported timestamp filters, the query checks chunk min/max first, decompresses the required Zstd frame, and validates and decodes timestamp values only in candidate chunks. Skipping a chunk does not require accumulating its deltas, including when an excluded chunk lies between two matching chunks. Zstd frame boundaries are unchanged: this saves value-decoding work, not necessarily Zstd decompression. Adaptive integer deltas use the same restart boundaries when that encoding wins compression selection. Existing archives remain readable; recompress them to gain persisted restart points.

Adaptive integer encoding tries both bit-packed and fixed-width 64-bit deltas, alongside its other codecs, and chooses the smallest result after Zstd compression. Both delta layouts use the same restart boundaries and preserve values exactly. No extra compression flag is needed.

Format 0.14 also tries independent packed-delta bit widths for each 65,536-row chunk in longer columns. Large differences inside one chunk no longer widen the encoding of every other chunk when this candidate wins. Normal packed and fixed-width deltas remain candidates. The chunk offset directory supports direct seeking without decoding preceding chunks; this does not change timestamp precision or the chunk min/max pruning boundaries. Reading new archives requires a reader that supports format 0.14.
