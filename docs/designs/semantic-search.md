# Semantic Search for clp-s

## Introduction

Semantic search lets you find log messages by *meaning* rather than exact
keywords. Searching for "database connection error" also returns messages like
"Failed to establish TCP connection to backend service on port 8080" — even
though the words don't overlap — because the log patterns are semantically
similar.

The key insight that makes this practical is that CLP already decomposes every
log message into a **logtype** (a static template like
`"Connection to <*> failed: timeout after <*> seconds"`) and encoded variables.
An archive with 100 million messages typically contains only 500–5,000 unique
logtypes. Rather than embedding 100M messages, CLP embeds those ~5,000 logtypes
— a 20,000–200,000x reduction in embedding cost — using structures that
compression already created. No separate vector index, no ingestion-time
embedding, no additional storage.

Embedding uses **bge-small-en-v1.5**, a small transformer model, running
in-process via ONNX Runtime on CPU. The entire model directory is ~130 MB and
inference takes roughly 10 ms per text.

## Quick Start

### Prerequisites

- Docker (for building clp-s and dependencies)
- A CLP archive created with `clp-s c`

### Build

Start the CLP build container:

```bash
docker run --rm -it \
  -v /path/to/clp:/clp \
  -w /clp \
  clp-core-dependencies-aarch64-manylinux_2_28:dev \
  bash
```

Inside the container, install all dependencies and build:

```bash
# Downloads ONNX Runtime, builds ortextensions, downloads BGE model, etc.
task deps:core

# Build clp-s
task core
```

### First Search

```bash
# Set library path for ONNX Runtime
export LD_LIBRARY_PATH=/clp/build/deps/cpp/onnxruntime-src/lib

# Compress some logs (if you haven't already)
clp-s c /path/to/archives /path/to/logs.jsonl

# Run a semantic search
clp-s s /path/to/archives 'database connection timeout' \
    --semantic-model-dir /clp/build/deps/cpp/bge-small-en-v1.5
```

The `--semantic-model-dir` points to the directory containing `tokenizer.onnx`,
`encoder.onnx`, and `libortextensions.so`. When built via `task deps:core`,
this is `build/deps/cpp/bge-small-en-v1.5/`.

CLP will embed the query and the archive's logtypes, rank them by cosine
similarity, and return all messages whose logtype is among the top-K most
similar.

## Query Examples

### Natural Language (Recommended for Getting Started)

Just type what you're looking for. When the query isn't valid KQL, clp-s
automatically interprets it as natural language:

```bash
# Simple natural language
clp-s s archives 'database connection timeout' \
    --semantic-model-dir /path/to/model

# With field:value pairs (extracted automatically)
clp-s s archives 'auth failures status_code:403' \
    --semantic-model-dir /path/to/model
# → interpreted as: semantic("auth failures") AND status_code: 403

# Field-number pairs are also detected (field name must contain _ or .)
clp-s s archives 'errors on status_code 500' \
    --semantic-model-dir /path/to/model
```

The natural language parser:
1. Extracts `field:value` pairs as exact-match filters
2. Strips filler words ("show me", "find", "search for", etc.)
3. Wraps remaining text in `semantic()` for embedding-based search
4. Combines all sub-expressions with AND

If the query contains temporal phrases (e.g., "last 2 hours"), a warning
suggests using `--tge`/`--tle` flags to specify a time range instead.

### Explicit KQL Syntax

The `semantic()` function can also be used directly in KQL expressions for more
control:

```bash
# Basic — uses default top-K and threshold
clp-s s archives 'semantic("database connection error")' \
    --semantic-model-dir /path/to/model

# Override top-K per expression
clp-s s archives 'semantic("security alert", 10)' \
    --semantic-model-dir /path/to/model

# Combine with keyword search
clp-s s archives 'semantic("disk space issue") and "*prod*"' \
    --semantic-model-dir /path/to/model

# Multiple semantic expressions
clp-s s archives 'semantic("auth failure") or semantic("permission denied")' \
    --semantic-model-dir /path/to/model
```

## CLI Reference

| Option | Default | Description |
|--------|---------|-------------|
| `--semantic-model-dir DIR` | *(none)* | Directory containing `tokenizer.onnx`, `encoder.onnx`, and `libortextensions.so`. Required when the query uses `semantic()`. |
| `--semantic-top-k K` | 5 | Number of top-matching logtypes to return per semantic expression. |
| `--semantic-threshold T` | 0.3 | Minimum cosine similarity (0.0–1.0). Logtypes below this threshold are excluded even if they would otherwise be in the top-K. |

## How It Works

### Logtypes: The Key Abstraction

During compression, CLP decomposes every log message into a **logtype**
(the static template) and **variables** (the dynamic parts). For example:

```
Raw message:   "Connection to database failed: timeout after 30 seconds"
Logtype:       "Connection to <*> failed: timeout after <*> seconds"
Variables:     ["database", 30]
```

Many messages share the same logtype. An archive with 100 million messages
typically has only 500–5,000 unique logtypes. This logtype dictionary already
exists as a byproduct of CLP's lossless compression — semantic search simply
reuses it.

### Query-Time Flow

1. **Read the logtype dictionary** from the archive (already exists from
   compression).
