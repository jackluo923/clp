#!/usr/bin/env python3
"""Measure dictionary preflight and case-insensitive matching on identical C++ CLP+ archives."""

# The standalone harness checks exact outputs and reports reproducible CLI measurements.
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
    """Run inside the project's manylinux container and include CLI startup in elapsed time."""
    start = time.perf_counter()
    result = subprocess.run(
        [str(binary), "--experimental", *map(str, arguments)], capture_output=True, check=False
    )
    elapsed = (time.perf_counter() - start) * 1000
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace")[-4000:])
    return result.stdout, elapsed


def fixture(directory: Path, rows: int) -> tuple[Path, Path, str, int]:
    """Create integer summaries beside a large dictionary of mixed-case strings."""
    source = directory / "input.jsonl"
    spec = directory / "parsing.spec"
    spec.write_text('delimiters: " \\t\\r\\n"\nnumber: "^[0-9]+$"\n')
    rng = random.Random(20261023)
    matches = 0
    target = ""
    with source.open("w") as output:
        for seq in range(rows):
            prefix = "Needle" if seq % 101 == 0 else "Value"
            target = prefix + "".join(rng.choices(string.ascii_letters, k=240))
            matches += "needle" in target.lower()
            record = {"seq": seq, "value": target}
            if seq % 2:
                record["other"] = True
            output.write(json.dumps(record, separators=(",", ":")) + "\n")
    return source, spec, target, matches


def main() -> None:
    """Check output equivalence, then interleave reader measurements after a warmup."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=30000)
    parser.add_argument("--repeats", type=int, default=7)
    args = parser.parse_args()
    if args.rows < 1000 or args.repeats < 1:
        parser.error("rows must be at least 1000 and repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    source, spec, target, matches = fixture(args.output, args.rows)
    prefix_matches = (args.rows + 100) // 101
    cases = [
        ("impossible-exact", f"seq > {args.rows} AND value:Missing", [], 0),
        ("impossible-wildcard", f"seq > {args.rows} AND value:*Needle*", [], 0),
        ("impossible-negated", f"NOT (seq < {args.rows}) AND value:*Needle*", [], 0),
        ("possible-or", f"seq > {args.rows} OR value:Needle*", [], prefix_matches),
        ("exact-ignore-case", f"value:{target.upper()}", ["--ignore-case"], 1),
        ("prefix-ignore-case", "value:NEEDLE*", ["--ignore-case"], prefix_matches),
        ("contains-ignore-case", "value:*NEEDLE*", ["--ignore-case"], matches),
        ("case-sensitive-control", "value:Needle*", [], prefix_matches),
        ("numeric-control", "seq:123", [], 1),
        ("all-count-control", "seq:*", [], args.rows),
    ]
    binaries = {"baseline": args.baseline, "candidate": args.candidate}
    results = {
        "rows": args.rows,
        "repeats": args.repeats,
        "binaries": {label: str(binary) for label, binary in binaries.items()},
        "workloads": {},
    }
    order = random.Random(20261023)
    for workload, storage in [
        ("packed", []),
        ("adaptive", ["--adaptive-dictionary-columns", "--separate-columns-min-size", "1"]),
    ]:
        archive = args.output / workload
        # The baseline writes each archive once; both readers consume exactly those bytes.
        run(args.baseline, ["c", "--parsing-specification", spec, *storage, archive, source])
        measurements = {}
        results["workloads"][workload] = measurements
        for name, query, options, count in cases:
            arguments = ["s", archive, query, "--count", *options]
            expected, _ = run(args.baseline, arguments)
            assert sum(json.loads(line)["count"] for line in expected.splitlines()) == count, name
            actual, _ = run(args.candidate, arguments)
            assert actual == expected, (workload, name)
            samples: dict[str, list[float]] = {label: [] for label in binaries}
            for _ in range(args.repeats):
                labels = list(binaries)
                order.shuffle(labels)
                for label in labels:
                    output, elapsed = run(binaries[label], arguments)
                    assert output == expected, (workload, name, label)
                    samples[label].append(elapsed)
            medians = {label: statistics.median(values) for label, values in samples.items()}
            measurements[name] = {
                "query": query,
                "options": options,
                "count": count,
                "sha256": hashlib.sha256(expected).hexdigest(),
                "samples_ms": samples,
                "median_ms": medians,
                "speedup": medians["baseline"] / medians["candidate"],
            }
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
            print(
                f"{workload} {name}: {medians['baseline']:.1f} -> "
                f"{medians['candidate']:.1f} ms ({measurements[name]['speedup']:.2f}x)",
                flush=True,
            )


if __name__ == "__main__":
    main()
