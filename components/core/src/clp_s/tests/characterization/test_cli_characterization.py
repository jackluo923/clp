# This isolated, non-installed pytest package intentionally uses package-relative imports.
# ruff: noqa: TID252

"""Black-box CLP-S command-line characterization and differential tests."""

from __future__ import annotations

import json
import re
import shutil
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Final, TYPE_CHECKING

import pytest

from .harness import (
    assert_views_equal,
    BinaryTarget,
    CharacterizationHarness,
    CommandObservation,
    mask_json_fields,
    semantic_comparison_view,
    tree_semantic_view,
    tree_structure_view,
    write_observation,
)

if TYPE_CHECKING:
    from .conftest import CharacterizationConfig


FIXTURE_PATH: Final = Path(__file__).parent / "fixtures" / "basic.jsonl"
SEARCH_FIXTURE_PATH: Final = Path(__file__).parent / "fixtures" / "search-semantics.jsonl"
IMPLEMENTATION_PAIR_COUNT: Final = 2
WORKFLOW_COMMAND_COUNT: Final = 4
MATCHING_ERROR_COUNT: Final = 2
_LATENCY_TOKEN_RE: Final = re.compile(
    r'"latency"\s*:\s*(-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?)'
)


class UnknownArchiveLayoutError(ValueError):
    """A workflow requested a layout outside the characterization matrix."""


class MissingLatencyTokenError(AssertionError):
    """Extracted JSON lost the fixture's original latency number token."""


@dataclass(frozen=True)
class CliCase:
    """One side-effect-free CLI contract probe."""

    name: str
    args: tuple[str, ...]
    returncode: int
    stderr_contains: tuple[str, ...]


@dataclass(frozen=True)
class SearchCase:
    """One deterministic query against a prepared characterization archive."""

    name: str
    archive: str
    query: str
    expected_ids: tuple[int, ...] | None = None
    prefix_args: tuple[str, ...] = ()
    suffix_args: tuple[str, ...] = ()
    returncode: int = 0
    stderr_contains: tuple[str, ...] = ()
    expected_records: tuple[Mapping[str, Any], ...] | None = None
    expected_key_orders: tuple[tuple[str, ...], ...] | None = None


CLI_CASES: Final = (
    CliCase("no-arguments", (), 1, ("Usage:",)),
    CliCase(
        "general-help",
        ("--help",),
        0,
        ("COMMAND is one of:", "c - compress", "x - decompress", "s - search"),
    ),
    CliCase("compression-help", ("c", "--help"), 0, ("Usage:", "Compression options:")),
    CliCase("extraction-help", ("x", "--help"), 0, ("Usage:", "Decompression Options:")),
    CliCase("search-help", ("s", "--help"), 0, ("Usage:", "Aggregation Controls:")),
    CliCase("unknown-command", ("z",), 1, ("Unknown action 'z'", "Usage:")),
    CliCase("compression-missing-output", ("c",), 1, ("No archives directory specified.",)),
    CliCase("extraction-missing-archive", ("x",), 1, ("No archive path specified",)),
    CliCase(
        "search-missing-archive",
        ("s",),
        1,
        ('missing required positional argument "ARCHIVES_DIR"',),
    ),
)


