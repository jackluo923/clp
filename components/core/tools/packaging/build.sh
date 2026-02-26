#!/usr/bin/env bash

# Builds CLP core binaries and packages them into universal packages.
#
# Supported formats:
#   deb  — Debian/Ubuntu package (built on manylinux_2_28, glibc >= 2.28)
#   rpm  — RHEL/Fedora package (built on manylinux_2_28, glibc >= 2.28)
#   apk  — Alpine package (built on musllinux_1_2, musl libc)
#
# The packages are "universal" — binaries are built on broad-compatibility base
# images and bundled with their non-system shared library dependencies via
# patchelf, so they work on any supported distribution without extra installs.
#
# Prerequisites: Docker (with buildx for cross-architecture builds)
#
# Usage: ./components/core/tools/packaging/build.sh [OPTIONS]
#
# Options:
#   --format FMT    Package format: deb, rpm, apk, or all (default: all)
#   --arch ARCH     Target architecture: aarch64, x86_64, or all (default: host)
#   --cores N       Parallel build jobs (default: nproc)
#   --version VER   Package version (default: from taskfile.yaml)
#   --output DIR    Output directory for packages (default: ./packages)
#   --clean         Remove build artifacts before building
#   --help          Show this help message

set -o errexit
set -o nounset
set -o pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd "${script_dir}/../../../.." && pwd)"

# Defaults
format="all"
cores="$(nproc 2>/dev/null || echo 4)"
version=""
output_dir="${repo_root}/packages"
target_arches=""
clean=false

# --- Argument parsing --------------------------------------------------------

while [[ $# -gt 0 ]]; do
    case $1 in
        --format)  [[ -n "${2:-}" ]] || { echo "ERROR: --format requires a value" >&2; exit 1; }
                   format="$2";        shift 2 ;;
        --arch)    [[ -n "${2:-}" ]] || { echo "ERROR: --arch requires a value" >&2; exit 1; }
                   target_arches="$2"; shift 2 ;;
        --cores)   [[ -n "${2:-}" ]] || { echo "ERROR: --cores requires a value" >&2; exit 1; }
                   cores="$2";         shift 2 ;;
        --version) [[ -n "${2:-}" ]] || { echo "ERROR: --version requires a value" >&2; exit 1; }
                   version="$2";       shift 2 ;;
        --output)  [[ -n "${2:-}" ]] || { echo "ERROR: --output requires a value" >&2; exit 1; }
                   output_dir="$2";    shift 2 ;;
        --clean)   clean=true;         shift ;;
        --help)    sed -n '/^# Usage:/,/^[^#]/{ /^#/s/^# \?//p; }' "$0"; exit 0 ;;
        *)         echo "Unknown option: $1"; echo "Use --help for usage"; exit 1 ;;
    esac
done

# --- Validate prerequisites --------------------------------------------------

if ! command -v docker &>/dev/null; then
    echo "ERROR: docker is required" >&2
    exit 1
fi

if ! docker buildx version &>/dev/null; then
    echo "ERROR: docker buildx is required but not installed." >&2
    echo "  The base image build scripts use 'docker buildx build --platform'." >&2
    echo "  Install buildx via one of:" >&2
    echo "    - Docker Desktop (includes buildx by default)" >&2
    echo "    - apt-get install docker-buildx-plugin" >&2
    echo "    - dnf install docker-buildx-plugin" >&2
    echo "    - https://github.com/docker/buildx#installing" >&2
    exit 1
fi

# --- Ensure submodules are initialized ---------------------------------------

if [[ ! -f "${repo_root}/tools/yscope-dev-utils/exports/taskfiles/utils/utils.yaml" ]]; then
    echo "==> Initializing git submodules..."
    git -C "${repo_root}" submodule update --init --recursive
fi

# --- Resolve defaults --------------------------------------------------------

valid_formats="deb rpm apk"
[[ "${format}" == "all" ]] && format="deb,rpm,apk"
IFS=',' read -ra format_list <<< "${format}"

# Validate formats early (before any side effects like stale-package cleanup)
for _fmt in "${format_list[@]}"; do
    _fmt=$(echo "${_fmt}" | xargs)
    if [[ ! " ${valid_formats} " =~ \ ${_fmt}\  ]]; then
        echo "ERROR: Unsupported format: ${_fmt} (use deb, rpm, apk, or all)" >&2
        exit 1
    fi
