# Design Document: SQL Search Interface for CLP-S

---

## 1. Motivation

CLP-S supports KQL (Kibana Query Language) as its primary query interface for searching compressed
JSON logs. KQL handles simple key-value and wildcard matching well, but lacks capabilities that are
standard in log analysis workflows: column projection, result limiting, type-filtered access, set
membership (`IN`), range checking (`BETWEEN`), existence tests (`IS NULL`), timestamp literals, and
aggregate functions.

SQL provides all of these in a syntax most engineers already know. This feature adds a SQL query
interface alongside KQL, reusing the existing search AST and evaluation pipeline.

### Background for non-SQL readers

SQL (Structured Query Language) is the standard language for querying relational databases. A SQL
`SELECT` query retrieves data from a table:

```sql
SELECT column1, column2 FROM table WHERE condition LIMIT n
```

- **`SELECT`** specifies which columns to return (`*` = all columns).
- **`FROM`** names the table to query (CLP ignores this since it has no tables).
- **`WHERE`** filters rows using predicates (`=`, `>`, `LIKE`, `BETWEEN`, `IN`, etc.).
- **`LIMIT`** caps the number of rows returned.

**Aggregate functions** compute a single value across matching rows: `COUNT(*)` counts rows,
`MIN(col)` / `MAX(col)` find extremes, `SUM(col)` totals values, and `AVG(col)` averages them.
Multiple aggregates can appear in a single query (e.g., `SELECT MIN(col1), MAX(col2)`).

**Non-reserved keywords** are keywords that can also be used as identifiers (column names). For
example, `TIMESTAMP` is a keyword when followed by a string literal (`TIMESTAMP '2024-01-15'`),
but an identifier when used as a column name (`WHERE timestamp > 100`). This avoids breaking
queries that reference columns named after SQL keywords.

## 2. Architecture

```
┌──────────────┐    ┌──────────────────┐    ┌─────────────────────┐
│   User CLI   │───▶│  SQL Detection   │───▶│   Keyword           │
│  (clp-s s)   │    │  (clp-s.cpp)     │    │   Normalizer        │
└──────────────┘    └──────────────────┘    └─────────┬───────────┘
                                                      │
                                                      ▼
                                           ┌─────────────────────┐
                                           │  ANTLR4 Lexer/Parser│
                                           │  (Sql.g4 → C++)     │
                                           └─────────┬───────────┘
                                                      │ parse tree
                                                      ▼
                                           ┌─────────────────────┐
                                           │  ParseTreeVisitor   │
                                           │  (sql.cpp)          │
                                           └─────────┬───────────┘
                                                      │ SqlQuerySpec
                                                      ▼
                    ┌─────────────────────────────────────────────────┐
                    │  Existing CLP-S Search Pipeline                 │
                    │  ┌──────────┐ ┌────────────┐ ┌──────────────┐  │
                    │  │ OrOfAnd  │▶│ NarrowTypes│▶│ SchemaMatch  │  │
                    │  │ Form     │ │            │ │              │  │
                    │  └──────────┘ └────────────┘ └──────┬───────┘  │
                    │                                      │         │
                    │  ┌──────────────────┐  ┌─────────────▼──────┐  │
                    │  │  Output (filter) │◀─│ QueryRunner        │  │
                    │  │  + result_limit  │  │                    │  │
                    │  └────────┬─────────┘  └────────────────────┘  │
                    │           │                                     │
                    │  ┌────────▼─────────┐                          │
                    │  │  OutputHandler   │ (projection / aggregate) │
                    │  └──────────────────┘                          │
                    └─────────────────────────────────────────────────┘
```

**Design principle: maximal reuse.** SQL parsing produces the same AST (Abstract Syntax Tree) nodes
that KQL produces. By targeting the same AST, SQL queries automatically benefit from all existing
optimization passes, schema matching, query execution, and output handling — none of which required
modification.

## 3. Design Decisions

### 3.1. SQL auto-detection

Queries are identified as SQL by a case-insensitive prefix check for `SELECT` followed by
whitespace. This is unambiguous with KQL, which has no `SELECT` keyword. On detection,
`parse_sql_query()` returns a `SqlQuerySpec`:

