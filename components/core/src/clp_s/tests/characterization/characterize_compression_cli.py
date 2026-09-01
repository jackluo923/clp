#!/usr/bin/env python3
"""Capture the pinned C++ clp-s local compression CLI contract.

This is a focused, stdlib-only probe rather than a test. It must run in the CLP manylinux
container and writes raw command observations plus byte-level output manifests to an ignored build
directory supplied by the caller.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any, Final


CONTAINER_MARKER: Final = "CLP_S_COMPRESSION_CHARACTERIZATION_IN_CONTAINER"
UUID_RE: Final = re.compile(
    r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b"
)
LOG_PREFIX_RE: Final = re.compile(
    r"(?m)^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})"
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def tree_manifest(root: Path) -> list[dict[str, Any]]:
    if not root.exists():
        return []
    if root.is_file():
        return [
            {
                "path": ".",
                "kind": "file",
                "size": root.stat().st_size,
                "sha256": sha256(root),
            }
        ]
    entries: list[dict[str, Any]] = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        normalized = UUID_RE.sub("<UUID>", relative)
        if path.is_symlink():
            entries.append({"path": normalized, "kind": "symlink", "target": os.readlink(path)})
        elif path.is_dir():
            entries.append({"path": normalized, "kind": "directory"})
        elif path.is_file():
            entries.append(
                {
                    "path": normalized,
                    "kind": "file",
                    "size": path.stat().st_size,
                    "sha256": sha256(path),
                }
            )
    return entries


class Runner:
    def __init__(self, binary: Path, output: Path) -> None:
        self.binary = binary.resolve()
        self.output = output.resolve()
        self.observations: dict[str, dict[str, Any]] = {}

    def normalize(self, value: str, work: Path) -> str:
        value = value.replace(str(self.binary), "<CLP-S>")
        value = value.replace(str(self.output), "<OUTPUT>")
        value = value.replace(str(work.resolve()), "<WORK>")
        value = LOG_PREFIX_RE.sub("<LOG_TIMESTAMP>", value)
        return UUID_RE.sub("<UUID>", value)

    def run(
        self,
        name: str,
        work: Path,
        args: list[str],
        *,
        capture: Path | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        argv = [str(self.binary), *args]
        env = os.environ.copy()
        env.update(
            {
                "LANG": "C.UTF-8",
                "LC_ALL": "C.UTF-8",
                "TZ": "UTC",
                "CLICOLOR": "0",
                "CLICOLOR_FORCE": "0",
                "NO_COLOR": "1",
            }
        )
        result = subprocess.run(
            argv,
            cwd=work,
            env=env,
            capture_output=True,
            check=False,
            timeout=120,
        )
        stdout = result.stdout.decode("utf-8", errors="backslashreplace")
        stderr = result.stderr.decode("utf-8", errors="backslashreplace")
        stats: list[Any] = []
        for line in stdout.splitlines():
            try:
                stats.append(json.loads(line))
            except json.JSONDecodeError:
                stats = []
                break
        self.observations[name] = {
            "argv": argv,
            "argv_normalized": [self.normalize(arg, work) for arg in argv],
            "cwd": str(work.resolve()),
            "returncode": result.returncode,
            "stdout": stdout,
            "stdout_normalized": self.normalize(stdout, work),
            "stderr": stderr,
            "stderr_normalized": self.normalize(stderr, work),
            "stats": stats,
            "capture_root": None if capture is None else str(capture.resolve()),
            "tree": [] if capture is None else tree_manifest(capture),
        }
        return result

    def write(self) -> None:
        payload = {
            "binary": str(self.binary),
            "binary_sha256": sha256(self.binary),
            "observations": self.observations,
        }
        path = self.output / "observations.json"
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write(path: Path, data: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(data, encoding="utf-8")


def make_case(output: Path, name: str) -> Path:
    path = output / "cases" / name
    path.mkdir(parents=True)
    return path


def layout_cases(runner: Runner) -> None:
    document = '{"id":0,"kind":"a"}\n{\n  "id":1,\n  "kind":"b"\n}{"id":2,"kind":"c"}\n'
    for layout in ("directory", "single_file", "directory_repeat"):
        work = make_case(runner.output, f"layout_{layout}")
        write(work / "input.json", document)
        args = ["c", "--disable-log-order", "--print-archive-stats"]
        if layout == "single_file":
            args.append("--single-file-archive")
        args.extend(["archives", "input.json"])
        runner.run(f"layout_{layout}", work, args, capture=work / "archives")


def path_cases(runner: Runner) -> None:
    def inputs(work: Path) -> None:
        write(work / "extra.json", '{"source":"extra"}\n')
        write(work / "inputs" / "first.json", '{"source":"first"}\n')
        write(work / "inputs" / "nested" / "second.json", '{"source":"second"}\n')

    cases = {
        "paths_default": ([], ["extra.json", "inputs"]),
        "paths_remove_prefix": (["--remove-path-prefix", "inputs"], ["inputs"]),
        "paths_remove_prefix_and_leading": (
            ["--remove-path-prefix", "inputs", "--remove-leading-slash"],
            ["inputs"],
        ),
        "paths_normalized": (["--normalize-paths"], ["inputs/../inputs"]),
        "paths_normalized_prefix_and_leading": (
            [
                "--normalize-paths",
                "--remove-path-prefix",
                "inputs/..",
                "--remove-leading-slash",
            ],
            ["inputs/../inputs"],
        ),
    }
    for name, (options, input_paths) in cases.items():
        work = make_case(runner.output, name)
        inputs(work)
        runner.run(
            name,
            work,
            ["c", "--print-archive-stats", *options, "archives", *input_paths],
            capture=work / "archives",
        )

    work = make_case(runner.output, "paths_files_from")
    inputs(work)
    write(work / "listed.json", '{"source":"listed"}\n')
    write(work / "paths.txt", "listed.json\n\n")
    runner.run(
        "paths_files_from",
        work,
        [
            "c",
            "--print-archive-stats",
            "--files-from",
            "paths.txt",
            "archives",
            "extra.json",
        ],
        capture=work / "archives",
    )

    work = make_case(runner.output, "paths_absolute_remove_leading")
    inputs(work)
    runner.run(
        "paths_absolute_remove_leading",
        work,
        [
            "c",
            "--print-archive-stats",
            "--remove-leading-slash",
            "archives",
            str((work / "inputs" / "first.json").resolve()),
        ],
        capture=work / "archives",
    )


def rotation_cases(runner: Runner) -> None:
    records = "".join(f'{{"n":{index}}}\n' for index in range(4))
    for target in (0, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 1_000_000):
        name = f"rotation_{target}"
        work = make_case(runner.output, name)
        write(work / "input.json", records)
        runner.run(
            name,
            work,
            [
                "c",
                "--print-archive-stats",
                "--target-encoded-size",
                str(target),
                "archives",
                "input.json",
            ],
            capture=work / "archives",
        )

    for target in (7, 8, 9, 15, 16, 17, 31, 32, 33):
        name = f"rotation_no_order_{target}"
        work = make_case(runner.output, name)
        write(work / "input.json", records)
        runner.run(
            name,
            work,
            [
                "c",
                "--disable-log-order",
                "--print-archive-stats",
                "--target-encoded-size",
                str(target),
                "archives",
                "input.json",
            ],
            capture=work / "archives",
        )


def no_order_cases(runner: Runner) -> None:
    work = make_case(runner.output, "disable_log_order")
    write(work / "input.json", '{"id":0}\n{"id":1}\n')
    runner.run(
        "disable_log_order_compress",
        work,
        ["c", "--disable-log-order", "--print-archive-stats", "archives", "input.json"],
        capture=work / "archives",
    )
    runner.run(
        "disable_log_order_extract_unordered",
        work,
        ["x", "archives", "unordered"],
        capture=work / "unordered",
    )
    runner.run(
        "disable_log_order_extract_ordered",
        work,
        ["x", "--ordered", "archives", "ordered"],
        capture=work / "ordered",
    )


def encoding_option_cases(runner: Runner) -> None:
    records = []
    for index in range(4000):
        selector = index % 4
        if selector == 0:
            records.append(
                json.dumps(
                    {
                        "schema": "a",
                        "id": index,
                        "message": "repeated prefix alpha beta gamma " + str(index % 17),
                    },
                    separators=(",", ":"),
                )
            )
        elif selector == 1:
            records.append(
                json.dumps(
                    {"schema": "b", "id": index, "enabled": bool(index % 2)},
                    separators=(",", ":"),
                )
            )
        elif selector == 2:
            records.append(
                json.dumps(
                    {"schema": "c", "value": index / 7, "nested": {"x": index % 13}},
                    separators=(",", ":"),
                )
            )
        else:
            records.append(
                json.dumps(
                    {"schema": "d", "array": [index % 5, "repeat", None]},
                    separators=(",", ":"),
                )
            )
    document = "\n".join(records) + "\n"

    settings = (
        ("level_neg1", -1, 1_048_576),
        ("level_0", 0, 1_048_576),
        ("level_1", 1, 1_048_576),
        ("level_3", 3, 1_048_576),
        ("level_19", 19, 1_048_576),
        ("level_20", 20, 1_048_576),
        ("level_100", 100, 1_048_576),
        ("min_0", 3, 0),
        ("min_1", 3, 1),
        ("min_32", 3, 32),
        ("min_default", 3, 1_048_576),
        ("min_huge", 3, 1_000_000_000),
    )
    for name, level, minimum in settings:
        work = make_case(runner.output, name)
        write(work / "input.json", document)
        runner.run(
            name,
            work,
            [
                "c",
                "--disable-log-order",
                "--single-file-archive",
                "--print-archive-stats",
                "--compression-level",
                str(level),
                "--min-table-size",
                str(minimum),
                "archives",
                "input.json",
            ],
            capture=work / "archives",
        )

    invalid_settings = {
        "level_text": ["--compression-level", "invalid"],
        "min_negative": ["--min-table-size", "-1"],
        "target_negative": ["--target-encoded-size", "-1"],
    }
    for name, setting in invalid_settings.items():
        work = make_case(runner.output, name)
        write(work / "input.json", '{"id":0}\n')
        runner.run(
            name,
            work,
            ["c", *setting, "archives", "input.json"],
            capture=work / "archives",
        )


def side_effect_cases(runner: Runner) -> None:
    work = make_case(runner.output, "existing_output")
    write(work / "input.json", '{"run":1}\n')
    write(work / "archives" / "sentinel", "preserve\n")
    runner.run(
        "existing_output_first",
        work,
        ["c", "--disable-log-order", "archives", "input.json"],
        capture=work / "archives",
    )
    write(work / "input.json", '{"run":2}\n')
    runner.run(
        "existing_output_second",
        work,
        ["c", "--disable-log-order", "archives", "input.json"],
        capture=work / "archives",
    )

    for layout in ("directory", "single_file"):
        name = f"partial_failure_{layout}"
        work = make_case(runner.output, name)
        write(work / "first.json", '{"source":"first","id":0}\n')
        write(work / "second.json", '{"source":"second","id":1}\n42\n')
        args = ["c", "--print-archive-stats"]
        if layout == "single_file":
            args.append("--single-file-archive")
        args.extend(["archives", "first.json", "second.json"])
        runner.run(name, work, args, capture=work / "archives")
        runner.run(
            f"{name}_extract",
            work,
            ["x", "--ordered", "archives", "extracted"],
            capture=work / "extracted",
        )

    work = make_case(runner.output, "invalid_first")
    write(work / "input.json", "[]\n")
    runner.run(
        "invalid_first",
        work,
        ["c", "--print-archive-stats", "archives", "input.json"],
        capture=work / "archives",
    )

    work = make_case(runner.output, "output_is_file")
    write(work / "input.json", '{"id":0}\n')
    write(work / "archives", "sentinel\n")
    runner.run(
        "output_is_file",
        work,
        ["c", "archives", "input.json"],
        capture=work / "archives",
    )

    work = make_case(runner.output, "missing_output_parent")
    write(work / "input.json", '{"id":0}\n')
    runner.run(
        "missing_output_parent",
        work,
        ["c", "missing/archives", "input.json"],
        capture=work / "missing",
    )

    work = make_case(runner.output, "no_inputs")
    runner.run("no_inputs", work, ["c", "archives"], capture=work / "archives")

    work = make_case(runner.output, "empty_input_directory")
    (work / "inputs").mkdir()
    runner.run(
        "empty_input_directory",
        work,
        ["c", "archives", "inputs"],
        capture=work / "archives",
    )

    work = make_case(runner.output, "prefix_not_ancestor")
    write(work / "input.json", '{"id":0}\n')
    (work / "other").mkdir()
    runner.run(
        "prefix_not_ancestor",
        work,
        ["c", "--remove-path-prefix", "other", "archives", "input.json"],
        capture=work / "archives",
    )

    work = make_case(runner.output, "prefix_missing")
    write(work / "input.json", '{"id":0}\n')
    runner.run(
        "prefix_missing",
        work,
        ["c", "--remove-path-prefix", "missing", "archives", "input.json"],
        capture=work / "archives",
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if os.environ.get(CONTAINER_MARKER) != "1":
        raise SystemExit(f"Refusing to run without {CONTAINER_MARKER}=1")
    if not args.binary.is_file():
        raise SystemExit(f"Binary does not exist: {args.binary}")
    args.output.mkdir(parents=True, exist_ok=False)

    runner = Runner(args.binary, args.output)
    layout_cases(runner)
    path_cases(runner)
    rotation_cases(runner)
    no_order_cases(runner)
    encoding_option_cases(runner)
    side_effect_cases(runner)
    runner.write()


if __name__ == "__main__":
    main()
