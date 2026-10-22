#!/usr/bin/env python3
"""
Compare the C++ column port with a baseline binary, inside the manylinux container.

Produces deterministic fixtures, checks search/extraction equivalence, and records median
warm-cache CLI timings. Each run needs a fresh output directory; archives are retained for review.
"""

# This standalone benchmark uses assertions for equivalence, literal fixture values, and CLI output.
# ruff: noqa: S101, PLR2004, T201

import argparse
import json
import random
import statistics
import subprocess
import time
from pathlib import Path
from typing import Any


def run(binary: Path, arguments: list[str | Path]) -> tuple[str, float]:
    """Runs a CLI command and returns its output and wall time."""
    start = time.perf_counter()
    result = subprocess.run(
        [str(binary), *map(str, arguments)], capture_output=True, text=True, check=False
    )
    elapsed = time.perf_counter() - start
    if result.returncode:
        message = f"{binary} {arguments}: {result.stderr[-4000:]}"
        raise RuntimeError(message)
    return result.stdout, elapsed


def count(binary: Path, prefix: list[str], archive: Path, query: str) -> tuple[int, float]:
    """Returns the total query count and CLI wall time."""
    output, elapsed = run(binary, [*prefix, "s", archive, query, "--count"])
    return sum(json.loads(line)["count"] for line in output.splitlines()), elapsed


def extract(
    binary: Path, prefix: list[str], archive: Path, output: Path
) -> tuple[list[Any], float]:
    """Extracts records in input order for comparison and returns the CLI wall time."""
    _, elapsed = run(binary, [*prefix, "x", "--ordered", archive, output])
    records = []
    for path in sorted(output.glob("*.jsonl")):
        records.extend(json.loads(line) for line in path.read_text().splitlines())
    return records, elapsed