```cpp
struct SqlQuerySpec {
    std::shared_ptr<Expression> where_expr;                     // WHERE clause AST (match-all if omitted)
    std::vector<std::string> select_columns;                    // Column projection (empty = SELECT *)
    std::string from_table;                                     // FROM table name (ignored by CLP)
    std::optional<int64_t> limit;                               // LIMIT value
    std::unordered_map<std::string, std::string> column_aliases;// col path → alias (non-aggregate queries only)
    std::vector<AggregateSpec> aggregations;                    // Aggregate functions (non-empty = aggregation query)
};
```

### 3.2. Keyword normalizer

ANTLR4 lexer rules use literal string matches, requiring keywords in uppercase. Rather than making
the grammar case-insensitive (which would produce `SELECT: [sS][eE][lL]...` for every keyword), a
pre-processing normalizer uppercases known keywords while preserving quoted strings verbatim.

This matches the approach used by Presto/Trino (popular distributed SQL query engines). The tradeoff
is that non-reserved keywords (e.g., `TIMESTAMP`, `COUNT`, `MIN`) used as unquoted column names are
uppercased. Users must quote them to preserve case: `WHERE "timestamp" = ...`.

The normalizer's word-boundary detection aligns with the ANTLR `IDENTIFIER` rule:
`(LETTER | '_') (LETTER | DIGIT | '_' | '@' | ':')*`, ensuring identifiers like `@timestamp` are
treated as single tokens.

### 3.3. ANTLR4 grammar

The grammar (`Sql.g4`) is a self-contained file defining both lexer and parser rules, derived from
the Presto SQL grammar and simplified for `SELECT`-only usage.

```
singleStatement → statement → query → querySpecification
                                         ├─ SELECT selectItem+
                                         ├─ FROM qualifiedName?
                                         ├─ WHERE booleanExpression?
                                         └─ LIMIT INTEGER_VALUE?
```

Key grammar design decisions:

1. **Arithmetic rules parsed but rejected:** The grammar includes `arithmeticUnary` and
   `arithmeticBinary` so ANTLR produces a clean parse tree for `WHERE x + 1 = 2`. The visitor
   then rejects it with a clear error, rather than a cryptic ANTLR syntax error.

2. **Non-reserved keywords** (listed in Section 3.2) appear in the grammar's `nonReserved` rule,
   allowing them to be used as column names. Disambiguation is context-based (e.g., `COUNT`
   followed by `(` is an aggregate; otherwise it is an identifier).

3. **CLP functions as keywords:** `CLP_GET_*` tokens precede `IDENTIFIER` in the lexer (ANTLR
   matches the first applicable rule). Case-insensitivity is handled by the normalizer.

4. **Aggregate rules share labels:** `ARBITRARY` and `ANY_VALUE` share the `#aggArbitrary` label,
   generating a single visitor method for both — no duplicate handling needed.

5. **`UNRECOGNIZED` catch-all:** A final lexer rule catches unmatched characters, enabling better
   parser error messages.

Generated C++ files are checked into `search/sql/generated/` (same approach as KQL).

### 3.4. AST translation

The `ParseTreeVisitor` translates SQL constructs into the same CLP-S search AST nodes that KQL
uses. Key translations:

- `BETWEEN` → `AndExpr` of `GTE` and `LTE` filters
- `IN` → `OrExpr` of equality filters
- `LIKE` → equality filter with CLP wildcards (`%` → `*`, `_` → `?`)
- `IS NULL` → inverted `EXISTS` filter
- `TIMESTAMP '...'` / `DATE '...'` → `TimestampLiteral(epoch_ms)` via `TimestampParser`
- No WHERE clause → match-all wildcard filter

This reuse means all downstream optimization passes work without modification:

- **OrOfAndForm** — normalizes boolean logic into a disjunctive normal form for efficient evaluation
- **NarrowTypes** — resolves literal types to match column schemas
- **ConvertToExists** — optimizes `IS NULL` / `IS NOT NULL` checks
- **SchemaMatch** — maps column names to physical column IDs in the archive
- **EvaluateTimestampIndex** — prunes archives using timestamp range indexes

