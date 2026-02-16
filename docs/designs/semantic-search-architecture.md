# CLP Semantic Search: Architecture & Comparison

## Core Idea

CLP performs **semantic search on schema-free log data** — no predefined
schema, no upfront indexing, no vector storage. Logs in any format (arbitrary
JSON, unstructured text, mixed schemas) are ingested as-is, and the structures
that enable semantic search are produced automatically as a byproduct of
CLP's lossless compression.

The key insight: **embed the logtype dictionary, not the messages.** An archive
with 100 million messages typically contains only 500–5,000 unique logtypes.
Rather than embedding 100M messages, CLP embeds ~5,000 logtypes — a
20,000–200,000x reduction in embedding cost — using structures that
compression already created.

## Step 1: Parsing Schema-Free Data

Log data has no guaranteed schema. Different services emit different formats,
fields appear and disappear, types change between deployments. CLP handles
this without requiring users to define a schema upfront.

### Log-Surgeon: Automatic Semantic Parsing

**log-surgeon** is CLP's high-performance log parsing library. During
ingestion, it automatically parses every log message — extracting timestamps,
tokenizing text, and identifying variables — using an optimized NFA engine.

What makes log-surgeon unique is **semantically labeled variable extraction**.
Users define domain-specific variable patterns via schema rules:

```
Schema rules (user-defined):
  user:       [a-zA-Z][a-zA-Z0-9_-]*
  ip:         \d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}
  request_id: [0-9a-f]{8}-[0-9a-f]{4}-...
  error_msg:  (invalid password|account locked|expired token|...)
```

Variables are labeled with **platform/domain-specific names** — not generic
types. A request ID isn't `<*>`, it's `<request_id>`. A user field isn't
`<string>`, it's `<user>`. Unrecognized tokens fall through to a generic
variable type.

```
Raw log:
  "2025-01-15 10:23:45 Auth failed for user admin: invalid password"

        │  log-surgeon
        ▼

Logtype:    "Auth failed for user <user>: <error_msg>"
Variables:  {user: "admin", error_msg: "invalid password"}
```

For **JSON logs**, CLP additionally auto-discovers the schema structure. No
schema definition file is needed — CLP inspects each record's fields and types
at ingestion time.

## Step 2: Lossless Semantic Compression

CLP compresses the parsed data using structures that capture the semantic
structure of the logs:

### Merged Schema Tree (MST)

For JSON logs, CLP builds the MST — a tree representing the union of all field
schemas seen across every record. Each node has a name, an automatically
inferred type, and a position in the hierarchy.

```
Root (Object)
├── message     (ClpString)    ← logtype-decomposed
├── level       (VarString)    ← short enumerable strings
├── service     (VarString)
├── status_code (Integer)      ← native int64
└── ts          (Integer)
```

The MST is **auto-discovered** from schema-free data — no upfront schema
definition. It's a compact summary: typically a handful of nodes regardless
of how many millions of messages exist.

### Logtype Dictionary + Variable Dictionary

CLP decomposes every text field value into:
- **Logtype**: the static template (`"Auth failed for user <user>: <error_msg>"`)
- **Variables**: the dynamic parts (`{user: "admin", error_msg: "invalid password"}`)

100M messages might produce only 847 unique logtypes. Each message stores just
a logtype ID + variable references, achieving 10–50x lossless compression.

### Columnar Schema Tables

Records sharing the same JSON schema are grouped into tables. Each column
stores values for one MST node across all records. The logtype ID is stored
as a column — directly readable without decompression.

### What Compression Produces

```
Raw schema-free logs (any format, any structure)
    │
    ▼  log-surgeon: parse, extract timestamps, label variables
    │
    ▼  CLP compression: discover schema, decompose, store columnar
    │
    ▼
┌── Archive ────────────────────────────────────────────────────┐
│                                                                │
│  MST                  — auto-discovered field schema            │
│  Logtype Dictionary   — unique log patterns with semantic labels│
│  Variable Dictionary  — extracted variable values               │
│  Columnar Tables      — records grouped by schema, per-field    │
│  Metadata             — time ranges, offsets, archive info       │
│                                                                │
│  Result: 10–50x compression, all structures available for       │
│  semantic search at zero additional cost                        │
└────────────────────────────────────────────────────────────────┘
```

