#!/usr/bin/env bash

set -o errexit
set -o nounset
set -o pipefail

readonly toolchain="nightly-2026-08-30"
readonly target="x86_64-unknown-linux-gnu"
readonly expected_rustc_commit="fd7ed57dfd3bdebb745a1d8158638727b0e7047a"
readonly expected_llvm_version="23.1.0"
readonly expected_cargo_version="cargo 1.100.0-nightly (e8cb624d5 2026-08-22)"
readonly expected_cargo_sha256="c23da3046e007c9e3997f3b72d5c38cd5018198d1d2834794434453296205522"
readonly expected_rustc_sha256="218ffe79d8aa029f8db43adc1b8de03d2a0f42eadaa166dd4fc50f108696f8c2"
readonly expected_llvm_profdata_sha256="35ad3351c2e69ac397f776b0482c587f6105b983d6b5d1a7cbc97d4957bd9269"
readonly repo_root="/mnt/repo"
readonly output_root="/mnt/output"
readonly input_root="/mnt/input"
readonly corpus="${input_root}/corpus"
readonly archive="${input_root}/archive"
readonly source_checksums="${input_root}/source.sha256"
readonly raw_profiles="${output_root}/raw"
readonly training_root="${output_root}/train"
readonly generate_target="${output_root}/target-generate"
readonly use_target="${output_root}/target-use"
readonly merged_profile="${output_root}/merged.profdata"
readonly final_binary="${output_root}/clp-s"
readonly validation_work="${output_root}/validation-work"
readonly common_rustflags="-Dclippy::all -Dclippy::nursery -Dclippy::pedantic --remap-path-prefix=/mnt/repo=/usr/src/clp --remap-path-prefix=/mnt/cargo-home=/usr/local/cargo-home --remap-path-prefix=/mnt/rustup-home=/usr/local/rustup-home"

