# Semantic Search for clp-s

## Background

clp-s decomposes each log message into a **logtype** (static template with
placeholders) and **encoded variables**. For example:

```
Raw message:   "Connection to database failed: timeout after 30 seconds"
Logtype:       "Connection to \x12 failed: timeout after \x11 seconds"
Variables:     [dict_id_for("database"), 30]
```

Many messages share the same logtype, so the logtype dictionary is orders of
magnitude smaller than the message count. Traditional keyword search
(`*database*`) matches by scanning variable dictionaries and logtype patterns.
However, users often want to find logs by *meaning* rather than exact text --
for example, searching for "database connection error" should also match
messages like "Failed to establish TCP connection to backend service on port
8080" even though the words don't overlap.

## Design Overview

Semantic search adds a `semantic("query text")` function to KQL that matches
log messages whose **logtype** is semantically similar to the query. It works
by:

1. Embedding the query and all logtypes using a transformer model
   (bge-small-en-v1.5)
2. Ranking logtypes by cosine similarity to the query embedding
3. Returning messages whose logtype is among the top-K most similar

Because CLP embeds **logtypes** (typically thousands per archive) rather than
individual messages (potentially millions), the inference cost is manageable
even with in-process CPU inference.

### Key Insight: Logtype-Level Matching

Semantic search operates at the logtype level, not the message level. This
means:

- A single embedding computation covers all messages sharing a logtype
- The embedding model sees the *structure* of the message (with `<*>`
  placeholders), not specific variable values
- Query `semantic("authentication failure")` matches all messages with logtype
  `"User \x12 authentication failed from \x12"` regardless of which user or IP
  appeared in each message

## Query Syntax

The `semantic()` function is a KQL expression:

```
# Basic semantic search (uses default top-K from CLI)
semantic("database connection error")

# With explicit top-K override
semantic("security alert", 10)

# Combined with regular KQL filters
semantic("disk space issue") and msg.host: "prod-server-01"
```

### Grammar

```antlr
semantic_expression
    : 'semantic(' query_text=QUOTED_STRING (',' top_k=UNQUOTED_LITERAL)? ')'
    ;
```

## Architecture

### Components

```
clp-s binary
  |
  +-- CommandLineArguments
  |     --semantic-model-dir <DIR>     (required if query uses semantic())
  |     --semantic-top-k <K>           (default: 5)
  |     --semantic-threshold <THRESH>  (default: 0.3, range [0.0, 1.0])
  |
  +-- OnnxEmbedder (in-process inference)
  |     tokenizer.onnx   -- BertTokenizer custom op (onnxruntime-extensions)
  |     encoder.onnx     -- bge-small-en-v1.5 transformer
  |     libortextensions.so -- custom ops shared library
  |
  +-- KQL Parser
  |     semantic_expression -> FilterExpr(SEMANTIC, SemanticLiteral)
  |
  +-- QueryRunner
        global_init():  populate_semantic_queries() -- embed query + logtypes
        schema_init():  constant_propagate() -- re-map match results
        evaluate():     evaluate_semantic_filter() -- check logtype ID membership
```

### OnnxEmbedder

The `OnnxEmbedder` class performs in-process inference using ONNX Runtime. It
uses a two-model architecture:

1. **Tokenizer session** (`tokenizer.onnx`): Uses `BertTokenizer` custom op
   from onnxruntime-extensions. Takes raw text strings, outputs
   `input_ids`, `token_type_ids`, and `attention_mask` (1D int64 tensors).

2. **Encoder session** (`encoder.onnx`): Standard ONNX ops only. Takes
   reshaped tokenizer outputs `[1, seq_len]`, outputs `last_hidden_state`
   `[1, seq_len, 384]`.

Post-processing is done in C++:
- **Mean pooling**: Average hidden states weighted by attention mask
- **L2 normalization**: Normalize to unit vectors for cosine similarity

Each text is processed individually because the tokenizer produces
variable-length sequences.

```cpp
class OnnxEmbedder {
public:
    explicit OnnxEmbedder(std::string const& model_dir);

    [[nodiscard]] auto embed(std::vector<std::string> const& texts) const
            -> std::vector<std::vector<float>>;

    [[nodiscard]] auto embedding_dim() const -> size_t;
};
```