2. **Embed** the query and all logtypes using bge-small-en-v1.5 via in-process
   ONNX inference — N+1 texts total (N logtypes + 1 query).
3. **Rank by cosine similarity** — dot product between the query vector and each
   logtype vector. Keep the top-K logtypes above the similarity threshold.
4. **Scan** the archive's columnar storage. For each message, check whether its
   logtype ID is in the matched set (O(1) hash lookup per message). No
   decompression needed.

Steps 1–3 run once per archive. Step 4 is a fast scan.

### Why It Scales

The dominant cost is embedding (step 2), and it scales with **unique logtypes**,
not message volume. Logtypes grow logarithmically relative to message count —
a system ingesting 1 billion messages/day typically produces only 1,000–10,000
unique logtypes. This means:

- Embedding cost stays low regardless of data volume
- No stored vectors, no vector index, no ingestion-time embedding
- Adding or removing archives requires no re-indexing — each archive is
  self-contained with its own logtype dictionary

### Logtype-Level Matching

Semantic search operates at the logtype level, not the message level:

- A single embedding covers all messages sharing a logtype
- The model sees the *structure* of the message (with `<*>` placeholders), not
  specific variable values
- `semantic("authentication failure")` matches all messages with logtype
  `"User <*> authentication failed from <*>"` regardless of which user or IP
  appeared

To filter on variable values, combine `semantic()` with KQL field filters
(e.g., `semantic("auth failure") and msg.user: "admin"`).

## How Does It Compare?

### Comparison Table

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

### Why Logs Are Different

Logs are fundamentally repetitive. Traditional vector search treats each record
as unique, but logs reuse the same structural patterns millions of times:

- **LanceDB**: 384-dim float32 per row. 1B messages = **1.4 TB** of vectors.
- **Elasticsearch kNN**: Similar vector overhead, plus HNSW graph.
- **CLP**: Embeds ~5,000 logtypes at query time. No stored vectors. Logtype
  dictionary typically < 1 MB.

### Trade-offs

1. **Logtype granularity** — Matches log patterns, not variable values.
   "Auth failed for user admin" and "...user root" share a logtype and both
   match. Use KQL field filters alongside `semantic()` for variable-level
   filtering.

2. **No pre-built vector index** — Embedding happens at query time. For
   archives with 50K+ logtypes, the embedding call can take seconds. Caching
   mitigates repeated queries.

3. **ClpString fields only** — Semantic search applies to logtype-decomposed
   string fields. Other field types (integers, short strings) use exact or
   wildcard matching.

## Dependencies & Manual Model Preparation

This section is for users who need to prepare model files without the task
system (e.g., custom deployments or CI environments).

### Dependencies

| Component | Version | Notes |
|-----------|---------|-------|
| ONNX Runtime | 1.21.0 | Pre-built from GitHub releases. Provides `libonnxruntime.so`. Architecture-specific (aarch64/x86_64). |
| ONNX Runtime Extensions | 0.14.0 | Built from source with minimal config (BertTokenizer only). Produces `libortextensions.so` (~2.5 MB). |
| BGE Model | bge-small-en-v1.5 | Encoder from HuggingFace BAAI. Tokenizer generated via `prepare_bge_model.py`. |

### Download the Encoder

```bash
mkdir -p /path/to/model
curl -fSL -o /path/to/model/encoder.onnx \
    https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/onnx/model.onnx
```

### Generate tokenizer.onnx

Requires Python with onnxruntime-extensions:

```bash
pip install onnx onnxruntime onnxruntime-extensions transformers
python components/core/tools/scripts/prepare_bge_model.py \
    --output-dir /path/to/model
```

### Build libortextensions.so

```bash
git clone --depth 1 --branch v0.14.0 \
    https://github.com/microsoft/onnxruntime-extensions.git
cd onnxruntime-extensions
mkdir build && cd build
cmake .. \
    -DCMAKE_BUILD_TYPE=Release \
    -DOCOS_BUILD_PYTHON=OFF \
    -DOCOS_ENABLE_VISION=OFF \
    -DOCOS_ENABLE_AUDIO=OFF \
    -DOCOS_ENABLE_AZURE=OFF \
    -DOCOS_ENABLE_MATH=OFF \
    -DOCOS_ENABLE_BERT_TOKENIZER=ON \
    -DONNXRUNTIME_LIB_DIR=/path/to/onnxruntime/lib \
    -DONNXRUNTIME_INCLUDE_DIR=/path/to/onnxruntime/include
cmake --build . --target extensions_shared -j$(nproc)
cp lib/libortextensions.so /path/to/model/
```

### Verify Model Directory

The model directory should contain:

```
/path/to/model/
  encoder.onnx          (~127 MB)
  tokenizer.onnx        (~0.2 MB)
  libortextensions.so   (~2.5 MB)
```

## Limitations and Future Work

- **CPU-only inference**: Uses single-threaded CPU inference. GPU acceleration
  via ONNX Runtime CUDA/TensorRT providers could improve throughput for
  archives with very large logtype dictionaries.

- **Fixed model**: The embedding model (bge-small-en-v1.5) is hardcoded. A more
  flexible design could support configurable models with different
  dimensionalities.

- **No embedding caching**: Logtype embeddings are recomputed for each query.
  Pre-computing and storing embeddings during compression would eliminate
  inference at query time entirely.