usage() {
    cat <<'EOF'
Usage: pgo-build.sh OPTIONS

Internal container entry point for build-pgo-in-container.sh. Direct host
execution is rejected.

Required options:
  --corpus-sha256 HASH
  --archive-sha256 HASH
  --timestamp-key KEY
  --rare-query QUERY
  --error-query QUERY
  -h, --help
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
    local resolved_path
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

    resolved_path="$(readlink -f -- "${repo_root}/${relative_path}")" \
        || die "Source path does not resolve: ${relative_path}"
    [[ "${resolved_path}" == "${repo_root}"/* ]] \
        || die "Source path resolves outside the repository: ${relative_path}"
    [[ -f "${resolved_path}" ]] || die "Source path is not a regular file: ${relative_path}"
}

record_command() {
    local argument
    for argument in "$@"; do
        printf '%q ' "${argument}" >>"${output_root}/training-commands.txt"
    done
    printf '\n' >>"${output_root}/training-commands.txt"
}

record_validation_command() {
    local argument
    for argument in "$@"; do
        printf '%q ' "${argument}" >>"${output_root}/validation-commands.txt"
    done
    printf '\n' >>"${output_root}/validation-commands.txt"
}

run_profiled() {
    record_command "$@"
    "$@" 2>&1 | tee -a "${output_root}/training.log"
    ((profiled_command_count += 1))
}

run_validation() {
    record_validation_command "$@"
    "$@" 2>&1 | tee -a "${output_root}/validation.log"
    ((validation_command_count += 1))
}

validate_single_nonempty_file_tree() {
    local label="$1"
    local directory="$2"
    local -a files=()

    mapfile -d '' files < <(find "${directory}" -type f -print0)
    ((${#files[@]} == 1)) || die "${label} must contain exactly one regular file."
    [[ -s "${files[0]}" ]] || die "${label} output is empty."
}

validate_exact_extraction() {
    local label="$1"
    local binary="$2"
    local source_archive="$3"
    local destination="$4"
    local expected_sha256="$5"
    local profile_file="${6-}"
    local actual_sha256
    local -a files=()

    if [[ -n "${profile_file}" ]]; then
        run_validation env "LLVM_PROFILE_FILE=${profile_file}" \
            "${binary}" x "${source_archive}" "${destination}"
    else
        run_validation "${binary}" x "${source_archive}" "${destination}"
    fi
    validate_single_nonempty_file_tree "${label}" "${destination}"
    mapfile -d '' files < <(find "${destination}" -type f -print0)
    actual_sha256="$(sha256sum "${files[0]}" | cut -d ' ' -f 1)"
    [[ "${actual_sha256}" == "${expected_sha256}" ]] \
        || die "${label} extraction hash was ${actual_sha256}; expected ${expected_sha256}."
    printf '%s_sha256=%s\n' "${label}" "${actual_sha256}" \
        >>"${output_root}/validation-summary.txt"
    [[ "${destination}" == "${validation_work}/"* ]] \
        || die "Refusing to remove validation output outside ${validation_work}."
    find "${destination}" -depth -delete
}

corpus_sha256=""
archive_sha256=""
timestamp_key=""
rare_query=""
error_query=""

while (($# > 0)); do
    case "$1" in
        --corpus-sha256)
            require_value "$1" "${2-}"
            corpus_sha256="$2"
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

require_value --corpus-sha256 "${corpus_sha256}"
require_value --archive-sha256 "${archive_sha256}"
require_value --timestamp-key "${timestamp_key}"
require_value --rare-query "${rare_query}"
require_value --error-query "${error_query}"
validate_sha256 "Corpus hash" "${corpus_sha256}"
validate_sha256 "Archive hash" "${archive_sha256}"

[[ "${CLP_S_PGO_CONTAINER:-}" == "1" && -f /.dockerenv ]] \
    || die "Run this script through build-pgo-in-container.sh."
[[ -n "${CLP_S_PGO_IMAGE_REFERENCE:-}" && -n "${CLP_S_PGO_IMAGE_ID:-}" ]] \
    || die "Container image provenance is missing."
[[ -d "${repo_root}" && -d "${output_root}" && -d "${input_root}" ]] \
    || die "Required container mounts are missing."
[[ -f "${corpus}" && -f "${archive}" && -f "${source_checksums}" ]] \
    || die "One or more read-only input mounts are missing."
[[ -d /mnt/cargo-home && -d /mnt/rustup-home ]] \
    || die "The mounted Rust toolchain is incomplete."
[[ ! -e /mnt/cargo-home/config && ! -e /mnt/cargo-home/config.toml ]] \
    || die "Cargo home must not contain configuration; use the checked repository configuration."
if [[ -n "$(find "${output_root}" -mindepth 1 -print -quit)" ]]; then
    die "Output directory must be empty: ${output_root}"
fi

export CARGO_HOME=/mnt/cargo-home
export RUSTUP_HOME=/mnt/rustup-home
export PATH="${CARGO_HOME}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export CARGO_NET_OFFLINE=true
export CARGO_INCREMENTAL=0
export LC_ALL=C.UTF-8
export TZ=UTC
export OMP_NUM_THREADS=1
export RAYON_NUM_THREADS=1
unset CARGO_ENCODED_RUSTFLAGS RUSTFLAGS

line_number=0
source_entry_count=0
declare -A seen_source_paths=()
declare -a required_source_paths=(
    .cargo/config.toml
    Cargo.lock
    Cargo.toml
    components/clp-s/Cargo.toml
    components/clp-s-container/Cargo.toml
    components/clp-s-container/build.rs
    components/core/tools/benchmarks/clp-s/build-pgo-in-container.sh
    components/core/tools/benchmarks/clp-s/pgo-build.sh
)
while IFS= read -r line || [[ -n "${line}" ]]; do
    ((line_number += 1))
    ((${#line} >= 67)) \
        || die "Invalid source checksum line ${line_number}: expected HASH, two spaces, and a path."
    source_hash="${line:0:64}"
    [[ "${line:64:2}" == "  " ]] \
        || die "Invalid source checksum line ${line_number}: expected exactly two separator spaces."
    relative_source_path="${line:66}"
    validate_sha256 "Source hash on line ${line_number}" "${source_hash}"
    validate_relative_source_path "${relative_source_path}"
    [[ -z "${seen_source_paths[${relative_source_path}]+present}" ]] \
        || die "Duplicate source path in checksum manifest: ${relative_source_path}"
    seen_source_paths["${relative_source_path}"]=1
    ((source_entry_count += 1))
done <"${source_checksums}"
((source_entry_count > 0)) || die "Source checksum manifest must contain at least one entry."
for relative_source_path in "${required_source_paths[@]}"; do
    [[ -n "${seen_source_paths[${relative_source_path}]+present}" ]] \
        || die "Source checksum manifest must include ${relative_source_path}."
done

while IFS= read -r -d '' required_source_path; do
    relative_source_path="${required_source_path#"${repo_root}"/}"
    [[ -n "${seen_source_paths[${relative_source_path}]+present}" ]] \
        || die "Source checksum manifest must include ${relative_source_path}."
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

LC_ALL=C sort "${source_checksums}" >"${output_root}/source-checksums.verified"
chmod 0444 "${output_root}/source-checksums.verified"
source_fingerprint="$(
    sha256sum "${output_root}/source-checksums.verified" | cut -d ' ' -f 1
)"
(
    cd "${repo_root}"
    sha256sum --check --strict --quiet "${output_root}/source-checksums.verified"
)
printf '%s  %s\n' \
    "${corpus_sha256}" "${corpus}" \
    "${archive_sha256}" "${archive}" \
    | sha256sum --check --strict --quiet

rustc_verbose="$(rustc +"${toolchain}" -vV)"
cargo_version="$(cargo +"${toolchain}" --version)"
[[ "${cargo_version}" == "${expected_cargo_version}" ]] \
    || die "nightly Cargo version does not match ${expected_cargo_version}."
grep -Fxq "commit-hash: ${expected_rustc_commit}" <<<"${rustc_verbose}" \
    || die "nightly rustc commit does not match ${expected_rustc_commit}."
grep -Fxq "host: ${target}" <<<"${rustc_verbose}" \
    || die "nightly rustc host is not ${target}."
grep -Fxq "LLVM version: ${expected_llvm_version}" <<<"${rustc_verbose}" \
    || die "nightly rustc LLVM version does not match ${expected_llvm_version}."

rust_sysroot="$(rustc +"${toolchain}" --print sysroot)"
rustc_binary="${rust_sysroot}/bin/rustc"
cargo_binary="${rust_sysroot}/bin/cargo"
llvm_profdata="${rust_sysroot}/lib/rustlib/${target}/bin/llvm-profdata"
[[ -x "${rustc_binary}" && -x "${cargo_binary}" ]] \
    || die "The dated toolchain is missing rustc or Cargo."
printf '%s  %s\n' \
    "${expected_rustc_sha256}" "${rustc_binary}" \
    "${expected_cargo_sha256}" "${cargo_binary}" \
    | sha256sum --check --strict --quiet
[[ -x "${llvm_profdata}" ]] || die "llvm-tools-preview is missing llvm-profdata: ${llvm_profdata}"
printf '%s  %s\n' "${expected_llvm_profdata_sha256}" "${llvm_profdata}" \
    | sha256sum --check --strict --quiet
llvm_profdata_version="$("${llvm_profdata}" --version)"
grep -Fq "LLVM version ${expected_llvm_version}-rust-" <<<"${llvm_profdata_version}" \
    || die "llvm-profdata does not match rustc's LLVM ${expected_llvm_version}."

metadata_command=(
    cargo "+${toolchain}" metadata
    --locked
    --offline
    --no-deps
    --format-version 1
)
printf '%q ' "${metadata_command[@]}" >"${output_root}/cargo-metadata-command.txt"
printf '\n' >>"${output_root}/cargo-metadata-command.txt"
"${metadata_command[@]}" \
    >"${output_root}/cargo-metadata.json" \
    2>"${output_root}/cargo-metadata.stderr"

python3 - \
    "${repo_root}" \
    "${output_root}/source-checksums.verified" \
    "${output_root}/cargo-metadata.json" <<'PY'
import json
import pathlib
import sys

repo_root = pathlib.Path(sys.argv[1]).resolve(strict=True)
checksum_manifest = pathlib.Path(sys.argv[2])
metadata_path = pathlib.Path(sys.argv[3])

manifest_paths = {
    line[66:]
    for line in checksum_manifest.read_text().splitlines()
}
metadata = json.loads(metadata_path.read_text())
metadata_root = pathlib.Path(metadata["workspace_root"]).resolve(strict=True)
if metadata_root != repo_root:
    raise SystemExit(f"Cargo workspace root {metadata_root} is not {repo_root}")

workspace_members = set(metadata["workspace_members"])
workspace_packages = [
    package for package in metadata["packages"] if package["id"] in workspace_members
]
if len(workspace_packages) != len(workspace_members):
    raise SystemExit("Cargo metadata omitted one or more workspace members")

required_paths = set()
for package in workspace_packages:
    candidates = [package["manifest_path"]]
    candidates.extend(
        target["src_path"]
        for target in package["targets"]
        if "custom-build" in target["kind"]
    )
    for candidate in candidates:
        resolved = pathlib.Path(candidate).resolve(strict=True)
        try:
            required_paths.add(resolved.relative_to(repo_root).as_posix())
        except ValueError as error:
            raise SystemExit(f"Cargo input resolves outside the repository: {resolved}") from error

missing = sorted(required_paths - manifest_paths)
if missing:
    raise SystemExit(
        "source checksum manifest omits Cargo-parsed inputs:\n  " + "\n  ".join(missing)
    )
PY

mkdir "${raw_profiles}" "${training_root}"

{
    printf '%s\n' "${rustc_verbose}"
    printf '%s\n' "${cargo_version}"
    printf '%s\n' "${llvm_profdata_version}"
    sha256sum "${rustc_binary}" "${cargo_binary}" "${llvm_profdata}"
} >"${output_root}/toolchain.txt"

generate_rustflags="${common_rustflags} -Cprofile-generate=${raw_profiles}"
use_rustflags="${common_rustflags} -Cprofile-use=${merged_profile} -Cllvm-args=-pgo-warn-missing-function"
cargo_build=(
    cargo "+${toolchain}" build
    --locked
    --offline
    --profile clp-s-release
    --package clp-s
    --bin clp-s
    --target "${target}"
)

{
    printf 'CARGO_TARGET_DIR=%q RUSTFLAGS=%q ' "${generate_target}" "${generate_rustflags}"
    printf '%q ' "${cargo_build[@]}"
    printf '\n'
} >"${output_root}/build-commands.txt"

env \
    "CARGO_TARGET_DIR=${generate_target}" \
    "RUSTFLAGS=${generate_rustflags}" \
    "${cargo_build[@]}" 2>&1 | tee "${output_root}/generate-build.log"

readonly generate_binary="${generate_target}/${target}/clp-s-release/clp-s"
[[ -x "${generate_binary}" ]] || die "Instrumented clp-s binary was not produced."
export LLVM_PROFILE_FILE="${raw_profiles}/clp-s-%m-%p.profraw"
profiled_command_count=0
: >"${output_root}/training-commands.txt"
: >"${output_root}/training.log"

run_profiled "${generate_binary}" c \
    --single-file-archive \
    --timestamp-key "${timestamp_key}" \
    "${training_root}/compress-log" \
    "${corpus}"

run_profiled "${generate_binary}" c \
    --single-file-archive \
    --timestamp-key "${timestamp_key}" \
    --disable-log-order \
    "${training_root}/compress-no-log" \
    "${corpus}"

run_profiled "${generate_binary}" x \
    "${archive}" \
    "${training_root}/extract-unordered"

run_profiled "${generate_binary}" x \
    --ordered \
    "${archive}" \
    "${training_root}/extract-ordered"

record_command "${generate_binary}" s "${archive}" "${rare_query}" \
    '>' "${training_root}/search-rare.ndjson"
"${generate_binary}" s \
    "${archive}" \
    "${rare_query}" \
    >"${training_root}/search-rare.ndjson" \
    2>>"${output_root}/training.log"
((profiled_command_count += 1))

record_command "${generate_binary}" s "${archive}" '*: *' --count \
    '>' "${training_root}/search-all-count.json"
"${generate_binary}" s \
    "${archive}" \
    '*: *' \
    --count \
    >"${training_root}/search-all-count.json" \
    2>>"${output_root}/training.log"
((profiled_command_count += 1))

record_command "${generate_binary}" s "${archive}" "${error_query}" --count \
    '>' "${training_root}/search-error-count.json"
"${generate_binary}" s \
    "${archive}" \
    "${error_query}" \
    --count \
    >"${training_root}/search-error-count.json" \
    2>>"${output_root}/training.log"
((profiled_command_count += 1))

((profiled_command_count == 7)) || die "Expected exactly seven profiled commands."
validate_single_nonempty_file_tree "Log-order compression" "${training_root}/compress-log"
validate_single_nonempty_file_tree "No-log-order compression" "${training_root}/compress-no-log"
validate_single_nonempty_file_tree "Unordered extraction" "${training_root}/extract-unordered"
validate_single_nonempty_file_tree "Ordered extraction" "${training_root}/extract-ordered"

mapfile -d '' unordered_outputs < <(
    find "${training_root}/extract-unordered" -type f -print0
)
printf '%s  %s\n' "${corpus_sha256}" "${unordered_outputs[0]}" \
    | sha256sum --check --strict --quiet
mapfile -d '' ordered_outputs < <(
    find "${training_root}/extract-ordered" -type f -print0
)
ordered_output_sha256="$(sha256sum "${ordered_outputs[0]}" | cut -d ' ' -f 1)"

python3 - \
    "${training_root}/search-rare.ndjson" \
    "${training_root}/search-all-count.json" \
    "${training_root}/search-error-count.json" <<'PY'
import json
import pathlib
import sys

rare_path, all_count_path, error_count_path = map(pathlib.Path, sys.argv[1:])
rare_records = [json.loads(line) for line in rare_path.read_text().splitlines() if line]
if not rare_records or not all(isinstance(record, dict) for record in rare_records):
    raise SystemExit("rare-record search must return at least one JSON object")
for label, path in (("all-count", all_count_path), ("error-count", error_count_path)):
    value = json.loads(path.read_text())
    if not isinstance(value, dict) or not isinstance(value.get("count"), int) or value["count"] <= 0:
        raise SystemExit(f"{label} search must return a positive integer count")
PY

mapfile -d '' profile_files < <(
    find "${raw_profiles}" -maxdepth 1 -type f -name '*.profraw' -print0 | LC_ALL=C sort -z
)
((${#profile_files[@]} == 7)) \
    || die "Expected seven raw profile files; found ${#profile_files[@]}."
for profile_file in "${profile_files[@]}"; do
    [[ -s "${profile_file}" ]] || die "Raw profile is empty: ${profile_file}"
done

record_command "${llvm_profdata}" merge -o "${merged_profile}" "${profile_files[@]}"
"${llvm_profdata}" merge \
    -o "${merged_profile}" \
    "${profile_files[@]}" 2>&1 | tee "${output_root}/profile-merge.log"
[[ -s "${merged_profile}" ]] || die "Merged profile was not produced."
"${llvm_profdata}" show "${merged_profile}" >"${output_root}/profile-summary.txt"

{
    printf 'CARGO_TARGET_DIR=%q RUSTFLAGS=%q ' "${use_target}" "${use_rustflags}"
    printf '%q ' "${cargo_build[@]}"
    printf '\n'
} >>"${output_root}/build-commands.txt"

env \
    "CARGO_TARGET_DIR=${use_target}" \
    "RUSTFLAGS=${use_rustflags}" \
    "${cargo_build[@]}" 2>&1 | tee "${output_root}/profile-use-build.log"

readonly use_binary="${use_target}/${target}/clp-s-release/clp-s"
[[ -x "${use_binary}" ]] || die "Profile-use clp-s binary was not produced."
missing_function_warning_lines="$(
    grep -Eic 'profile.*(missing|unavailable)|no profile data|missing function' \
        "${output_root}/profile-use-build.log" || true
)"
grep -Ei '(^warning:|profile.*(missing|unavailable)|no profile data|missing function)' \
    "${output_root}/profile-use-build.log" \
    >"${output_root}/profile-use-warnings.txt" || true
((missing_function_warning_lines == 0)) \
    || die "Profile-use build reported ${missing_function_warning_lines} missing-profile warnings."

chmod 0555 "${use_binary}"
install -m 0555 "${use_binary}" "${final_binary}"
unset LLVM_PROFILE_FILE

mkdir "${validation_work}"
validation_command_count=0
: >"${output_root}/validation-commands.txt"
: >"${output_root}/validation.log"
printf 'format_version=1\n' >"${output_root}/validation-summary.txt"

run_validation "${final_binary}" c \
    --single-file-archive \
    --timestamp-key "${timestamp_key}" \
    "${validation_work}/final-compress-log" \
    "${corpus}"
run_validation "${final_binary}" c \
    --single-file-archive \
    --timestamp-key "${timestamp_key}" \
    --disable-log-order \
    "${validation_work}/final-compress-no-log" \
    "${corpus}"

validate_single_nonempty_file_tree \
    "Final log-order compression" \
    "${validation_work}/final-compress-log"
validate_single_nonempty_file_tree \
    "Final no-log-order compression" \
    "${validation_work}/final-compress-no-log"
mapfile -d '' generate_log_archives < <(
    find "${training_root}/compress-log" -type f -print0
)
mapfile -d '' generate_no_log_archives < <(
    find "${training_root}/compress-no-log" -type f -print0
)
mapfile -d '' final_log_archives < <(
    find "${validation_work}/final-compress-log" -type f -print0
)
mapfile -d '' final_no_log_archives < <(
    find "${validation_work}/final-compress-no-log" -type f -print0
)
printf 'final_log_archive_sha256=%s\n' \
    "$(sha256sum "${final_log_archives[0]}" | cut -d ' ' -f 1)" \
    >>"${output_root}/validation-summary.txt"
printf 'final_no_log_archive_sha256=%s\n' \
    "$(sha256sum "${final_no_log_archives[0]}" | cut -d ' ' -f 1)" \
    >>"${output_root}/validation-summary.txt"

validate_exact_extraction \
    final_reads_generate_log \
    "${final_binary}" \
    "${generate_log_archives[0]}" \
    "${validation_work}/final-reads-generate-log" \
    "${corpus_sha256}"
validate_exact_extraction \
    final_reads_generate_no_log \
    "${final_binary}" \
    "${generate_no_log_archives[0]}" \
    "${validation_work}/final-reads-generate-no-log" \
    "${corpus_sha256}"
validate_exact_extraction \
    generate_reads_final_log \
    "${generate_binary}" \
    "${final_log_archives[0]}" \
    "${validation_work}/generate-reads-final-log" \
    "${corpus_sha256}" \
    "${validation_work}/cross-read-log-%m-%p.profraw"
validate_exact_extraction \
    generate_reads_final_no_log \
    "${generate_binary}" \
    "${final_no_log_archives[0]}" \
    "${validation_work}/generate-reads-final-no-log" \
    "${corpus_sha256}" \
    "${validation_work}/cross-read-no-log-%m-%p.profraw"

validate_exact_extraction \
    final_reference_unordered \
    "${final_binary}" \
    "${archive}" \
    "${validation_work}/final-reference-unordered" \
    "${corpus_sha256}"
validate_exact_extraction \
    final_reference_ordered \
    "${final_binary}" \
    "${archive}" \
    "${validation_work}/final-reference-ordered" \
    "${ordered_output_sha256}"

record_validation_command "${final_binary}" s "${archive}" "${rare_query}" \
    '>' "${validation_work}/search-rare.ndjson"
"${final_binary}" s \
    "${archive}" \
    "${rare_query}" \
    >"${validation_work}/search-rare.ndjson" \
    2>>"${output_root}/validation.log"
((validation_command_count += 1))
record_validation_command "${final_binary}" s "${archive}" '*: *' --count \
    '>' "${validation_work}/search-all-count.json"
"${final_binary}" s \
    "${archive}" \
    '*: *' \
    --count \
    >"${validation_work}/search-all-count.json" \
    2>>"${output_root}/validation.log"
((validation_command_count += 1))
record_validation_command "${final_binary}" s "${archive}" "${error_query}" --count \
    '>' "${validation_work}/search-error-count.json"
"${final_binary}" s \
    "${archive}" \
    "${error_query}" \
    --count \
    >"${validation_work}/search-error-count.json" \
    2>>"${output_root}/validation.log"
((validation_command_count += 1))

for search_name in search-rare.ndjson search-all-count.json search-error-count.json; do
    cmp --silent \
        "${training_root}/${search_name}" \
        "${validation_work}/${search_name}" \
        || die "Final ${search_name} output differs from the instrumented build."
    printf '%s_sha256=%s\n' \
        "${search_name//[-.]/_}" \
        "$(sha256sum "${validation_work}/${search_name}" | cut -d ' ' -f 1)" \
        >>"${output_root}/validation-summary.txt"
done
((validation_command_count == 11)) \
    || die "Expected exactly eleven final validation commands."
printf 'validation_command_count=%d\n' "${validation_command_count}" \
    >>"${output_root}/validation-summary.txt"
find "${validation_work}" -depth -delete
[[ ! -e "${validation_work}" ]] || die "Failed to remove temporary validation payloads."

(
    cd "${repo_root}"
    sha256sum --check --strict --quiet "${output_root}/source-checksums.verified"
)
printf '%s  %s\n' \
    "${corpus_sha256}" "${corpus}" \
    "${archive_sha256}" "${archive}" \
    | sha256sum --check --strict --quiet
printf '%s  %s\n' \
    "${expected_rustc_sha256}" "${rustc_binary}" \
    "${expected_cargo_sha256}" "${cargo_binary}" \
    "${expected_llvm_profdata_sha256}" "${llvm_profdata}" \
    | sha256sum --check --strict --quiet
mapfile -d '' final_profile_files < <(
    find "${raw_profiles}" -maxdepth 1 -type f -name '*.profraw' -print0 | LC_ALL=C sort -z
)
((${#final_profile_files[@]} == 7)) \
    || die "Validation changed the seven-profile training set."
printf 'inputs_reverified_after_validation=1\n' \
    >>"${output_root}/validation-summary.txt"

{
    printf 'format_version=1\n'
    printf 'image_reference=%s\n' "${CLP_S_PGO_IMAGE_REFERENCE}"
    printf 'image_id=%s\n' "${CLP_S_PGO_IMAGE_ID}"
    printf 'toolchain=%s\n' "${toolchain}"
    printf 'target=%s\n' "${target}"
    printf 'rustc_commit=%s\n' "${expected_rustc_commit}"
    printf 'rustc_sha256=%s\n' "${expected_rustc_sha256}"
    printf 'cargo_version=%s\n' "${expected_cargo_version}"
    printf 'cargo_sha256=%s\n' "${expected_cargo_sha256}"
    printf 'llvm_version=%s\n' "${expected_llvm_version}"
    printf 'llvm_profdata_sha256=%s\n' "${expected_llvm_profdata_sha256}"
    printf 'source_fingerprint_sha256=%s\n' "${source_fingerprint}"
    printf 'source_entry_count=%d\n' "${source_entry_count}"
    printf 'corpus_sha256=%s\n' "${corpus_sha256}"
    printf 'archive_sha256=%s\n' "${archive_sha256}"
    printf 'ordered_extraction_sha256=%s\n' "${ordered_output_sha256}"
    printf 'common_rustflags=%s\n' "${common_rustflags}"
    printf 'profiled_command_count=%d\n' "${profiled_command_count}"
    printf 'raw_profile_count=%d\n' "${#profile_files[@]}"
    printf 'profile_use_missing_function_warning_lines=%s\n' "${missing_function_warning_lines}"
    printf 'validation_command_count=%d\n' "${validation_command_count}"
} >"${output_root}/provenance.txt"

(
    cd "${output_root}"
    sha256sum \
        raw/*.profraw \
        merged.profdata \
        clp-s \
        source-checksums.verified \
        toolchain.txt \
        cargo-metadata-command.txt \
        cargo-metadata.json \
        cargo-metadata.stderr \
        build-commands.txt \
        generate-build.log \
        training-commands.txt \
        training.log \
        profile-merge.log \
        profile-summary.txt \
        profile-use-build.log \
        profile-use-warnings.txt \
        validation-commands.txt \
        validation.log \
        validation-summary.txt \
        provenance.txt \
        >SHA256SUMS
    stat -c '%s %a %n' raw/*.profraw merged.profdata clp-s >ARTIFACT-SIZES
)

printf 'PGO binary: %s\n' "${final_binary}"
sha256sum "${final_binary}" "${merged_profile}"
stat -c '%s %a %n' "${final_binary}" "${merged_profile}"
