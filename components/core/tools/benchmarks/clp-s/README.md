# CLP-S performance harness

This directory contains the reproducible, paired benchmark harness used to compare the C++ and
Rust `clp-s` binaries. It measures already-built binaries; it never builds either implementation.
Both binaries must be compiled with release settings in the
`components/core/tools/docker-images/clp-env-base-manylinux_2_28` image before running this
harness.

Normal benchmark execution is container-only. `benchmark.py` rejects execution outside the
container launched by `run-in-container.sh`. Manifest validation and the Python unit tests are safe
to run on the host because they never invoke `clp-s`.

## Profile-guided Rust build

`build-pgo-in-container.sh` produces a balanced PGO `clp-s` binary without compiling on the host.
It accepts only the x86-64 `clp-env-base-manylinux_2_28` image family, disables networking, mounts
the repository and training inputs read-only, mounts only the toolchain's `.cargo` and `.rustup`
directories read-write, and runs Cargo with `--locked --offline`. The inspected immutable image ID,
not a mutable tag, is used for the container run. The mounted toolchain must already contain the
exact `nightly-2026-08-30` toolchain and its matching `llvm-tools-preview`; the builder verifies the
Cargo version and hash, rustc commit and hash, LLVM version, and `llvm-profdata` hash. Use a dedicated
toolchain parent: Cargo-home `config` files are rejected so that the checked repository configuration
is authoritative.

First create an explicit checksum manifest for every repository file that can influence this build.
This example covers the workspace controls, `clp-s`, its native container dependency, and the PGO
driver itself, including uncommitted files:

```shell
{
  printf '%s\0' \
    Cargo.toml Cargo.lock \
    components/core/tools/benchmarks/clp-s/build-pgo-in-container.sh \
    components/core/tools/benchmarks/clp-s/pgo-build.sh
  find .cargo components/clp-s components/clp-s-container \
    \( -type f -o -type l \) -print0
  find components -mindepth 2 -maxdepth 2 \
    \( -type f -o -type l \) \
    \( -name Cargo.toml -o -name build.rs \) -print0
} | LC_ALL=C sort -zu | xargs -0 sha256sum -- > build/clp-s-pgo-source.sha256
```

Then supply that manifest and the expected hashes of a stable corpus/reference-archive pair:

```shell
components/core/tools/benchmarks/clp-s/build-pgo-in-container.sh \
  --output-dir "$PWD/build/clp-s-pgo" \
  --toolchain-dir "$PWD/build/toolchains/rust" \
  --source-checksums "$PWD/build/clp-s-pgo-source.sha256" \
  --corpus /absolute/path/to/training.ndjson \
  --corpus-sha256 CORPUS_SHA256 \
  --archive /absolute/path/to/reference-archive.clp \
  --archive-sha256 ARCHIVE_SHA256 \
  --timestamp-key timestamp_ms \
  --rare-query 'request_id: 999999' \
  --error-query 'level: ERROR'
```

The reference archive must extract unordered to the corpus's exact SHA-256. Training executes each
of seven shapes once: log-order and no-log-order compression, unordered and ordered extraction, a
rare-record search, an all-record count, and an error count. It then merges all seven raw profiles
and builds the `clp-s-release` profile with missing-function warnings enabled; any missing-profile
warning fails the build. Cargo metadata also verifies that every workspace manifest and custom build
target parsed by Cargo appears in the source checksum manifest.

Before publishing the binary, the script runs eleven validation commands. The profile-use binary
repeats both compression modes, reads both instrumented archives, extracts the reference archive in
both modes, and repeats all three searches. The instrumented binary also reads both profile-use
archives with validation profiles isolated from the seven merged profiles. Extracted bytes and search
results must match exactly. Large validation payloads are removed after their hashes are recorded,
then the sources, corpus, archive, and toolchain executables are reverified before provenance is sealed.
The final read-only binary is `OUTPUT_DIR/clp-s`; `provenance.txt`, `validation-summary.txt`,
`SHA256SUMS`, Cargo metadata, build and validation logs, exact command records, raw profiles, and the
merged profile remain beside it. Queries are recorded verbatim, so use only non-sensitive training
expressions. For release artifacts, pass the mandatory image by registry digest with `--image` rather
than relying on the mutable `:dev` tag; the wrapper still locks execution to the locally inspected ID.

## Quick smoke run

First build both binaries using the manylinux image, placing the results somewhere below the
repository root. Copy `build-metadata.example.json`, replace every placeholder with the exact
release settings and binary hashes, and keep that generated file with the build artifacts. The
runner rejects metadata whose source commit or binary hashes do not match. Then run:

```shell
components/core/tools/benchmarks/clp-s/run-in-container.sh \
  --results-dir "$PWD/build/benchmark-results" \
  --cpuset-cpus 2-5 \
  -- \
  --manifest /mnt/repo/components/core/tools/benchmarks/clp-s/manifest.smoke.json \
  --cpp-binary /mnt/repo/build/cpp/clp-s \
  --rust-binary /mnt/repo/build/rust/clp-s \
  --build-metadata /mnt/repo/build/clp-s-build-metadata.json \
  --results-dir /mnt/results
```

The default image is `clp-core-dependencies-x86-manylinux_2_28:dev` on x86-64 and
`clp-core-dependencies-aarch64-manylinux_2_28:dev` on arm64. Override it with `--image` when using
a pinned registry digest. The resolved image ID is recorded in every result.

The smoke fixture is intentionally tiny. It tests the benchmark machinery and cross-reader
compatibility, but its timings are too noisy to support performance conclusions.

For a large external corpus, mount it read-only and refer to its container path in the manifest:

```shell
components/core/tools/benchmarks/clp-s/run-in-container.sh \
  --data-dir /srv/clp-benchmark-data \
  --results-dir /srv/clp-benchmark-results \
  --image clp-core-dependencies-x86-manylinux_2_28@sha256:IMAGE_DIGEST \
  -- \
  --manifest /mnt/data/manifests/release.json \
  --cpp-binary /mnt/repo/build/cpp/clp-s \
  --rust-binary /mnt/repo/build/rust/clp-s \
  --build-metadata /mnt/data/build-metadata/release.json \
  --results-dir /mnt/results
```

The wrapper disables networking, mounts the repository and datasets read-only, mounts only the
results directory read-write, and runs as the calling user. Binary paths passed to the runner must
be explicit absolute paths inside the container. A results directory inside the repository must be
Git-ignored (for example, below `build/`) so the dirty-tree fingerprint cannot include the run that
is producing it.

## Manifest model

`manifest.schema.json` is the authoritative machine-readable shape. `manifest.smoke.json` is a
runnable example. A manifest contains:

- `datasets`: user-managed files or directories. Every entry requires a SHA-256 checksum. An
  optional `logical_size_bytes` can represent the uncompressed amount of data when it differs from
  the files' physical size.
- `preparations`: unmeasured, recorded commands that produce shared inputs. For example, the smoke
  manifest creates one C++ reference archive so C++ and Rust extraction/search trials consume
  exactly the same archive bytes. Preparations run once before trials and record their output
  checksum.
- `workloads`: compression, extraction, or search commands. The runner invokes the same arguments
  with each explicit binary path.
- `defaults`: warm-up count, measured pair count, timeout, and inherited gates. A workload can
  override every one of these values. Supplying an empty workload `gates` array disables inherited
  gates for that workload.
- `environment`: explicit environment overrides applied identically to both implementations.

`--build-metadata` is required for every measured run. Its schema is
`build-metadata.schema.json`. It records information that cannot reliably be recovered from an ELF
binary: the C++ CMake build type and arguments, effective compiler/linker flags and LTO mode; the
Rust Cargo profile settings, invocation and effective `RUSTFLAGS`; target CPUs/triples; compilers;
and allocators. It also binds these declarations to the current source commit and exact SHA-256 of
each binary. Do not use the zero hashes or `REPLACE_WITH_...` values from the example.

Arguments are arrays, never shell strings. The runner directly executes the binary without a
shell. These exact templates are available inside argument strings:

| Template | Value |
| --- | --- |
| `{input}` | Verified dataset or prepared artifact path |
| `{output}` | Empty output directory unique to this invocation |
| `{workdir}` | Unique invocation directory |
| `{stdout}` | Captured stdout file path |
| `{stderr}` | Captured stderr file path |
| `{implementation}` | `cpp` or `rust` |

Other braces remain unchanged, allowing KQL object expressions. Preparations must use `{input}`
and `{output}`; workloads must use `{input}`.

### Dataset directory checksums

A regular file uses its ordinary SHA-256. Symbolic links and non-regular files are rejected. A
directory uses this deterministic tree digest:

1. Initialize SHA-256 with the bytes `clp-s-benchmark-directory-v1` followed by a NUL byte.
2. Visit regular files in sorted relative POSIX-path order.
3. For every file, append the relative path byte length as an unsigned 64-bit big-endian integer,
   the UTF-8 path bytes, the file size as an unsigned 64-bit big-endian integer, and the 32 raw
   bytes of the file's SHA-256 digest.

To print the checksum accepted by the harness without invoking either binary, use:

