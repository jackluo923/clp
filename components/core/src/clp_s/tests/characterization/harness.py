"""Reusable process and filesystem observation support for CLP-S characterization tests."""

from __future__ import annotations

import copy
import difflib
import hashlib
import json
import os
import re
import stat
import subprocess
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Final

CONTAINER_MARKER_ENV_VAR: Final = "CLP_S_CHARACTERIZATION_IN_CONTAINER"
MAX_CAPTURED_TEXT_BYTES: Final = 1024 * 1024
CONTROLLED_ENVIRONMENT: Final = {
    "LANG": "C.UTF-8",
    "LC_ALL": "C.UTF-8",
    "TZ": "UTC",
    "CLICOLOR": "0",
    "CLICOLOR_FORCE": "0",
    "NO_COLOR": "1",
}

_ANSI_ESCAPE_RE: Final = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")
_LOG_TIMESTAMP_RE: Final = re.compile(
    r"(?m)^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{4})"
)
_UUID_RE: Final = re.compile(
    r"(?i)(?<![0-9a-f])[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}"
    r"-[0-9a-f]{12}(?![0-9a-f])"
)


class CharacterizationError(ValueError):
    """A characterization target or source-state precondition failed."""


class JsonLineTypeError(TypeError):
    """A JSON-lines helper received a non-object record."""


class DifferentialMismatchError(AssertionError):
    """The Rust candidate's normalized behavior differs from the C++ oracle."""


@dataclass(frozen=True)
class BinaryTarget:
    """A named CLP-S implementation under test."""

    name: str
    path: Path

    def validate(self) -> None:
        """Raise a descriptive error when the target cannot be executed."""
        if not self.path.is_file():
            message = f"{self.name} clp-s binary does not exist: {self.path}"
            raise CharacterizationError(message)
        if not os.access(self.path, os.X_OK):
            message = f"{self.name} clp-s binary is not executable: {self.path}"
            raise CharacterizationError(message)


@dataclass(frozen=True)
class CommandObservation:
    """The complete deterministic and diagnostic observation of one command."""

    case: str
    target: str
    raw_argv: tuple[str, ...]
    normalized_argv: tuple[str, ...]
    returncode: int | None
    timed_out: bool
    raw_stdout: str
    raw_stderr: str
    stdout: str
    stderr: str
    environment: Mapping[str, Any]
    output_trees: Mapping[str, Mapping[str, Any]]

    def to_dict(self) -> dict[str, Any]:
        """Return a JSON-serializable representation."""
        return {
            "case": self.case,
            "target": self.target,
            "command": {
                "raw": list(self.raw_argv),
                "normalized": list(self.normalized_argv),
            },
            "returncode": self.returncode,
            "timed_out": self.timed_out,
            "stdout": {"raw": self.raw_stdout, "normalized": self.stdout},
            "stderr": {"raw": self.raw_stderr, "normalized": self.stderr},
            "environment": self.environment,
            "output_trees": self.output_trees,
        }


class OutputNormalizer:
    """Normalize only known sources of cross-run nondeterminism."""

    def __init__(self, binary_path: Path, work_dir: Path) -> None:
        """Bind normalization to one executable and isolated working directory."""
        self._binary_path = str(binary_path.resolve())
        self._work_dir = str(work_dir.resolve())
        self._uuid_tokens: dict[str, str] = {}

    def normalize_token(self, value: str) -> str:
        """Normalize paths and UUIDs in an argument or filesystem path."""
        normalized = value.replace(self._binary_path, "<CLP-S>")
        normalized = normalized.replace(self._work_dir, "<WORKDIR>")
        return self._replace_uuids(normalized)

    def normalize_text(self, value: str, *, sort_json_lines: bool = False) -> str:
        """Normalize a captured UTF-8 stream or file."""
        normalized = value.replace("\r\n", "\n").replace("\r", "\n")
        normalized = _ANSI_ESCAPE_RE.sub("", normalized)
        normalized = normalized.replace(self._binary_path, "<CLP-S>")
        normalized = normalized.replace(self._work_dir, "<WORKDIR>")
        normalized = _LOG_TIMESTAMP_RE.sub("<LOG_TIMESTAMP>", normalized)
        normalized = self._replace_uuids(normalized)
        return _canonicalize_json_lines(normalized, sort_lines=sort_json_lines)

    def _replace_uuids(self, value: str) -> str:
        def replace(match: re.Match[str]) -> str:
            uuid = match.group(0).lower()
            if uuid not in self._uuid_tokens:
                self._uuid_tokens[uuid] = f"<UUID:{len(self._uuid_tokens) + 1}>"
            return self._uuid_tokens[uuid]

        return _UUID_RE.sub(replace, value)