## Step 3: Semantic Search (Query Time)

Semantic search reuses the compression artifacts. No separate index build.

```
Archive (100M messages, 847 unique logtypes)
    │
    ▼
┌── Logtype Dictionary (from compression) ──────────────────────┐
│  "Auth failed for user <user>: <error_msg>"     → logtype 0   │
│  "Connection timeout to <hostname> after <dur>s" → logtype 1   │
│  "Request <request_id> processed in <dur>ms"     → logtype 2   │
│  ... (847 total)                                               │
└────────────────────────────────────────────────────────────────┘
    │
    ▼  Batch-embed 848 texts (847 logtypes + 1 query)
    │  In-process ONNX inference, ~10ms/text
    ▼
┌── Cosine Similarity Ranking ──────────────────────────────────┐
│  query: "authentication failures"                              │
│    logtype 0 "Auth failed for user <user>:..."        → 0.87 ✓│
│    logtype 2 "Request <request_id> processed..."      → 0.15 ✗│
│  Top-K logtype IDs → hash set {0, 3, 5}                       │
└────────────────────────────────────────────────────────────────┘
    │
    ▼  Scan columnar storage
    │  Per message: logtype_id ∈ {0, 3, 5}?  → O(1) hash lookup
    ▼
┌── Results ────────────────────────────────────────────────────┐
│  100M messages scanned, only 848 embeddings computed           │
└────────────────────────────────────────────────────────────────┘
```

### Algorithm

1. **Read logtype dictionary** — already exists from compression

2. **Clean logtypes** — replace binary variable placeholders with
   human-readable semantic labels from log-surgeon

3. **Embed** — in-process ONNX inference with N+1 texts (N logtypes + 1 query)

4. **Rank by cosine similarity** — dot product between query vector and each
   logtype vector. `partial_sort` for top-K logtypes (avoids full sort when
   K << N). Store matched IDs in a hash set.

5. **Type narrowing** — prune the semantic filter from non-ClpString columns
   before scanning begins

6. **Row scan** — per message, read logtype ID from columnar storage, check
   membership in hash set. O(1) per message, no decompression.

Steps 1–4 run once per archive. Step 6 is a hash lookup per row.

### Why It Scales

The dominant cost is embedding (step 3), and it scales with **unique
logtypes** — not message volume. This works because:

- **Logtype dictionary** — CLP compression already deduplicates log patterns.
  100M messages → ~847 logtypes. Embed 848 texts, not 100M.
- **MST** — CLP compression already summarizes the schema. 100M messages
  across 5 fields → embed 6 texts for field resolution, not scan all records.
- **Semantic labels** — log-surgeon's domain-specific variable labels
  (`<user>`, `<error_msg>`) enrich logtypes for embedding, improving
  similarity matching without any additional processing.

All three are **compression artifacts**. Semantic search adds no storage,
no ingestion cost, and no separate index — only a query-time embedding call
proportional to the size of these compact summaries.

## Natural Language Query Fallback

For users who don't write KQL directly, clp-s automatically detects natural
language queries and interprets them. When a query fails KQL parsing (e.g.,
multi-word phrases without explicit AND/OR), clp-s falls back to an in-process
C++ natural language parser.

```
"authentication failures with status code 403"
    │
    ├─ 1. Try KQL parse ──→ fails (no AND/OR operators)
    │
    ├─ 2. NL fallback (in-process C++ regex)
    │     ├─ Extract field:value pairs ──→ status_code:403
    │     ├─ Extract implicit field-number pairs
    │     ├─ Strip filler words ("show me", "find", etc.)
    │     └─ Remaining text → semantic("authentication failures")
    │
    └─ Result: semantic("authentication failures") AND status_code: 403
```

The NL parser uses regex-based heuristics with no external dependencies:

1. **Extract field:value pairs** — `field:value` or `field:"quoted value"`
2. **Extract implicit field-number pairs** — e.g., `status_code 403` (field
   name must contain `_` or `.` to avoid false positives)
