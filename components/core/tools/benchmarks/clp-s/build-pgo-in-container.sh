#!/usr/bin/env bash

set -o errexit
set -o nounset
set -o pipefail

readonly script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
readonly repo_root="$(git -C "${script_dir}" rev-parse --show-toplevel)"
readonly required_image_repository="clp-core-dependencies-x86-manylinux_2_28"
readonly default_image="${required_image_repository}:dev"

usage() {
    cat <<'EOF'
Usage: build-pgo-in-container.sh OPTIONS

Builds a balanced, profile-guided clp-s release binary entirely inside the
mandatory x86-64 manylinux_2_28 dependency image. The repository and training
inputs are mounted read-only, Cargo is offline, and only the output and existing
Cargo/rustup homes are writable.

Required options:
  --output-dir DIR          Empty host directory for profiles, targets, and clp-s.
  --toolchain-dir DIR       Existing CARGO_HOME/RUSTUP_HOME parent containing
                            .cargo and .rustup with nightly-2026-08-30 installed.
  --source-checksums FILE   SHA-256 manifest for repository build inputs. Each
                            line is: HASH<two spaces>REPO_RELATIVE_PATH.
  --corpus FILE             NDJSON training corpus.
  --corpus-sha256 HASH      Expected corpus SHA-256.
  --archive FILE            Reference single-file CLP-S archive.
  --archive-sha256 HASH     Expected archive SHA-256.
  --timestamp-key KEY       Timestamp field used for log-order compression.
  --rare-query QUERY        Search expected to return a small non-empty result.
  --error-query QUERY       Error-shaped search used with --count; must be non-empty.

Optional:
  --image IMAGE             manylinux image tag or digest (default:
                            clp-core-dependencies-x86-manylinux_2_28:dev).
  -h, --help                Show this help.

The image reference may include a registry prefix, tag, or digest, but its
repository name must remain clp-core-dependencies-x86-manylinux_2_28.
EOF
}

die() {
    echo >&2 "ERROR: $*"
    exit 2
}

require_value() {
    local option="$1"
    local value="${2-}"
    [[ -n "${value}" ]] || die "${option} requires a value."
}

validate_sha256() {
    local label="$1"
    local value="$2"
    [[ "${value}" =~ ^[0-9a-f]{64}$ ]] || die "${label} must be 64 lowercase hexadecimal characters."
}