SEARCH_CASES: Final = (
    # AND and OR intentionally have equal precedence and associate from the left. Conventional
    # AND-before-OR parsing would also return id 0 for this expression.
    SearchCase(
        "mixed-and-or-left-associative",
        "structured",
        "logic_a:true OR logic_b:true AND logic_c:true",
        (1, 3),
    ),
    # Quoting permits KQL punctuation/spaces but does not force string typing. Equality can still
    # match the original numeric/boolean/null lexeme when it was archived as a VarString.
    SearchCase("quoted-number-range", "structured", 'typed_number > "6.5"', (0,)),
    SearchCase("quoted-number-equality", "structured", 'typed_number: "7"', (0, 1)),
    SearchCase("quoted-boolean", "structured", 'typed_bool: "true"', (0, 1)),
    SearchCase("quoted-null", "structured", 'typed_null: "null"', (0, 1)),
    SearchCase("bare-value-default-namespace", "structured", "needle", (0,)),
    SearchCase("unescaped-value-wildcard", "structured", "wild:a*b", (0, 1)),
    SearchCase("escaped-value-wildcard", "structured", r"wild:a\*b", (0,)),
    SearchCase("nested-path", "structured", "dot.key:nested", (1,)),
    SearchCase("escaped-dot-in-path", "structured", r"dot\.key:flat", (0,)),
    SearchCase(
        "range-metadata-namespace",
        "structured",
        "$_filename:*",
        (0, 1, 3, 2, 11, 13, 10, 12),
    ),
    SearchCase("escaped-metadata-prefix", "structured", r"\$_filename:event-file", (0,)),
    # A path-token '*' matches zero or more hierarchy levels; escaping it addresses a literal key.
    SearchCase("wildcard-path-token", "structured", "path.*.leaf:*", (0, 3, 2)),
    SearchCase(
        "escaped-wildcard-path-token",
        "structured",
        r"path.\*.leaf:literal-star",
        (0,),
    ),
    SearchCase("exists", "structured", "feature:true AND optional:*", (0, 1, 3)),
    SearchCase("nexists", "structured", "feature:true AND NOT optional:*", (2,)),
    # A negated value predicate requires the field to exist; id 2 is deliberately absent.
    SearchCase(
        "negated-value-does-not-match-missing",
        "structured",
        "feature:true AND NOT optional:yes",
        (1, 3),
    ),
    # The two predicates may be satisfied by different objects in one structured array.
    SearchCase(
        "structured-array-independent-existentials",
        "structured",
        "items.x:1 AND items.y:2",
        (0, 1),
    ),
    # Search walks physical schema-table order, not input/log_event_idx order.
    SearchCase("physical-schema-table-order", "structured", "order_group:*", (11, 13, 10, 12)),
    SearchCase(
        "inclusive-timestamp-bounds",
        "structured",
        "order_group:*",
        (11, 12),
        prefix_args=("--tge", "6000000", "--tle", "7000000"),
    ),
    SearchCase("case-sensitive-ascii", "structured", "case_ascii:mixed", ()),
    SearchCase(
        "ignore-case-ascii",
        "structured",
        "case_ascii:mixed",
        (0,),
        prefix_args=("--ignore-case",),
    ),
    # ASCII letters fold around an unchanged non-ASCII codepoint, but É and é do not fold.
    SearchCase(
        "ignore-case-ascii-with-unicode",
        "structured",
        "case_unicode:ÉCOLE",
        (0,),
        prefix_args=("--ignore-case",),
    ),
    SearchCase(
        "ignore-case-does-not-fold-unicode-codepoint",
        "structured",
        "case_unicode:éCOLE",
        (),
        prefix_args=("--ignore-case",),
    ),
    # C++ emits no aggregation document when an archive has zero matches.
    SearchCase("zero-match-count", "structured", "id:9999", (), prefix_args=("--count",)),
    SearchCase("unstructured-array-equality", "unstructured", "arr.n:5", (1, 3)),
    # Suspected C++ bug/policy decision: every non-EQ numeric array operation currently behaves as
    # inequality. The four cases deliberately have the same result, including values on both sides
    # of the operand. Do not silently turn these into mathematically correct ranges in a port.
    SearchCase("unstructured-array-greater-than", "unstructured", "arr.n > 5", (0, 2)),
    SearchCase("unstructured-array-less-than", "unstructured", "arr.n < 5", (0, 2)),
    SearchCase("unstructured-array-greater-equal", "unstructured", "arr.n >= 5", (0, 2)),
    SearchCase("unstructured-array-less-equal", "unstructured", "arr.n <= 5", (0, 2)),
    # A second suspected C++ bug/policy decision: unresolved NEXISTS inside an unstructured array
    # matches no schema/row, even though every feature record lacks arr.missing.
    SearchCase(
        "unstructured-array-missing-path-nexists",
        "unstructured",
        "feature:true AND NOT arr.missing:*",
        (),
    ),
    SearchCase(
        "timestamp-bound-without-authoritative-column",
        "no-authoritative-timestamp",
        "feature:true",
        prefix_args=("--tge", "0"),
        returncode=1,
        stderr_contains=("but no authoritative timestamp column was found for this archive",),
    ),
    # Membership is resolved from the requested projection, but output follows schema order and a
    # missing field is silently omitted.
    SearchCase(
        "projection-schema-order-and-missing",
        "structured",
        "id:0",
        suffix_args=("--projection", "project_z", "id", "project_a", "missing"),
        expected_records=({"id": 0, "project_z": "z", "project_a": "a"},),
        expected_key_orders=(("id", "project_z", "project_a"),),
    ),
    SearchCase(
        "projection-only-missing",
        "structured",
        "id:0",
        suffix_args=("--projection", "missing"),
        expected_records=({},),
        expected_key_orders=((),),
    ),
    SearchCase(
        "projection-duplicate",
        "structured",
        "id:0",
        suffix_args=("--projection", "id", "id"),
        returncode=1,
    ),
)

