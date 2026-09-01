# This isolated, non-installed pytest package intentionally uses package-relative imports.
# ruff: noqa: TID252

"""Host-safe unit tests for CLP-S observation normalization helpers."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

from .harness import (
    capture_tree,
    CommandObservation,
    mask_json_fields,
    MAX_CAPTURED_TEXT_BYTES,
    OutputNormalizer,
    semantic_comparison_view,
    tree_semantic_view,
    tree_structure_view,
)


def test_normalizer_replaces_paths_timestamps_uuids_and_canonicalizes_json(tmp_path: Path) -> None:
    """Normalize only declared nondeterministic tokens and JSON formatting."""
    binary = tmp_path / "bin" / "clp-s"
    work_dir = tmp_path / "work"
    normalizer = OutputNormalizer(binary, work_dir)
    archive_id = "123e4567-e89b-42d3-a456-426614174000"

    log_value = f"2026-08-30T03:10:11.123-0400 [error] {binary} failed under {work_dir}\n"
    assert normalizer.normalize_text(log_value) == (
        "<LOG_TIMESTAMP> [error] <CLP-S> failed under <WORKDIR>\n"
    )
    json_value = f'{{"z":1,"id":"{archive_id}","a":2}}\n'
    assert normalizer.normalize_text(json_value) == ('{"a":2,"id":"<UUID:1>","z":1}\n')
    assert normalizer.normalize_token(f"{archive_id}_0_4.jsonl") == "<UUID:1>_0_4.jsonl"


def test_semantic_view_keeps_data_output_but_classifies_exit_and_omits_diagnostics() -> None:
    """Treat exact nonzero codes and diagnostic wording as human-only compatibility."""
    observation = CommandObservation(
        case="invalid-command",
        target="cpp",
        raw_argv=("/bin/clp-s", "invalid"),
        normalized_argv=("<CLP-S>", "invalid"),
        returncode=23,
        timed_out=False,
        raw_stdout='{"data":1}\n',
        raw_stderr="implementation-specific failure\n",
        stdout='{"data":1}\n',
        stderr="implementation-specific failure\n",
        environment={"controlled": {}, "additional_names": []},
        output_trees={},
    )

    view = semantic_comparison_view(observation)

    assert view["exit_status"] == "failure"
    assert view["stdout"] == '{"data":1}\n'
    assert "returncode" not in view
    assert "stderr" not in view


def test_mask_json_fields_preserves_other_fields() -> None:
    """Mask explicitly ignored metrics without discarding neighboring fields."""
    value = '{"id":"<UUID:1>","size":91,"uncompressed_size":120}\n'
    assert mask_json_fields(value, {"size"}) == (
        '{"id":"<UUID:1>","size":"<IGNORED>","uncompressed_size":120}\n'
    )


def test_tree_views_select_structure_or_semantic_content() -> None:
    """Select layout-only and semantic comparison views independently."""
    tree = {
        "exists": True,
        "entries": [
            {"path": ".", "kind": "directory", "mode": "0755"},
            {
                "path": "records.jsonl",
                "kind": "file",
                "mode": "0644",
                "size": 10,
                "raw_sha256": "abc",
                "text": '{"b":2,"a":1}\n',
                "normalized_text": '{"a":1,"b":2}\n',
            },
        ],
    }
    assert tree_structure_view(tree) == {
        "exists": True,
        "entries": [
            {"path": ".", "kind": "directory"},
            {"path": "records.jsonl", "kind": "file"},
        ],
    }
    assert tree_semantic_view(tree) == {
        "exists": True,
        "entries": [
            {"path": ".", "kind": "directory"},
            {
                "path": "records.jsonl",
                "kind": "file",
                "normalized_text": '{"a":1,"b":2}\n',
            },
        ],
    }


def test_large_file_is_hashed_without_reading_it_all_at_once(tmp_path: Path) -> None:
    """Stream large files through hashing without loading their contents."""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    large_file = work_dir / "archive-section"
    with large_file.open("wb") as file:
        file.seek(MAX_CAPTURED_TEXT_BYTES)
        file.write(b"x")

    normalizer = OutputNormalizer(tmp_path / "clp-s", work_dir)
    with patch.object(Path, "read_bytes", side_effect=AssertionError("must stream large files")):
        tree = capture_tree(large_file, normalizer)

    entry = tree["entries"][0]
    assert entry["size"] == MAX_CAPTURED_TEXT_BYTES + 1
    assert "raw_sha256" in entry
    assert "text" not in entry