validate_relative_source_path() {
    local relative_path="$1"
    local component
    local -a components=()

    [[ -n "${relative_path}" ]] || die "Source checksum manifest contains an empty path."
    [[ "${relative_path}" != /* ]] || die "Source path must be repository-relative: ${relative_path}"
    [[ "${relative_path}" != *\\* ]] || die "Backslashes are not supported in source paths: ${relative_path}"
    [[ "${relative_path}" != *$'\r'* && "${relative_path}" != *$'\t'* ]] \
        || die "Control characters are not supported in source paths."
    [[ "${relative_path}" =~ ^[A-Za-z0-9._/@+\ -]+$ ]] \
        || die "Source path contains unsupported characters: ${relative_path}"

    IFS='/' read -r -a components <<<"${relative_path}"
    for component in "${components[@]}"; do
        [[ -n "${component}" && "${component}" != "." && "${component}" != ".." ]] \
            || die "Source path contains an unsafe component: ${relative_path}"
    done
}

validate_source_manifest() {
    local manifest="$1"
    local line
    local hash
    local required_path
    local relative_path
    local resolved_path
    local line_number=0
    local entry_count=0
    local -a required_paths=(
        .cargo/config.toml
        Cargo.lock
        Cargo.toml
        components/clp-s/Cargo.toml
        components/clp-s-container/Cargo.toml
        components/clp-s-container/build.rs
        components/core/tools/benchmarks/clp-s/build-pgo-in-container.sh
        components/core/tools/benchmarks/clp-s/pgo-build.sh
    )
    declare -A seen_paths=()

    while IFS= read -r line || [[ -n "${line}" ]]; do
        ((line_number += 1))
        ((${#line} >= 67)) \
            || die "Invalid source checksum line ${line_number}: expected HASH, two spaces, and a path."
        hash="${line:0:64}"
        [[ "${line:64:2}" == "  " ]] \
            || die "Invalid source checksum line ${line_number}: expected exactly two separator spaces."
        relative_path="${line:66}"
        validate_sha256 "Source hash on line ${line_number}" "${hash}"
        validate_relative_source_path "${relative_path}"
        [[ -z "${seen_paths[${relative_path}]+present}" ]] \
            || die "Duplicate source path in checksum manifest: ${relative_path}"
        seen_paths["${relative_path}"]=1

        resolved_path="$(readlink -f -- "${repo_root}/${relative_path}")" \
            || die "Source path does not resolve: ${relative_path}"
        [[ "${resolved_path}" == "${repo_root}"/* ]] \
            || die "Source path resolves outside the repository: ${relative_path}"
        [[ -f "${resolved_path}" ]] || die "Source path is not a regular file: ${relative_path}"
        ((entry_count += 1))
    done <"${manifest}"

    ((entry_count > 0)) || die "Source checksum manifest must contain at least one entry."
    for required_path in "${required_paths[@]}"; do
        [[ -n "${seen_paths[${required_path}]+present}" ]] \
            || die "Source checksum manifest must include ${required_path}."
    done

    while IFS= read -r -d '' required_path; do
        relative_path="${required_path#"${repo_root}"/}"
        [[ -n "${seen_paths[${relative_path}]+present}" ]] \
            || die "Source checksum manifest must include ${relative_path}."
    done < <(
        {
            find \
                "${repo_root}/.cargo" \
                "${repo_root}/components/clp-s" \
                "${repo_root}/components/clp-s-container" \
                \( -type f -o -type l \) \
                -print0
            find "${repo_root}/components" \
                -mindepth 2 \
                -maxdepth 2 \
                \( -type f -o -type l \) \
                \( -name Cargo.toml -o -name build.rs \) \
                -print0
        } | LC_ALL=C sort -zu
    )
}

paths_overlap() {
    local left="$1"
    local right="$2"

    [[ \
        "${left}" == "${right}" \
            || "${left}" == "${right}/"* \
            || "${right}" == "${left}/"* \
    ]]
}

reject_cargo_home_config() {
    local cargo_home="$1"
    local config

    for config in config config.toml; do
        [[ ! -e "${cargo_home}/${config}" ]] \
            || die "Cargo home must not contain ${config}; use the repository's checked configuration."
    done
}

image="${CLP_S_PGO_IMAGE:-${default_image}}"
output_dir=""
toolchain_dir=""
source_checksums=""
corpus=""
corpus_sha256=""
archive=""
archive_sha256=""
timestamp_key=""
rare_query=""
error_query=""

while (($# > 0)); do
    case "$1" in
        --image)
            require_value "$1" "${2-}"
            image="$2"
            shift 2
            ;;
        --output-dir)
            require_value "$1" "${2-}"
            output_dir="$2"
            shift 2
            ;;
        --toolchain-dir)
            require_value "$1" "${2-}"
            toolchain_dir="$2"
            shift 2
            ;;
        --source-checksums)
            require_value "$1" "${2-}"
            source_checksums="$2"
            shift 2
            ;;
        --corpus)
            require_value "$1" "${2-}"
            corpus="$2"
            shift 2
            ;;
        --corpus-sha256)
            require_value "$1" "${2-}"
            corpus_sha256="$2"
            shift 2
            ;;
        --archive)
            require_value "$1" "${2-}"
            archive="$2"
            shift 2
            ;;
        --archive-sha256)
            require_value "$1" "${2-}"
            archive_sha256="$2"
            shift 2
            ;;
        --timestamp-key)
            require_value "$1" "${2-}"
            timestamp_key="$2"
            shift 2
            ;;
        --rare-query)
            require_value "$1" "${2-}"
            rare_query="$2"
            shift 2
            ;;
        --error-query)
            require_value "$1" "${2-}"
            error_query="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "Unknown option '$1'."
            ;;
    esac
done

require_value --output-dir "${output_dir}"
require_value --toolchain-dir "${toolchain_dir}"
require_value --source-checksums "${source_checksums}"
require_value --corpus "${corpus}"
require_value --corpus-sha256 "${corpus_sha256}"
require_value --archive "${archive}"
require_value --archive-sha256 "${archive_sha256}"
require_value --timestamp-key "${timestamp_key}"
require_value --rare-query "${rare_query}"
require_value --error-query "${error_query}"
validate_sha256 "Corpus hash" "${corpus_sha256}"
validate_sha256 "Archive hash" "${archive_sha256}"

case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) die "This PGO workflow requires an x86-64 host and target." ;;
esac

image_without_digest="${image%%@*}"
image_basename="${image_without_digest##*/}"
image_repository="${image_basename%%:*}"
[[ "${image_repository}" == "${required_image_repository}" ]] \
    || die "Image repository must be ${required_image_repository}; got ${image}."

command -v docker >/dev/null || die "docker is not available."
[[ -f "${source_checksums}" ]] || die "Source checksum manifest does not exist: ${source_checksums}"
[[ -f "${corpus}" ]] || die "Corpus does not exist: ${corpus}"
[[ -f "${archive}" ]] || die "Archive does not exist: ${archive}"
[[ -d "${toolchain_dir}/.cargo" && -d "${toolchain_dir}/.rustup" ]] \
    || die "Toolchain directory must contain .cargo and .rustup: ${toolchain_dir}"

readonly resolved_source_checksums="$(readlink -f -- "${source_checksums}")"
readonly resolved_corpus="$(readlink -f -- "${corpus}")"
readonly resolved_archive="$(readlink -f -- "${archive}")"
readonly resolved_toolchain_dir="$(readlink -f -- "${toolchain_dir}")"
readonly resolved_cargo_home="$(readlink -f -- "${toolchain_dir}/.cargo")"
readonly resolved_rustup_home="$(readlink -f -- "${toolchain_dir}/.rustup")"
paths_overlap "${resolved_cargo_home}" "${resolved_rustup_home}" \
    && die "Cargo home and rustup home must not overlap."
for resolved_input in \
    "${resolved_source_checksums}" \
    "${resolved_corpus}" \
    "${resolved_archive}"; do
    paths_overlap "${resolved_input}" "${resolved_cargo_home}" \
        && die "Read-only inputs must not overlap Cargo home: ${resolved_input}"
    paths_overlap "${resolved_input}" "${resolved_rustup_home}" \
        && die "Read-only inputs must not overlap rustup home: ${resolved_input}"
done
reject_cargo_home_config "${resolved_cargo_home}"
validate_source_manifest "${resolved_source_checksums}"

mkdir -p -- "${output_dir}"
readonly resolved_output_dir="$(readlink -f -- "${output_dir}")"
[[ "${resolved_output_dir}" != "${repo_root}" ]] \
    || die "The repository root cannot be used as the output directory."
[[ "${resolved_output_dir}" != "${resolved_toolchain_dir}" ]] \
    || die "The toolchain directory cannot be used as the output directory."
paths_overlap "${resolved_output_dir}" "${resolved_cargo_home}" \
    && die "The output directory must not overlap Cargo home."
paths_overlap "${resolved_output_dir}" "${resolved_rustup_home}" \
    && die "The output directory must not overlap rustup home."
if [[ -n "$(find "${resolved_output_dir}" -mindepth 1 -print -quit)" ]]; then
    die "Output directory must be empty: ${resolved_output_dir}"
fi
if [[ "${resolved_output_dir}" == "${repo_root}"/* ]]; then
    relative_output_dir="${resolved_output_dir#"${repo_root}"/}"
    git -C "${repo_root}" check-ignore --quiet -- "${relative_output_dir}" \
        || die "An output directory inside the repository must be Git-ignored (use build/)."
fi

read -r image_id image_os image_architecture < <(
    docker image inspect --format '{{.Id}} {{.Os}} {{.Architecture}}' "${image}"
)
[[ "${image_os}" == "linux" && "${image_architecture}" == "amd64" ]] \
    || die "Image must be linux/amd64; got ${image_os}/${image_architecture}."

docker_args=(
    run
    --rm
    --init
    --network none
    --read-only
    --cap-drop ALL
    --security-opt no-new-privileges
    --user "$(id -u):$(id -g)"
    --mount "type=bind,src=${repo_root},dst=/mnt/repo,readonly"
    --mount "type=bind,src=${resolved_source_checksums},dst=/mnt/input/source.sha256,readonly"
    --mount "type=bind,src=${resolved_corpus},dst=/mnt/input/corpus,readonly"
    --mount "type=bind,src=${resolved_archive},dst=/mnt/input/archive,readonly"
    --mount "type=bind,src=${resolved_cargo_home},dst=/mnt/cargo-home"
    --mount "type=bind,src=${resolved_rustup_home},dst=/mnt/rustup-home"
    --mount "type=bind,src=${resolved_output_dir},dst=/mnt/output"
    --tmpfs /tmp:rw,nosuid,nodev,exec,mode=1777
    --workdir /mnt/repo
    --env CLP_S_PGO_CONTAINER=1
    --env "CLP_S_PGO_IMAGE_REFERENCE=${image}"
    --env "CLP_S_PGO_IMAGE_ID=${image_id}"
)

exec docker "${docker_args[@]}" "${image_id}" \
    bash /mnt/repo/components/core/tools/benchmarks/clp-s/pgo-build.sh \
    --corpus-sha256 "${corpus_sha256}" \
    --archive-sha256 "${archive_sha256}" \
    --timestamp-key "${timestamp_key}" \
    --rare-query "${rare_query}" \
    --error-query "${error_query}"
