# This isolated, non-installed pytest package intentionally uses package-relative imports.
# ruff: noqa: TID252

"""Local pytest configuration for the isolated CLP-S characterization suite."""

from __future__ import annotations

import json
import os
import re
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast, Final

import pytest

from .harness import (
    BinaryTarget,
    collect_git_source_state,
    CONTAINER_MARKER_ENV_VAR,
    sha256_file,
    write_observation,
)

REPOSITORY_ROOT: Final = Path(__file__).parents[6]
FIXTURE_PATHS: Final = tuple(sorted((Path(__file__).parent / "fixtures").glob("*.jsonl")))


@dataclass(frozen=True)
class CharacterizationConfig:
    """Resolved test targets and artifact settings."""

    cpp: BinaryTarget
    rust: BinaryTarget | None
    observations_dir: Path
    timeout_seconds: float
    run_metadata: Mapping[str, Any]

    @property
    def targets(self) -> tuple[BinaryTarget, ...]:
        """Return the reference target followed by the optional candidate."""
        if self.rust is None:
            return (self.cpp,)
        return (self.cpp, self.rust)


def pytest_addoption(parser: pytest.Parser) -> None:
    """Declare explicit implementation and artifact paths."""
    group = parser.getgroup("clp-s-characterization")
    group.addoption(
        "--clp-s-cpp",
        action="store",
        default=os.environ.get("CLP_S_CPP_BIN"),
        metavar="PATH",
        help="Path to the reference C++ clp-s binary (or set CLP_S_CPP_BIN).",
    )
    group.addoption(
        "--clp-s-rust",
        action="store",
        default=os.environ.get("CLP_S_RUST_BIN"),
        metavar="PATH",
        help="Optional path to the candidate Rust clp-s binary (or set CLP_S_RUST_BIN).",
    )
    group.addoption(
        "--clp-s-observations-dir",
        action="store",
        default=os.environ.get("CLP_S_OBSERVATIONS_DIR"),
        metavar="PATH",
        help="Directory for JSON observations; defaults to a pytest temporary directory.",
    )
    group.addoption(
        "--clp-s-timeout",
        action="store",
        default="120",
        metavar="SECONDS",
        help="Per-command timeout in seconds (default: 120).",
    )
    group.addoption(
        "--clp-s-run-metadata",
        action="store",
        default=os.environ.get("CLP_S_RUN_METADATA"),
        metavar="PATH",
        help="Required JSON metadata describing the container, target, builds, and native deps.",
    )
    group.addoption(
        "--clp-s-allow-dirty-source",
        action="store_true",
        default=False,
        help="Allow a non-reproducible dirty source tree while retaining dirty-state hashes.",
    )


@pytest.fixture(scope="session")
def characterization_config(
    pytestconfig: pytest.Config,
    tmp_path_factory: pytest.TempPathFactory,
) -> CharacterizationConfig:
    """Validate that binary execution is explicit and container-confined."""
    cpp_option = cast("str | None", pytestconfig.getoption("--clp-s-cpp"))
    if cpp_option is None:
        pytest.skip("Pass --clp-s-cpp PATH to run the CLP-S characterization suite.")

    if os.environ.get(CONTAINER_MARKER_ENV_VAR) != "1":
        pytest.fail(
            f"Refusing to run clp-s outside the designated container. Set "
            f"{CONTAINER_MARKER_ENV_VAR}=1 inside "
            "components/core/tools/docker-images/clp-env-base-manylinux_2_28."
        )

    rust_option = cast("str | None", pytestconfig.getoption("--clp-s-rust"))
    observations_option = cast(
        "str | None",
        pytestconfig.getoption("--clp-s-observations-dir"),
    )
    timeout_option = cast("str", pytestconfig.getoption("--clp-s-timeout"))
    run_metadata_option = cast("str | None", pytestconfig.getoption("--clp-s-run-metadata"))
    if run_metadata_option is None:
        pytest.fail(
            "Pass --clp-s-run-metadata PATH. See run-metadata.example.json for the mandatory "
            "baseline provenance fields."
        )
    timeout_seconds = _parse_timeout(timeout_option)

    observations_dir = (
        Path(observations_option).resolve()
        if observations_option is not None
        else tmp_path_factory.mktemp("clp-s-observations")
    )
    cpp, rust = _resolve_targets(cpp_option, rust_option)

    run_metadata = _load_run_metadata(Path(run_metadata_option), rust is not None)
    try:
        source_state = collect_git_source_state(REPOSITORY_ROOT)
    except ValueError as error:
        pytest.fail(str(error))
    allow_dirty = cast("bool", pytestconfig.getoption("--clp-s-allow-dirty-source"))
    if source_state["dirty"] and not allow_dirty:
        pytest.fail(
            "Refusing to admit characterization observations from a dirty source tree. "
            "Commit/stash changes, or use --clp-s-allow-dirty-source for diagnostic-only output."
        )

    targets = [cpp]
    if rust is not None:
        targets.append(rust)
    manifest = {
        "schema_version": 1,
        "source": source_state,
        "declared_run_metadata": run_metadata,
        "targets": [
            {
                "name": target.name,
                "path": str(target.path),
                "sha256": sha256_file(target.path),
            }
            for target in targets
        ],
        "inputs": {
            str(fixture_path.relative_to(REPOSITORY_ROOT)): sha256_file(fixture_path)
            for fixture_path in FIXTURE_PATHS
        },
    }
    write_observation(observations_dir / "run-manifest.json", manifest)

    observations_dir.mkdir(parents=True, exist_ok=True)
    return CharacterizationConfig(
        cpp=cpp,
        rust=rust,
        observations_dir=observations_dir,
        timeout_seconds=timeout_seconds,
        run_metadata=run_metadata,
    )


