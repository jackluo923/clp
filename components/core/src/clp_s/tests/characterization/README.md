# CLP-S characterization harness

This directory contains black-box tests for the `clp-s` executable only. The harness does not
locate or invoke `clp`, `glt`, `log-converter`, `reducer-server`, MongoDB, or any CLP service.

The suite has two layers:

- `test_cli_characterization.py` records the exit status, stdout, stderr, and requested filesystem
  trees for a C++ reference binary. If `--clp-s-rust` is supplied, the same cases are run against
  the Rust candidate and their deterministic views are compared.
- `test_harness.py` tests normalization and comparison helpers without invoking `clp-s`.

The behavior matrix covers top-level and command help, missing arguments, an unknown command, and a
local `c`/`s`/`x` workflow. It also pins KQL precedence and typing, escaping and namespaces,
existence, structured and unstructured arrays, projection and physical result order, timestamp
bounds, case folding, and zero-match aggregation output. The round-trip workflow covers
authoritative timestamps, exact float spellings, ordered extraction, directory and single-file
archives, and archive-stat output.

## Container-only binary runs

Build and run both implementations in the image defined by
`components/core/tools/docker-images/clp-env-base-manylinux_2_28`. Do not build either binary on
the host. For an x86-64 checkout, build the dependency image on the host and use it as follows:

```sh
components/core/tools/docker-images/clp-env-base-manylinux_2_28/build.sh

REPO_ROOT="$(pwd)"
GIT_COMMON_DIR="$(git rev-parse --path-format=absolute --git-common-dir)"
docker run --rm \
  --user "$(id -u):$(id -g)" \
  --volume "$REPO_ROOT:$REPO_ROOT" \
  --volume "$GIT_COMMON_DIR:$GIT_COMMON_DIR:ro" \
  --workdir "$REPO_ROOT" \
  clp-core-dependencies-x86-manylinux_2_28:dev \
  bash -lc '
    task deps:core
    cmake -S components/core -B build/core -DCMAKE_BUILD_TYPE=Release
    cmake --build build/core --target clp-s --parallel "$(getconf _NPROCESSORS_ONLN)"
    uv sync --project integration-tests --only-group dev
    CLP_S_CHARACTERIZATION_IN_CONTAINER=1 \
      uv run --no-sync --project integration-tests pytest \
        components/core/src/clp_s/tests/characterization/test_cli_characterization.py \
        --clp-s-cpp "$PWD/build/core/clp-s" \
        --clp-s-run-metadata "$PWD/build/clp-s-characterization/run-metadata.json" \
        --clp-s-observations-dir "$PWD/build/clp-s-characterization"
  '
```

For arm64, build with `PLATFORM=linux/arm64` and use the
`clp-core-dependencies-aarch64-manylinux_2_28:dev` tag. Once the Rust binary exists, build it in the
same container and append:

```text
--clp-s-rust /absolute/path/to/checkout/target/release/clp-s
```

Mounting the checkout at the same absolute path keeps linked-worktree and submodule Git metadata
resolvable inside the container. Mounting the common Git directory read-only lets the harness
record the exact source revision without granting it write access to repository metadata.

Supplying a binary path without `CLP_S_CHARACTERIZATION_IN_CONTAINER=1` fails immediately. The
marker is an accidental-host-execution guard; CI should set it only for a job using this image.

Before running, copy `run-metadata.example.json` to the path passed via `--clp-s-run-metadata` and
replace every placeholder. The image digest must be the full ID reported by `docker image inspect`.
The other required values record the target platform, compiler/toolchain and effective build flags,
and versions of zstd, msgpack-cxx, simdjson, and libarchive. The harness rejects an incomplete
template and, by default, a dirty Git tree.

## Observations and comparison policy

Each JSON observation retains raw and normalized argv/stdout/stderr. A filesystem snapshot records
sorted relative paths, kinds, modes, sizes, SHA-256 digests, and normalized UTF-8 content where the
file is small enough. No symlink is followed while walking an output tree.

Normalization is deliberately narrow:

- the implementation path and per-test work directory become `<CLP-S>` and `<WORKDIR>`;
- spdlog's leading timestamp becomes `<LOG_TIMESTAMP>`;
- UUIDs receive stable encounter-order tokens such as `<UUID:1>`;
- ANSI escapes and platform newline differences are removed;
- streams made entirely of JSON lines are serialized with sorted object keys; search result lines
  are also sorted when result ordering is not part of the contract.

Raw observations remain available for diagnosis. Differential comparison excludes raw fields. The
primary dual-target assertion compares process completion as success, failure, or timeout rather
than requiring the same implementation-specific nonzero value. Normalized stderr remains in every
observation, but is diagnostic evidence rather than a machine-output compatibility field: the
compatibility policy explicitly allows reviewed changes to human wording, capitalization,
punctuation, and help wrapping. Failed CLI parsing must still leave stdout empty and emit a stderr
diagnostic; successful help may use stdout or stderr.

Data output is not relaxed by that diagnostic policy. Normalized stdout and requested output trees
remain strict comparison inputs. The local workflow compares archive layout but not compressed
payload bytes or the archive-stat `size` field; compression ratio and size belong to the performance
gate. Extracted JSON and search output are compared semantically, while ordered extraction preserves
record order. The formatted float tokens in the fixture are also extracted from raw output and
compared lexically, so values such as `12.50` cannot silently become `12.5`.

Each run also writes `run-manifest.json`. It includes Git commit and dirty-state hashes, recursive
submodule revisions, declared image/target/toolchain metadata, binary paths and SHA-256 digests, and
input fixture hashes. Raw command observations record argv and the names of any additional
environment variables without recording secret values. A dirty run can be enabled explicitly with
`--clp-s-allow-dirty-source`, but its output is diagnostic and should not be admitted as a golden.

By default pytest writes observations to a temporary directory. Use
`--clp-s-observations-dir PATH` (or `CLP_S_OBSERVATIONS_DIR`) for durable CI artifacts. Binary paths
can also be provided using `CLP_S_CPP_BIN` and `CLP_S_RUST_BIN`.
