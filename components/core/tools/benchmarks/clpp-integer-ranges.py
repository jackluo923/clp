#!/usr/bin/env python3
"""Compare integer range pruning on identical CLP+ archives inside the manylinux container."""

# This benchmark uses assertions for equivalence, literal fixture values, and CLI output.
# ruff: noqa: S101, PLR2004, T201

import argparse
import json
import random
import statistics
import subprocess
import time
from pathlib import Path


def run(binary: Path, arguments: list[str | Path]) -> tuple[str, float]:
    """Runs a CLI command and returns its output and wall time."""
    start = time.perf_counter()
    result = subprocess.run(
        [str(binary), "--experimental", *map(str, arguments)],
        capture_output=True,
        text=True,
        check=False,
    )
    elapsed = time.perf_counter() - start
    if result.returncode:
        message = f"{binary} {arguments}: {result.stderr[-4000:]}"
        raise RuntimeError(message)
    return result.stdout, elapsed


def compare(  # noqa: PLR0913
    binaries: list[tuple[str, Path]],
    archive: Path,
    query: str,
    expected: int | None,
    repeats: int,
    order: random.Random,
) -> dict:
    """Checks counts and measures a warmup followed by interleaved CLI invocations."""
    samples: dict[str, list[float]] = {label: [] for label, _ in binaries}
    for iteration in range(repeats + 1):
        order.shuffle(binaries)
        for label, binary in binaries:
            output, seconds = run(binary, ["s", archive, query, "--count"])
            count = sum(json.loads(line)["count"] for line in output.splitlines())
            if expected is None:
                expected = count
            assert count == expected, (label, archive, query, count, expected)
            if iteration:
                samples[label].append(seconds)
    return {
        "count": expected,
        "median_seconds": {label: statistics.median(values) for label, values in samples.items()},
        "samples_seconds": samples,
    }


def main() -> None:
    """Creates fixtures and writes reproducible query measurements to results.json."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=1000000)
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument(
        "--hive-archive", type=Path, help="Optionally query an existing Hive CLP+ archive."
    )
    args = parser.parse_args()
    if args.rows < 1000 or args.repeats < 1:
        parser.error("rows must be at least 1000 and repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    source = args.output / "numeric.jsonl"
    spec = args.output / "parsing.spec"
    spec.write_text('delimiters: " \\t\\r\\n"\nnumber: "^[0-9]+$"\n')
    values = random.Random(20261023)
    selected = 0
    with source.open("w") as output:
        for seq in range(args.rows):
            sensor = values.randrange(2000)
            selected += sensor > 1950
            output.write(f'{{"seq":{seq},"sensor":{sensor}}}\n')
    queries = [
        (f"seq >= 0 AND seq < {args.rows}", args.rows),
        ("seq < 0", 0),
        ("seq >= 0 AND sensor > 1950", selected),
        (f"seq > {args.rows - 10}", 9),
        ("seq: 999", 1),
    ]
    workloads = []
    for name, options in [
        ("packed", []),
        ("adaptive", ["--adaptive-dictionary-columns", "--separate-columns-min-size", "1"]),
    ]:
        archive = args.output / name
        run(
            args.candidate,
            ["c", "--parsing-specification", spec, *options, archive, source],
        )
        workloads.append((name, archive, queries))
    if args.hive_archive:
        workloads.append(
            (
                "hive",
                args.hive_archive,
                [
                    ("message.blockID.blockNum >= 0", None),
                    ("message.blockID.blockNum > 2000000000", None),
                    ("message.blockID.blockNum >= 1073746491", None),
                    ("message: *PacketResponder* AND message.blockID.blockNum >= 0", None),
                ],
            )
        )
    binaries = [("baseline", args.baseline), ("candidate", args.candidate)]
    order = random.Random(20261023)
    results = {
        "rows": args.rows,
        "repeats": args.repeats,
        "binaries": {label: str(binary) for label, binary in binaries},
        "workloads": {},
    }
    for name, archive, cases in workloads:
        measurements = {}
        results["workloads"][name] = measurements
        for query, expected in cases:
            measurement = compare(binaries, archive, query, expected, args.repeats, order)
            measurements[query] = measurement
            print(name, query, measurement["count"], measurement["median_seconds"], flush=True)
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
