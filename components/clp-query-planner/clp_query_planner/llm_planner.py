"""LLM-based KQL generation — escalation path when embedding scores are low."""

from __future__ import annotations

import json
import re
from typing import Optional

import requests

from .query_synthesizer import SynthesizedQuery
from .schema_tree import FieldInfo

_KQL_SYNTAX_REFERENCE = """\
KQL Syntax for clp-s:

1. String wildcard match:   field_path: "*value*"
2. Exact string match:      field_path: "exact value"
3. Numeric match:           field_path: 504
4. Boolean match:           field_path: true
5. Semantic search:         field_path: semantic("natural language query", K)
   - Only valid for ClpString fields
   - K is the number of top matching logtypes (default 5)
6. OR combinator:           clause1 OR clause2
7. AND combinator:          clause1 AND clause2
8. Parentheses:             (clause1 OR clause2) AND clause3

Time filtering is done via CLI flags, not KQL:
  --tge <epoch_ms>   (timestamp >= value)
  --tle <epoch_ms>   (timestamp <= value)
"""

_SYSTEM_PROMPT = """\
You are a KQL query generator for the clp-s log search engine. Given a natural language \
query and a schema tree, generate a KQL query that best answers the user's intent.

{kql_syntax}

Rules:
- Use semantic() ONLY for ClpString fields when the query is conceptual/fuzzy
- Use wildcard "*value*" for VarString fields
- Use exact numeric values for Integer/Float fields
- Combine clauses with OR when searching across multiple fields
- Combine clauses with AND when all conditions must be met
- For time-based queries, output tge/tle epoch millisecond values
- Output ONLY valid JSON with keys: "kql", "tge" (optional int), "tle" (optional int)
- Do not explain, just output the JSON
"""

_USER_PROMPT_TEMPLATE = """\
Schema fields:
{schema_fields}

Natural language query: {query}

Generate the KQL query as JSON: {{"kql": "...", "tge": null, "tle": null}}
"""


def _format_schema_fields(fields: list[FieldInfo]) -> str:
    lines = []
    for f in fields:
        lines.append(f"  - {f.full_path} ({f.node_type})")
    return "\n".join(lines)


def _try_parse_json(text: str) -> Optional[dict]:
    """Try to parse JSON, with fixups for common LLM formatting errors."""
    text = text.strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        pass

    # Fix: trailing quote before closing brace — e.g. ...null"}  → ...null}
    fixed = re.sub(r'"(\s*)\}$', r"\1}", text)
    try:
        return json.loads(fixed)
    except json.JSONDecodeError:
        pass

    # Fix: missing closing brace
    if text.startswith("{") and not text.endswith("}"):
        try:
            return json.loads(text + "}")
        except json.JSONDecodeError:
            pass

    return None


def _extract_json(text: str) -> Optional[dict]:
    """Extract JSON object from LLM response text."""
    text = text.strip()

    # Try direct parse first
    result = _try_parse_json(text)
    if result:
        return result

    # Try to find JSON block in markdown code fence
    match = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL)
    if match:
        result = _try_parse_json(match.group(1))
        if result:
            return result

    # Try to find any JSON object containing "kql"
    match = re.search(r"\{[^{}]*\"kql\"[^{}]*\}", text)
    if match:
        result = _try_parse_json(match.group(0))
        if result:
            return result

    return None


def plan_query_with_llm(
    query: str,
    fields: list[FieldInfo],
    llm_url: str,
    semantic_top_k: int = 5,
    model: str = "",
    api_key: str = "",
) -> Optional[SynthesizedQuery]:
    """Call an LLM (OpenAI-compatible API) to generate KQL from a natural language query.

    Works with any OpenAI-compatible endpoint:
      - Local: ollama (http://localhost:11434/v1), llama.cpp, vLLM
      - Cloud: OpenRouter (https://openrouter.ai/api/v1)

    Args:
        query: The natural language query
        fields: Schema fields from the archive
        llm_url: Base URL of the OpenAI-compatible API
        semantic_top_k: Default K for semantic() calls
        model: Model name to use (required for cloud APIs like OpenRouter)
        api_key: API key for authenticated endpoints (e.g. OpenRouter)

    Returns:
        SynthesizedQuery if successful, None if LLM call fails
    """
    system_msg = _SYSTEM_PROMPT.format(kql_syntax=_KQL_SYNTAX_REFERENCE)
    user_msg = _USER_PROMPT_TEMPLATE.format(
        schema_fields=_format_schema_fields(fields),
        query=query,
    )

    messages = [
        {"role": "system", "content": system_msg},
        {"role": "user", "content": user_msg},
    ]

    request_body: dict = {
        "messages": messages,
        "temperature": 0.0,
        "max_tokens": 2048,
    }
    if model:
        request_body["model"] = model

    headers: dict[str, str] = {"Content-Type": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"

    endpoint = llm_url.rstrip("/") + "/chat/completions"
    try:
        resp = requests.post(endpoint, json=request_body, headers=headers, timeout=60)
        resp.raise_for_status()
    except (requests.ConnectionError, requests.Timeout, requests.HTTPError) as e:
        import sys
        print(f"LLM request failed: {e}", file=sys.stderr)
        return None

    resp_json = resp.json()

    # Handle reasoning models that return content in different formats
    choice = resp_json.get("choices", [{}])[0]
    message = choice.get("message", {})
    content = message.get("content", "")

    # Some models may return empty content with a reasoning field
    if not content and "reasoning" in message:
        content = message.get("reasoning", "")

    parsed = _extract_json(content)
    if parsed is None:
        import sys
        print(f"Failed to parse LLM response: {content[:200]}", file=sys.stderr)
        return None

    kql = parsed.get("kql", "")
    if not kql:
        return None

    tge = parsed.get("tge")
    tle = parsed.get("tle")

    # Coerce to int if present
    if tge is not None:
        try:
            tge = int(tge)
        except (ValueError, TypeError):
            tge = None
    if tle is not None:
        try:
            tle = int(tle)
        except (ValueError, TypeError):
            tle = None

    return SynthesizedQuery(kql=kql, tge=tge, tle=tle)
