#!/usr/bin/env python3
"""Compare ingestion, archive payloads, extraction and search inside the manylinux container."""

# Standalone benchmark: deterministic fixture constants, equivalence assertions and CLI output.
# ruff: noqa: S101, PLR2004, T201

import argparse
import hashlib
import json
import random
import shutil
import statistics
import subprocess
import time
from pathlib import Path


def run(binary: Path, arguments: list[str | Path]) -> tuple[bytes, float]:
    """Return CLI output and elapsed milliseconds, including process startup."""
    start = time.perf_counter()
    result = subprocess.run([str(binary), *map(str, arguments)], capture_output=True, check=False)
    elapsed = (time.perf_counter() - start) * 1000
    if result.returncode:
        message = f"{arguments}: {result.stderr.decode(errors='replace')[-4000:]}"
        raise RuntimeError(message)
    return result.stdout, elapsed


def fixture(path: Path, name: str, rows: int) -> None:
    """Exercise stable layouts, interleaving, cache churn, oversized layouts and CLP+ shapes."""
    with path.open("w") as output:
        for seq in range(rows):
            mode = seq % 4
            record = {"seq": seq}
            if name == "clpp":
                record.update(
                    {
                        "ts": f"2024-02-29T12:34:{seq % 60:02d}.120000000Z",
                        "message": (
                            f"{'Receiving' if mode < 2 else 'Sending'} "
                            f"blk_{100 + seq % 7}_{seq % 11}"
                        ),
                        "values": [seq % 3, {"value": seq % 5}, []],
                        "nested": {"state": "busy" if mode % 2 else "idle", "empty": {}},
                        "decimal": 1.25,
                    }
                )
            else:
                width = 512 if name == "wide" else 1 if name == "narrow-churn" else 24
                fields = range(width)
                if name == "interleaved" and mode % 2:
                    fields = reversed(fields)
                prefix = f"type{seq % 32}_" if name in {"churn", "narrow-churn"} else "field_"
                record.update({f"{prefix}{field:03d}": seq % (field + 17) for field in fields})
                if name == "interleaved":
                    record["nested"] = {f"kind{mode}": seq, "empty": None if mode < 2 else {}}
            output.write(json.dumps(record, separators=(",", ":")) + "\n")


def payload_hashes(root: Path) -> list[dict[str, str]]:
    """Ignore headers containing randomized range identities; compare every data section."""
    payloads = [
        {
            part.name: hashlib.sha256(part.read_bytes()).hexdigest()
            for part in sorted(archive.iterdir())
            if part.name != "header"
        }
        for archive in root.iterdir()
        if archive.is_dir()
    ]
    return sorted(payloads, key=lambda payload: json.dumps(payload, sort_keys=True))


def extract(binary: Path, prefix: list[str], archive: Path, target: Path) -> list[bytes]:
    """Compare exact reconstructed lines in record order across archive splits."""
    run(binary, [*prefix, "x", "--ordered", archive, target])
    lines = [line for path in target.glob("*.jsonl") for line in path.read_bytes().splitlines()]
    shutil.rmtree(target)
    return sorted(lines, key=lambda line: json.loads(line)["seq"])


def verify(
    binaries: dict[str, Path],
    prefix: list[str],
    archives: dict[str, Path],
    source: Path,
    root: Path,
) -> None:
    """Both readers must reconstruct both writers' archives and agree on query counts."""
    original = [json.loads(line) for line in source.read_text().splitlines()]
    expected = None
    for writer, archive in archives.items():
        for reader, binary in binaries.items():
            actual = extract(binary, prefix, archive, root / f"extract-{writer}-{reader}")
            assert [json.loads(line) for line in actual] == original, (writer, reader)
            if expected is None:
                expected = actual
            assert actual == expected, (writer, reader)
            for query, count in [("seq: *", len(original)), ("seq: 17", 1)]:
                result, _ = run(binary, [*prefix, "s", archive, query, "--count"])
                assert sum(json.loads(line)["count"] for line in result.splitlines()) == count
    assert payload_hashes(archives["baseline"]) == payload_hashes(archives["candidate"])


def main() -> None:  # noqa: PLR0915
    """Validate each fixture, then interleave warm-cache compression measurements."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=50000)
    parser.add_argument("--repeats", type=int, default=7)
    args = parser.parse_args()
    if args.rows < 1000 or args.repeats < 1:
        parser.error("rows must be at least 1000 and repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    spec = args.output / "parsing.spec"
    spec.write_text(
        'delimiters: " \\t\\r\\n"\nnumber: "^[0-9]+$"\n'
        'blockID (0): "blk_(?<blockNum>[0-9]+)_(?<genStamp>[0-9]+)"\n'
    )
    binaries = {"baseline": args.baseline, "candidate": args.candidate}
    results = {
        "rows": args.rows,
        "repeats": args.repeats,
        "binaries": {label: str(binary) for label, binary in binaries.items()},
        "workloads": {},
    }
    order = random.Random(20261024)
    for name in ["stable", "interleaved", "churn", "narrow-churn", "wide", "clpp"]:
        source = args.output / f"{name}.jsonl"
        rows = min(args.rows, 2000) if name == "wide" else args.rows
        fixture(source, name, rows)
        prefix = ["--experimental"] if name == "clpp" else []
        options = ["--structurize-arrays", "--timestamp-key", "ts"]
        if prefix:
            options += ["--parsing-specification", str(spec)]
        for storage, flags in [
            ("packed", []),
            ("adaptive", ["--adaptive-dictionary-columns", "--separate-columns-min-size", "1"]),
        ]:
            root = args.output / f"{name}-{storage}"
            root.mkdir()
            archives = {label: root / label for label in binaries}
            for label, binary in binaries.items():
                run(binary, [*prefix, "c", *options, *flags, archives[label], source])
            verify(binaries, prefix, archives, source, root)
            samples: dict[str, list[float]] = {label: [] for label in binaries}
            expected = payload_hashes(archives["baseline"])
            for repeat in range(args.repeats):
                labels = list(binaries)
                order.shuffle(labels)
                for label in labels:
                    archive = root / f"timed-{repeat}-{label}"
                    _, elapsed = run(
                        binaries[label], [*prefix, "c", *options, *flags, archive, source]
                    )
                    samples[label].append(elapsed)
                    assert payload_hashes(archive) == expected, (name, storage, label, repeat)
                    shutil.rmtree(archive)
            medians = {label: statistics.median(values) for label, values in samples.items()}
            size = sum(
                path.stat().st_size for path in archives["baseline"].rglob("*") if path.is_file()
            )
            results["workloads"][f"{name}-{storage}"] = {
                "rows": rows,
                "input_bytes": source.stat().st_size,
                "archive_bytes": size,
                "payload_hashes": expected,
                "samples_ms": samples,
                "median_ms": medians,
                "speedup": medians["baseline"] / medians["candidate"],
            }
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
            print(
                f"{name}-{storage}: {medians['baseline']:.1f} -> {medians['candidate']:.1f} ms",
                flush=True,
            )

    # Repeated archive splits must reset both node IDs and cached ERT IDs.
    root = args.output / "splits"
    root.mkdir()
    source = root / "input.jsonl"
    fixture(source, "interleaved", 200)
    archives = {label: root / label for label in binaries}
    for label, binary in binaries.items():
        run(binary, ["c", "--target-encoded-size", "4096", archives[label], source])
        assert len(list(archives[label].iterdir())) > 1
    verify(binaries, [], archives, source, root)
    print("Archive split equivalence passed.", flush=True)


if __name__ == "__main__":
    main()
