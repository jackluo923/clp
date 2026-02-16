# Getting Started: Semantic Search

This guide walks through setting up and running semantic search end-to-end.

## Prerequisites

- Docker (for building clp-s and dependencies)
- A CLP archive created with `clp-s c`

## 1. Build Dependencies and clp-s

Build inside the CLP Docker container using the task system. This downloads
ONNX Runtime, builds onnxruntime-extensions from source, downloads the BGE
model files, and builds clp-s:

```bash
docker run --rm -it \
  -v /path/to/clp:/clp \
  -w /clp \
  clp-core-dependencies-aarch64-manylinux_2_28:dev \
  bash
```

Inside the container:

```bash
# Install all dependencies (including ONNX Runtime, ortextensions, BGE model)
task deps:core

# Build clp-s
task core
```

The `deps:core` task handles:
- **onnxruntime**: Downloads pre-built ONNX Runtime 1.21.0
- **onnxruntime-extensions**: Builds from source (C++ only, BertTokenizer enabled)
- **bge-model**: Downloads bge-small-en-v1.5 encoder, generates tokenizer.onnx,
  assembles model directory at `build/deps/cpp/bge-small-en-v1.5/`

## 2. Compress Logs

```bash
clp-s c /path/to/archives /path/to/logs.jsonl
```

## 3. Run Semantic Search

```bash
# Set library path for ONNX Runtime
export LD_LIBRARY_PATH=/clp/build/deps/cpp/onnxruntime-src/lib

# Basic semantic search
clp-s s /path/to/archives 'semantic("database connection error")' \
    --semantic-model-dir /clp/build/deps/cpp/bge-small-en-v1.5
```

The `--semantic-model-dir` points to the directory containing `tokenizer.onnx`,
`encoder.onnx`, and `libortextensions.so`. When built via `task deps:core`,
this is `build/deps/cpp/bge-small-en-v1.5/`.

## Query Syntax

### Natural Language (Recommended for Getting Started)

Just type what you're looking for — clp-s auto-detects natural language when
the query isn't valid KQL:

```bash
# Simple natural language query
clp-s s archives 'database connection timeout' \
    --semantic-model-dir /path/to/model

# With field:value pairs (extracted automatically)
clp-s s archives 'auth failures status_code:403' \
    --semantic-model-dir /path/to/model

# Field-number pairs are also detected (field name must contain _ or .)
clp-s s archives 'errors on status_code 500' \
    --semantic-model-dir /path/to/model
```

Natural language queries are parsed as follows:
1. `field:value` pairs are extracted as exact-match filters
2. Filler words ("show me", "find", etc.) are stripped
3. Remaining text is wrapped in `semantic()` for embedding-based search
4. All sub-expressions are combined with AND

If the query contains temporal phrases (e.g., "last 2 hours"), a warning
suggests using `--tge`/`--tle` flags to specify a time range.

### Explicit KQL Syntax

The `semantic()` function can also be used directly in KQL expressions for
more control:

```bash
# Basic — uses default top-K (5) and threshold (0.3)
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

## CLI Options

| Option | Default | Description |
|--------|---------|-------------|
| `--semantic-model-dir DIR` | (none) | Directory with tokenizer.onnx, encoder.onnx, libortextensions.so. Required if query uses `semantic()`. |
| `--semantic-top-k K` | 5 | Number of top-matching logtypes to return |
| `--semantic-threshold T` | 0.3 | Minimum cosine similarity floor (0.0–1.0) |

## How It Works

Semantic search operates on **logtypes**, not individual messages:

1. CLP decomposes each message into a logtype (template) and variables
2. At query time, the query and all logtypes are embedded using bge-small-en-v1.5
3. Logtypes are ranked by cosine similarity to the query
4. Messages whose logtype is in the top-K are returned

Since archives typically have thousands of logtypes (not millions of messages),
in-process CPU inference is fast enough. On aarch64:

| Phase | Time |
|-------|------|
| Model loading (one-time) | ~250 ms |
| Embedding 16 texts | ~155 ms |
| Per-text embedding | ~10 ms |

The model is only loaded when the query contains a `semantic()` expression.
Regular keyword searches have zero overhead.

## Manual Model Preparation

If you need to prepare model files without the task system:

### Download encoder

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

### Verify model directory

The model directory should contain:

```
/path/to/model/
  encoder.onnx          (~127 MB)
  tokenizer.onnx        (~0.2 MB)
  libortextensions.so   (~2.5 MB)
```
