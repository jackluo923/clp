#!/usr/bin/env python3
"""Compare projected output and dictionary-prefix reads on identical C++ archives in manylinux."""

# The standalone harness checks exact output and writes measurements for review.
# ruff: noqa: S101, PLR2004, T201

import argparse
import hashlib
import json
import random
import statistics
import string
import subprocess
import time
from pathlib import Path


def run(binary: Path, arguments: list[str | Path]) -> tuple[bytes, float]:
    """Return command output and elapsed wall time, including process startup."""
    start = time.perf_counter()
    result = subprocess.run([str(binary), *map(str, arguments)], capture_output=True, check=False)
    elapsed = time.perf_counter() - start
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace")[-4000:])
    return result.stdout, elapsed


def fixtures(directory: Path, rows: int) -> dict[str, Path]:
    """Create a large unused dictionary tail and an ERT with wide output columns."""
    rng = random.Random(20261023)
    prefix = directory / "prefix.jsonl"
    with prefix.open("w") as output:
        for _ in range(1000):
            output.write('{"id":1,"value":"early"}\n')
        for index in range(rows):
            if index == 50:
                output.write('{"id":2,"value":"middle","second":true}\n')
            token = "".join(rng.choices(string.ascii_lowercase, k=256))
            output.write(json.dumps({"id": -1, "unused": token}, separators=(",", ":")) + "\n")
        output.write('{"id":3,"value":"late","third":true}\n')
    wide = directory / "wide.jsonl"
    with wide.open("w") as output:
        for index in range(rows):
            record = {"seq": index * 2, "value": index % 97}
            record.update(
                {
                    f"text{column}": "".join(rng.choices(string.ascii_lowercase, k=64))
                    for column in range(8)
                }
            )
            output.write(json.dumps(record, separators=(",", ":")) + "\n")
    return {"prefix": prefix, "wide": wide}


def main() -> None:
    """Verify equivalence, then measure interleaved searches after a warmup."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=30000)
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument("--clpp-archive", type=Path)
    args = parser.parse_args()
    if args.rows < 100 or args.repeats < 1:
        parser.error("rows must be at least 100 and repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    archives = {}
    for name, source in fixtures(args.output, args.rows).items():
        destination = args.output / f"{name}-archive"
        run(
            args.baseline,
            [
                "c",
                "--adaptive-dictionary-columns",
                "--separate-columns-min-size",
                "1",
                destination,
                source,
            ],
        )
        archives[name] = destination
    # Each archive is created once by the baseline and read by both binaries.
    cases = [
        ("early-prefix", "prefix", "id:1", []),
        ("growing-prefix", "prefix", "id > 0 AND id < 3", ["--projection", "value"]),
        ("late-prefix", "prefix", "id:3", []),
        ("dictionary-predicate", "prefix", "value:ear*", []),
        ("numeric-projection", "wide", "seq:100", ["--projection", "seq", "value"]),
        ("zero-matches", "wide", "seq:101", []),
        ("full-record", "wide", "seq:100", []),
        ("all-records", "wide", "seq:*", []),
        ("count-control", "wide", "seq:100", ["--count"]),
    ]
    if args.clpp_archive:
        archives["clpp"] = args.clpp_archive
        cases.extend(
            [
                ("clpp-numeric-output", "clpp", "message.blockID.blockNum:1073746491", []),
                (
                    "clpp-string-output",
                    "clpp",
                    "message:*PacketResponder*",
                    ["--projection", "message"],
                ),
                (
                    "clpp-shape-output",
                    "clpp",
                    "message:*PacketResponder*",
                    ["--projection", "shape(message)"],
                ),
            ]
        )
    results = {
        "rows": args.rows,
        "repeats": args.repeats,
        "baseline": str(args.baseline),
        "candidate": str(args.candidate),
        "cases": {},
    }
    binaries = {"baseline": args.baseline, "candidate": args.candidate}
    for name, workload, query, options in cases:
        prefix = ["--experimental"] if workload == "clpp" else []
        arguments = [*prefix, "s", archives[workload], query, *options]
        expected, _ = run(args.baseline, arguments)
        actual, _ = run(args.candidate, arguments)
        assert actual == expected, f"Output differs for {name}"
        digest = hashlib.sha256(expected).hexdigest()
        times: dict[str, list[float]] = {key: [] for key in binaries}
        for repeat in range(args.repeats):
            order = ["baseline", "candidate"] if repeat % 2 == 0 else ["candidate", "baseline"]
            for label in order:
                output, elapsed = run(binaries[label], arguments)
                assert hashlib.sha256(output).hexdigest() == digest, name
                times[label].append(elapsed * 1000)
        medians = {label: statistics.median(samples) for label, samples in times.items()}
        results["cases"][name] = {
            "query": query,
            "options": options,
            "output_bytes": len(expected),
            "output_lines": expected.count(b"\n"),
            "sha256": digest,
            "samples_ms": times,
            "median_ms": medians,
            "speedup": medians["baseline"] / medians["candidate"],
        }
        (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(
            f"{name}: {medians['baseline']:.1f} -> {medians['candidate']:.1f} ms "
            f"({medians['baseline'] / medians['candidate']:.2f}x)",
            flush=True,
        )


if __name__ == "__main__":
    main()
