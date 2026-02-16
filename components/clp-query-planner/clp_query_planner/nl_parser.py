"""Decompose natural language query into structured intent (regex/heuristic)."""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Optional


@dataclass
class TemporalConstraint:
    """Time range extracted from the NL query."""

    begin_ts: Optional[int] = None  # epoch ms
    end_ts: Optional[int] = None  # epoch ms


@dataclass
class FieldValuePair:
    """An explicit field-value pair extracted from the query."""

    field_hint: str  # e.g. "status_code"
    value: str  # e.g. "504"


@dataclass
class QueryIntent:
    """Structured decomposition of a natural language query."""

    value_phrases: list[str] = field(default_factory=list)
    temporal: Optional[TemporalConstraint] = None
    field_hints: list[str] = field(default_factory=list)
    negations: list[str] = field(default_factory=list)
    field_value_pairs: list[FieldValuePair] = field(default_factory=list)


def _now_utc() -> datetime:
    return datetime.now(timezone.utc)


def _to_epoch_ms(dt: datetime) -> int:
    return int(dt.timestamp() * 1000)


# Preposition prefix that may appear before temporal phrases
_PREP_PREFIX = r"(?:(?:from|in|during|within|over)\s+(?:the\s+)?)?"

# Temporal patterns — each includes optional leading preposition
_TEMPORAL_PATTERNS: list[tuple[re.Pattern, str]] = [
    # "yesterday" / "from yesterday"
    (
        re.compile(_PREP_PREFIX + r"\byesterday\b", re.IGNORECASE),
        "yesterday",
    ),
    # "last N hours/minutes/days" / "in the last 2 hours"
    (
        re.compile(
            _PREP_PREFIX + r"\blast\s+(\d+)\s+(hour|minute|day|min|hr)s?\b",
            re.IGNORECASE,
        ),
        "last_n",
    ),
    # "last hour/minute/day" / "in the last hour"
    (
        re.compile(
            _PREP_PREFIX + r"\blast\s+(hour|minute|day|min|hr)\b", re.IGNORECASE
        ),
        "last_1",
    ),
    # "since YYYY-MM-DD" or "since YYYY-MM-DDTHH:MM:SS"
    (
        re.compile(
            r"\bsince\s+(\d{4}-\d{2}-\d{2}(?:T\d{2}:\d{2}:\d{2})?)\b",
            re.IGNORECASE,
        ),
        "since",
    ),
    # "from YYYY-MM-DD" (same as since, but only when followed by a date)
    (
        re.compile(
            r"\bfrom\s+(\d{4}-\d{2}-\d{2}(?:T\d{2}:\d{2}:\d{2})?)\b",
            re.IGNORECASE,
        ),
        "from_date",
    ),
]

# Explicit field-value pairs: field:value, field=value, field:"value"
_FIELD_VALUE_PATTERN = re.compile(
    r"\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s*[:=]\s*"
    r'(?:"([^"]+)"|(\S+))'
)

# Implicit pairs: field_name followed by a number (e.g. "status_code 504")
_FIELD_NUMBER_PATTERN = re.compile(
    r"\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s+(-?\d+(?:\.\d+)?)\b"
)

# Field hint pattern — dotted identifiers like "error.message"
_FIELD_HINT_PATTERN = re.compile(r"\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)+)\b")

# Negation pattern
_NEGATION_PATTERN = re.compile(
    r"\bnot\s+(?:from\s+|in\s+)?(\w+)\b", re.IGNORECASE
)

_UNIT_TO_TIMEDELTA = {
    "hour": timedelta(hours=1),
    "hr": timedelta(hours=1),
    "minute": timedelta(minutes=1),
    "min": timedelta(minutes=1),
    "day": timedelta(days=1),
}

# Filler words/phrases at the start of a query
_FILLER_PATTERN = re.compile(
    r"^(search\s+for|show\s+me|find\s+me|give\s+me|find|show|get|search)\s+",
    re.IGNORECASE,
)

# Trailing noise after extraction
_TRAILING_NOISE = re.compile(r"\b(in|from|during|within|over|the)\b", re.IGNORECASE)