SEARCH_ARCHIVE_NAMES: Final = (
    "structured",
    "unstructured",
    "no-authoritative-timestamp",
)


@pytest.mark.parametrize("case", CLI_CASES, ids=lambda case: case.name)
def test_cli_contract(
    case: CliCase,
    characterization_config: CharacterizationConfig,
    tmp_path: Path,
) -> None:
    """Capture reference behavior and compare a candidate when supplied."""
    observations = [
        _run_cli_case(target, case, characterization_config, tmp_path)
        for target in characterization_config.targets
    ]
    reference = observations[0]
    assert not reference.timed_out
    assert reference.returncode == case.returncode
    assert reference.stdout == ""
    for expected_text in case.stderr_contains:
        assert expected_text in reference.stderr

    if len(observations) == IMPLEMENTATION_PAIR_COUNT:
        candidate = observations[1]
        _assert_candidate_human_cli_semantics(case, candidate)
        assert_views_equal(
            _human_cli_comparison_view(reference),
            _human_cli_comparison_view(candidate),
        )


@pytest.mark.parametrize("archive_layout", ["directory", "single-file"])
def test_local_round_trip_search_and_count(
    archive_layout: str,
    characterization_config: CharacterizationConfig,
    tmp_path: Path,
) -> None:
    """Exercise c/x/s without any CLP, GLT, reducer, database, or network service."""
    workflows = [
        _run_workflow(target, archive_layout, characterization_config, tmp_path)
        for target in characterization_config.targets
    ]
    reference_observations = workflows[0]
    _assert_workflow_reference(reference_observations)

    if len(workflows) == IMPLEMENTATION_PAIR_COUNT:
        assert_views_equal(
            _workflow_comparison_view(reference_observations),
            _workflow_comparison_view(workflows[1]),
        )


def test_search_semantics(
    characterization_config: CharacterizationConfig,
    tmp_path: Path,
) -> None:
    """Pin schema-aware KQL, projection, ordering, timestamp, and array behavior."""
    workflows = [
        _run_search_semantics_workflow(target, characterization_config, tmp_path)
        for target in characterization_config.targets
    ]
    _assert_search_semantics_reference(workflows[0])

    if len(workflows) == IMPLEMENTATION_PAIR_COUNT:
        assert_views_equal(
            _search_semantics_comparison_view(workflows[0]),
            _search_semantics_comparison_view(workflows[1]),
        )


def _run_cli_case(
    target: BinaryTarget,
    case: CliCase,
    config: CharacterizationConfig,
    temporary_root: Path,
) -> CommandObservation:
    work_dir = temporary_root / target.name / case.name
    harness = CharacterizationHarness(
        target,
        work_dir,
        timeout_seconds=config.timeout_seconds,
    )
    observation = harness.run(case.name, case.args)
    write_observation(
        config.observations_dir / "cli" / target.name / f"{case.name}.json",
        observation.to_dict(),
    )
    return observation


def _assert_candidate_human_cli_semantics(
    case: CliCase,
    observation: CommandObservation,
) -> None:
    """Require equivalent CLI outcomes without pinning human-only text or help's stream."""
    assert not observation.timed_out, case.name
    assert (observation.returncode == 0) == (case.returncode == 0), case.name
    if case.returncode == 0:
        assert observation.stdout or observation.stderr, f"{case.name} emitted no help text"
        return
    assert observation.stdout == "", f"{case.name} wrote diagnostic text to stdout"
    assert observation.stderr, f"{case.name} emitted no failure diagnostic"


def _human_cli_comparison_view(observation: CommandObservation) -> Mapping[str, Any]:
    """Compare a side-effect-free human CLI probe while allowing help text on either stream."""
    view = semantic_comparison_view(observation)
    del view["stdout"]
    return view