done

output_dir="$(mkdir -p "${output_dir}" && cd "${output_dir}" && pwd)"

if [[ -z "${target_arches}" ]]; then
    case "$(uname -m)" in
        x86_64)        target_arches="x86_64" ;;
        aarch64|arm64) target_arches="aarch64" ;;
        *)             echo "ERROR: Unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
    esac
fi
[[ "${target_arches}" == "all" ]] && target_arches="aarch64,x86_64"
IFS=',' read -ra arch_list <<< "${target_arches}"

if [[ -z "${version}" ]]; then
    version=$(grep 'G_PACKAGE_VERSION:' "${repo_root}/taskfile.yaml" \
        | head -1 \
        | sed 's/.*"\(.*\)".*/\1/')
    if [[ -z "${version}" ]]; then
        echo "ERROR: Could not extract version from taskfile.yaml and --version not provided" >&2
        exit 1
    fi
fi

# If the version has a pre-release suffix (anything after a hyphen, e.g., "-dev",
# "-beta", "-rc1"), replace it with a snapshot identifier for reproducibility.
# Any existing suffix is replaced, so passing --version "0.9.1-foo" will still
# regenerate the snapshot from the current HEAD commit.
# E.g., "0.9.1-dev" -> "0.9.1-20260214.5f1d7ca". Each package format then maps
# this to its own convention:
#   deb: 0.9.1~20260214.5f1d7ca-1  (~ pre-release, -1 debian revision)
#   rpm: 0.9.1~20260214.5f1d7ca    (~ pre-release, Release: 1 in spec)
#   apk: 0.9.1_git20260214         (_git suffix, hash in pkgdesc — apk is digits-only)
if [[ "${version}" == *-* ]]; then
    git_date=$(git -C "${repo_root}" log -1 --format=%cd --date=format:%Y%m%d)
    git_hash=$(git -C "${repo_root}" log -1 --format=%h)
    version="${version%%-*}-${git_date}.${git_hash}"
fi

# --- Print configuration ----------------------------------------------------

echo "==> CLP Universal Package Build"
echo "    Formats:  ${format_list[*]}"
echo "    Version:  ${version}"
echo "    Cores:    ${cores}"
echo "    Arches:   ${arch_list[*]}"
echo "    Output:   ${output_dir}"
echo ""

# Remove stale packages from the output directory (only for formats being built)
for _fmt in "${format_list[@]}"; do
    _fmt=$(echo "${_fmt}" | xargs)
    rm -f "${output_dir}"/clp-core*."${_fmt}"
done

# --- Helper: point build/ and .task/ at the right image family ---------------
#
# glibc and musl binaries are incompatible, so each image family gets its own
# build directory (build-manylinux_2_28/, build-musllinux_1_2/, etc.). The
# task runner expects build/ and .task/, so we symlink them to the active
# family's directories.

activate_build_family() {
    local target_family="$1"

    # Create family-specific directories if they don't exist
    mkdir -p "${repo_root}/build-${target_family}" "${repo_root}/.task-${target_family}"

    # Remove any real directory at the symlink target (ln -sfn cannot atomically
    # replace a real directory — it would create the symlink inside it instead).
    for dir_name in build .task; do
        local target="${repo_root}/${dir_name}"
        if [[ -d "${target}" && ! -L "${target}" ]]; then
            rm -rf "${target}"
        fi
    done

    # Point build/ and .task/ at the target family
    ln --symbolic --force --no-dereference "build-${target_family}" "${repo_root}/build"
    ln --symbolic --force --no-dereference ".task-${target_family}" "${repo_root}/.task"
}

# --- Build for each format and architecture ----------------------------------