def parse_nl_query(query: str) -> QueryIntent:
    """Parse a natural language query into structured intent."""
    intent = QueryIntent()
    remaining = query

    # Extract temporal constraints
    for pattern, kind in _TEMPORAL_PATTERNS:
        match = pattern.search(remaining)
        if match is None:
            continue

        now = _now_utc()
        temporal = TemporalConstraint()

        if kind == "yesterday":
            yesterday = now - timedelta(days=1)
            start = yesterday.replace(hour=0, minute=0, second=0, microsecond=0)
            end = start + timedelta(days=1) - timedelta(milliseconds=1)
            temporal.begin_ts = _to_epoch_ms(start)
            temporal.end_ts = _to_epoch_ms(end)
        elif kind == "last_n":
            n = int(match.group(1))
            unit = match.group(2).lower()
            delta = _UNIT_TO_TIMEDELTA.get(unit, timedelta(hours=1))
            temporal.begin_ts = _to_epoch_ms(now - delta * n)
        elif kind == "last_1":
            unit = match.group(1).lower()
            delta = _UNIT_TO_TIMEDELTA.get(unit, timedelta(hours=1))
            temporal.begin_ts = _to_epoch_ms(now - delta)
        elif kind in ("since", "from_date"):
            date_str = match.group(1)
            try:
                if "T" in date_str:
                    dt = datetime.fromisoformat(date_str).replace(tzinfo=timezone.utc)
                else:
                    dt = datetime.strptime(date_str, "%Y-%m-%d").replace(
                        tzinfo=timezone.utc
                    )
                temporal.begin_ts = _to_epoch_ms(dt)
            except ValueError:
                continue

        intent.temporal = temporal
        remaining = remaining[: match.start()] + remaining[match.end() :]
        break  # Only extract one temporal constraint

    # Extract explicit field-value pairs (field:value, field=value)
    for match in _FIELD_VALUE_PATTERN.finditer(remaining):
        field_name = match.group(1)
        value = match.group(2) if match.group(2) is not None else match.group(3)
        intent.field_value_pairs.append(FieldValuePair(field_hint=field_name, value=value))
    remaining = _FIELD_VALUE_PATTERN.sub("", remaining)

    # Extract implicit field-number pairs (e.g. "status_code 504")
    # Only match if the field name contains _ or . (to avoid matching generic words)
    for match in _FIELD_NUMBER_PATTERN.finditer(remaining):
        field_name = match.group(1)
        if "_" in field_name or "." in field_name:
            value = match.group(2)
            intent.field_value_pairs.append(FieldValuePair(field_hint=field_name, value=value))
            remaining = remaining[:match.start()] + remaining[match.end():]

    # Extract field hints (dotted identifiers)
    for match in _FIELD_HINT_PATTERN.finditer(remaining):
        intent.field_hints.append(match.group(1))
    remaining = _FIELD_HINT_PATTERN.sub("", remaining)

    # Extract negations
    for match in _NEGATION_PATTERN.finditer(remaining):
        intent.negations.append(match.group(1))
    remaining = _NEGATION_PATTERN.sub("", remaining)

    # Remove filler words at the start
    remaining = _FILLER_PATTERN.sub("", remaining)

    # Clean up trailing noise words that were left after temporal/negation extraction
    # Only remove if they're standalone (not part of a meaningful phrase)
    words = remaining.split()
    cleaned_words = []
    for i, word in enumerate(words):
        stripped = word.strip(" ,;")
        # Remove isolated noise words at boundaries (first/last) or adjacent to gaps
        if _TRAILING_NOISE.fullmatch(stripped):
            # Keep if sandwiched between two content words
            has_prev = i > 0 and not _TRAILING_NOISE.fullmatch(
                cleaned_words[-1] if cleaned_words else ""
            )
            has_next = i < len(words) - 1 and not _TRAILING_NOISE.fullmatch(
                words[i + 1].strip(" ,;")
            )
            if has_prev and has_next:
                cleaned_words.append(word)
        else:
            cleaned_words.append(word)

    cleaned = " ".join(cleaned_words).strip(" ,;")
    if cleaned:
        intent.value_phrases.append(cleaned)

    return intent
