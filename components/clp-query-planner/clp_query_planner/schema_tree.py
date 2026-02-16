"""Parse schema tree JSON from `clp-s i` and build a field catalog."""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass, field


_FIELD_NAME_KEYWORDS: dict[str, str] = {
    "message": "log message text body content",
    "msg": "log message text body content",
    "body": "log message text body content",
    "error": "error failure exception fault crash",
    "err": "error failure exception fault crash",
    "exception": "error failure exception traceback stack",
    "status": "status code http response state",
    "status_code": "status code http response",
    "code": "status code error return",
    "level": "log level severity debug info warn error fatal",
    "severity": "log level severity debug info warn error fatal",
    "service": "service application component microservice",
    "host": "host hostname server machine ip address",
    "hostname": "host hostname server machine ip address",
    "ip": "ip address network source destination",
    "url": "url path endpoint uri route request",
    "path": "url path file route endpoint",
    "method": "http method get post put delete request",
    "user": "user username account identity principal",
    "username": "user username account identity login",
    "auth": "authentication authorization login credential token",
    "token": "authentication token jwt session credential",
    "timestamp": "timestamp time date when occurred",
    "time": "timestamp time date when occurred",
    "duration": "duration elapsed time latency response",
    "latency": "duration elapsed time latency response slow",
    "request": "request http incoming call invocation",
    "response": "response http outgoing reply result",
    "pid": "process id pid worker thread",
    "thread": "thread id worker process concurrent",
    "trace": "trace id distributed tracing span correlation",
    "span": "span id distributed tracing trace",
}

_NODE_TYPE_DESCRIPTIONS: dict[str, str] = {
    "ClpString": "text string field with mixed static and variable content",
    "VarString": "short variable string field like identifiers or enum values",
    "Integer": "integer numeric field",
    "Float": "floating point numeric field",
    "DeltaInteger": "delta-encoded integer numeric field",
    "FormattedFloat": "formatted floating point numeric field",
    "DictionaryFloat": "dictionary-encoded floating point numeric field",
    "Boolean": "boolean true/false field",
    "DateString": "date/time string field",
}


@dataclass
class FieldInfo:
    """A searchable leaf field from the schema tree."""

    node_id: int
    full_path: str  # e.g. "auth.status"
    node_type: str  # e.g. "ClpString", "VarString", "Integer"
    parent_context: str  # e.g. "auth"

    def rich_description(self) -> str:
        """Format for embedding — includes hierarchy context and keyword expansion."""
        type_desc = _NODE_TYPE_DESCRIPTIONS.get(self.node_type, self.node_type)

        # Split field name into parts for keyword expansion
        leaf_name = self.full_path.rsplit(".", 1)[-1]
        name_parts = leaf_name.replace("_", " ").replace("-", " ").split()

        keywords: list[str] = []
        for part in name_parts:
            kw = _FIELD_NAME_KEYWORDS.get(part.lower())
            if kw:
                keywords.append(kw)
        # Also try the full leaf name (e.g. "status_code")
        kw = _FIELD_NAME_KEYWORDS.get(leaf_name.lower())
        if kw and kw not in keywords:
            keywords.append(kw)

        parts = [f"{self.full_path} — {type_desc}"]
        if self.parent_context:
            parts.append(f", under {self.parent_context}")
        if keywords:
            parts.append(f". Related: {'; '.join(keywords)}")
        return "".join(parts)


# Node types that are containers, not searchable leaf fields
_CONTAINER_TYPES = {"Object", "Metadata", "StructuredArray", "UnstructuredArray"}

# Skip the metadata namespace (internal implementation nodes like log_event_idx)
_SKIP_KEY_NAMES = {""}


def parse_schema_json(schema_json: dict) -> list[FieldInfo]:
    """Build a field catalog from the JSON output of `clp-s i`.

    Only leaf nodes in the default (non-metadata) namespace are included.
    """
    nodes = schema_json["nodes"]
    nodes_by_id: dict[int, dict] = {n["id"]: n for n in nodes}

    # Build parent path lookup
    def _build_path(node_id: int) -> str:
        parts: list[str] = []
        nid = node_id
        while nid >= 0:
            node = nodes_by_id[nid]
            key = node["key_name"]
            if key:
                parts.append(key)
            nid = node["parent_id"]
        parts.reverse()
        return ".".join(parts)

    # Find metadata subtree root(s) to exclude
    metadata_roots: set[int] = set()
    for node in nodes:
        if node["parent_id"] == -1 and node["type"] == "Metadata":
            metadata_roots.add(node["id"])

    def _is_under_metadata(node_id: int) -> bool:
        nid = node_id
        while nid >= 0:
            if nid in metadata_roots:
                return True
            nid = nodes_by_id[nid]["parent_id"]
        return False

    fields: list[FieldInfo] = []
    for node in nodes:
        # Skip containers — only leaf nodes are searchable
        if node["type"] in _CONTAINER_TYPES:
            continue

        # Skip metadata subtree
        if _is_under_metadata(node["id"]):
            continue

        full_path = _build_path(node["id"])
        if not full_path:
            continue

        # Parent context
        parent_context = ""
        if node["parent_id"] >= 0:
            parent = nodes_by_id[node["parent_id"]]
            parent_context = parent["key_name"]

        fields.append(
            FieldInfo(
                node_id=node["id"],
                full_path=full_path,
                node_type=node["type"],
                parent_context=parent_context,
            )
        )

    return fields


def load_schema_from_archive(
    archive_path: str,
    clp_s_bin: str = "clp-s",
    auth: str | None = None,
) -> dict:
    """Run `clp-s i` and return the parsed JSON."""
    cmd = [clp_s_bin, "i", archive_path]
    if auth:
        cmd.extend(["--auth", auth])

    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return json.loads(result.stdout)
