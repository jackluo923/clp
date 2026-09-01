#!/usr/bin/env bash

set -o errexit
set -o nounset
set -o pipefail

readonly script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
readonly repo_root="$(git -C "${script_dir}" rev-parse --show-toplevel)"

usage() {
    cat <<'EOF'
Usage: run-in-container.sh [WRAPPER_OPTIONS] -- RUNNER_OPTIONS

Runs the CLP-S benchmark harness in the manylinux_2_28 dependency image. Paths in
RUNNER_OPTIONS are paths inside the container: the repository is /mnt/repo, the
optional data directory is /mnt/data, and the results directory is /mnt/results.

Wrapper options:
  --image IMAGE        Override the dependency image.
  --data-dir DIR       Mount a host dataset directory read-only at /mnt/data.
  --results-dir DIR    Host results directory (required; mounted at /mnt/results).
  --cpuset-cpus LIST   Pin the container to Docker's CPU list syntax (for example 2-5).
  -h, --help           Show this help.

Example:
  ./run-in-container.sh --results-dir "$PWD/build/benchmark-results" -- \
    --manifest /mnt/repo/components/core/tools/benchmarks/clp-s/manifest.smoke.json \
    --cpp-binary /mnt/repo/build/cpp/clp-s \
    --rust-binary /mnt/repo/build/rust/clp-s \
    --build-metadata /mnt/repo/build/clp-s-build-metadata.json \
    --results-dir /mnt/results
EOF
}

case "$(uname -m)" in
    x86_64 | amd64)
        default_image="clp-core-dependencies-x86-manylinux_2_28:dev"
        ;;
    aarch64 | arm64)
        default_image="clp-core-dependencies-aarch64-manylinux_2_28:dev"
        ;;
    *)
        default_image=""
        ;;
esac

image="${CLP_S_BENCH_IMAGE:-${default_image}}"
data_dir=""
results_dir=""
cpuset_cpus="${CLP_S_BENCH_CPUSET:-}"

while (($# > 0)); do
    case "$1" in
        --image)
            image="${2:?--image requires a value}"
            shift 2
            ;;
        --data-dir)
            data_dir="${2:?--data-dir requires a value}"
            shift 2
            ;;
        --results-dir)
            results_dir="${2:?--results-dir requires a value}"
            shift 2
            ;;
        --cpuset-cpus)
            cpuset_cpus="${2:?--cpuset-cpus requires a value}"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            echo >&2 "ERROR: Unknown wrapper option '$1'."
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "${results_dir}" ]]; then
    echo >&2 "ERROR: --results-dir is required."
    exit 2
fi
if [[ -z "${image}" ]]; then
    echo >&2 "ERROR: Unsupported architecture '$(uname -m)'. Set --image explicitly."
    exit 2
fi
if (($# == 0)); then
    echo >&2 "ERROR: Runner options must follow --."
    exit 2
fi
if [[ -n "${data_dir}" && ! -d "${data_dir}" ]]; then
    echo >&2 "ERROR: Data directory does not exist: ${data_dir}"
    exit 2
fi

mkdir -p "${results_dir}"
readonly resolved_results_dir="$(readlink -f "${results_dir}")"
if [[ "${resolved_results_dir}" == "${repo_root}" ]]; then
    echo >&2 "ERROR: The repository root cannot be used as the results directory."
    exit 2
fi
if [[ "${resolved_results_dir}" == "${repo_root}"/* ]]; then
    relative_results_dir="${resolved_results_dir#"${repo_root}"/}"
    if ! git -C "${repo_root}" check-ignore --quiet "${relative_results_dir}"; then
        echo >&2 "ERROR: A results directory inside the repository must be git-ignored."
        echo >&2 "Use an external directory or a path below ${repo_root}/build."
        exit 2
    fi
fi
readonly image_id="$(docker image inspect --format '{{.Id}}' "${image}")"

docker_args=(
    run
    --rm
    --init
    --network none
    --user "$(id -u):$(id -g)"
    --mount "type=bind,src=${repo_root},dst=/mnt/repo,readonly"
    --mount "type=bind,src=${resolved_results_dir},dst=/mnt/results"
    --workdir /mnt/repo
    --env CLP_S_BENCHMARK_CONTAINER=1
    --env "CLP_S_BENCH_IMAGE_REFERENCE=${image}"
    --env "CLP_S_BENCH_IMAGE_ID=${image_id}"
)

if [[ -n "${data_dir}" ]]; then
    docker_args+=(
        --mount "type=bind,src=$(readlink -f "${data_dir}"),dst=/mnt/data,readonly"
    )
fi
if [[ -n "${cpuset_cpus}" ]]; then
    docker_args+=(--cpuset-cpus "${cpuset_cpus}")
fi

exec docker "${docker_args[@]}" "${image}" \
    python3 /mnt/repo/components/core/tools/benchmarks/clp-s/benchmark.py "$@"
