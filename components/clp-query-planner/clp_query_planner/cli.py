"""Entry point — orchestrate schema loading, field resolution, NL parsing, and KQL synthesis."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys

from .field_resolver import resolve_fields, resolve_fields_by_keywords
from .llm_planner import plan_query_with_llm
from .nl_parser import parse_nl_query
from .query_synthesizer import synthesize_query
from .schema_tree import load_schema_from_archive, parse_schema_json


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="clp-query-planner",
        description="Translate natural language queries into KQL for clp-s",
    )
    parser.add_argument("archive_path", help="Path to the CLP archive")
    parser.add_argument("query", help="Natural language query")
    parser.add_argument(
        "--clp-s-bin",
        default="clp-s",
        help="Path to the clp-s binary (default: clp-s)",
    )
    parser.add_argument(
        "--embed-url",
        default="http://localhost:8080",
        help="URL of the embedding service (default: http://localhost:8080)",
    )
    parser.add_argument(
        "--llm-url",
        default=None,
        help="URL of an OpenAI-compatible LLM API for complex query planning "
        "(e.g. http://localhost:8080/v1). If not set, uses the embed-url with /v1 appended.",
    )
    parser.add_argument(
        "--llm-model",
        default="",
        help="Model name to request from the LLM API (default: server default)",
    )
    parser.add_argument(
        "--llm-api-key",
        default=None,
        help="API key for the LLM endpoint (default: $CLP_LLM_API_KEY env var)",
    )
    parser.add_argument(
        "--no-llm",
        action="store_true",
        help="Disable LLM escalation even if --llm-url is available",
    )
    parser.add_argument(
        "--auth",
        default=None,
        help="Authentication method for archive access (e.g. s3)",
    )
    parser.add_argument(
        "--semantic-top-k",
        type=int,
        default=5,
        help="Number of top logtypes for semantic() calls (default: 5)",
    )
    parser.add_argument(
        "--field-top-k",
        type=int,
        default=5,
        help="Number of top fields to match against (default: 5)",
    )
    parser.add_argument(
        "--field-threshold",
        type=float,
        default=0.3,
        help="Minimum similarity threshold for field matching (default: 0.3)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the generated KQL and flags without executing clp-s",
    )

    args = parser.parse_args(argv)

    # Step 1: Load schema tree from archive
    try:
        schema_json = load_schema_from_archive(
            args.archive_path,
            clp_s_bin=args.clp_s_bin,
            auth=args.auth,
        )
    except subprocess.CalledProcessError as e:
        print(f"Failed to inspect archive: {e.stderr}", file=sys.stderr)
        return 1
    except Exception as e:
        print(f"Failed to load schema: {e}", file=sys.stderr)
        return 1

    # Step 2: Build field catalog
    fields = parse_schema_json(schema_json)
    if not fields:
        print("No searchable fields found in archive schema.", file=sys.stderr)
        return 1

    # Step 3: Parse NL query
    intent = parse_nl_query(args.query)

    if not intent.value_phrases and not intent.field_value_pairs:
        print("No search phrases extracted from query.", file=sys.stderr)
        return 1

    # Step 4-5: Resolve fields and generate KQL via waterfall
    #   1. Embedding resolution → if confident, synthesize directly
    #   2. LLM escalation → if embedding scores low, ask LLM
    #   3. Keyword fallback → synonym-based matching as last resort
    resolved = []
    result = None
    used_llm = False
    query_text = " ".join(intent.value_phrases) if intent.value_phrases else ""

    # Try embedding-based field resolution
    if intent.value_phrases:
        try:
            resolved = resolve_fields(
                query=query_text,
                fields=fields,
                embed_url=args.embed_url,
                top_k=args.field_top_k,
                threshold=args.field_threshold,
            )
        except Exception as e:
            print(f"Warning: embedding resolution failed: {e}", file=sys.stderr)

    # If embedding found matches or we have field-value pairs, synthesize directly
    if resolved or intent.field_value_pairs:
        result = synthesize_query(
            value_phrases=intent.value_phrases,
            resolved_fields=resolved,
            temporal=intent.temporal,
            semantic_top_k=args.semantic_top_k,
            field_value_pairs=intent.field_value_pairs,
            all_fields=fields,
        )

    # Escalate to LLM if embedding wasn't confident
    llm_api_key = args.llm_api_key or os.environ.get("CLP_LLM_API_KEY", "")
    if result is None and not args.no_llm:
        llm_url = args.llm_url or os.environ.get(
            "CLP_LLM_URL", args.embed_url.rstrip("/") + "/v1"
        )
        llm_model = args.llm_model or os.environ.get("CLP_LLM_MODEL", "")
        print("Embedding scores low, escalating to LLM...", file=sys.stderr)
        result = plan_query_with_llm(
            query=args.query,
            fields=fields,
            llm_url=llm_url,
            semantic_top_k=args.semantic_top_k,
            model=llm_model,
            api_key=llm_api_key,
        )
        if result:
            used_llm = True
        else:
            print("LLM unavailable, falling back to keywords.", file=sys.stderr)

    # Final fallback: keyword-based field resolution
    if result is None and intent.value_phrases:
        resolved = resolve_fields_by_keywords(query_text, fields, args.field_top_k)
        result = synthesize_query(
            value_phrases=intent.value_phrases,
            resolved_fields=resolved,
            temporal=intent.temporal,
            semantic_top_k=args.semantic_top_k,
            field_value_pairs=intent.field_value_pairs,
            all_fields=fields,
        )

    if result is None:
        print("Could not generate a query.", file=sys.stderr)
        return 1

    # Step 6: Execute or print
    if args.dry_run:
        if used_llm:
            print("(Generated by LLM)")
        print(f"KQL:  {result.kql}")
        if result.tge is not None:
            print(f"--tge {result.tge}")
        if result.tle is not None:
            print(f"--tle {result.tle}")
        if resolved:
            print()
            print("Fields matched:")
            for r in resolved:
                print(
                    f"  {r.field.full_path} ({r.field.node_type})"
                    f" — similarity: {r.similarity:.3f}"
                )
        return 0

    # Build clp-s command
    cmd = [args.clp_s_bin, "s", args.archive_path, result.kql]
    cmd.extend(["--semantic-url", args.embed_url])
    cmd.extend(["--semantic-top-k", str(args.semantic_top_k)])
    if result.tge is not None:
        cmd.extend(["--tge", str(result.tge)])
    if result.tle is not None:
        cmd.extend(["--tle", str(result.tle)])
    if args.auth:
        cmd.extend(["--auth", args.auth])

    try:
        proc = subprocess.run(cmd, check=False)
        return proc.returncode
    except FileNotFoundError:
        print(f"clp-s binary not found: {args.clp_s_bin}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