### 3.5. Aggregate function design

```cpp
enum class AggregateKind { CountStar, Count, Min, Max, Sum, Avg, Arbitrary };

struct AggregateSpec {
    AggregateKind kind;
    std::string column;  // empty for CountStar; dotted path for others
    std::string alias;   // from AS alias, or auto-generated default
};
```

`SqlQuerySpec::aggregations` is non-empty when the query is an aggregation query. The parser
enforces mutual exclusivity with `select_columns` — mixing aggregate and non-aggregate items
in `SELECT` is rejected.

#### Aggregation via OutputHandler

Aggregation is implemented as a `LocalAggregateOutputHandler` subclass of `OutputHandler`. This
achieves it with **zero changes** to `Output`, `SchemaReader`, or `QueryRunner`.

**`should_marshal_records()` optimization:** The `OutputHandler` base class controls whether
`SchemaReader` serializes matched records to JSON before calling `write()`. For `COUNT(*)`-only
queries, serialization is skipped entirely — each `write()` call represents one matching record
with no JSON overhead. All other aggregates require serialization so the handler can extract
column values via simdjson.

**Type handling for mixed-type columns:**

- `MIN`/`MAX`: numeric values take precedence over string values, avoiding undefined cross-type
  comparison semantics. String-only `MIN`/`MAX` is supported when no numeric values exist.
- `SUM`/`AVG`: only operate on numeric values; non-numeric values are silently skipped.
- `ARBITRARY`/`ANY_VALUE`: returns the first non-null scalar value (strings, numbers, booleans).
  JSON objects and arrays are skipped. Supporting both function names reduces friction for users
  from different SQL dialect backgrounds (Presto vs. MySQL/BigQuery).

**JSON value extraction:** The handler uses simdjson's on-demand API — a forward-only streaming
JSON parser optimized for extracting a few fields without fully materializing the document. Because
the parser is forward-only (each field can only be visited once), each aggregate that needs a
different field must re-iterate the document. This is acceptable because on-demand parsing is fast
and each aggregate typically extracts just one field.

**Scope:** The `clp-s` binary operates on a single archive. `LocalAggregateOutputHandler` computes
aggregates over all matched records within that archive and emits one JSON result line on `finish()`.
Cross-archive aggregation is outside the scope of `clp-s` and deferred to future work.

### 3.6. Column aliases and SELECT * semantics

#### Alias renaming (AS)

Renaming is implemented via `RenamingOutputHandler`, a decorator that wraps any `OutputHandler`
and post-processes JSON records before delegating. This avoids threading alias information through
the `Projection → SchemaReader → JSON generation` pipeline. The tradeoff is per-record JSON
parse-and-reserialize overhead, which is acceptable because alias renaming is an opt-in feature.

**Differences from standard SQL aliasing:**

| Behavior | Standard SQL | CLP |
|----------|-------------|-----|
| Top-level rename (`col AS alias`) | Renames column header | Renames JSON key |
| Nested rename (`a.b AS alias`) | Renames column header (flat result set) | Extracts value, flattens to top level |
| `SELECT *, col AS alias` | All columns + additional aliased duplicate | All columns with aliased column renamed in place (no duplication) |

The key difference arises from CLP's JSON output format. In standard SQL, a result set is a flat
table — aliases simply rename column headers. In CLP, records are nested JSON objects, so aliasing
a nested field (`meta.latency AS lat`) must extract and flatten it. Users writing
`SELECT meta.latency AS lat` expect `{"lat": 42}`, not `{"meta": {"latency": 42}}`.

#### SELECT * mixed with named columns

When `*` appears anywhere in the SELECT clause (e.g., `SELECT *, col` or `SELECT col, *`), the
result is the superset — all columns are returned. Any aliases from named columns are still
preserved and applied. This differs from standard SQL where `SELECT *, col AS alias` returns all
columns **plus** an additional column. CLP does not duplicate values — the alias renames in place.

### 3.7. LIMIT and aggregation interaction

When aggregation specs are present, `LIMIT` is not passed to `search_archive()`, ensuring the full
dataset is scanned for correct aggregate results. Column projection is also ignored for aggregation
queries.