class CharacterizationHarness:
    """Run one CLP-S implementation and record stable observations."""

    def __init__(
        self,
        target: BinaryTarget,
        work_dir: Path,
        *,
        timeout_seconds: float = 120.0,
    ) -> None:
        """Validate and retain one target's execution settings."""
        target.validate()
        if timeout_seconds <= 0:
            message = "timeout_seconds must be greater than zero"
            raise CharacterizationError(message)
        self.target = target
        self.work_dir = work_dir.resolve()
        self.timeout_seconds = timeout_seconds
        self.work_dir.mkdir(parents=True, exist_ok=True)
        self._normalizer = OutputNormalizer(target.path, self.work_dir)

    def run(
        self,
        case: str,
        args: Sequence[str | Path],
        *,
        capture_roots: Mapping[str, Path] | None = None,
        sort_stdout_json_lines: bool = False,
        extra_env: Mapping[str, str] | None = None,
    ) -> CommandObservation:
        """Run CLP-S without a shell and capture its streams and requested trees."""
        argv = (str(self.target.path.resolve()), *(str(arg) for arg in args))
        env = os.environ.copy()
        env.update(CONTROLLED_ENVIRONMENT)
        if extra_env is not None:
            env.update(extra_env)

        timed_out = False
        returncode: int | None
        stdout_bytes: bytes
        stderr_bytes: bytes
        try:
            result = subprocess.run(
                argv,
                cwd=self.work_dir,
                env=env,
                capture_output=True,
                check=False,
                timeout=self.timeout_seconds,
            )
            returncode = result.returncode
            stdout_bytes = result.stdout
            stderr_bytes = result.stderr
        except subprocess.TimeoutExpired as error:
            timed_out = True
            returncode = None
            stdout_bytes = _as_bytes(error.stdout)
            stderr_bytes = _as_bytes(error.stderr)

        raw_stdout = stdout_bytes.decode("utf-8", errors="backslashreplace")
        raw_stderr = stderr_bytes.decode("utf-8", errors="backslashreplace")
        output_trees = {
            name: capture_tree(path, self._normalizer)
            for name, path in sorted((capture_roots or {}).items())
        }
        return CommandObservation(
            case=case,
            target=self.target.name,
            raw_argv=argv,
            normalized_argv=tuple(self._normalizer.normalize_token(arg) for arg in argv),
            returncode=returncode,
            timed_out=timed_out,
            raw_stdout=raw_stdout,
            raw_stderr=raw_stderr,
            stdout=self._normalizer.normalize_text(
                raw_stdout,
                sort_json_lines=sort_stdout_json_lines,
            ),
            stderr=self._normalizer.normalize_text(raw_stderr),
            environment={
                "controlled": CONTROLLED_ENVIRONMENT,
                "additional_names": sorted(extra_env or {}),
            },
            output_trees=output_trees,
        )


def capture_tree(root: Path, normalizer: OutputNormalizer) -> dict[str, Any]:
    """Capture a sorted, non-following manifest for a file or directory tree."""
    root = root.resolve()
    if not root.exists():
        return {"exists": False, "entries": []}

    paths = [root]
    if root.is_dir():
        paths.extend(sorted(root.rglob("*"), key=lambda path: path.as_posix()))

    entries: list[dict[str, Any]] = []
    for path in paths:
        relative_path = "." if path == root else path.relative_to(root).as_posix()
        normalized_path = normalizer.normalize_token(relative_path)
        metadata = path.lstat()
        mode = stat.S_IMODE(metadata.st_mode)

        if path.is_symlink():
            entries.append(
                {
                    "path": normalized_path,
                    "kind": "symlink",
                    "mode": f"{mode:04o}",
                    "target": normalizer.normalize_token(str(path.readlink())),
                }
            )
        elif path.is_dir():
            entries.append({"path": normalized_path, "kind": "directory", "mode": f"{mode:04o}"})
        elif path.is_file():
            entries.append(_capture_file(path, normalized_path, mode, normalizer))
        else:
            entries.append({"path": normalized_path, "kind": "special", "mode": f"{mode:04o}"})

    return {"exists": True, "entries": entries}


def comparison_view(observation: CommandObservation) -> dict[str, Any]:
    """Return the deterministic portion used for reference-versus-candidate comparisons."""
    return {
        "command": list(observation.normalized_argv),
        "returncode": observation.returncode,
        "timed_out": observation.timed_out,
        "stdout": observation.stdout,
        "stderr": observation.stderr,
        "environment": copy.deepcopy(observation.environment),
        "output_trees": copy.deepcopy(observation.output_trees),
    }


def semantic_comparison_view(observation: CommandObservation) -> dict[str, Any]:
    """Compare machine-visible output and success/failure while retaining diagnostics elsewhere."""
    view = comparison_view(observation)
    if observation.timed_out:
        exit_status = "timed-out"
    elif observation.returncode == 0:
        exit_status = "success"
    else:
        exit_status = "failure"
    view["exit_status"] = exit_status
    del view["returncode"]
    del view["stderr"]
    return view