for cur_format in "${format_list[@]}"; do
    cur_format=$(echo "${cur_format}" | xargs)

    # Resolve format-specific settings
    case "${cur_format}" in
        deb)
            format_dir="${script_dir}/universal-deb"
            base_image_family="manylinux_2_28"
            # deb and rpm share the same builder image (same Dockerfile installs
            # both dpkg and rpm-build) so Docker layer caching is shared.
            dockerfile_dir="${script_dir}/universal-deb"
            builder_image_prefix="clp-manylinux-pkg-builder"
            ;;
        rpm)
            format_dir="${script_dir}/universal-rpm"
            base_image_family="manylinux_2_28"
            # Shares the deb Dockerfile (installs both dpkg and rpm-build)
            dockerfile_dir="${script_dir}/universal-deb"
            builder_image_prefix="clp-manylinux-pkg-builder"
            ;;
        apk)
            format_dir="${script_dir}/alpine-apk"
            base_image_family="musllinux_1_2"
            dockerfile_dir="${script_dir}/alpine-apk"
            builder_image_prefix="clp-apk-builder"
            ;;
        *)
            echo "ERROR: Unsupported format: ${cur_format} (use deb, rpm, apk, or all)" >&2
            exit 1
            ;;
    esac

    if [[ ! -f "${format_dir}/package.sh" ]]; then
        echo "ERROR: Package script not found: ${format_dir}/package.sh" >&2
        exit 1
    fi

    activate_build_family "${base_image_family}"

    # Clean if requested (once per build family, before iterating architectures).
    # deb and rpm share manylinux_2_28 — skip if already cleaned this run.
    # Only clean the current family's directories (other families may have
    # root-owned files from Docker that would cause permission errors).
    if [[ "${clean}" == "true" ]] && [[ ! " ${cleaned_families:-} " =~ \ ${base_image_family}\  ]]; then
        echo "==> Cleaning build artifacts for ${base_image_family}..."
        # Docker runs as root, so build dirs may contain root-owned files.
        # Use a container to clean them to avoid requiring sudo on the host.
        if [[ -d "${repo_root}/build-${base_image_family}" ]] || [[ -d "${repo_root}/.task-${base_image_family}" ]]; then
            docker run --rm -v "${repo_root}:/clp" -w /clp alpine:3.20 \
                sh -c "rm -rf /clp/build-${base_image_family} /clp/.task-${base_image_family}" \
                2>/dev/null || rm -rf "${repo_root}/build-${base_image_family}" "${repo_root}/.task-${base_image_family}"
        fi
        # Remove symlinks if they point to the family being cleaned
        for dir_name in build .task; do
            local_target="${repo_root}/${dir_name}"
            if [[ -L "${local_target}" ]]; then
                rm -f "${local_target}"
            fi
        done
        activate_build_family "${base_image_family}"
        cleaned_families="${cleaned_families:-} ${base_image_family}"
    fi

    # For musl (apk) builds, pre-populate dependencies that can't be built
    # natively inside the 3.22 builder image.  This must run AFTER the clean
    # step so copied files aren't wiped.
    if [[ "${base_image_family}" == "musllinux_1_2" ]]; then
        # --- Model files (platform-independent ONNX models) ---
        # These are identical on any libc, but the Python generation step
        # (pip install onnxruntime) has no musl wheels.
        # NOTE: libortextensions.so is NOT copied — it must be built natively.
        manylinux_model_dir="${repo_root}/build-manylinux_2_28/deps/cpp/bge-small-en-v1.5"
        if [[ -f "${manylinux_model_dir}/encoder.onnx" ]]; then
            echo "==> Copying model files from manylinux build to musl build..."
            docker run --rm -v "${repo_root}:/clp" -w /clp alpine:3.20 sh -c '
                mkdir -p /clp/build-musllinux_1_2/deps/cpp/bge-small-en-v1.5
                for f in encoder.onnx tokenizer.onnx tokenizer.json vocab.txt; do
                    src="/clp/build-manylinux_2_28/deps/cpp/bge-small-en-v1.5/${f}"
                    if [ ! -f "${src}" ]; then
                        echo "ERROR: Missing model file: ${src}" >&2
                        exit 1
                    fi
                    cp "${src}" /clp/build-musllinux_1_2/deps/cpp/bge-small-en-v1.5/
                done
            '
        else
            echo "WARNING: No manylinux model files found at ${manylinux_model_dir}." >&2
            echo "  Build deb or rpm first to generate model files, then build apk." >&2
            echo "  Example: $0 --format deb,apk" >&2
            exit 1
        fi

        # --- ONNX Runtime (pre-built musl binaries from Alpine 3.23) ---
        # No official musl binaries exist on GitHub releases, but Alpine 3.23+
        # packages onnxruntime natively.  We install it inside a 3.23 container
        # and copy the libraries + headers into our 3.22 build tree.  The .so
        # is built against musl 1.2.5 (same ABI as our 3.22 builder).
        ort_dest="${repo_root}/build-musllinux_1_2/deps/cpp/onnxruntime-src"
        if [[ ! -f "${ort_dest}/lib/libonnxruntime.so" ]]; then
            echo "==> Extracting onnxruntime from Alpine 3.23 packages..."
            docker run --rm -v "${repo_root}:/clp" -w /clp alpine:3.23 sh -c '
                set -e
                apk add --no-cache onnxruntime=1.23.0-r0 onnxruntime-dev=1.23.0-r0

                dest="/clp/build-musllinux_1_2/deps/cpp/onnxruntime-src"
                mkdir -p "${dest}/lib" "${dest}/include"

                # Copy onnxruntime headers
                cp -a /usr/include/onnxruntime/* "${dest}/include/"

                # Copy all user-space shared libraries EXCEPT system libs that
                # must come from the target system (copying them would create
                # package conflicts with Alpine base packages).
                for lib in /usr/lib/*.so*; do
                    case "${lib##*/}" in
                        libc.musl*|ld-musl*) ;;  # musl libc
                        libcrypto*|libssl*)   ;;  # openssl
                        libz.so*)             ;;  # zlib
                        libstdc++*|libgcc_s*) ;;  # C++ runtime
                        *) cp -an "${lib}" "${dest}/lib/" 2>/dev/null || true ;;
                    esac
                done
            '
            echo "    Done — $(ls "${ort_dest}/lib/"*.so* 2>/dev/null | wc -l) shared libraries extracted."
        else
            echo "==> onnxruntime already extracted at ${ort_dest} — skipping."
        fi
    fi

    for target_arch in "${arch_list[@]}"; do
        target_arch=$(echo "${target_arch}" | xargs)

        case "${target_arch}" in
            x86_64)
                docker_suffix="x86_64"
                docker_platform="linux/amd64"
                # Existing image convention uses "x86" (not "x86_64") for x86_64
                image_arch_tag="x86"
                deb_arch="amd64"
                ;;
            aarch64)
                docker_suffix="aarch64"
                docker_platform="linux/arm64"
                image_arch_tag="aarch64"
                deb_arch="arm64"
                ;;
            *)
                echo "ERROR: Unsupported architecture: ${target_arch}" >&2
                exit 1
                ;;
        esac

        # Package arch naming varies by format:
        #   deb: amd64, arm64          (Debian convention)
        #   rpm/apk: x86_64, aarch64   (upstream convention)
        if [[ "${cur_format}" == "deb" ]]; then
            pkg_arch="${deb_arch}"
        else
            pkg_arch="${target_arch}"
        fi

        base_image_tag="clp-core-dependencies-${image_arch_tag}-${base_image_family}:dev"
        builder_image="${builder_image_prefix}-${docker_suffix}:dev"

        echo "========================================"
        echo "Building ${cur_format} for ${target_arch}"
        echo "========================================"

        # Build the base image if not present
        if ! docker image inspect "${base_image_tag}" &>/dev/null; then
            echo "==> Building base image ${base_image_tag}..."
            bash "${repo_root}/components/core/tools/docker-images/clp-env-base-${base_image_family}-${docker_suffix}/build.sh"
        fi

        # Build the builder image (base + packaging tools)
        if ! docker image inspect "${builder_image}" &>/dev/null; then
            echo "==> Building builder image ${builder_image}..."
            docker buildx build \
                --platform "${docker_platform}" \
                --build-arg "BASE_IMAGE=${base_image_tag}" \
                --tag "${builder_image}" \
                "${dockerfile_dir}" \
                --file "${dockerfile_dir}/Dockerfile" \
                --load
        fi

        # Remove stale packages from the build directory (root-owned from Docker)
        docker run --rm -v "${repo_root}:/clp" -w /clp alpine:3.20 \
            sh -c "rm -f /clp/build/clp-core*.${cur_format}" 2>/dev/null || true

        echo "==> Starting build and packaging..."
        # Run as root inside the container so that:
        #   - Package tools (dpkg-deb, rpmbuild, abuild) produce root-owned files
        #     in the package, which is correct for system binaries
        #   - abuild -F works without fakeroot (avoids musllinux compatibility issues)
        # Disable _FORTIFY_SOURCE for best performance. On Alpine/musllinux this
        # also avoids a GCC LTO incompatibility with fortify-headers.
        # Safe to overwrite CFLAGS/CXXFLAGS since the container has no prior flags.
        docker run --rm \
            --platform "${docker_platform}" \
            -v "${repo_root}:/clp" \
            -w /clp \
            -e "CORES=${cores}" \
            -e "PKG_VERSION=${version}" \
            -e "PKG_ARCH=${pkg_arch}" \
            -e "HOST_UID=$(id -u)" \
            -e "HOST_GID=$(id -g)" \
            -e "CFLAGS=-U_FORTIFY_SOURCE" \
            -e "CXXFLAGS=-U_FORTIFY_SOURCE" \
            "${builder_image}" \
            bash -c '
                set -o errexit
                set -o nounset
                set -o pipefail

                git config --global --add safe.directory "*"

                # Replace pipx cmake/ctest/cpack Python wrappers with symlinks
                # to the real binaries. The wrappers trigger a pathlib race
                # condition in Python 3.12 under parallel builds.
                cmake_data_bin="$(dirname "$(realpath "$(which cmake)")")/../lib/python*/site-packages/cmake/data/bin"
                cmake_data_bin="$(echo ${cmake_data_bin})"  # expand glob
                if [[ -d "${cmake_data_bin}" ]]; then
                    for tool in cmake ctest cpack; do
                        [[ -f "${cmake_data_bin}/${tool}" ]] && \
                            ln -sf "${cmake_data_bin}/${tool}" "/usr/local/bin/${tool}"
                    done
                fi

                echo "==> Building dependencies..."
                CLP_CPP_MAX_PARALLELISM_PER_BUILD_TASK="${CORES}" task deps:core

                echo "==> Building core binaries..."
                CLP_CPP_MAX_PARALLELISM_PER_BUILD_TASK="${CORES}" task core

                # BIN_DIR must match the CMake binary output directory (task core
                # builds into /clp/build/core).
                echo "==> Packaging..."
                BIN_DIR=/clp/build/core \
                MODEL_DIR=/clp/build/deps/cpp/bge-small-en-v1.5 \
                OUTPUT_DIR=/clp/build \
                    /clp/components/core/tools/packaging/'"${format_dir##*/}"'/package.sh

                # Restore host ownership on mounted volume paths
                chown -R "${HOST_UID}:${HOST_GID}" /clp/build /clp/.task
            '

        # Copy only the freshly-built package to the output directory.
        # The build directory may contain packages from earlier runs (different
        # commits), so we identify the newest one and copy only that.
        echo "==> Copying package to ${output_dir}..."
        newest_pkg=$(find -L "${repo_root}/build" -maxdepth 1 -name "clp-core*.${cur_format}" \
            -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
        if [[ -z "${newest_pkg}" ]]; then
            echo "ERROR: No .${cur_format} package found in ${repo_root}/build" >&2
            exit 1
        fi
        cp "${newest_pkg}" "${output_dir}/"
        echo ""
    done
done

echo "========================================"
echo "All builds complete!"
echo "========================================"
echo ""
for _fmt in "${format_list[@]}"; do
    _fmt=$(echo "${_fmt}" | xargs)
    ls -lh "${output_dir}"/clp-core*."${_fmt}" 2>/dev/null || true
done
echo ""

for cur_format in "${format_list[@]}"; do
    cur_format=$(echo "${cur_format}" | xargs)
    case "${cur_format}" in
        deb)
            echo "Test deb:"
            echo "  docker run --rm -v '${output_dir}':/pkgs debian:bookworm bash -c \\"
            echo "    'dpkg -i /pkgs/clp-core_*.deb && clp-s --help'"
            ;;
        rpm)
            echo "Test rpm:"
            echo "  docker run --rm -v '${output_dir}':/pkgs almalinux:9 bash -c \\"
            echo "    'rpm -i /pkgs/clp-core-*.rpm && clp-s --help'"
            ;;
        apk)
            echo "Test apk:"
            echo "  docker run --rm -v '${output_dir}':/pkgs alpine:3.20 sh -c \\"
            echo "    'apk add --allow-untrusted /pkgs/clp-core-*.apk && clp-s --help'"
            ;;
    esac
done