def main() -> None:  # noqa: C901, PLR0912, PLR0915
    """Creates archives, verifies equivalence, and writes benchmark measurements."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument(
        "--baseline-adaptive",
        action="store_true",
        help="Use adaptive storage for the baseline too when comparing subsequent reader changes.",
    )
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=100000)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--clpp-input", type=Path)
    parser.add_argument("--parsing-spec", type=Path)
    args = parser.parse_args()
    if args.rows <= 0 or args.repeats <= 0:
        parser.error("rows and repeats must be positive")
    if bool(args.clpp_input) != bool(args.parsing_spec):
        parser.error("clpp-input and parsing-spec must be supplied together")
    args.output.mkdir(parents=True, exist_ok=False)
    source = args.output / "numeric.jsonl"
    random_source = random.Random(20261022)
    with source.open("w") as output:
        for i in range(args.rows):
            row = {
                "seq": i,
                "counter": 1700000000000 + i * 25,
                "sensor": random_source.randrange(2000),
                "x": 50 + random_source.randrange(-10000, 10000) / 100000,
                "constant": 42,
                "state": ["idle", "busy", "failed"][i % 3],
                "message": f"Receiving blk_{1073746491 + i % 1000}_{5667 + i % 23}",
                "mixed": -(1 << 63) if i % 100 == 0 else (1 << 63) - 1 if i % 101 == 0 else i,
                "values": [i % 3, i % 7],
            }
            if i % 7 == 0:
                row["optional"] = bool(i % 2)
            output.write(json.dumps(row, separators=(",", ":")) + "\n")
    workloads = [
        (
            "numeric",
            source,
            [],
            ["--no-retain-float-format"],
            [
                "seq: 999",
                "seq > 95000",
                "seq < 10 AND state: busy",
                "state: b*",
                "NOT state: busy",
                "state: absent OR seq: 999",
                "sensor: *67*",
                "mixed: -9223372036854775808",
                "mixed: 9223372036854775807",
                "*: busy",
                "constant: 42",
                "constant: 99",
                "values: 2",
                "optional: true",
                "NOT (seq: 999 OR state: busy)",
                "x > 50.05",
                "message: *blk_1073746491_*",
            ],
        )
    ]
    if args.clpp_input:
        workloads.append(
            (
                "clpp",
                args.clpp_input,
                ["--experimental"],
                ["--parsing-specification", args.parsing_spec],
                [
                    "message: *blk_1073746491_5667*",
                    "message: *5667*",
                    "message: *Exception*",
                    "message: *zzzz*",
                    "message: *a*b*",
                    "message: *blk_*",
                    "message: *INFO*",
                    "message.blockID.blockNum: 1073746491",
                ],
            )
        )
    results = {
        "rows": args.rows,
        "repeats": args.repeats,
        "baseline_adaptive": args.baseline_adaptive,
        "workloads": {},
    }
    for name, input_file, prefix, compress_options, queries in workloads:
        configurations = [
            (
                "baseline",
                args.baseline,
                ["--adaptive-numeric-columns", "--separate-columns-min-size", "1048576"]
                if args.baseline_adaptive
                else [],
            ),
            ("legacy", args.candidate, []),
            ("separate", args.candidate, ["--separate-columns-min-size", "1"]),
            (
                "adaptive",
                args.candidate,
                ["--adaptive-numeric-columns", "--separate-columns-min-size", "1048576"],
            ),
        ]
        expected_records = None
        archives = {}
        measurements = {}
        for label, binary, options in configurations:
            durations = []
            for iteration in range(args.repeats):
                directory = args.output / f"{name}-{label}-{iteration}"
                _, elapsed = run(
                    binary,
                    [
                        *prefix,
                        "c",
                        "--single-file-archive",
                        *compress_options,
                        *options,
                        directory,
                        input_file,
                    ],
                )
                durations.append(elapsed)
                archives[label] = directory
            sizes = [
                sum(p.stat().st_size for p in (args.output / f"{name}-{label}-{i}").iterdir())
                for i in range(args.repeats)
            ]
            actual_records, extraction_time = extract(
                binary, prefix, archives[label], args.output / f"{name}-{label}-extracted"
            )
            measurements[label] = {
                "archive_bytes": int(statistics.median(sizes)),
                "ingest_seconds": statistics.median(durations),
                "extraction_seconds": extraction_time,
                "queries": {},
            }
            if label == "baseline":
                expected_records = actual_records
            else:
                assert actual_records == expected_records, (name, label, "extraction differs")
        # Complete query times, with a warmup and round-robin ordering across configurations.
        for query in queries:
            counts = {}
            times = {label: [] for label, _, _ in configurations}
            for label, binary, _ in configurations:
                counts[label], _ = count(binary, prefix, archives[label], query)
            assert len(set(counts.values())) == 1, (name, query, counts)
            for _ in range(args.repeats):
                for label, binary, _ in configurations:
                    actual, elapsed = count(binary, prefix, archives[label], query)
                    assert actual == counts[label]
                    times[label].append(elapsed)
            for label, durations in times.items():
                measurements[label]["queries"][query] = {
                    "count": counts[label],
                    "seconds": statistics.median(durations),
                }
            baseline_count, _ = count(args.candidate, prefix, archives["baseline"], query)
            assert baseline_count == counts["baseline"], (
                name,
                query,
                "baseline archive read differs",
            )
        if not args.baseline_adaptive and name == "numeric" and args.rows >= 100000:
            # Readers predating the port must reject the new layout, not misinterpret its bytes.
            rejected = subprocess.run(
                [
                    str(args.baseline),
                    *prefix,
                    "s",
                    str(archives["adaptive"]),
                    queries[0],
                    "--count",
                ],
                capture_output=True,
                check=False,
            )
            assert rejected.returncode != 0, "baseline unexpectedly accepted adaptive archive"
        results["workloads"][name] = measurements
        (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps({"workload": name, "results": measurements}), flush=True)
    print("Search counts, ordered extraction, and baseline archive reads agree.", flush=True)


if __name__ == "__main__":
    main()
