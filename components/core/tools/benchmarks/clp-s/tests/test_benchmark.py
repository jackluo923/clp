from __future__ import annotations

import copy
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path


BENCHMARK_DIR = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("clp_s_benchmark", BENCHMARK_DIR / "benchmark.py")
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("failed to load benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class ManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest_path = BENCHMARK_DIR / "manifest.smoke.json"
        self.manifest, _ = benchmark.load_manifest(self.manifest_path)

    def test_smoke_manifest_and_dataset_are_valid(self) -> None:
        datasets = benchmark.resolve_datasets(self.manifest, self.manifest_path)
        self.assertEqual(
            "b80ee9df3f376d36c1fb2da95aabbeb417aadc00fdf2e7a2df505bf1b090ed31",
            datasets["smoke-ndjson"].sha256,
        )
        self.assertGreater(datasets["smoke-ndjson"].size_bytes, 0)

    def test_schema_is_json(self) -> None:
        schema = json.loads((BENCHMARK_DIR / "manifest.schema.json").read_text(encoding="utf-8"))
        self.assertEqual("CLP-S benchmark manifest", schema["title"])

        build_schema = json.loads(
            (BENCHMARK_DIR / "build-metadata.schema.json").read_text(encoding="utf-8")
        )
        self.assertEqual("CLP-S benchmark build metadata", build_schema["title"])

    def test_unknown_exact_template_is_rejected(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["workloads"][0]["args"].append("{typo}")
        with self.assertRaisesRegex(benchmark.ConfigurationError, "unknown template"):
            benchmark.validate_manifest(manifest)

    def test_nonfinite_gate_is_rejected(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["defaults"]["gates"][0]["threshold"] = float("nan")
        with self.assertRaisesRegex(benchmark.ConfigurationError, "positive number"):
            benchmark.validate_manifest(manifest)

    def test_kql_braces_are_not_treated_as_templates(self) -> None:
        rendered = benchmark._render_templates(
            ["{ object: {nested: value} }", "{input}"],
            {key: f"value-{key}" for key in benchmark.TEMPLATE_KEYS},
        )
        self.assertEqual("{ object: {nested: value} }", rendered[0])
        self.assertEqual("value-input", rendered[1])

    def test_build_metadata_example_has_valid_shape(self) -> None:
        metadata, digest = benchmark.load_build_metadata(
            BENCHMARK_DIR / "build-metadata.example.json"
        )
        self.assertEqual(1, metadata["schema_version"])
        self.assertEqual(64, len(digest))

    def test_environment_values_are_allowlisted_or_redacted(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["environment"]["AWS_SECRET_ACCESS_KEY"] = "must-not-appear"
        redacted = benchmark._redacted_manifest(manifest)
        self.assertEqual("C", redacted["environment"]["LC_ALL"])
        self.assertEqual("<redacted>", redacted["environment"]["AWS_SECRET_ACCESS_KEY"])
        self.assertNotIn("must-not-appear", json.dumps(redacted))


class ChecksumTest(unittest.TestCase):
    def test_directory_digest_is_order_independent_and_content_sensitive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            (root / "z").write_bytes(b"last")
            (root / "a").write_bytes(b"first")
            initial = benchmark.tree_sha256(root)
            self.assertEqual(initial, benchmark.tree_sha256(root))
            (root / "a").write_bytes(b"changed")
            self.assertNotEqual(initial, benchmark.tree_sha256(root))

    def test_symlinks_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            target = root / "target"
            target.write_bytes(b"data")
            (root / "link").symlink_to(target)
            with self.assertRaisesRegex(benchmark.ConfigurationError, "symbolic links"):
                benchmark.tree_sha256(root)


class PairingAndGateTest(unittest.TestCase):
    def test_pair_order_is_reproducible_and_complete(self) -> None:
        first = benchmark.randomized_pair_orders(1729, "search", "trial", 20)
        second = benchmark.randomized_pair_orders(1729, "search", "trial", 20)
        self.assertEqual(first, second)
        self.assertTrue(all(sorted(pair) == ["cpp", "rust"] for pair in first))
        self.assertGreater(len({tuple(pair) for pair in first}), 1)

    def test_min_and_max_ratio_gates(self) -> None:
        gates = [
            {
                "metric": "throughput_per_second",
                "statistic": "median",
                "comparison": "min_ratio",
                "threshold": 0.95,
            },
            {
                "metric": "peak_rss_bytes",
                "statistic": "p95",
                "comparison": "max_ratio",
                "threshold": 1.05,
            },
        ]
        results = benchmark.evaluate_gates(
            gates,
            {
                "throughput_per_second": [0.96, 0.98, 1.01],
                "peak_rss_bytes": [1.01, 1.02, 1.03],
            },
        )
        self.assertTrue(all(result["passed"] for result in results))

    def test_missing_metric_fails_gate(self) -> None:
        results = benchmark.evaluate_gates(
            [
                {
                    "metric": "throughput_per_second",
                    "statistic": "median",
                    "comparison": "min_ratio",
                    "threshold": 0.95,
                }
            ],
            {},
        )
        self.assertFalse(results[0]["passed"])
        self.assertIsNone(results[0]["observed_ratio"])


class CommandMeasurementTest(unittest.TestCase):
    def test_wait4_metrics_and_timeout_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            completed = benchmark.run_command(
                ("/bin/true",),
                root,
                os.environ,
                root / "true.stdout",
                root / "true.stderr",
                1,
            )
            self.assertEqual(0, completed["exit_code"])
            self.assertFalse(completed["timed_out"])
            self.assertGreaterEqual(completed["peak_rss_bytes"], 0)

            timed_out = benchmark.run_command(
                ("/bin/sleep", "60"),
                root,
                os.environ,
                root / "sleep.stdout",
                root / "sleep.stderr",
                0.02,
            )
            self.assertTrue(timed_out["timed_out"])
            self.assertNotEqual(0, timed_out["exit_code"])


if __name__ == "__main__":
    unittest.main()
