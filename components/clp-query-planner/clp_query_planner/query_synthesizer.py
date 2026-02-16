"""Build KQL from resolved fields, routing by NodeType."""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Optional

from .field_resolver import ResolvedField
from .nl_parser import FieldValuePair, TemporalConstraint

_NUMBER_PATTERN = re.compile(r"-?\d+(?:\.\d+)?")


@dataclass
class SynthesizedQuery:
    """The output of query synthesis."""

    kql: str
    tge: Optional[int] = None  # --tge flag value (epoch ms)
    tle: Optional[int] = None  # --tle flag value (epoch ms)


def _escape_kql_value(value: str) -> str:
    """Escape special characters in a KQL string value."""
    return value.replace("\\", "\\\\").replace('"', '\\"')


def _synthesize_for_field(
    field_path: str,
    node_type: str,
    phrase: str,
    semantic_top_k: int,
) -> list[str]:
    """Generate KQL clause(s) for a single field + phrase combination."""
    escaped = _escape_kql_value(phrase)

    if node_type == "ClpString":
        return [f'{field_path}: semantic("{escaped}", {semantic_top_k})']
    elif node_type == "VarString":
        return [f'{field_path}: "*{escaped}*"']
    elif node_type in (
        "Integer", "Float", "DeltaInteger", "FormattedFloat", "DictionaryFloat",
    ):
        try:
            float(phrase)
            return [f"{field_path}: {phrase}"]
        except ValueError:
            matches = _NUMBER_PATTERN.findall(phrase)
            if matches:
                return [f"{field_path}: {n}" for n in matches]
            return []
    elif node_type == "Boolean":
        lower = phrase.lower().strip()
        if lower in ("true", "false"):
            return [f"{field_path}: {lower}"]
    # DateString, Timestamp — handled via --tge/--tle, not KQL
    return []


def synthesize_query(
    value_phrases: list[str],
    resolved_fields: list[ResolvedField],
    temporal: Optional[TemporalConstraint] = None,
    semantic_top_k: int = 5,
    field_value_pairs: Optional[list[FieldValuePair]] = None,
    all_fields: Optional[list] = None,
) -> SynthesizedQuery:
    """Generate a KQL query from resolved fields and value phrases.

    Routes each field by its NodeType:
      ClpString    -> semantic("query", K)
      VarString    -> "*value*"
      Integer/Float/DeltaInteger/... -> numeric match (if value is numeric)
      Boolean      -> true/false
      DateString   -> handled via --tge/--tle flags

    field_value_pairs are matched directly to schema fields by name.
    """
    _NUMERIC_TYPES = frozenset({
        "Integer", "Float", "DeltaInteger", "FormattedFloat", "DictionaryFloat",
    })
    _TEXT_TYPES = frozenset({"ClpString", "VarString"})

    fv_clauses: list[str] = []    # from explicit field-value pairs
    text_clauses: list[str] = []  # from text fields (semantic / wildcard)
    num_clauses: list[str] = []   # from numeric fields (exact match)

    # Handle explicit field-value pairs first (e.g. status_code:504)
    if field_value_pairs and all_fields:
        field_lookup = {f.full_path: f for f in all_fields}
        # Also index by leaf name for partial matches
        leaf_lookup: dict[str, list] = {}
        for f in all_fields:
            leaf = f.full_path.rsplit(".", 1)[-1]
            leaf_lookup.setdefault(leaf, []).append(f)

        for pair in field_value_pairs:
            # Try exact match first, then leaf name match
            matched = []
            if pair.field_hint in field_lookup:
                matched = [field_lookup[pair.field_hint]]
            elif pair.field_hint in leaf_lookup:
                matched = leaf_lookup[pair.field_hint]

            for field_info in matched:
                fv_clauses.extend(_synthesize_for_field(
                    field_info.full_path,
                    field_info.node_type,
                    pair.value,
                    semantic_top_k,
                ))

    # Handle value phrases matched against embedding-resolved fields
    for phrase in value_phrases:
        for resolved in resolved_fields:
            parts = _synthesize_for_field(
                resolved.field.full_path,
                resolved.field.node_type,
                phrase,
                semantic_top_k,
            )
            if resolved.field.node_type in _TEXT_TYPES:
                text_clauses.extend(parts)
            elif resolved.field.node_type in _NUMERIC_TYPES:
                num_clauses.extend(parts)
            else:
                text_clauses.extend(parts)

    # Join: OR within each group, AND between groups.
    # e.g. "auth failures with status code 403" →
    #   (message: semantic(...) OR level: "*...*") AND status_code: 403
    groups = []
    for group in [fv_clauses, text_clauses, num_clauses]:
        if group:
            inner = " OR ".join(group)
            groups.append(f"({inner})" if len(group) > 1 and len(groups) > 0 else inner)
    # Re-wrap: if multiple groups, wrap any multi-clause group in parens
    if len(groups) > 1:
        wrapped = []
        for group in [fv_clauses, text_clauses, num_clauses]:
            if group:
                inner = " OR ".join(group)
                wrapped.append(f"({inner})" if len(group) > 1 else inner)
        kql = " AND ".join(wrapped)
    elif groups:
        kql = groups[0]
    else:
        kql = "*"

    tge = temporal.begin_ts if temporal else None
    tle = temporal.end_ts if temporal else None

    return SynthesizedQuery(kql=kql, tge=tge, tle=tle)