LIMIT for non-aggregate queries is enforced in `Output::filter()`. LIMIT only applies to archive
searches; KV-IR streams do not currently accept a limit parameter.

## 4. Known Limitations

1. Non-reserved keyword case-folding: users must quote identifiers to preserve case (see 3.2).
2. `SELECT DISTINCT` is rejected at parse time.
3. No `GROUP BY`, `HAVING`, or `ORDER BY`.
4. No `LIMIT` for KV-IR streams.
5. `ARBITRARY`/`ANY_VALUE` result is non-deterministic across runs.
6. Arithmetic expressions in `WHERE` are rejected (grammar parses them for clear errors; see 3.3).

## 5. Future Work

1. **Cross-archive aggregate merging.** Since `clp-s` operates on a single archive, a higher-level
   coordinator would be needed to merge results across archives. `COUNT`, `SUM`, `MIN`, and `MAX`
   merge trivially; `AVG` requires `SUM` + `COUNT` per archive to compute a weighted average.
2. **`ORDER BY` support.** Requires materializing and sorting results. Pairs well with `LIMIT`
   for "top-N" queries (e.g., `ORDER BY latency DESC LIMIT 10`).
3. **`GROUP BY` / `HAVING` support.** Requires hash-based grouping, which conflicts with CLP's
   streaming architecture. Could be a post-processing step over materialized results.
4. **`LIMIT` for KV-IR streams.** The KV-IR search path does not currently accept a limit parameter.
5. **`COUNT(DISTINCT col)`.** Requires an unbounded hash set of seen values.
6. **Arithmetic expressions in `WHERE`.** The grammar already parses them (for clean error messages);
   implementing evaluation would enable queries like `WHERE latency_ms / 1000 > 5`.

---

## Appendix A. Current Implementation's Feature Capability Matrix

A fully working implementation is available at
https://github.com/jackluo923/clp/commits/feat/sql-search/.

Quick start examples:

```bash
# Filter with timestamp range and column projection
./clp-s s /path/to/archive \
  "SELECT level, status, msg FROM logs
   WHERE \"@timestamp\" BETWEEN TIMESTAMP '2024-01-15 00:00:00' AND TIMESTAMP '2024-01-16 00:00:00'
     AND status BETWEEN 400 AND 599
   LIMIT 50"

# Aggregate: count errors and get latency stats in one query
./clp-s s /path/to/archive \
  "SELECT COUNT(*) AS total, MIN(latency_ms) AS min_lat, MAX(latency_ms) AS max_lat
   FROM logs WHERE level IN ('ERROR', 'FATAL')"

# Flatten nested JSON fields with aliases
./clp-s s /path/to/archive \
  "SELECT meta.region AS region, meta.latency AS lat FROM logs WHERE meta.region = 'us-east-1'"
# Output: {"region": "us-east-1", "lat": 42}  (nested paths flattened to top level)

# Type-filtered access and wildcard column search
./clp-s s /path/to/archive \
  "SELECT * FROM logs WHERE CLP_GET_INT('status') >= 500 AND CLP_WILDCARD_COLUMN() LIKE '%timeout%'"
```

Every entry marked "Not supported" includes a rationale tied to CLP's architectural constraints.

### Statements

| Feature | Status | Rationale |
|---------|--------|-----------|
| `SELECT` | Supported | Only statement type supported |
| `INSERT` / `UPDATE` / `DELETE` | Not supported | CLP is a read-only compressed log store |
| DDL (`CREATE TABLE`, etc.) | Not supported | CLP has no mutable schema |

### SELECT clause

| Feature | Status | Rationale |
|---------|--------|-----------|
| `SELECT *` | Supported | Returns full records |
| `SELECT col1, col2` | Supported | Column projection |
| `SELECT col AS alias` | Supported | Renames field in JSON output; nested columns are flattened to top level |
| `SELECT col alias` | Supported | Implicit alias (without `AS` keyword) |
| `SELECT *, col` | Supported | Superset: returns all columns (same as `SELECT *`) |
| `SELECT *, col AS alias` | Supported | Returns all columns with the aliased column renamed in place |
| `SELECT DISTINCT` | Not supported | Requires materializing and deduplicating all results |
| `SELECT expr + expr` | Not supported | No arithmetic expression evaluation |

