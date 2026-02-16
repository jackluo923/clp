"""Semantic field matching — embed fields + query, rank by cosine similarity."""

from __future__ import annotations

import math
import re
from dataclasses import dataclass

import requests

from .schema_tree import FieldInfo


@dataclass
class ResolvedField:
    """A field matched by semantic similarity to the query."""

    field: FieldInfo
    similarity: float


def _cosine_similarity(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    norm_a = math.sqrt(sum(x * x for x in a))
    norm_b = math.sqrt(sum(x * x for x in b))
    if norm_a == 0 or norm_b == 0:
        return 0.0
    return dot / (norm_a * norm_b)


def _tokenize(text: str) -> set[str]:
    """Split text into lowercase tokens on non-alphanumeric boundaries."""
    return set(re.findall(r"[a-z0-9]+", text.lower()))


# Expand query terms to related words that may appear in field descriptions
_QUERY_SYNONYMS: dict[str, set[str]] = {
    "auth": {"authentication", "authorization", "login", "credential", "token"},
    "authentication": {"auth", "login", "credential", "token"},
    "authorization": {"auth", "login", "credential", "token"},
    "login": {"auth", "authentication", "credential", "user"},
    "error": {"failure", "fault", "crash", "exception", "fatal", "error"},
    "errors": {"failure", "fault", "crash", "exception", "fatal", "error"},
    "failure": {"error", "fault", "crash", "exception", "fatal"},
    "failures": {"error", "fault", "crash", "exception", "fatal"},
    "crash": {"error", "failure", "fault", "exception", "fatal"},
    "slow": {"latency", "duration", "timeout", "response"},
    "timeout": {"latency", "duration", "slow", "response"},
    "http": {"status", "code", "request", "response", "url", "method"},
    "request": {"http", "url", "method", "path", "endpoint"},
    "response": {"http", "status", "code", "latency", "duration"},
    "log": {"message", "text", "body", "content"},
    "message": {"log", "text", "body", "content"},
    "user": {"username", "account", "identity", "principal"},
    "server": {"host", "hostname", "machine", "ip"},
    "ip": {"address", "host", "network", "source", "destination"},
}


def _expand_tokens(tokens: set[str]) -> set[str]:
    """Expand a token set with synonyms."""
    expanded = set(tokens)
    for token in tokens:
        synonyms = _QUERY_SYNONYMS.get(token)
        if synonyms:
            expanded |= synonyms
    return expanded


def resolve_fields_by_keywords(
    query: str,
    fields: list[FieldInfo],
    top_k: int = 5,
) -> list[ResolvedField]:
    """Match query words against field descriptions using token overlap with synonym expansion.

    Used as fallback when the embedding service is unavailable.
    """
    if not fields:
        return []

    query_tokens = _expand_tokens(_tokenize(query))
    scored: list[ResolvedField] = []
    for field_info in fields:
        desc_tokens = _tokenize(field_info.rich_description())
        overlap = len(query_tokens & desc_tokens)
        if overlap > 0:
            # Score = overlap count normalized by description size (favors more specific fields)
            sim = overlap / len(desc_tokens)
            scored.append(ResolvedField(field=field_info, similarity=sim))

    scored.sort(key=lambda r: r.similarity, reverse=True)
    return scored[:top_k]


def resolve_fields(
    query: str,
    fields: list[FieldInfo],
    embed_url: str,
    top_k: int = 5,
    threshold: float = 0.3,
) -> list[ResolvedField]:
    """Embed the query and field descriptions, return top-K fields above threshold.

    Uses batch embedding via POST /embed with {"texts": [...]}.
    Returns empty list if no fields exceed threshold — caller decides fallback strategy.
    """
    if not fields:
        return []

    # Build texts: query first, then each field's rich description
    texts = [query] + [f.rich_description() for f in fields]

    resp = requests.post(f"{embed_url}/embed", json={"texts": texts}, timeout=30)
    resp.raise_for_status()
    embeddings = resp.json()["embeddings"]

    query_embedding = embeddings[0]
    field_embeddings = embeddings[1:]

    # Rank by cosine similarity
    scored: list[ResolvedField] = []
    for field_info, field_emb in zip(fields, field_embeddings):
        sim = _cosine_similarity(query_embedding, field_emb)
        if sim >= threshold:
            scored.append(ResolvedField(field=field_info, similarity=sim))

    scored.sort(key=lambda r: r.similarity, reverse=True)
    return scored[:top_k]