def _run_workflow(
    target: BinaryTarget,
    archive_layout: str,
    config: CharacterizationConfig,
    temporary_root: Path,
) -> list[CommandObservation]:
    work_dir = temporary_root / target.name / f"local-workflow-{archive_layout}"
    input_dir = work_dir / "input"
    input_dir.mkdir(parents=True)
    input_path = input_dir / FIXTURE_PATH.name
    shutil.copyfile(FIXTURE_PATH, input_path)

    archives_dir = work_dir / "archives"
    extraction_dir = work_dir / "extracted"
    harness = CharacterizationHarness(
        target,
        work_dir,
        timeout_seconds=config.timeout_seconds,
    )
    compression_args: list[str | Path] = [
        "c",
        "--timestamp-key",
        "ts",
        "--structurize-arrays",
        "--print-archive-stats",
        "--remove-path-prefix",
        input_dir,
    ]
    if archive_layout == "single-file":
        compression_args.append("--single-file-archive")
    elif archive_layout != "directory":
        message = f"Unknown archive layout: {archive_layout}"
        raise UnknownArchiveLayoutError(message)
    compression_args.extend((archives_dir, input_path))
    observations = [
        harness.run(
            f"compress-local-json-{archive_layout}",
            compression_args,
            capture_roots={"archives": archives_dir},
        )
    ]

    if observations[-1].returncode == 0:
        observations.append(
            harness.run(
                "search-errors",
                ("s", archives_dir, "level: ERROR"),
                sort_stdout_json_lines=True,
            )
        )
        observations.append(
            harness.run(
                "count-errors",
                ("s", "--count", archives_dir, "level: ERROR"),
                sort_stdout_json_lines=True,
            )
        )
        observations.append(
            harness.run(
                "extract-ordered",
                (
                    "x",
                    "--ordered",
                    "--target-ordered-chunk-size",
                    "0",
                    archives_dir,
                    extraction_dir,
                ),
                capture_roots={"extracted": extraction_dir},
            )
        )

    write_observation(
        config.observations_dir / "workflows" / f"{target.name}-local-{archive_layout}.json",
        [observation.to_dict() for observation in observations],
    )
    return observations


def _run_search_semantics_workflow(
    target: BinaryTarget,
    config: CharacterizationConfig,
    temporary_root: Path,
) -> dict[str, CommandObservation]:
    work_dir = temporary_root / target.name / "search-semantics"
    input_dir = work_dir / "input"
    input_dir.mkdir(parents=True)
    input_path = input_dir / SEARCH_FIXTURE_PATH.name
    shutil.copyfile(SEARCH_FIXTURE_PATH, input_path)

    archive_paths = {name: work_dir / name for name in SEARCH_ARCHIVE_NAMES}
    harness = CharacterizationHarness(
        target,
        work_dir,
        timeout_seconds=config.timeout_seconds,
    )
    observations: dict[str, CommandObservation] = {}
    compression_options = {
        "structured": ("--timestamp-key", "ts", "--structurize-arrays"),
        "unstructured": ("--timestamp-key", "ts"),
        "no-authoritative-timestamp": ("--structurize-arrays",),
    }
    for archive_name in SEARCH_ARCHIVE_NAMES:
        case_name = f"compress-search-{archive_name}"
        archive_path = archive_paths[archive_name]
        observations[case_name] = harness.run(
            case_name,
            (
                "c",
                *compression_options[archive_name],
                "--remove-path-prefix",
                input_dir,
                archive_path,
                input_path,
            ),
            capture_roots={"archive": archive_path},
        )

    if all(
        observations[f"compress-search-{archive_name}"].returncode == 0
        for archive_name in SEARCH_ARCHIVE_NAMES
    ):
        for case in SEARCH_CASES:
            observations[case.name] = harness.run(
                case.name,
                (
                    "s",
                    *case.prefix_args,
                    archive_paths[case.archive],
                    case.query,
                    *case.suffix_args,
                ),
            )

    write_observation(
        config.observations_dir / "search-semantics" / f"{target.name}.json",
        {name: observation.to_dict() for name, observation in observations.items()},
    )
    return observations


def _assert_search_semantics_reference(
    observations: Mapping[str, CommandObservation],
) -> None:
    for archive_name in SEARCH_ARCHIVE_NAMES:
        case_name = f"compress-search-{archive_name}"
        observation = observations[case_name]
        assert not observation.timed_out, case_name
        assert observation.returncode == 0, observation.raw_stderr
        archive_tree = observation.output_trees["archive"]
        assert archive_tree["exists"], case_name
        assert archive_tree["entries"], case_name

    for case in SEARCH_CASES:
        observation = observations[case.name]
        assert not observation.timed_out, case.name
        assert observation.returncode == case.returncode, observation.raw_stderr
        for expected_text in case.stderr_contains:
            assert expected_text in observation.stderr

        records = _json_lines(observation.raw_stdout)
        if case.expected_records is not None:
            assert records == list(case.expected_records), case.name
        elif case.expected_ids is not None:
            assert [record["id"] for record in records] == list(case.expected_ids), case.name
        else:
            assert records == [], case.name

        if case.expected_key_orders is not None:
            assert tuple(tuple(record) for record in records) == case.expected_key_orders, case.name