3. **Strip filler words** — "show me", "find", "search for", etc.
4. **Warn on temporal phrases** — if the query contains "last N hours",
   "yesterday", etc., a warning suggests using `--tge`/`--tle` flags
5. **Remaining text → `semantic()`** — wrapped in a `FilterExpr(SEMANTIC,
   SemanticLiteral)` with wildcard `*` column
6. **Combine with AND** — all sub-expressions are AND'd together

## Integration with CLP Metastore

In a multi-archive deployment, the metastore tracks archive time ranges,
schemas, and locations. Semantic search integrates naturally:

```
"auth failures last 24h"
    │
    ▼  Metastore: filter by time range → [archive_17, archive_18]
    │
    ▼  Per archive: embed query vs that archive's logtype dictionary
    │
    ▼  Merge results across archives, ordered by timestamp
```

Each archive is self-contained — its own logtype dictionary, MST, and columnar
data. No global vector index. Adding or removing archives requires no
re-indexing.

## Comparison with Traditional Approaches

Traditional semantic search systems require a predefined schema and embed every
record at ingestion time. CLP works on schema-free data, discovers structure
automatically, and embeds only at query time against compact summaries.

| | **CLP** | **Elasticsearch kNN** | **LanceDB** | **Cursor/Copilot** |
|---|---|---|---|---|
| **Schema requirement** | None (auto-discovered) | Mapping definition | Table schema | None |
| **What gets embedded** | Unique logtypes (100s–1000s) | Every document (millions) | Every row (millions) | Code chunks |
| **When embedding happens** | Query time only | Ingest + query | Ingest + query | Ingest + query |
| **Ingestion cost for search** | **Zero** — compression byproduct | Per-doc embedding | Per-row embedding | Per-chunk embedding |
| **Vector storage** | None | HNSW index (GBs) | IVF/HNSW index | Vector index |
| **Storage overhead** | 0% | 30–50% | Vector column + index | Separate store |
| **Query latency** | ~50ms embed + scan | ~10ms ANN | ~10ms ANN | ~100ms |
| **Scales with** | Unique log patterns (slow growth) | Total docs (linear) | Total rows (linear) | Codebase size |

### Why This Matters for Logs

Logs are fundamentally repetitive. 1 billion messages/day typically produce
1,000–10,000 unique logtypes. Traditional approaches ignore this:

- **LanceDB**: 384-dim float32 per row. 1B messages = **1.4 TB** vectors alone.
- **Elasticsearch kNN**: similar vector overhead, plus HNSW graph.
- **CLP**: embeds ~5,000 logtypes at query time. No stored vectors. Logtype
  dictionary typically **< 1 MB**.

### Trade-offs

1. **Logtype granularity** — matches log patterns, not variable values.
   "Auth failed for user admin" and "...user root" share a logtype.
   Variable-level filtering uses KQL wildcards or exact match alongside
   `semantic()`.

2. **No pre-built vector index** — embedding happens at query time. For 50K+
   logtypes, the embedding call takes seconds. Caching mitigates repeats.

3. **ClpString only** — semantic search applies to logtype-decomposed fields.
   Other field types use exact or wildcard matching.

## For LanceDB Users

| LanceDB | CLP |
|---|---|
| Define table schema | Auto-discovered from schema-free data |
| Embed every row at ingest | Embed logtypes at query time (compression byproduct) |
| Vector column (per-row) | Logtype dictionary (per-pattern) |
| ANN index (IVF-PQ, HNSW) | Brute-force cosine (fast because N is small) |
| SQL filter | KQL (wildcard, exact, boolean, semantic) |

**Gain**: schema-free ingestion, no vector storage, no index maintenance,
no ingestion cost for search, scales with log pattern count not volume,
10–50x compression.

**Lose**: per-message vector similarity, pre-computed index, mature vector
ecosystem.

**Sweet spot**: CLP for log storage + search (structured, semi-structured,
semantic). LanceDB for use cases needing per-record similarity (e.g. stack
trace deduplication).