### SemanticLiteral AST Node

A new `Literal` subclass stores the query text and optional per-expression
top-K override:

```cpp
class SemanticLiteral : public Literal {
    std::string m_query_text;
    std::optional<size_t> m_top_k;  // overrides CLI default if set
};
```

`SemanticLiteral` reports `matches_type(ClpStringT) == true` since semantic
search operates on CLP-encoded string columns (logtypes).

### QueryRunner Integration

Semantic search hooks into QueryRunner's two-phase initialization:

**`global_init()` phase** (once per archive):
1. `populate_string_queries()` skips SEMANTIC filters
2. `populate_semantic_queries()` walks the AST to find all SEMANTIC filters
3. For each unique `(query_text, top_k)` pair, calls
   `match_logtypes_semantically()` which:
   - Cleans logtypes by replacing placeholder bytes with `<*>` and stripping
     escape characters
   - Embeds `[query, logtype_0, logtype_1, ...]` via `OnnxEmbedder::embed()`
   - Ranks by cosine similarity and keeps top-K above threshold
   - Stores results in `m_semantic_match_results`

**`schema_init()` phase** (per schema):
- `constant_propagate()` re-maps SEMANTIC match results for the copied
  expression tree using stable `(query_text, top_k)` keys (not raw pointers,
  which are invalidated by the expression tree copy)

**Evaluation** (per message):
- `evaluate_semantic_filter()` checks if the current message's logtype ID
  appears in the pre-computed match set
- Iterates all CLP string column readers (since semantic filters use wildcard
  columns)

### AST Pass Handling

The SEMANTIC operation requires special handling in existing AST passes:

| Pass | Handling |
|------|----------|
| `NarrowTypes` | Skip (like EXISTS/NEXISTS) -- prevents type erasure |
| `ConstantProp` | Re-map match results, return `Unknown` |
| `populate_string_queries` | Skip -- no logtype/variable dictionary queries |

## Dependencies

### ONNX Runtime (pre-built)

- Version: 1.21.0
- Downloaded from GitHub releases as pre-built tarball
- Provides `libonnxruntime.so` and C++ headers (`onnxruntime_cxx_api.h`)
- Architecture-specific (aarch64 / x86_64)
- Added to taskfile as `deps:onnxruntime` task

### ONNX Runtime Extensions (built from source)

- Version: 0.14.0
- Built with minimal config (text ops only):
  ```
  -DOCOS_BUILD_PYTHON=OFF -DOCOS_ENABLE_VISION=OFF -DOCOS_ENABLE_AUDIO=OFF
  -DOCOS_ENABLE_AZURE=OFF -DOCOS_ENABLE_MATH=OFF -DOCOS_ENABLE_BERT_TOKENIZER=ON
  ```
- Produces `libortextensions.so` (~2.5 MB)
- Loaded at runtime via `RegisterCustomOpsLibrary()` (dlopen)
- Added to taskfile as `deps:onnxruntime-extensions` task

### BGE Model Files

The model directory must contain three files:

| File | Description | Size |
|------|-------------|------|
| `tokenizer.onnx` | BertTokenizer custom op graph | ~0.2 MB |
| `encoder.onnx` | bge-small-en-v1.5 transformer | ~127 MB |
| `libortextensions.so` | Custom ops shared library | ~2.5 MB |

The encoder is downloaded directly from HuggingFace `BAAI/bge-small-en-v1.5`.
The tokenizer.onnx is generated from the HuggingFace tokenizer config using
`components/core/tools/scripts/prepare_bge_model.py`. The `deps:bge-model`
taskfile task automates this entire process.

## CLI Usage

```bash
# Compress logs
clp-s c /path/to/archives /path/to/logs.jsonl

# Semantic search (explicit KQL)
clp-s s /path/to/archives 'semantic("database connection error")' \
    --semantic-model-dir /path/to/bge-model

# With custom parameters
clp-s s /path/to/archives 'semantic("security alert", 10)' \
    --semantic-model-dir /path/to/bge-model \
    --semantic-threshold 0.4

# Combined with keyword search
clp-s s /path/to/archives \
    'semantic("disk space issue") and "*prod*"' \
    --semantic-model-dir /path/to/bge-model

# Natural language (auto-detected when KQL parse fails)
clp-s s /path/to/archives 'database connection timeout' \
    --semantic-model-dir /path/to/bge-model

# NL with field:value extraction
clp-s s /path/to/archives 'auth failures status_code:403' \
    --semantic-model-dir /path/to/bge-model
```