def _search_semantics_comparison_view(
    observations: Mapping[str, CommandObservation],
) -> Mapping[str, Any]:
    views: dict[str, dict[str, Any]] = {}
    for name, observation in observations.items():
        view = semantic_comparison_view(observation)
        if name.startswith("compress-search-"):
            view["output_trees"]["archive"] = tree_structure_view(view["output_trees"]["archive"])
        views[name] = view

    return {
        "observations": views,
        # Stream normalization intentionally canonicalizes object keys. Retain projection order as
        # a separate semantic observation so a differential candidate cannot reorder selected keys.
        "projection_key_orders": {
            case.name: [list(record) for record in _json_lines(observations[case.name].raw_stdout)]
            for case in SEARCH_CASES
            if case.expected_key_orders is not None
        },
    }


def _json_lines(text: str) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line in text.splitlines():
        value = json.loads(line)
        assert isinstance(value, dict)
        records.append(value)
    return records


def _assert_workflow_reference(observations: Sequence[CommandObservation]) -> None:
    assert len(observations) == WORKFLOW_COMMAND_COUNT, (
        "Compression failed before the workflow could complete"
    )
    for observation in observations:
        assert not observation.timed_out, observation.case
        assert observation.returncode == 0, f"{observation.case} failed:\n{observation.raw_stderr}"

    search_records = [json.loads(line) for line in observations[1].stdout.splitlines()]
    assert len(search_records) == MATCHING_ERROR_COUNT
    assert all(record["level"] == "ERROR" for record in search_records)

    count_documents = [json.loads(line) for line in observations[2].stdout.splitlines()]
    assert len(count_documents) == 1
    assert count_documents[0]["count"] == MATCHING_ERROR_COUNT

    extracted_tree = observations[3].output_trees["extracted"]
    extracted_records = _json_records_from_tree(extracted_tree)
    # Ordered extraction follows the archive's log_event_idx, not the authoritative timestamp.
    assert [record["ts"] for record in extracted_records] == [3000, 1000, 2000, 4000]
    assert _latency_lexemes_from_tree(extracted_tree) == {
        1000: "0.0",
        2000: "8.2500",
        3000: "12.50",
        4000: "13",
    }


def _workflow_comparison_view(
    observations: Sequence[CommandObservation],
) -> Mapping[str, Any]:
    if len(observations) != WORKFLOW_COMMAND_COUNT:
        return {
            "incomplete": [semantic_comparison_view(observation) for observation in observations]
        }

    compression = semantic_comparison_view(observations[0])
    compression["stdout"] = mask_json_fields(compression["stdout"], {"size"})
    compression["output_trees"]["archives"] = tree_structure_view(
        compression["output_trees"]["archives"]
    )

    extraction = semantic_comparison_view(observations[3])
    extraction["output_trees"]["extracted"] = tree_semantic_view(
        extraction["output_trees"]["extracted"]
    )
    return {
        "compression": compression,
        "search": semantic_comparison_view(observations[1]),
        "count": semantic_comparison_view(observations[2]),
        "extraction": extraction,
        "extracted_latency_lexemes": _latency_lexemes_from_tree(
            observations[3].output_trees["extracted"]
        ),
    }


def _json_records_from_tree(tree: Mapping[str, Any]) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for entry in tree["entries"]:
        normalized_text = entry.get("normalized_text")
        if entry["kind"] != "file" or normalized_text is None:
            continue
        records.extend(json.loads(line) for line in normalized_text.splitlines() if line)
    return records


def _latency_lexemes_from_tree(tree: Mapping[str, Any]) -> dict[int, str]:
    lexemes: dict[int, str] = {}
    for entry in tree["entries"]:
        text = entry.get("text")
        if entry["kind"] != "file" or text is None:
            continue
        for line in text.splitlines():
            record = json.loads(line)
            match = _LATENCY_TOKEN_RE.search(line)
            if match is None:
                message = f"Missing latency number token in extracted record: {line}"
                raise MissingLatencyTokenError(message)
            lexemes[int(record["ts"])] = match.group(1)
    return lexemes
