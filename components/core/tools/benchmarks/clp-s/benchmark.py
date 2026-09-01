#!/usr/bin/env python3
"""Run paired CLP-S C++/Rust performance comparisons inside the CLP build image."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
import platform
import random
import re
import resource
import select
import shutil
import signal
import stat
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

HARNESS_NAME = "clp-s-benchmark"
HARNESS_VERSION = "1.0.0"
RESULT_SCHEMA_VERSION = 1
MANIFEST_VERSION = 1
METADATA_COMMAND_TIMEOUT_SECONDS = 30

IMPLEMENTATIONS = ("cpp", "rust")
OPERATIONS = {"compression", "extraction", "search"}
METRICS = {
    "wall_time_seconds",
    "cpu_time_seconds",
    "user_cpu_seconds",
    "system_cpu_seconds",
    "peak_rss_bytes",
    "output_size_bytes",
    "stdout_size_bytes",
    "throughput_per_second",
}
STATISTICS = {"median", "p05", "p95"}
COMPARISONS = {"min_ratio", "max_ratio"}
THROUGHPUT_SOURCES = {
    "none",
    "input_bytes",
    "logical_input_bytes",
    "output_bytes",
    "stdout_bytes",
    "stdout_lines",
    "output_lines",
}
DEFAULT_THROUGHPUT_SOURCE = {
    "compression": "logical_input_bytes",
    "extraction": "output_bytes",
    "search": "input_bytes",
}
NAME_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
ENVIRONMENT_NAME_PATTERN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
EXACT_TEMPLATE_PATTERN = re.compile(r"\{([A-Za-z_][A-Za-z0-9_]*)\}")
TEMPLATE_KEYS = {"input", "output", "workdir", "stdout", "stderr", "implementation"}
INHERITED_ENVIRONMENT_KEYS = (
    "GLIBC_TUNABLES",
    "LANG",
    "LC_ALL",
    "LD_LIBRARY_PATH",
    "MALLOC_CONF",
    "MALLOC_MMAP_THRESHOLD_",
    "MALLOC_TRIM_THRESHOLD_",
    "OMP_NUM_THREADS",
    "PATH",
    "RAYON_NUM_THREADS",
    "RUST_BACKTRACE",
    "RUSTFLAGS",
    "TZ",
)
SAFE_ENVIRONMENT_VALUE_KEYS = {
    "GLIBC_TUNABLES",
    "LANG",
    "LC_ALL",
    "LD_LIBRARY_PATH",
    "MALLOC_CONF",
    "MALLOC_MMAP_THRESHOLD_",
    "MALLOC_TRIM_THRESHOLD_",
    "OMP_NUM_THREADS",
    "PATH",
    "RAYON_NUM_THREADS",
    "RUST_BACKTRACE",
    "RUSTFLAGS",
    "TZ",
}


class ConfigurationError(ValueError):
    """Raised when a benchmark configuration is invalid."""


class BenchmarkError(RuntimeError):
    """Raised when benchmark setup cannot be completed."""


def _reject_duplicate_json_keys(pairs: Sequence[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ConfigurationError(f"JSON document contains duplicate key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite_json_number(value: str) -> Any:
    raise ConfigurationError(f"JSON document contains non-finite number {value!r}")


@dataclass(frozen=True)
class Artifact:
    """An input artifact and its verified provenance."""

    name: str
    kind: str
    path: Path
    sha256: str
    size_bytes: int
    logical_size_bytes: int


def _utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="microseconds")


def _require_mapping(value: Any, location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConfigurationError(f"{location} must be an object")
    return value


def _require_list(value: Any, location: str) -> list[Any]:
    if not isinstance(value, list):
        raise ConfigurationError(f"{location} must be an array")
    return value


def _check_keys(
    value: Mapping[str, Any], location: str, allowed: set[str], required: set[str] = frozenset()
) -> None:
    missing = required - value.keys()
    if missing:
        raise ConfigurationError(f"{location} is missing: {', '.join(sorted(missing))}")
    unknown = value.keys() - allowed
    if unknown:
        raise ConfigurationError(f"{location} has unknown keys: {', '.join(sorted(unknown))}")


def _validate_name(value: Any, location: str) -> str:
    if not isinstance(value, str) or NAME_PATTERN.fullmatch(value) is None:
        raise ConfigurationError(
            f"{location} must match {NAME_PATTERN.pattern!r}; got {value!r}"
        )
    return value


def _validate_nonnegative_integer(value: Any, location: str) -> int:
    if type(value) is not int or value < 0:
        raise ConfigurationError(f"{location} must be a non-negative integer")
    return value


def _validate_positive_integer(value: Any, location: str) -> int:
    result = _validate_nonnegative_integer(value, location)
    if result == 0:
        raise ConfigurationError(f"{location} must be greater than zero")
    return result


def _validate_positive_number(value: Any, location: str) -> float:
    if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
        raise ConfigurationError(f"{location} must be a positive number")
    return float(value)


def _validate_environment(value: Any, location: str) -> dict[str, str]:
    environment = _require_mapping(value, location)
    for key, item in environment.items():
        if ENVIRONMENT_NAME_PATTERN.fullmatch(key) is None:
            raise ConfigurationError(f"{location} has an invalid environment name: {key!r}")
        if not isinstance(item, str):
            raise ConfigurationError(f"{location}.{key} must be a string")
    return environment


def _validate_arguments(value: Any, location: str, *, preparation: bool) -> list[str]:
    arguments = _require_list(value, location)
    if not arguments:
        raise ConfigurationError(f"{location} must not be empty")
    for index, argument in enumerate(arguments):
        if not isinstance(argument, str):
            raise ConfigurationError(f"{location}[{index}] must be a string")
        for match in EXACT_TEMPLATE_PATTERN.finditer(argument):
            if match.group(1) not in TEMPLATE_KEYS:
                raise ConfigurationError(
                    f"{location}[{index}] contains unknown template {match.group(0)!r}"
                )
    joined = "\0".join(arguments)
    if "{input}" not in joined:
        raise ConfigurationError(f"{location} must contain an {{input}} template")
    if preparation and "{output}" not in joined:
        raise ConfigurationError(f"{location} must contain an {{output}} template")
    return arguments


def _validate_input_reference(
    value: Any,
    location: str,
    dataset_names: set[str],
    available_preparations: set[str],
) -> None:
    reference = _require_mapping(value, location)
    _check_keys(reference, location, {"dataset", "preparation"})
    if len(reference) != 1:
        raise ConfigurationError(
            f"{location} must contain exactly one of 'dataset' or 'preparation'"
        )
    if "dataset" in reference:
        name = _validate_name(reference["dataset"], f"{location}.dataset")
        if name not in dataset_names:
            raise ConfigurationError(f"{location} references unknown dataset {name!r}")
    else:
        name = _validate_name(reference["preparation"], f"{location}.preparation")
        if name not in available_preparations:
            raise ConfigurationError(f"{location} references unavailable preparation {name!r}")


def _validate_gate(value: Any, location: str) -> None:
    gate = _require_mapping(value, location)
    _check_keys(
        gate,
        location,
        {"metric", "statistic", "comparison", "threshold"},
        {"metric", "statistic", "comparison", "threshold"},
    )
    if gate["metric"] not in METRICS:
        raise ConfigurationError(f"{location}.metric must be one of {sorted(METRICS)}")
    if gate["statistic"] not in STATISTICS:
        raise ConfigurationError(f"{location}.statistic must be one of {sorted(STATISTICS)}")
    if gate["comparison"] not in COMPARISONS:
        raise ConfigurationError(
            f"{location}.comparison must be one of {sorted(COMPARISONS)}"
        )
    _validate_positive_number(gate["threshold"], f"{location}.threshold")


def _validate_run_options(value: Any, location: str) -> None:
    options = _require_mapping(value, location)
    _check_keys(options, location, {"warmups", "trials", "timeout_seconds", "gates"})
    if "warmups" in options:
        _validate_nonnegative_integer(options["warmups"], f"{location}.warmups")
    if "trials" in options:
        _validate_positive_integer(options["trials"], f"{location}.trials")
    if "timeout_seconds" in options:
        _validate_positive_number(options["timeout_seconds"], f"{location}.timeout_seconds")
    if "gates" in options:
        for index, gate in enumerate(_require_list(options["gates"], f"{location}.gates")):
            _validate_gate(gate, f"{location}.gates[{index}]")


def validate_manifest(manifest: Any) -> dict[str, Any]:
    """Validate a decoded manifest and return it with no mutation."""
    root = _require_mapping(manifest, "manifest")
    _check_keys(
        root,
        "manifest",
        {
            "$schema",
            "version",
            "name",
            "metadata",
            "environment",
            "defaults",
            "datasets",
            "preparations",
            "workloads",
        },
        {"version", "name", "datasets", "workloads"},
    )
    if type(root["version"]) is not int or root["version"] != MANIFEST_VERSION:
        raise ConfigurationError(
            f"manifest.version must be {MANIFEST_VERSION}; got {root['version']!r}"
        )
    _validate_name(root["name"], "manifest.name")
    if "$schema" in root and not isinstance(root["$schema"], str):
        raise ConfigurationError("manifest.$schema must be a string")
    if "metadata" in root:
        metadata = _require_mapping(root["metadata"], "manifest.metadata")
        scalar_types = (str, int, float, bool, type(None))
        for key, value in metadata.items():
            if not isinstance(key, str) or not isinstance(value, scalar_types):
                raise ConfigurationError("manifest.metadata must contain scalar JSON values")
            if isinstance(value, float) and not math.isfinite(value):
                raise ConfigurationError("manifest.metadata numbers must be finite")
    if "environment" in root:
        _validate_environment(root["environment"], "manifest.environment")
    if "defaults" in root:
        _validate_run_options(root["defaults"], "manifest.defaults")

    datasets = _require_mapping(root["datasets"], "manifest.datasets")
    if not datasets:
        raise ConfigurationError("manifest.datasets must not be empty")
    dataset_names: set[str] = set()
    for name, raw_dataset in datasets.items():
        _validate_name(name, f"manifest.datasets key {name!r}")
        dataset_names.add(name)
        dataset = _require_mapping(raw_dataset, f"manifest.datasets.{name}")
        _check_keys(
            dataset,
            f"manifest.datasets.{name}",
            {"path", "sha256", "logical_size_bytes", "description"},
            {"path", "sha256"},
        )
        if not isinstance(dataset["path"], str) or not dataset["path"]:
            raise ConfigurationError(f"manifest.datasets.{name}.path must be a non-empty string")
        if not isinstance(dataset["sha256"], str) or SHA256_PATTERN.fullmatch(
            dataset["sha256"]
        ) is None:
            raise ConfigurationError(
                f"manifest.datasets.{name}.sha256 must be a lowercase SHA-256 digest"
            )
        if "logical_size_bytes" in dataset:
            _validate_nonnegative_integer(
                dataset["logical_size_bytes"],
                f"manifest.datasets.{name}.logical_size_bytes",
            )
        if "description" in dataset and not isinstance(dataset["description"], str):
            raise ConfigurationError(f"manifest.datasets.{name}.description must be a string")

    available_preparations: set[str] = set()
    preparations = _require_list(root.get("preparations", []), "manifest.preparations")
    for index, raw_preparation in enumerate(preparations):
        location = f"manifest.preparations[{index}]"
        preparation = _require_mapping(raw_preparation, location)
        _check_keys(
            preparation,
            location,
            {"name", "implementation", "input", "args", "environment", "timeout_seconds"},
            {"name", "implementation", "input", "args"},
        )
        name = _validate_name(preparation["name"], f"{location}.name")
        if name in available_preparations:
            raise ConfigurationError(f"duplicate preparation name {name!r}")
        if preparation["implementation"] not in IMPLEMENTATIONS:
            raise ConfigurationError(
                f"{location}.implementation must be one of {IMPLEMENTATIONS}"
            )
        _validate_input_reference(
            preparation["input"],
            f"{location}.input",
            dataset_names,
            available_preparations,
        )
        _validate_arguments(preparation["args"], f"{location}.args", preparation=True)
        if "environment" in preparation:
            _validate_environment(preparation["environment"], f"{location}.environment")
        if "timeout_seconds" in preparation:
            _validate_positive_number(
                preparation["timeout_seconds"], f"{location}.timeout_seconds"
            )
        available_preparations.add(name)

    workloads = _require_list(root["workloads"], "manifest.workloads")
    if not workloads:
        raise ConfigurationError("manifest.workloads must not be empty")
    workload_names: set[str] = set()
    for index, raw_workload in enumerate(workloads):
        location = f"manifest.workloads[{index}]"
        workload = _require_mapping(raw_workload, location)
        _check_keys(
            workload,
            location,
            {
                "name",
                "operation",
                "input",
                "args",
                "environment",
                "warmups",
                "trials",
                "timeout_seconds",
                "throughput_source",
                "gates",
            },
            {"name", "operation", "input", "args"},
        )
        name = _validate_name(workload["name"], f"{location}.name")
        if name in workload_names:
            raise ConfigurationError(f"duplicate workload name {name!r}")
        workload_names.add(name)
        if workload["operation"] not in OPERATIONS:
            raise ConfigurationError(f"{location}.operation must be one of {sorted(OPERATIONS)}")
        _validate_input_reference(
            workload["input"],
            f"{location}.input",
            dataset_names,
            available_preparations,
        )
        _validate_arguments(workload["args"], f"{location}.args", preparation=False)
        if "environment" in workload:
            _validate_environment(workload["environment"], f"{location}.environment")
        _validate_run_options(
            {
                key: workload[key]
                for key in ("warmups", "trials", "timeout_seconds", "gates")
                if key in workload
            },
            location,
        )
        if workload.get("throughput_source", "none") not in THROUGHPUT_SOURCES:
            raise ConfigurationError(
                f"{location}.throughput_source must be one of {sorted(THROUGHPUT_SOURCES)}"
            )
    return root


def load_manifest(path: Path) -> tuple[dict[str, Any], str]:
    """Load, validate, and fingerprint a manifest."""
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise ConfigurationError(f"failed to read manifest {path}: {error}") from error
    try:
        decoded = json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_nonfinite_json_number,
        )
    except json.JSONDecodeError as error:
        raise ConfigurationError(f"failed to parse manifest {path}: {error}") from error
    return validate_manifest(decoded), hashlib.sha256(raw).hexdigest()


def _validate_string(value: Any, location: str) -> str:
    if not isinstance(value, str) or not value:
        raise ConfigurationError(f"{location} must be a non-empty string")
    return value


def _validate_string_list(value: Any, location: str) -> list[str]:
    result = _require_list(value, location)
    for index, item in enumerate(result):
        if not isinstance(item, str):
            raise ConfigurationError(f"{location}[{index}] must be a string")
    return result


def validate_build_metadata(value: Any) -> dict[str, Any]:
    """Validate explicit, non-inferable build settings for both benchmark binaries."""
    metadata = _require_mapping(value, "build metadata")
    _check_keys(
        metadata,
        "build metadata",
        {"$schema", "schema_version", "source_commit", "cpp", "rust", "notes"},
        {"schema_version", "source_commit", "cpp", "rust"},
    )
    if "$schema" in metadata and not isinstance(metadata["$schema"], str):
        raise ConfigurationError("build metadata.$schema must be a string")
    if type(metadata["schema_version"]) is not int or metadata["schema_version"] != 1:
        raise ConfigurationError("build metadata.schema_version must be 1")
    source_commit = _validate_string(metadata["source_commit"], "build metadata.source_commit")
    if re.fullmatch(r"[0-9a-f]{40,64}", source_commit) is None:
        raise ConfigurationError("build metadata.source_commit must be a full Git object ID")
    if "notes" in metadata and not isinstance(metadata["notes"], str):
        raise ConfigurationError("build metadata.notes must be a string")

    cpp = _require_mapping(metadata["cpp"], "build metadata.cpp")
    cpp_string_fields = {
        "binary_sha256",
        "build_type",
        "compiler",
        "compiler_version",
        "target_cpu",
        "allocator",
    }
    cpp_list_fields = {"cmake_arguments", "compiler_flags", "linker_flags"}
    _check_keys(
        cpp,
        "build metadata.cpp",
        cpp_string_fields | cpp_list_fields | {"lto", "notes"},
        cpp_string_fields | cpp_list_fields | {"lto"},
    )
    for field in cpp_string_fields:
        _validate_string(cpp[field], f"build metadata.cpp.{field}")
    if SHA256_PATTERN.fullmatch(cpp["binary_sha256"]) is None:
        raise ConfigurationError("build metadata.cpp.binary_sha256 must be a SHA-256 digest")
    for field in cpp_list_fields:
        _validate_string_list(cpp[field], f"build metadata.cpp.{field}")
    if type(cpp["lto"]) not in (bool, str):
        raise ConfigurationError("build metadata.cpp.lto must be a boolean or string")
    if isinstance(cpp["lto"], str) and not cpp["lto"]:
        raise ConfigurationError("build metadata.cpp.lto must not be an empty string")
    if "notes" in cpp and not isinstance(cpp["notes"], str):
        raise ConfigurationError("build metadata.cpp.notes must be a string")

    rust = _require_mapping(metadata["rust"], "build metadata.rust")
    rust_string_fields = {
        "binary_sha256",
        "profile",
        "rustc_version",
        "target_triple",
        "target_cpu",
        "allocator",
    }
    rust_list_fields = {"cargo_arguments", "rustflags"}
    _check_keys(
        rust,
        "build metadata.rust",
        rust_string_fields | rust_list_fields | {"profile_settings", "notes"},
        rust_string_fields | rust_list_fields | {"profile_settings"},
    )
    for field in rust_string_fields:
        _validate_string(rust[field], f"build metadata.rust.{field}")
    if SHA256_PATTERN.fullmatch(rust["binary_sha256"]) is None:
        raise ConfigurationError("build metadata.rust.binary_sha256 must be a SHA-256 digest")
    for field in rust_list_fields:
        _validate_string_list(rust[field], f"build metadata.rust.{field}")
    profile_settings = _require_mapping(
        rust["profile_settings"], "build metadata.rust.profile_settings"
    )
    for key, item in profile_settings.items():
        if not isinstance(key, str) or not isinstance(item, (str, int, float, bool, type(None))):
            raise ConfigurationError(
                "build metadata.rust.profile_settings must contain scalar JSON values"
            )
        if isinstance(item, float) and not math.isfinite(item):
            raise ConfigurationError(
                "build metadata.rust.profile_settings numbers must be finite"
            )
    if "notes" in rust and not isinstance(rust["notes"], str):
        raise ConfigurationError("build metadata.rust.notes must be a string")
    return metadata


def load_build_metadata(path: Path) -> tuple[dict[str, Any], str]:
    """Load and fingerprint required C++/Rust build provenance."""
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise ConfigurationError(f"failed to read build metadata {path}: {error}") from error
    try:
        decoded = json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_nonfinite_json_number,
        )
    except json.JSONDecodeError as error:
        raise ConfigurationError(f"failed to parse build metadata {path}: {error}") from error
    return validate_build_metadata(decoded), hashlib.sha256(raw).hexdigest()


def _redact_environment(environment: Mapping[str, str]) -> dict[str, str]:
    """Record only explicitly safe performance-control values; redact every other value."""
    return {
        key: value if key in SAFE_ENVIRONMENT_VALUE_KEYS else "<redacted>"
        for key, value in environment.items()
    }


def _redacted_manifest(manifest: Mapping[str, Any]) -> dict[str, Any]:
    """Copy a manifest while ensuring arbitrary environment values cannot enter results."""
    redacted = json.loads(json.dumps(manifest))
    if "environment" in redacted:
        redacted["environment"] = _redact_environment(redacted["environment"])
    for section in ("preparations", "workloads"):
        for item in redacted.get(section, []):
            if "environment" in item:
                item["environment"] = _redact_environment(item["environment"])
    return redacted


def _hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _regular_tree_files(path: Path) -> list[Path]:
    if path.is_symlink():
        raise ConfigurationError(f"symbolic links are not supported in benchmark data: {path}")
    if path.is_file():
        return [path]
    if not path.is_dir():
        raise ConfigurationError(f"benchmark data path is not a regular file or directory: {path}")
    files: list[Path] = []
    for item in path.rglob("*"):
        if item.is_symlink():
            raise ConfigurationError(f"symbolic links are not supported in benchmark data: {item}")
        if item.is_file():
            files.append(item)
        elif not item.is_dir():
            raise ConfigurationError(f"non-regular benchmark data entry: {item}")
    return sorted(files, key=lambda item: item.relative_to(path).as_posix())


def tree_size_bytes(path: Path) -> int:
    """Return the sum of regular file sizes beneath path."""
    return sum(item.stat().st_size for item in _regular_tree_files(path))


def tree_sha256(path: Path) -> str:
    """Hash a file normally, or a directory using the documented CLP-S tree digest."""
    if path.is_file() and not path.is_symlink():
        return _hash_file(path)
    digest = hashlib.sha256(b"clp-s-benchmark-directory-v1\0")
    for item in _regular_tree_files(path):
        relative = item.relative_to(path).as_posix().encode("utf-8")
        item_size = item.stat().st_size
        item_digest = bytes.fromhex(_hash_file(item))
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(item_size.to_bytes(8, "big"))
        digest.update(item_digest)
    return digest.hexdigest()


def resolve_datasets(manifest: Mapping[str, Any], manifest_path: Path) -> dict[str, Artifact]:
    """Resolve and checksum every source dataset in a manifest."""
    artifacts: dict[str, Artifact] = {}
    for name, config in manifest["datasets"].items():
        configured_path = Path(config["path"])
        if not configured_path.is_absolute():
            configured_path = manifest_path.parent / configured_path
        if configured_path.is_symlink():
            raise ConfigurationError(f"dataset {name!r} may not be a symbolic link")
        try:
            path = configured_path.resolve(strict=True)
        except OSError as error:
            raise ConfigurationError(f"failed to resolve dataset {name!r}: {error}") from error
        actual_digest = tree_sha256(path)
        if actual_digest != config["sha256"]:
            raise ConfigurationError(
                f"dataset {name!r} checksum mismatch: expected {config['sha256']}, "
                f"got {actual_digest}"
            )
        size = tree_size_bytes(path)
        artifacts[name] = Artifact(
            name=name,
            kind="dataset",
            path=path,
            sha256=actual_digest,
            size_bytes=size,
            logical_size_bytes=config.get("logical_size_bytes", size),
        )
    return artifacts


def _render_templates(values: Sequence[str], context: Mapping[str, str]) -> list[str]:
    rendered: list[str] = []
    for value in values:
        result = value
        for key in TEMPLATE_KEYS:
            result = result.replace("{" + key + "}", context[key])
        rendered.append(result)
    return rendered


def _count_lines_in_file(path: Path) -> int:
    count = 0
    final_byte = b""
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            count += chunk.count(b"\n")
            final_byte = chunk[-1:]
    if final_byte and final_byte != b"\n":
        count += 1
    return count


def tree_line_count(path: Path) -> int:
    """Count newline-delimited records in a file tree."""
    return sum(_count_lines_in_file(item) for item in _regular_tree_files(path))


def _wait4_with_timeout(
    process: subprocess.Popen[bytes], timeout_seconds: float
) -> tuple[int, Any, bool]:
    """Wait for a Linux child and return wait status, rusage, and whether it timed out."""
    timed_out = False
    pidfd: int | None = None
    try:
        if hasattr(os, "pidfd_open"):
            pidfd = os.pidfd_open(process.pid)
            poller = select.poll()
            poller.register(pidfd, select.POLLIN)
            events = poller.poll(max(1, int(timeout_seconds * 1000)))
            if not events:
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            _, status, usage = os.wait4(process.pid, 0)
            return status, usage, timed_out

        deadline = time.monotonic() + timeout_seconds
        while True:
            waited_pid, status, usage = os.wait4(process.pid, os.WNOHANG)
            if waited_pid == process.pid:
                return status, usage, timed_out
            if time.monotonic() >= deadline:
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                _, status, usage = os.wait4(process.pid, 0)
                return status, usage, timed_out
            time.sleep(0.01)
    finally:
        if pidfd is not None:
            os.close(pidfd)


def run_command(
    command: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    stdout_path: Path,
    stderr_path: Path,
    timeout_seconds: float,
) -> dict[str, Any]:
    """Run one direct binary invocation and collect wall-clock and wait4 resource usage."""
    started_at = _utc_now()
    start_ns = time.perf_counter_ns()
    try:
        with stdout_path.open("wb") as stdout_file, stderr_path.open("wb") as stderr_file:
            process = subprocess.Popen(
                list(command),
                cwd=cwd,
                env=dict(environment),
                stdin=subprocess.DEVNULL,
                stdout=stdout_file,
                stderr=stderr_file,
                start_new_session=True,
            )
            try:
                status, usage, timed_out = _wait4_with_timeout(process, timeout_seconds)
            except BaseException:
                # Never leave CLP-S or any descendants running after cancellation or a wait error.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    os.wait4(process.pid, 0)
                except ChildProcessError:
                    pass
                raise
            process.returncode = os.waitstatus_to_exitcode(status)
    except OSError as error:
        raise BenchmarkError(f"failed to execute {command[0]}: {error}") from error
    elapsed_seconds = (time.perf_counter_ns() - start_ns) / 1_000_000_000
    return {
        "started_at": started_at,
        "finished_at": _utc_now(),
        "exit_code": process.returncode,
        "timed_out": timed_out,
        "wall_time_seconds": elapsed_seconds,
        "user_cpu_seconds": usage.ru_utime,
        "system_cpu_seconds": usage.ru_stime,
        "cpu_time_seconds": usage.ru_utime + usage.ru_stime,
        # Linux reports ru_maxrss in KiB. Normal execution is container-guarded to Linux.
        "peak_rss_bytes": usage.ru_maxrss * 1024,
    }


def _quantile(values: Sequence[float], probability: float) -> float:
    if not values:
        raise ValueError("cannot calculate a quantile of an empty sequence")
    ordered = sorted(values)
    position = (len(ordered) - 1) * probability
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = position - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def summarize_values(values: Sequence[float]) -> dict[str, float]:
    """Return stable descriptive statistics for a non-empty sample."""
    if not values:
        raise ValueError("cannot summarize an empty sequence")
    return {
        "count": len(values),
        "min": min(values),
        "p05": _quantile(values, 0.05),
        "median": statistics.median(values),
        "mean": statistics.fmean(values),
        "p95": _quantile(values, 0.95),
        "max": max(values),
    }


def _ratio(rust_value: float, cpp_value: float) -> float | None:
    if cpp_value == 0:
        return 1.0 if rust_value == 0 else None
    return rust_value / cpp_value


def randomized_pair_orders(
    seed: int, workload_name: str, phase: str, count: int
) -> list[list[str]]:
    """Return deterministic but independently randomized implementation order for each pair."""
    material = f"{seed}\0{workload_name}\0{phase}".encode()
    derived_seed = int.from_bytes(hashlib.sha256(material).digest()[:8], "big")
    generator = random.Random(derived_seed)
    orders: list[list[str]] = []
    for _ in range(count):
        order = list(IMPLEMENTATIONS)
        generator.shuffle(order)
        orders.append(order)
    return orders


def evaluate_gates(
    gates: Sequence[Mapping[str, Any]], pair_ratios: Mapping[str, Sequence[float]]
) -> list[dict[str, Any]]:
    """Evaluate configured gates over paired Rust/C++ ratios."""
    results: list[dict[str, Any]] = []
    for gate in gates:
        metric = gate["metric"]
        ratios = list(pair_ratios.get(metric, []))
        result: dict[str, Any] = dict(gate)
        result["ratio_definition"] = "rust / cpp"
        if not ratios:
            result.update(
                {
                    "passed": False,
                    "observed_ratio": None,
                    "reason": "no complete successful pair produced this metric",
                }
            )
            results.append(result)
            continue
        ratio_summary = summarize_values(ratios)
        observed = ratio_summary[gate["statistic"]]
        if gate["comparison"] == "min_ratio":
            passed = observed >= gate["threshold"]
        else:
            passed = observed <= gate["threshold"]
        result.update(
            {
                "passed": passed,
                "observed_ratio": observed,
                "paired_ratios": ratio_summary,
            }
        )
        results.append(result)
    return results


def _command_output(command: Sequence[str], cwd: Path, timeout: float = 5.0) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            list(command),
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=timeout,
            text=True,
            errors="replace",
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"available": False, "error": str(error)}
    return {
        "available": True,
        "exit_code": completed.returncode,
        "output": completed.stdout[:16_384].rstrip(),
        "output_truncated": len(completed.stdout) > 16_384,
    }


def _hash_command_output(command: Sequence[str], cwd: Path) -> dict[str, Any]:
    """Hash complete command output without retaining potentially large or sensitive contents."""
    digest = hashlib.sha256()
    size = 0
    with tempfile.TemporaryFile() as output:
        try:
            process = subprocess.Popen(
                list(command),
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=output,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        except OSError as error:
            return {"available": False, "error": str(error)}
        timed_out = False
        try:
            exit_code = process.wait(timeout=METADATA_COMMAND_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            exit_code = process.wait()
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            raise
        output.seek(0)
        while chunk := output.read(1024 * 1024):
            digest.update(chunk)
            size += len(chunk)
    return {
        "available": True,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "sha256": digest.hexdigest(),
        "size_bytes": size,
    }


def _repository_dirty_fingerprint(repo_root: Path) -> dict[str, Any]:
    """Fingerprint all tracked changes and the names/content of every untracked file."""
    tracked = _hash_command_output(
        ("git", "diff", "--binary", "--no-ext-diff", "--submodule=short", "HEAD", "--"),
        repo_root,
    )
    status = _hash_command_output(
        ("git", "status", "--porcelain=v2", "-z", "--untracked-files=all"), repo_root
    )
    try:
        untracked_command = subprocess.run(
            ("git", "ls-files", "--others", "--exclude-standard", "-z"),
            cwd=repo_root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=METADATA_COMMAND_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {
            "available": False,
            "tracked_diff": tracked,
            "status": status,
            "error": str(error),
        }
    if untracked_command.returncode != 0:
        return {
            "available": False,
            "tracked_diff": tracked,
            "status": status,
            "error": untracked_command.stderr.decode(errors="replace")[:16_384],
        }

    overall = hashlib.sha256(b"clp-s-benchmark-git-dirty-v1\0")
    tracked_digest = tracked.get("sha256")
    if isinstance(tracked_digest, str):
        overall.update(bytes.fromhex(tracked_digest))
    status_digest = status.get("sha256")
    if isinstance(status_digest, str):
        overall.update(bytes.fromhex(status_digest))
    untracked: list[dict[str, Any]] = []
    total_untracked_bytes = 0
    paths = [item for item in untracked_command.stdout.split(b"\0") if item]
    for raw_relative in sorted(paths):
        relative = os.fsdecode(raw_relative)
        path = repo_root / relative
        file_stat = path.lstat()
        if stat.S_ISREG(file_stat.st_mode):
            content_digest = _hash_file(path)
            size = file_stat.st_size
            kind = "file"
        elif stat.S_ISLNK(file_stat.st_mode):
            target = os.fsencode(os.readlink(path))
            content_digest = hashlib.sha256(target).hexdigest()
            size = len(target)
            kind = "symlink"
        else:
            content_digest = hashlib.sha256(b"unsupported-untracked-entry").hexdigest()
            size = 0
            kind = "other"
        total_untracked_bytes += size
        overall.update(len(raw_relative).to_bytes(8, "big"))
        overall.update(raw_relative)
        overall.update(file_stat.st_mode.to_bytes(8, "big"))
        overall.update(size.to_bytes(8, "big"))
        overall.update(bytes.fromhex(content_digest))
        untracked.append(
            {
                "path": relative,
                "kind": kind,
                "mode": oct(file_stat.st_mode & 0o7777),
                "size_bytes": size,
                "sha256": content_digest,
            }
        )
    return {
        "available": True,
        "sha256": overall.hexdigest(),
        "tracked_diff": tracked,
        "status": status,
        "untracked_count": len(untracked),
        "untracked_size_bytes": total_untracked_bytes,
        "untracked": untracked,
    }


def _header_version_macros(
    header: str, macro_prefixes: Sequence[str], repo_root: Path
) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            ("c++", "-dM", "-E", "-x", "c++", "-include", header, "/dev/null"),
            cwd=repo_root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=10,
            text=True,
            errors="replace",
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"available": False, "error": str(error)}
    version_lines: list[str] = []
    for line in completed.stdout.splitlines():
        fields = line.split(maxsplit=2)
        if len(fields) < 2 or fields[0] != "#define":
            continue
        macro = fields[1].upper()
        if "VERSION" in macro and any(macro.startswith(prefix) for prefix in macro_prefixes):
            version_lines.append(line)
    return {
        "available": True,
        "exit_code": completed.returncode,
        "version_macros": version_lines,
    }


def _native_dependency_metadata(repo_root: Path) -> dict[str, Any]:
    """Best-effort format-dependency versions from package metadata and public headers."""
    probes = {
        "zstd": {
            "pkg_config": ("libzstd", "zstd"),
            "cmake": "zstd",
            "header": "zstd.h",
            "macro_prefixes": ("ZSTD",),
            "rpm": ("libzstd-devel", "zstd-devel", "zstd"),
        },
        "libarchive": {
            "pkg_config": ("libarchive",),
            "cmake": "LibArchive",
            "header": "archive.h",
            "macro_prefixes": ("ARCHIVE",),
            "rpm": ("libarchive-devel", "libarchive"),
        },
        "simdjson": {
            "pkg_config": ("simdjson",),
            "cmake": "simdjson",
            "header": "simdjson.h",
            "macro_prefixes": ("SIMDJSON",),
            "rpm": ("simdjson-devel", "simdjson"),
        },
        "msgpack": {
            "pkg_config": ("msgpack-cxx", "msgpack"),
            "cmake": "msgpack-cxx",
            "header": "msgpack/version_master.h",
            "macro_prefixes": ("MSGPACK",),
            "rpm": ("msgpack-devel", "msgpack"),
        },
    }
    result: dict[str, Any] = {}
    for name, probe in probes.items():
        pkg_config: dict[str, Any] = {}
        for package in probe["pkg_config"]:
            pkg_config[package] = {
                "version": _command_output(("pkg-config", "--modversion", package), repo_root),
                "pcfiledir": _command_output(
                    ("pkg-config", "--variable=pcfiledir", package), repo_root
                ),
            }
        result[name] = {
            "pkg_config": pkg_config,
            "cmake_find_package": _command_output(
                (
                    "cmake",
                    "--find-package",
                    f"-DNAME={probe['cmake']}",
                    "-DCOMPILER_ID=GNU",
                    "-DLANGUAGE=CXX",
                    "-DMODE=EXIST",
                ),
                repo_root,
            ),
            "header_macros": _header_version_macros(
                probe["header"], probe["macro_prefixes"], repo_root
            ),
            "rpm_packages": _command_output(
                ("rpm", "-q", "--qf", "%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\\n", *probe["rpm"]),
                repo_root,
            ),
        }
    return result


def validate_build_context(
    metadata: Mapping[str, Any], binaries: Mapping[str, Path], repo_root: Path
) -> None:
    """Bind declared build settings to the exact binaries and checked-out source commit."""
    for implementation, binary in binaries.items():
        actual = _hash_file(binary)
        expected = metadata[implementation]["binary_sha256"]
        if actual != expected:
            raise ConfigurationError(
                f"build metadata {implementation}.binary_sha256 mismatch: expected {expected}, "
                f"got {actual}"
            )
    commit = _command_output(("git", "rev-parse", "HEAD"), repo_root)
    actual_commit = commit.get("output", "").strip() if commit.get("exit_code") == 0 else ""
    if actual_commit != metadata["source_commit"]:
        raise ConfigurationError(
            "build metadata source_commit does not match the benchmark checkout: "
            f"expected {metadata['source_commit']}, got {actual_commit or '<unavailable>'}"
        )


def _parse_os_release() -> dict[str, str]:
    result: dict[str, str] = {}
    try:
        lines = Path("/etc/os-release").read_text(encoding="utf-8").splitlines()
    except OSError:
        return result
    for line in lines:
        if "=" not in line or line.startswith("#"):
            continue
        key, value = line.split("=", 1)
        result[key] = value.strip().strip('"')
    return result


def _read_optional(path: str) -> str | None:
    try:
        return Path(path).read_text(encoding="utf-8").strip()
    except OSError:
        return None


def _binary_metadata(path: Path, repo_root: Path) -> dict[str, Any]:
    stat = path.stat()
    return {
        "path": str(path),
        "sha256": _hash_file(path),
        "size_bytes": stat.st_size,
        "mode": oct(stat.st_mode & 0o7777),
        "file": _command_output(("file", str(path)), repo_root),
        "elf_notes": _command_output(("readelf", "--notes", str(path)), repo_root),
        "dynamic_dependencies": _command_output(("ldd", str(path)), repo_root),
    }


def collect_environment_metadata(
    manifest_path: Path,
    manifest_sha256: str,
    build_metadata_path: Path,
    build_metadata_sha256: str,
    build_metadata: Mapping[str, Any],
    repo_root: Path,
    binaries: Mapping[str, Path],
    seed: int,
) -> dict[str, Any]:
    """Collect enough execution and toolchain metadata to identify a benchmark run."""
    try:
        affinity = sorted(os.sched_getaffinity(0))
    except (AttributeError, OSError):
        affinity = []
    cpu_model = None
    try:
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.lower().startswith(("model name", "hardware")) and ":" in line:
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    tool_commands = {
        "python": (sys.executable, "--version"),
        "cmake": ("cmake", "--version"),
        "c++": ("c++", "--version"),
        "rustc": ("rustc", "--version", "--verbose"),
        "cargo": ("cargo", "--version", "--verbose"),
        "ldd": ("ldd", "--version"),
    }
    return {
        "harness": {"name": HARNESS_NAME, "version": HARNESS_VERSION},
        "manifest": {"path": str(manifest_path), "sha256": manifest_sha256},
        "build_metadata": {
            "path": str(build_metadata_path),
            "sha256": build_metadata_sha256,
            "content": build_metadata,
        },
        "seed": seed,
        "container": {
            "image_reference": os.environ.get("CLP_S_BENCH_IMAGE_REFERENCE"),
            "image_id": os.environ.get("CLP_S_BENCH_IMAGE_ID"),
        },
        "repository": {
            "root": str(repo_root),
            "commit": _command_output(("git", "rev-parse", "HEAD"), repo_root),
            "status": _command_output(("git", "status", "--porcelain=v1"), repo_root),
            "submodules_recursive": _command_output(
                ("git", "submodule", "status", "--recursive"), repo_root
            ),
            "dirty_fingerprint": _repository_dirty_fingerprint(repo_root),
        },
        "platform": {
            "uname": platform.uname()._asdict(),
            "os_release": _parse_os_release(),
            "cpu_model": cpu_model,
            "cpu_affinity": affinity,
            "cpu_count": os.cpu_count(),
            "cgroup_cpu_max": _read_optional("/sys/fs/cgroup/cpu.max"),
            "cgroup_memory_max": _read_optional("/sys/fs/cgroup/memory.max"),
            "meminfo": _read_optional("/proc/meminfo"),
        },
        "environment": {
            key: os.environ[key]
            for key in INHERITED_ENVIRONMENT_KEYS
            if key in os.environ and key in SAFE_ENVIRONMENT_VALUE_KEYS
        },
        "tools": {
            name: _command_output(command, repo_root) for name, command in tool_commands.items()
        },
        "binaries": {
            implementation: _binary_metadata(path, repo_root)
            for implementation, path in binaries.items()
        },
        "native_format_dependencies": _native_dependency_metadata(repo_root),
    }


class JsonlWriter:
    """Append and flush one raw result event at a time."""

    def __init__(self, path: Path):
        self._stream = path.open("x", encoding="utf-8")

    def emit(self, event: Mapping[str, Any]) -> None:
        json.dump(event, self._stream, sort_keys=True, allow_nan=False)
        self._stream.write("\n")
        self._stream.flush()

    def close(self) -> None:
        self._stream.close()


class BenchmarkRunner:
    """Execute preparations, paired workloads, and gate evaluation."""

    def __init__(
        self,
        manifest: Mapping[str, Any],
        manifest_path: Path,
        manifest_sha256: str,
        build_metadata_path: Path,
        build_metadata_sha256: str,
        build_metadata: Mapping[str, Any],
        binaries: Mapping[str, Path],
        repo_root: Path,
        run_root: Path,
        seed: int,
        selected_workloads: set[str] | None,
        keep_artifacts: str,
        ignore_gates: bool,
    ):
        self.manifest = manifest
        self.manifest_path = manifest_path
        self.manifest_sha256 = manifest_sha256
        self.build_metadata_path = build_metadata_path
        self.build_metadata_sha256 = build_metadata_sha256
        self.build_metadata = build_metadata
        self.binaries = binaries
        self.repo_root = repo_root
        self.run_root = run_root
        self.seed = seed
        self.selected_workloads = selected_workloads
        self.keep_artifacts = keep_artifacts
        self.ignore_gates = ignore_gates
        self.datasets: dict[str, Artifact] = {}
        self.prepared: dict[str, Artifact] = {}
        self.preparation_results: list[dict[str, Any]] = []
        self.measurements: list[dict[str, Any]] = []
        self.summaries: list[dict[str, Any]] = []
        self.fatal_error: str | None = None
        self.raw_writer = JsonlWriter(run_root / "raw-results.jsonl")

    def _base_environment(self, overrides: Mapping[str, str] | None = None) -> dict[str, str]:
        environment = dict(os.environ)
        environment.update(self.manifest.get("environment", {}))
        if overrides:
            environment.update(overrides)
        return environment

    def _resolve_input(self, reference: Mapping[str, str]) -> Artifact:
        if "dataset" in reference:
            return self.datasets[reference["dataset"]]
        return self.prepared[reference["preparation"]]

    def _execute(
        self,
        implementation: str,
        arguments: Sequence[str],
        input_artifact: Artifact,
        workdir: Path,
        timeout_seconds: float,
        environment_overrides: Mapping[str, str] | None,
        identity: Mapping[str, Any],
        throughput_source: str = "none",
    ) -> dict[str, Any]:
        workdir.mkdir(parents=True)
        output_path = workdir / "output"
        output_path.mkdir()
        stdout_path = workdir / "stdout.bin"
        stderr_path = workdir / "stderr.bin"
        context = {
            "input": str(input_artifact.path),
            "output": str(output_path),
            "workdir": str(workdir),
            "stdout": str(stdout_path),
            "stderr": str(stderr_path),
            "implementation": implementation,
        }
        rendered_arguments = _render_templates(arguments, context)
        command = [str(self.binaries[implementation]), *rendered_arguments]
        result = run_command(
            command,
            workdir,
            self._base_environment(environment_overrides),
            stdout_path,
            stderr_path,
            timeout_seconds,
        )
        output_size = tree_size_bytes(output_path)
        stdout_size = stdout_path.stat().st_size
        stderr_size = stderr_path.stat().st_size
        source_amount: int | None
        throughput_unit: str | None
        if throughput_source == "input_bytes":
            source_amount, throughput_unit = input_artifact.size_bytes, "bytes/second"
        elif throughput_source == "logical_input_bytes":
            source_amount, throughput_unit = input_artifact.logical_size_bytes, "bytes/second"
        elif throughput_source == "output_bytes":
            source_amount, throughput_unit = output_size, "bytes/second"
        elif throughput_source == "stdout_bytes":
            source_amount, throughput_unit = stdout_size, "bytes/second"
        elif throughput_source == "stdout_lines":
            source_amount, throughput_unit = _count_lines_in_file(stdout_path), "records/second"
        elif throughput_source == "output_lines":
            source_amount, throughput_unit = tree_line_count(output_path), "records/second"
        else:
            source_amount, throughput_unit = None, None
        throughput = (
            source_amount / result["wall_time_seconds"]
            if source_amount is not None and result["wall_time_seconds"] > 0
            else None
        )
        result.update(identity)
        result.update(
            {
                "implementation": implementation,
                "command": command,
                "cwd": str(workdir),
                "timeout_seconds": timeout_seconds,
                "environment_overrides": {
                    **_redact_environment(self.manifest.get("environment", {})),
                    **_redact_environment(environment_overrides or {}),
                },
                "input": {
                    "name": input_artifact.name,
                    "kind": input_artifact.kind,
                    "path": str(input_artifact.path),
                    "sha256": input_artifact.sha256,
                },
                "input_size_bytes": input_artifact.size_bytes,
                "logical_input_size_bytes": input_artifact.logical_size_bytes,
                "output_size_bytes": output_size,
                "stdout_size_bytes": stdout_size,
                "stderr_size_bytes": stderr_size,
                "throughput_source": throughput_source,
                "throughput_amount": source_amount,
                "throughput_unit": throughput_unit,
                "throughput_per_second": throughput,
                "artifacts": {
                    "workdir": str(workdir.relative_to(self.run_root)),
                    "output": str(output_path.relative_to(self.run_root)),
                    "stdout": str(stdout_path.relative_to(self.run_root)),
                    "stderr": str(stderr_path.relative_to(self.run_root)),
                    "retained": True,
                },
            }
        )
        return result

    def _maybe_remove_artifacts(self, measurement: dict[str, Any]) -> None:
        success = measurement["exit_code"] == 0 and not measurement["timed_out"]
        retain = self.keep_artifacts == "all" or (
            self.keep_artifacts == "failures" and not success
        )
        if retain:
            return
        workdir = self.run_root / measurement["artifacts"]["workdir"]
        shutil.rmtree(workdir)
        measurement["artifacts"]["retained"] = False

    def _run_preparations(self) -> None:
        defaults = self.manifest.get("defaults", {})
        for preparation in self.manifest.get("preparations", []):
            input_artifact = self._resolve_input(preparation["input"])
            name = preparation["name"]
            workdir = self.run_root / "preparations" / name
            measurement = self._execute(
                preparation["implementation"],
                preparation["args"],
                input_artifact,
                workdir,
                float(preparation.get("timeout_seconds", defaults.get("timeout_seconds", 3600))),
                preparation.get("environment"),
                {"event": "preparation", "preparation": name},
            )
            if measurement["exit_code"] != 0 or measurement["timed_out"]:
                self.preparation_results.append(measurement)
                self.raw_writer.emit(measurement)
                raise BenchmarkError(
                    f"preparation {name!r} failed with exit code {measurement['exit_code']}"
                )
            output_path = workdir / "output"
            output_digest = tree_sha256(output_path)
            output_size = tree_size_bytes(output_path)
            artifact = Artifact(
                name=name,
                kind="preparation",
                path=output_path,
                sha256=output_digest,
                size_bytes=output_size,
                logical_size_bytes=input_artifact.logical_size_bytes,
            )
            measurement["prepared_artifact"] = {
                "path": str(output_path),
                "sha256": output_digest,
                "size_bytes": output_size,
                "logical_size_bytes": artifact.logical_size_bytes,
            }
            self.prepared[name] = artifact
            self.preparation_results.append(measurement)
            self.raw_writer.emit(measurement)

    def _run_workload(self, workload: Mapping[str, Any]) -> None:
        defaults = self.manifest.get("defaults", {})
        warmups = workload.get("warmups", defaults.get("warmups", 1))
        trials = workload.get("trials", defaults.get("trials", 5))
        timeout = float(workload.get("timeout_seconds", defaults.get("timeout_seconds", 3600)))
        throughput_source = workload.get(
            "throughput_source", DEFAULT_THROUGHPUT_SOURCE[workload["operation"]]
        )
        input_artifact = self._resolve_input(workload["input"])
        workload_failed = False
        for phase, count in (("warmup", warmups), ("trial", trials)):
            for pair_index, order in enumerate(
                randomized_pair_orders(self.seed, workload["name"], phase, count)
            ):
                for order_index, implementation in enumerate(order):
                    workdir = (
                        self.run_root
                        / "workloads"
                        / workload["name"]
                        / f"{phase}-{pair_index:03d}"
                        / implementation
                    )
                    measurement = self._execute(
                        implementation,
                        workload["args"],
                        input_artifact,
                        workdir,
                        timeout,
                        workload.get("environment"),
                        {
                            "event": "measurement",
                            "workload": workload["name"],
                            "operation": workload["operation"],
                            "phase": phase,
                            "pair_index": pair_index,
                            "order_index": order_index,
                            "pair_order": order,
                        },
                        throughput_source,
                    )
                    if measurement["exit_code"] != 0 or measurement["timed_out"]:
                        workload_failed = True
                    self._maybe_remove_artifacts(measurement)
                    self.measurements.append(measurement)
                    self.raw_writer.emit(measurement)
                if workload_failed:
                    break
            if workload_failed:
                break

    def _summarize_workload(self, workload: Mapping[str, Any]) -> dict[str, Any]:
        measurements = [
            item
            for item in self.measurements
            if item["workload"] == workload["name"] and item["phase"] == "trial"
        ]
        successful = [
            item for item in measurements if item["exit_code"] == 0 and not item["timed_out"]
        ]
        command_failures = [
            {
                "phase": item["phase"],
                "pair_index": item["pair_index"],
                "implementation": item["implementation"],
                "exit_code": item["exit_code"],
                "timed_out": item["timed_out"],
            }
            for item in self.measurements
            if item["workload"] == workload["name"]
            and (item["exit_code"] != 0 or item["timed_out"])
        ]
        by_implementation: dict[str, dict[str, Any]] = {}
        for implementation in IMPLEMENTATIONS:
            implementation_measurements = [
                item for item in successful if item["implementation"] == implementation
            ]
            metric_summaries: dict[str, Any] = {}
            for metric in sorted(METRICS):
                values = [
                    float(item[metric])
                    for item in implementation_measurements
                    if item.get(metric) is not None
                ]
                if values:
                    metric_summaries[metric] = summarize_values(values)
            by_implementation[implementation] = {
                "successful_trials": len(implementation_measurements),
                "metrics": metric_summaries,
            }

        indexed: dict[tuple[int, str], dict[str, Any]] = {
            (item["pair_index"], item["implementation"]): item for item in successful
        }
        pair_ratios: dict[str, list[float]] = {metric: [] for metric in METRICS}
        complete_pairs = 0
        pair_indexes = sorted({item["pair_index"] for item in successful})
        for pair_index in pair_indexes:
            cpp = indexed.get((pair_index, "cpp"))
            rust = indexed.get((pair_index, "rust"))
            if cpp is None or rust is None:
                continue
            complete_pairs += 1
            for metric in METRICS:
                if cpp.get(metric) is None or rust.get(metric) is None:
                    continue
                ratio = _ratio(float(rust[metric]), float(cpp[metric]))
                if ratio is not None:
                    pair_ratios[metric].append(ratio)
        paired_summary = {
            metric: summarize_values(ratios)
            for metric, ratios in sorted(pair_ratios.items())
            if ratios
        }
        gates = (
            workload["gates"]
            if "gates" in workload
            else self.manifest.get("defaults", {}).get("gates", [])
        )
        gate_results = evaluate_gates(gates, pair_ratios)
        expected_trials = workload.get(
            "trials", self.manifest.get("defaults", {}).get("trials", 5)
        )
        complete = complete_pairs == expected_trials and not command_failures
        gates_passed = all(gate["passed"] for gate in gate_results)
        return {
            "workload": workload["name"],
            "operation": workload["operation"],
            "expected_pairs": expected_trials,
            "complete_pairs": complete_pairs,
            "complete": complete,
            "command_failures": command_failures,
            "implementations": by_implementation,
            "paired_ratio_definition": "rust / cpp",
            "paired_ratios": paired_summary,
            "gates": gate_results,
            "gates_passed": gates_passed,
            "passed": complete and (gates_passed or self.ignore_gates),
        }

    def _cleanup_preparations(self, succeeded: bool) -> None:
        retain = self.keep_artifacts == "all" or (
            self.keep_artifacts == "failures" and not succeeded
        )
        if retain:
            return
        preparations_dir = self.run_root / "preparations"
        if preparations_dir.exists():
            shutil.rmtree(preparations_dir)
        for result in self.preparation_results:
            result["artifacts"]["retained"] = False

    def run(self) -> dict[str, Any]:
        """Execute the benchmark and always write a final JSON document."""
        metadata: dict[str, Any] = {}
        succeeded = False
        try:
            metadata = collect_environment_metadata(
                self.manifest_path,
                self.manifest_sha256,
                self.build_metadata_path,
                self.build_metadata_sha256,
                self.build_metadata,
                self.repo_root,
                self.binaries,
                self.seed,
            )
            self.raw_writer.emit({"event": "metadata", "recorded_at": _utc_now(), **metadata})
            self.datasets = resolve_datasets(self.manifest, self.manifest_path)
            for artifact in self.datasets.values():
                self.raw_writer.emit(
                    {
                        "event": "dataset",
                        "name": artifact.name,
                        "path": str(artifact.path),
                        "sha256": artifact.sha256,
                        "size_bytes": artifact.size_bytes,
                        "logical_size_bytes": artifact.logical_size_bytes,
                    }
                )
            self._run_preparations()
            workloads = [
                workload
                for workload in self.manifest["workloads"]
                if self.selected_workloads is None
                or workload["name"] in self.selected_workloads
            ]
            for workload in workloads:
                self._run_workload(workload)
                summary = self._summarize_workload(workload)
                self.summaries.append(summary)
                self.raw_writer.emit({"event": "workload_summary", **summary})
            succeeded = bool(self.summaries) and all(item["passed"] for item in self.summaries)
        except (BenchmarkError, ConfigurationError, OSError, subprocess.SubprocessError) as error:
            self.fatal_error = f"{type(error).__name__}: {error}"
            self.raw_writer.emit(
                {"event": "fatal_error", "recorded_at": _utc_now(), "error": self.fatal_error}
            )
        finally:
            try:
                self._cleanup_preparations(succeeded)
            except OSError as error:
                succeeded = False
                cleanup_error = f"failed to clean preparation artifacts: {error}"
                self.fatal_error = (
                    f"{self.fatal_error}; {cleanup_error}" if self.fatal_error else cleanup_error
                )
            final_result = {
                "schema_version": RESULT_SCHEMA_VERSION,
                "run": {
                    "name": self.manifest["name"],
                    "run_directory": str(self.run_root),
                    "finished_at": _utc_now(),
                    "passed": succeeded,
                    "fatal_error": self.fatal_error,
                    "gates_ignored": self.ignore_gates,
                    "keep_artifacts": self.keep_artifacts,
                },
                "manifest": _redacted_manifest(self.manifest),
                "metadata": metadata,
                "datasets": [
                    {
                        "name": item.name,
                        "kind": item.kind,
                        "path": str(item.path),
                        "sha256": item.sha256,
                        "size_bytes": item.size_bytes,
                        "logical_size_bytes": item.logical_size_bytes,
                    }
                    for item in self.datasets.values()
                ],
                "preparations": self.preparation_results,
                "measurements": self.measurements,
                "workloads": self.summaries,
            }
            final_path = self.run_root / "results.json"
            temporary_path = self.run_root / "results.json.tmp"
            temporary_path.write_text(
                json.dumps(final_result, indent=2, sort_keys=True, allow_nan=False) + "\n",
                encoding="utf-8",
            )
            os.replace(temporary_path, final_path)
            self.raw_writer.emit(
                {"event": "run_summary", "recorded_at": _utc_now(), **final_result["run"]}
            )
            self.raw_writer.close()
        return final_result


def _absolute_executable(value: str, label: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        raise ConfigurationError(f"{label} must be an absolute path inside the container")
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise ConfigurationError(f"failed to resolve {label}: {error}") from error
    if not resolved.is_file() or not os.access(resolved, os.X_OK | os.R_OK):
        raise ConfigurationError(f"{label} is not a readable executable regular file: {resolved}")
    return resolved


def _create_run_root(results_dir: Path, requested_run_id: str | None, seed: int) -> Path:
    if requested_run_id is not None:
        run_id = _validate_name(requested_run_id, "--run-id")
    else:
        timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        run_id = f"clp-s-{timestamp}-s{seed}-p{os.getpid()}"
    try:
        results_dir.mkdir(parents=True, exist_ok=True)
        run_root = results_dir / run_id
        run_root.mkdir()
    except OSError as error:
        raise ConfigurationError(f"failed to create results directory: {error}") from error
    return run_root


def _parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True, help="Benchmark manifest JSON.")
    parser.add_argument(
        "--cpp-binary", help="Absolute path to the C++ clp-s binary inside the container."
    )
    parser.add_argument(
        "--rust-binary", help="Absolute path to the Rust clp-s binary inside the container."
    )
    parser.add_argument(
        "--build-metadata",
        type=Path,
        help="Required JSON describing non-inferable C++ and Rust release build settings.",
    )
    parser.add_argument(
        "--results-dir", type=Path, help="Results parent directory inside the container."
    )
    parser.add_argument("--repo-root", type=Path, default=Path("/mnt/repo"))
    parser.add_argument("--run-id", help="Stable output directory name; must not already exist.")
    parser.add_argument("--seed", type=int, default=1729, help="Pair-order randomization seed.")
    parser.add_argument(
        "--workload",
        action="append",
        dest="workloads",
        help="Run only this workload (repeatable). Preparations still run.",
    )
    parser.add_argument(
        "--keep-artifacts",
        choices=("never", "failures", "all"),
        default="failures",
        help="Which command outputs/stdout/stderr to retain (default: failures).",
    )
    parser.add_argument(
        "--no-gates",
        action="store_true",
        help="Record gate evaluations but do not fail the run because of them.",
    )
    parser.add_argument(
        "--validate-only",
        action="store_true",
        help="Validate the manifest and dataset checksums without running either binary.",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    try:
        manifest_path = args.manifest.resolve(strict=True)
        manifest, manifest_sha256 = load_manifest(manifest_path)
        known_workloads = {item["name"] for item in manifest["workloads"]}
        selected = set(args.workloads) if args.workloads else None
        if selected is not None and not selected <= known_workloads:
            unknown = selected - known_workloads
            raise ConfigurationError(f"unknown workloads requested: {', '.join(sorted(unknown))}")
        if args.validate_only:
            datasets = resolve_datasets(manifest, manifest_path)
            print(
                json.dumps(
                    {
                        "manifest": str(manifest_path),
                        "sha256": manifest_sha256,
                        "datasets": {
                            name: {"sha256": item.sha256, "size_bytes": item.size_bytes}
                            for name, item in datasets.items()
                        },
                        "workloads": sorted(known_workloads),
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        if os.environ.get("CLP_S_BENCHMARK_CONTAINER") != "1" or not Path(
            "/.dockerenv"
        ).exists():
            raise ConfigurationError(
                "benchmark execution is container-only; use run-in-container.sh"
            )
        if (
            not args.cpp_binary
            or not args.rust_binary
            or args.build_metadata is None
            or args.results_dir is None
        ):
            raise ConfigurationError(
                "--cpp-binary, --rust-binary, --build-metadata, and --results-dir are required "
                "for execution"
            )
        binaries = {
            "cpp": _absolute_executable(args.cpp_binary, "--cpp-binary"),
            "rust": _absolute_executable(args.rust_binary, "--rust-binary"),
        }
        repo_root = args.repo_root.resolve(strict=True)
        build_metadata_path = args.build_metadata.resolve(strict=True)
        build_metadata, build_metadata_sha256 = load_build_metadata(build_metadata_path)
        validate_build_context(build_metadata, binaries, repo_root)
        run_root = _create_run_root(args.results_dir, args.run_id, args.seed)
        runner = BenchmarkRunner(
            manifest,
            manifest_path,
            manifest_sha256,
            build_metadata_path,
            build_metadata_sha256,
            build_metadata,
            binaries,
            repo_root,
            run_root,
            args.seed,
            selected,
            args.keep_artifacts,
            args.no_gates,
        )
        result = runner.run()
        print(f"Results: {run_root / 'results.json'}")
        for workload in result["workloads"]:
            outcome = "PASS" if workload["passed"] else "FAIL"
            print(
                f"{outcome} {workload['workload']}: "
                f"{workload['complete_pairs']}/{workload['expected_pairs']} complete pairs"
            )
            for gate in workload["gates"]:
                print(
                    f"  {'PASS' if gate['passed'] else 'FAIL'} {gate['metric']} "
                    f"{gate['statistic']} rust/cpp={gate['observed_ratio']} "
                    f"{gate['comparison']} {gate['threshold']}"
                )
        if result["run"]["fatal_error"]:
            print(f"ERROR: {result['run']['fatal_error']}", file=sys.stderr)
        return 0 if result["run"]["passed"] else 1
    except (ConfigurationError, OSError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