def mask_json_fields(text: str, fields: Iterable[str]) -> str:
    """Mask selected top-level fields in a stream consisting entirely of JSON lines."""
    fields_to_mask = frozenset(fields)
    had_final_newline = text.endswith("\n")
    lines = text.splitlines()
    masked_lines: list[str] = []
    for line in lines:
        if not line.strip():
            masked_lines.append(line)
            continue
        value = json.loads(line)
        if not isinstance(value, dict):
            message = "Expected each JSON line to contain an object"
            raise JsonLineTypeError(message)
        for field in fields_to_mask:
            if field in value:
                value[field] = "<IGNORED>"
        masked_lines.append(_canonical_json(value))
    result = "\n".join(masked_lines)
    if had_final_newline:
        result += "\n"
    return result


def tree_structure_view(tree: Mapping[str, Any]) -> dict[str, Any]:
    """Remove file payload, size, and mode observations while preserving layout."""
    result = copy.deepcopy(dict(tree))
    for entry in result.get("entries", []):
        for field in ("mode", "raw_sha256", "size", "text", "normalized_text"):
            entry.pop(field, None)
    return result


def tree_semantic_view(tree: Mapping[str, Any]) -> dict[str, Any]:
    """Compare normalized text payloads without requiring byte-identical serialization."""
    result = copy.deepcopy(dict(tree))
    for entry in result.get("entries", []):
        for field in ("mode", "raw_sha256", "size", "text"):
            entry.pop(field, None)
    return result


def assert_views_equal(reference: Mapping[str, Any], candidate: Mapping[str, Any]) -> None:
    """Raise an assertion containing a unified JSON diff when two views differ."""
    reference_json = json.dumps(
        reference, indent=2, sort_keys=True, ensure_ascii=False
    ).splitlines()
    candidate_json = json.dumps(
        candidate, indent=2, sort_keys=True, ensure_ascii=False
    ).splitlines()
    if reference_json == candidate_json:
        return
    diff = "\n".join(
        difflib.unified_diff(
            reference_json,
            candidate_json,
            fromfile="cpp-reference",
            tofile="rust-candidate",
            lineterm="",
        )
    )
    message = f"CLP-S differential mismatch:\n{diff}"
    raise DifferentialMismatchError(message)


def write_observation(path: Path, value: Mapping[str, Any] | Sequence[Any]) -> None:
    """Write a stable, human-readable observation artifact."""
    path.parent.mkdir(parents=True, exist_ok=True)
    serialized = json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    temporary_path = path.with_suffix(f"{path.suffix}.tmp")
    temporary_path.write_text(serialized, encoding="utf-8")
    temporary_path.replace(path)


def sha256_file(path: Path) -> str:
    """Return the SHA-256 digest of a file without loading it all into memory."""
    digest = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def collect_git_source_state(repository_root: Path) -> dict[str, Any]:
    """Collect the exact source commit and enough dirty-state data to reject weak baselines."""
    commit = _run_git(repository_root, "rev-parse", "HEAD").decode().strip()
    status = _run_git(
        repository_root,
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
    )
    diff = _run_git(repository_root, "diff", "--binary", "HEAD", "--")
    submodules = _run_git(repository_root, "submodule", "status", "--recursive")
    return {
        "repository_root": str(repository_root.resolve()),
        "commit": commit,
        "dirty": bool(status),
        "status": status.decode("utf-8", errors="backslashreplace").splitlines(),
        "status_sha256": hashlib.sha256(status).hexdigest(),
        "tracked_diff_sha256": hashlib.sha256(diff).hexdigest(),
        "submodules": submodules.decode("utf-8", errors="backslashreplace").splitlines(),
    }


def _capture_file(
    path: Path,
    normalized_path: str,
    mode: int,
    normalizer: OutputNormalizer,
) -> dict[str, Any]:
    size = path.stat().st_size
    entry: dict[str, Any] = {
        "path": normalized_path,
        "kind": "file",
        "mode": f"{mode:04o}",
        "size": size,
        "raw_sha256": sha256_file(path),
    }
    if size > MAX_CAPTURED_TEXT_BYTES:
        return entry

    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return entry
    if "\x00" in text:
        return entry

    entry["text"] = text
    entry["normalized_text"] = normalizer.normalize_text(text)
    return entry


def _canonicalize_json_lines(text: str, *, sort_lines: bool) -> str:
    if not text:
        return text

    had_final_newline = text.endswith("\n")
    lines = text.splitlines()
    parsed_lines: list[str] = []
    for line in lines:
        if not line.strip():
            return text
        try:
            parsed_lines.append(_canonical_json(json.loads(line)))
        except (json.JSONDecodeError, TypeError, ValueError):
            return text

    if sort_lines:
        parsed_lines.sort()
    result = "\n".join(parsed_lines)
    if had_final_newline:
        result += "\n"
    return result


def _canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def _as_bytes(value: bytes | str | None) -> bytes:
    if value is None:
        return b""
    if isinstance(value, bytes):
        return value
    return value.encode("utf-8", errors="backslashreplace")


def _run_git(repository_root: Path, *args: str) -> bytes:
    try:
        result = subprocess.run(
            ("git", "-C", str(repository_root), *args),
            capture_output=True,
            check=True,
            timeout=30,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        message = f"Failed to collect git source state: {error}"
        raise CharacterizationError(message) from error
    return result.stdout