def _load_run_metadata(path: Path, has_rust_candidate: bool) -> Mapping[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        pytest.fail(f"Failed to read --clp-s-run-metadata {path}: {error}")
    if not isinstance(value, dict):
        pytest.fail("--clp-s-run-metadata must contain a JSON object")

    _validate_required_metadata_fields(value, _required_metadata_paths(has_rust_candidate))
    if value["schema_version"] != 1:
        pytest.fail("Unsupported run metadata schema_version (expected 1)")
    digest = value["container"]["digest"]
    if re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
        pytest.fail("container.digest must be a complete lowercase sha256 image digest")
    return value


def _required_metadata_paths(has_rust_candidate: bool) -> list[tuple[str, ...]]:
    required_paths = [
        ("schema_version",),
        ("container", "image"),
        ("container", "digest"),
        ("target", "architecture"),
        ("target", "os"),
        ("target", "libc"),
        ("builds", "cpp", "compiler"),
        ("builds", "cpp", "compiler_version"),
        ("builds", "cpp", "configuration"),
        ("builds", "cpp", "flags"),
        ("native_dependencies", "zstd"),
        ("native_dependencies", "msgpack-cxx"),
        ("native_dependencies", "simdjson"),
        ("native_dependencies", "libarchive"),
    ]
    if has_rust_candidate:
        required_paths.extend(
            [
                ("builds", "rust", "rustc"),
                ("builds", "rust", "cargo_profile"),
                ("builds", "rust", "flags"),
            ]
        )
    return required_paths


def _validate_required_metadata_fields(
    metadata: Mapping[str, Any],
    required_paths: list[tuple[str, ...]],
) -> None:
    for keys in required_paths:
        current: Any = metadata
        for key in keys:
            if not isinstance(current, dict) or key not in current:
                pytest.fail(f"Missing run metadata field: {'.'.join(keys)}")
            current = current[key]
        if current in (None, "", []):
            pytest.fail(f"Run metadata field must not be empty: {'.'.join(keys)}")
        if _contains_placeholder(current):
            pytest.fail(f"Replace the example value for run metadata field: {'.'.join(keys)}")


def _parse_timeout(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError:
        pytest.fail(f"--clp-s-timeout must be numeric, got {value!r}")
    if timeout <= 0:
        pytest.fail("--clp-s-timeout must be greater than zero")
    return timeout


def _resolve_targets(
    cpp_path: str,
    rust_path: str | None,
) -> tuple[BinaryTarget, BinaryTarget | None]:
    cpp = BinaryTarget("cpp", Path(cpp_path).resolve())
    rust = BinaryTarget("rust", Path(rust_path).resolve()) if rust_path is not None else None
    try:
        cpp.validate()
        if rust is not None:
            rust.validate()
    except ValueError as error:
        pytest.fail(str(error))
    return cpp, rust


def _contains_placeholder(value: Any) -> bool:
    if isinstance(value, str):
        return "REPLACE_WITH" in value
    if isinstance(value, list):
        return any(_contains_placeholder(item) for item in value)
    if isinstance(value, dict):
        return any(_contains_placeholder(item) for item in value.values())
    return False