```shell
python3 components/core/tools/benchmarks/clp-s/benchmark.py \
  --manifest components/core/tools/benchmarks/clp-s/manifest.smoke.json \
  --validate-only
```

For a new dataset, calculate a provisional digest using the same `tree_sha256` function or put a
temporary 64-character lowercase digest in a local manifest and use the mismatch's reported actual
digest. Commit checksums for stable corpora; never silently update them during a benchmark run.

## Trial protocol and metrics

Each warm-up and measured trial is a *pair*: C++ runs once and Rust runs once. Their order is
randomized independently using the recorded seed. Pairing controls for gradual machine drift, and
randomization avoids systematically favoring whichever implementation runs first. Workload and
phase names derive independent random streams, so filtering workloads does not change their order.

The runner records the following raw values for every invocation:

- elapsed monotonic wall time;
- user, system, and total CPU time from Linux `wait4`;
- peak RSS from Linux `ru_maxrss`;
- physical and logical input sizes;
- recursive output, stdout, and stderr sizes;
- configured byte or record throughput; and
- command, exit status, timeout status, environment overrides, pair order, and artifact paths.

Compression normally uses logical input bytes per second, extraction output bytes per second, and
search physical archive bytes per second. Set `throughput_source` explicitly when another measure
is more meaningful. Supported sources include stdout/output line counts for result-heavy searches.

The harness does not clear the operating system page cache. Record whether a release run is a
warm-cache or cold-cache experiment, never mix the two in one comparison, pin CPUs, isolate the
machine from unrelated work, and use datasets large enough to dominate process startup.

## Gates

Gates operate on the per-pair ratio `Rust / C++`, rather than a ratio of unrelated aggregate runs.
Each gate selects a metric, a ratio statistic (`p05`, `median`, or `p95`), and either:

- `min_ratio`: observed ratio must be at least the threshold, suitable for throughput; or
- `max_ratio`: observed ratio must be at most the threshold, suitable for latency, memory, and
  archive size.

For example:

```json
{
  "metric": "throughput_per_second",
  "statistic": "p05",
  "comparison": "min_ratio",
  "threshold": 0.95
}
```

Use `--no-gates` only for exploratory runs. It records gate failures but does not make them fail the
run. Binary failures, timeouts, or incomplete pairs still fail.

The initial release thresholds and required workload coverage are documented in
`workload-matrix.md`. Re-estimate runner variance before treating those proposed thresholds as
release policy.

## Results and provenance

Every run creates a new directory containing:

- `raw-results.jsonl`: append-only raw metadata, dataset, preparation, measurement, and summary
  events, flushed after every invocation so partial runs remain inspectable;
- `results.json`: the complete manifest snapshot, raw measurements, descriptive statistics,
  paired ratios, gate outcomes, and provenance; and
- retained command artifacts according to `--keep-artifacts=never|failures|all` (default:
  `failures`).

Provenance includes the image reference and ID, repository commit, recursive submodule status, and
a complete dirty-tree fingerprint. The latter hashes the full binary Git diff plus every untracked
path, mode, and content without storing tracked diff contents. It also includes binary paths, sizes
and SHA-256 digests, declared build settings, ELF metadata and dependencies, compiler/tool versions,
kernel and OS, CPU model and affinity, cgroup limits, memory information, harness version, manifest
checksum, and random seed.

Because zstd, libarchive, simdjson, and msgpack affect archive bytes or parsing behavior, their
versions are probed independently through all available sources: `pkg-config`, CMake package
discovery, public version macros, RPM package metadata, and each binary's dynamic dependency list.
Unavailable probes are retained as such rather than silently omitted.

Result files never record values for arbitrary environment variables. Only a fixed allowlist of
non-secret performance controls (locale/timezone, allocator tunables, thread counts, paths, and
Rust flags) may retain values; every other manifest environment value is replaced by `<redacted>`.
The manifest checksum still identifies the exact private input without copying secrets into the
results. Do not put credentials in command arguments or build metadata either; the wrapper disables
networking and release benchmark corpora must be local.

Do not compare results when binary hashes, image IDs, build flags, zstd/library versions, CPU
affinity, or corpus checksums differ unintentionally.

## Host-only validation

These checks do not build or execute CLP-S:

```shell
python3 -m py_compile components/core/tools/benchmarks/clp-s/benchmark.py
python3 -m unittest discover \
  -s components/core/tools/benchmarks/clp-s/tests \
  -p 'test_*.py'
bash -n components/core/tools/benchmarks/clp-s/run-in-container.sh
bash -n \
  components/core/tools/benchmarks/clp-s/build-pgo-in-container.sh \
  components/core/tools/benchmarks/clp-s/pgo-build.sh
```