### Natural Language Fallback

When a query is not valid KQL (e.g., multi-word phrases without AND/OR), clp-s
automatically interprets it as natural language. The NL parser extracts
`field:value` pairs and wraps remaining text in `semantic()`. All
sub-expressions are combined with AND.

For example, `auth failures status_code:403` becomes:
`semantic("auth failures") AND status_code: 403`

If the query contains temporal phrases (e.g., "last 2 hours"), a warning is
logged suggesting the use of `--tge`/`--tle` flags instead.

Note: `LD_LIBRARY_PATH` must include the ONNX Runtime library directory at
runtime (e.g., `build/deps/cpp/onnxruntime-src/lib`).

## File Inventory

### New Files

| File | Description |
|------|-------------|
| `search/OnnxEmbedder.hpp` | ONNX Runtime embedder class declaration |
| `search/OnnxEmbedder.cpp` | Two-model inference: tokenize, encode, pool, normalize |
| `search/SemanticSearchUtils.hpp` | `SemanticMatchResult`, similarity utilities |
| `search/SemanticSearchUtils.cpp` | Logtype cleaning, cosine similarity, top-K matching |
| `search/ast/SemanticLiteral.hpp` | AST literal for `semantic()` expressions |
| `search/ast/SemanticLiteral.cpp` | SemanticLiteral factory and print |
| `search/NaturalLanguageParser.hpp` | NL query parser declaration |
| `search/NaturalLanguageParser.cpp` | Regex-based NL → AST expression tree builder |
| `tools/scripts/prepare_bge_model.py` | Generates tokenizer.onnx from HF tokenizer |

### Modified Files

| File | Change |
|------|--------|
| `search/kql/Kql.g4` | Added `semantic_expression` grammar rule |
| `search/kql/generated/*` | Regenerated ANTLR parser |
| `search/kql/kql.cpp` | Added `visitSemantic_expression` visitor |
| `search/ast/FilterOperation.hpp` | Added `SEMANTIC` enum value |
| `search/ast/NarrowTypes.cpp` | Skip narrowing for SEMANTIC |
| `search/QueryRunner.hpp` | `SemanticSearchConfig`, semantic members |
| `search/QueryRunner.cpp` | `populate_semantic_queries`, `evaluate_semantic_filter`, constant prop re-mapping |
| `CommandLineArguments.hpp/cpp` | `--semantic-model-dir`, `--semantic-top-k`, `--semantic-threshold` |
| `clp-s.cpp` | OnnxEmbedder construction, semantic config wiring, NL fallback |
| `search/kql/kql.cpp` | KQL parse errors downgraded to DEBUG for NL fallback |
| `search/CMakeLists.txt` | Added source files, linked onnxruntime |
| `search/ast/CMakeLists.txt` | Added SemanticLiteral sources |
| `CMakeLists.txt` (core) | Find onnxruntime, create imported target |
| `cmake/Options/options.cmake` | `CLP_NEED_ONNXRUNTIME` dependency |
| `taskfiles/deps/main.yaml` | `onnxruntime`, `onnxruntime-extensions`, `bge-model` tasks |

## Limitations and Future Work

- **CPU-only inference**: The current implementation uses single-threaded CPU
  inference. GPU acceleration via ONNX Runtime CUDA/TensorRT execution
  providers could improve throughput for archives with very large logtype
  dictionaries.

- **Fixed model**: The embedding model (bge-small-en-v1.5) is hardcoded. A
  more flexible design could support configurable models with different
  dimensionalities.

- **No embedding caching**: Logtype embeddings are recomputed for each archive.
  Pre-computing and storing embeddings during compression would eliminate
  inference at query time entirely.

- **Single query batching**: Each text is tokenized individually due to
  variable-length sequences. Padding-based batching could improve throughput.