### Aggregate functions

| Feature | Status | Rationale |
|---------|--------|-----------|
| `COUNT(*)` | Supported | Optimized path: skips JSON serialization |
| `COUNT(col)` | Supported | Counts non-null occurrences only |
| `COUNT(DISTINCT col)` | Not supported | Requires unbounded hash set of seen values |
| `MIN(col)` | Supported | Numeric and string; numeric takes precedence on mixed-type columns |
| `MAX(col)` | Supported | Numeric and string; numeric takes precedence on mixed-type columns |
| `SUM(col)` | Supported | Numeric only; non-numeric values silently skipped |
| `AVG(col)` | Supported | Numeric only; returns `null` if no numeric values found |
| `ARBITRARY(col)` | Supported | Presto-style; returns first non-null scalar value (objects/arrays skipped) |
| `ANY_VALUE(col)` | Supported | MySQL/BigQuery-style alias for `ARBITRARY` |
| `AS alias` on aggregates | Supported | Default alias auto-generated if omitted (e.g., `count(*)`) |
| Aggregates in `WHERE` | Not supported | Standard SQL uses `HAVING`, which depends on `GROUP BY` |
| Mixed aggregate + non-aggregate in `SELECT` | Not supported | Implies implicit `GROUP BY`, which is unsupported |
| Nested aggregates (e.g., `MAX(COUNT(*))`) | Not supported | No nested aggregate evaluation |

### FROM clause

| Feature | Status | Rationale |
|---------|--------|-----------|
| `FROM table` | Supported | Table name accepted but ignored (CLP has no tables) |
| `FROM` omitted | Supported | Clause is entirely optional |
| `FROM db.schema.table` | Supported | Dotted names accepted but ignored |
| `JOIN` | Not supported | CLP has no relational join model |
| Subqueries in `FROM` | Not supported | No subquery support |

### WHERE clause

| Feature | Status | Rationale |
|---------|--------|-----------|
| Comparison (`=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`) | Supported | All standard operators |
| `AND` / `OR` / `NOT` | Supported | With parenthesized grouping |
| `LIKE` / `NOT LIKE` | Supported | `%` and `_` converted to CLP wildcards `*` and `?` |
| `BETWEEN` / `NOT BETWEEN` | Supported | Inclusive range; translated to `>= AND <=` |
| `IN` / `NOT IN` | Supported | Translated to `OR` of equality checks |
| `IS NULL` / `IS NOT NULL` | Supported | Tests column existence |
| `TIMESTAMP '...'` / `DATE '...'` | Supported | Parsed to epoch milliseconds via `TimestampParser` |
| Arithmetic (`col + 1 = 2`) | Not supported | Grammar parses it for clear error messages, visitor rejects |
| `EXISTS` subquery | Not supported | No subquery support |
| `CASE WHEN` | Not supported | No conditional expression support |

### GROUP BY / HAVING / ORDER BY

| Feature | Status | Rationale |
|---------|--------|-----------|
| `GROUP BY` | Not supported | Requires hash-based grouping; conflicts with CLP's streaming architecture |
| `HAVING` | Not supported | Depends on `GROUP BY` |
| `ORDER BY` | Not supported | Requires materializing and sorting all results |

### LIMIT

| Feature | Status | Rationale |
|---------|--------|-----------|
| `LIMIT n` | Supported | Caps result count for non-aggregate queries |
| `LIMIT` with aggregates | Ignored | Aggregate queries scan all matching records |
| `OFFSET` | Not supported | Requires materializing skipped results |
| `LIMIT` for KV-IR streams | Not supported | KV-IR search path does not accept a limit parameter |

### Literals and types

| Feature | Status | Rationale |
|---------|--------|-----------|
| String (`'hello'`) | Supported | Single-quoted; `''` escapes literal `'` |
| Integer (`42`, `-5`) | Supported | |
| Decimal (`3.14`) | Supported | |
| Boolean (`TRUE`, `FALSE`) | Supported | Case-insensitive |
| `NULL` | Supported | Used with `IS NULL` / `IS NOT NULL` |
| `TIMESTAMP '...'` | Supported | Converts to epoch milliseconds |
| `DATE '...'` | Supported | Converts to epoch milliseconds at midnight |
| Array / Map literals | Not supported | No complex type literals |

### Column references

| Feature | Status | Rationale |
|---------|--------|-----------|
| Simple identifier (`col`) | Supported | |
| Dotted path (`a.b.c`) | Supported | Nested JSON field access |
| Double-quoted (`"col"`) | Supported | Preserves case and special characters |
| Backtick-quoted (`` `col` ``) | Supported | MySQL-style quoting |
| `@` / `:` in identifiers | Supported | Common in log schemas (e.g., `@timestamp`) |
| `CLP_GET_INT/FLOAT/STRING/BOOL` | Supported | Type-filtered column access |
| `CLP_GET_JSON_STRING(col)` | Supported | Any-type access, returns JSON |
| `CLP_GET_JSON_STRING()` | Supported | Entire record as JSON (SELECT only) |
| `CLP_WILDCARD_COLUMN()` | Supported | Matches any column (equivalent to KQL `*`) |

### Other SQL features

| Feature | Status | Rationale |
|---------|--------|-----------|
| Case-insensitive keywords | Supported | Pre-processing normalizer uppercases keywords before parsing |
| SQL comments (`--`, `/* */`) | Supported | Routed to HIDDEN channel by lexer |
| `UNION` / `INTERSECT` / `EXCEPT` | Not supported | No set operations |
| Window functions (`OVER`) | Not supported | Requires partitioned ordered evaluation |
| CTEs (`WITH ... AS`) | Not supported | No common table expression support |
| User-defined functions | Not supported | Only built-in CLP functions |
| Type casting (`CAST(x AS INT)`) | Not supported | Use `CLP_GET_*` functions instead |

## Appendix B. Error Conditions

| Error condition | Behavior |
|---|---|
| ANTLR lexer/parser error | Logged, return `nullopt` |
| Visitor exception (semantic) | Caught, logged, return `nullopt` |
| Invalid timestamp string | Exception (caught by visitor) |
| Arithmetic in WHERE | Exception with descriptive message |
| Empty field name in CLP_GET_* | Exception with descriptive message |
| CLP_GET_JSON_STRING() in WHERE | Exception with descriptive message |
| Aggregate in WHERE clause | Exception with descriptive message |
| `SELECT DISTINCT` | Exception with descriptive message |
| Mixed aggregate + non-aggregate SELECT | Logged, return `nullopt` |

All visitor-level exceptions are caught in `parse_sql_query()`, logged, and converted to `nullopt`.

## Appendix C. Test Coverage

| Area | Coverage |
|---|---|
| Query metadata parsing | SELECT columns, FROM, LIMIT, optional FROM, no WHERE |
| Case-insensitive keywords | Keywords and CLP functions in mixed case |
| Comparison operators | All 7 operators |
| Literal types | String, int, decimal, boolean, NULL |
| Column references | Simple, dotted, CLP_GET_*, wildcards, quoted identifiers |
| CLP_WILDCARD_COLUMN | Equality, comparison, LIKE, IS NULL, boolean logic |
| Boolean logic | AND, OR, NOT, parenthesized |
| BETWEEN / IN / LIKE predicates | Normal and NOT variants, escaping |
| TIMESTAMP and DATE literals | ISO-8601, milliseconds, DATE, BETWEEN, backward compat |
| Aggregate functions | All aggregate kinds, AS alias, dotted paths, WHERE guard, mixed error |
| LocalAggregateOutputHandler | COUNT, MIN/MAX (numeric + string), SUM, AVG, ARBITRARY, null handling |
| SELECT * mixed with columns | `*` alone, `*, col`, `col, *`, `*, col AS alias`, multiple `*` |
| Column aliases | Simple, multiple, nested path, implicit AS |
| RenamingOutputHandler | Top-level rename, nested flatten, empty parent cleanup |
| Error handling | Invalid SQL, arithmetic, unsupported features, SELECT DISTINCT |

