# clp-json SQL search syntax

In addition to [KQL](reference-json-search-syntax), CLP supports a subset of SQL for querying JSON
logs. SQL queries are auto-detected when the query string starts with `SELECT` (case-insensitive)
followed by whitespace.

## Supported syntax

```sql
SELECT [columns | * | aggregate functions | CLP functions]
[FROM <table>]
[WHERE <conditions>]
[LIMIT <n>]
```

* `columns` is a comma-separated list of column names or CLP functions.
* `table` is any valid identifier. CLP does not have a concept of tables, so this value is ignored.
  The `FROM` clause itself is optional and can be omitted entirely. The following queries are all
  equivalent:
  ```sql
  SELECT * FROM logs WHERE level = 'ERROR'
  SELECT * FROM my_data WHERE level = 'ERROR'
  SELECT * WHERE level = 'ERROR'
  ```
* `conditions` is a boolean expression of predicates.
* `n` is the maximum number of results to return.

## Column references

Columns can be referenced in several ways:

```sql
-- Simple column name
WHERE level = 'ERROR'

-- Nested (dotted) column name
WHERE meta.region = 'us-east-1'

-- Deeply nested column name
WHERE meta.app.version = '2.1.0'

-- Quoted identifier (for column names starting with @ or containing special characters)
WHERE "@timestamp" > TIMESTAMP '2024-01-15 10:30:00'
WHERE "my-column" = 'value'

-- Backtick-quoted identifier (MySQL-style)
WHERE `my-column` = 'value'
```

Unquoted identifiers must start with a letter or underscore and may contain letters, digits,
underscores, `@`, and `:`. Column names that start with `@` or contain other special characters
must be quoted with double quotes or backticks.

:::{note}
Non-reserved keywords (`TIMESTAMP`, `DATE`, `ALL`, `AS`, `DISTINCT`, `LIMIT`, `COUNT`, `MIN`,
`MAX`, `SUM`, `AVG`, `ANY_VALUE`, `ARBITRARY`) are case-folded to uppercase when used as unquoted
column names. If your data has a lowercase column named `timestamp` or `date`, use double quotes to
preserve the original case:
```sql
-- Matches column "timestamp" (lowercase) in the data
WHERE "timestamp" > TIMESTAMP '2024-01-15 10:30:00'

-- Matches column "TIMESTAMP" (uppercase) — may not match lowercase data
WHERE timestamp > TIMESTAMP '2024-01-15 10:30:00'
```
:::

## Literal types

SQL queries support the following literal types:

| Type | Example | Notes |
|------|---------|-------|
| String | `'hello'`, `'it''s'` | Single-quoted; use `''` to escape a literal `'` |
| Integer | `42`, `-5` | Matches integer columns |
| Decimal | `3.14`, `-0.5` | Matches float columns |
| Boolean | `TRUE`, `FALSE` | Case-insensitive |
| NULL | `NULL` | Used with `IS NULL` / `IS NOT NULL` |
| Timestamp | `TIMESTAMP '2024-01-15 10:30:00'` | Presto-style timestamp literal |
| Date | `DATE '2024-01-15'` | Date-only (implies `00:00:00`) |

:::{note}
In SQL, string values **must** be quoted with single quotes. Unquoted values (other than numbers,
booleans, and NULL) are interpreted as column references, not strings.
:::

## Timestamp and date literals

`TIMESTAMP` and `DATE` literals provide a way to compare columns against timestamp values without
using raw epoch integers. Both keywords are case-insensitive.

```sql
-- Presto-style timestamp literal
SELECT * FROM logs WHERE "@timestamp" > TIMESTAMP '2024-01-15 10:30:00'

-- Date-only shorthand (implies 00:00:00)
SELECT * FROM logs WHERE "@timestamp" >= DATE '2024-01-15'

-- TIMESTAMP also accepts date-only strings (implies 00:00:00)
SELECT * FROM logs WHERE "@timestamp" >= TIMESTAMP '2024-01-15'

-- Works in BETWEEN
SELECT * FROM logs
WHERE "@timestamp" BETWEEN TIMESTAMP '2024-01-01 00:00:00' AND TIMESTAMP '2024-12-31 23:59:59'
```

:::{note}
`TIMESTAMP` and `DATE` are non-reserved keywords — they can still be used as column names.
When followed by a string literal, they are interpreted as timestamp literals.
:::

## Comparison operators

| Operator | Meaning |
|----------|---------|
| `=` | Equal to |
| `!=`, `<>` | Not equal to |
| `<` | Less than |
| `<=` | Less than or equal to |
| `>` | Greater than |
| `>=` | Greater than or equal to |

Example:

```sql
SELECT * FROM logs WHERE status >= 400 AND latency_ms > 1000.0
```

## Boolean logic

Conditions can be combined using `AND`, `OR`, `NOT`, and parentheses:

```sql
SELECT * FROM logs
WHERE (level = 'ERROR' OR level = 'FATAL') AND NOT user = 'bot'
```

## LIKE predicate

`LIKE` performs pattern matching on string values. SQL wildcards are automatically converted to CLP
wildcards:

| SQL wildcard | Meaning | CLP equivalent |
|-------------|---------|----------------|
| `%` | Match any sequence of characters | `*` |
| `_` | Match any single character | `?` |

Escape SQL wildcards with a backslash to match them literally: `\%`, `\_`.

Example:

```sql
SELECT * FROM logs WHERE msg LIKE '%timeout%'
SELECT * FROM logs WHERE msg NOT LIKE 'test_%'
```

## BETWEEN predicate

`BETWEEN` checks if a value is within an inclusive range:

```sql
SELECT * FROM logs WHERE latency_ms BETWEEN 100 AND 500
SELECT * FROM logs WHERE status NOT BETWEEN 200 AND 299
```

## IN predicate

`IN` checks if a value matches any value in a list:

```sql
SELECT * FROM logs WHERE level IN ('ERROR', 'FATAL', 'WARN')
SELECT * FROM logs WHERE status NOT IN (200, 201, 204)
```

## IS NULL / IS NOT NULL

Check whether a column exists in the log event:

```sql
-- Find log events where the column does NOT exist
SELECT * FROM logs WHERE error_code IS NULL

-- Find log events where the column exists
SELECT * FROM logs WHERE error_code IS NOT NULL
```

## CLP functions

CLP provides special functions for type-filtered column access and wildcard matching.

### Type-filtered column access

These functions access a column and restrict matching to the specified type:

| Function | Type filter |
|----------|------------|
| `CLP_GET_INT(field)` | Integer |
| `CLP_GET_FLOAT(field)` | Float |
| `CLP_GET_STRING(field)` | String |
| `CLP_GET_BOOL(field)` | Boolean |
| `CLP_GET_JSON_STRING(field)` | Any type (returns JSON) |
| `CLP_GET_JSON_STRING()` | Entire record as JSON (SELECT only) |

The field argument can be a string literal, an identifier, or a dotted path:

```sql
-- String literal (supports escape sequences)
WHERE CLP_GET_INT('status') >= 400

-- Dotted path via string literal
WHERE CLP_GET_STRING('meta.region') = 'us-east-1'

-- Dotted path via identifier
WHERE CLP_GET_STRING(meta.region) = 'us-east-1'

-- Wildcard in field path
WHERE CLP_GET_STRING('meta.*') = 'us-east-1'

-- Escaped dot (matches a literal dot in the column name)
WHERE CLP_GET_STRING('app\.name') = 'web'
```

### Wildcard column matching

`CLP_WILDCARD_COLUMN()` matches any column in the log event, equivalent to KQL's `*:value` syntax:

```sql
-- Find log events where ANY column equals 'error'
SELECT * FROM logs WHERE CLP_WILDCARD_COLUMN() = 'error'

-- Find log events where ANY column contains 'timeout'
SELECT * FROM logs WHERE CLP_WILDCARD_COLUMN() LIKE '%timeout%'

-- Combine with other conditions
SELECT * FROM logs WHERE CLP_WILDCARD_COLUMN() = 'error' AND status = 500
```

## Aggregate functions

CLP supports the following SQL aggregate functions in the `SELECT` clause:

| Function | Description | Output type |
|----------|-------------|-------------|
| `COUNT(*)` | Count all matching records | Integer |
| `COUNT(col)` | Count records where `col` exists and is non-null | Integer |
| `MIN(col)` | Minimum value of `col` (numeric or string) | Number or string |
| `MAX(col)` | Maximum value of `col` (numeric or string) | Number or string |
| `SUM(col)` | Sum of numeric values in `col` | Number |
| `AVG(col)` | Average of numeric values in `col` | Number |
| `ARBITRARY(col)` | First non-null scalar value from `col` (Presto-style) | Scalar |
| `ANY_VALUE(col)` | Alias for `ARBITRARY(col)` (MySQL/BigQuery-style) | Scalar |

All aggregate functions support the `AS` keyword for aliasing. If no alias is provided, a default
alias is generated (e.g., `count(*)`, `min(latency)`).

```sql
-- Count matching records
SELECT COUNT(*) AS cnt FROM logs WHERE level = 'ERROR'

-- Multiple aggregates in one query
SELECT MIN(latency_ms) AS min_lat, MAX(latency_ms) AS max_lat FROM logs WHERE status >= 500

-- Sum and average
SELECT SUM(bytes) AS total, AVG(latency_ms) AS avg_lat FROM logs

-- Nested column paths
SELECT MIN(meta.latency) FROM logs

-- Count records that have a specific field
SELECT COUNT(error_code) FROM logs

-- First non-null value (useful for sampling)
SELECT ARBITRARY(region) AS sample_region, COUNT(*) AS cnt FROM logs
```

Aggregate queries output a JSON object containing all requested aggregate values:
```json
{"cnt": 42, "min_lat": 1.2, "max_lat": 998.5}
```

:::{note}
CLP stores data across multiple archives. Each archive produces its own aggregate result line,
so the output may contain multiple JSON lines (one per archive). To obtain a single global result,
merge the per-archive outputs externally (e.g., sum the counts, take the overall min/max). Note
that `AVG` results cannot be correctly merged from per-archive averages alone — use `SUM` and
`COUNT` instead, then compute the average externally.
:::

### Type handling for aggregates

- **`COUNT(*)`**: Does not require record serialization — each matching record increments the count.
  This is the most efficient aggregate.
- **`COUNT(col)`**: Counts only records where the column exists and is not null.
- **`SUM` / `AVG`**: Only operate on numeric values. Non-numeric values in the column are silently
  skipped. If no numeric values are found, the result is `null`.
- **`MIN` / `MAX`**: Support both numeric and string values. Numeric comparisons take precedence
  over string comparisons (lexicographic). If only string values are found, string comparison is
  used. If no values are found, the result is `null`.
- **`ARBITRARY` / `ANY_VALUE`**: Returns the first non-null scalar value encountered (strings,
  numbers, booleans). JSON objects and arrays in the column are skipped. If no non-null scalar
  values exist, the result is `null`.
- **`AVG`** with zero matching numeric records outputs `null` (not an error).

### Restrictions

- Aggregate functions are **not allowed in the WHERE clause**. Standard SQL uses `HAVING` for
  filtering on aggregates, which CLP does not support.
- Mixing aggregate and non-aggregate columns in the same `SELECT` clause is not allowed. This would
  require `GROUP BY` support, which CLP does not implement.
- `SELECT DISTINCT` is not supported.
- `GROUP BY`, `HAVING`, and `ORDER BY` are not supported. Aggregate queries cover all matching
  records without grouping.

## SELECT clause

The `SELECT` clause specifies which columns to include in the output:

```sql
-- Select all columns
SELECT * FROM logs

-- Select specific columns
SELECT level, status, msg FROM logs

-- Select using CLP functions
SELECT CLP_GET_INT('status'), CLP_GET_STRING('msg') FROM logs

-- Dump entire record as JSON
SELECT CLP_GET_JSON_STRING() FROM logs
```

### Column aliases (AS)

Columns can be renamed in the output using `AS` (or the implicit alias syntax without `AS`):

```sql
-- Rename a top-level column
SELECT level AS severity, status AS code FROM logs
-- Output: {"severity": "ERROR", "code": 500}

-- Rename a nested column (flattened to top level)
SELECT meta.latency AS lat FROM logs
-- Output: {"lat": 42}

-- Implicit alias (without AS keyword)
SELECT level severity FROM logs
-- Output: {"severity": "ERROR"}

-- Mix aliased and non-aliased columns
SELECT level AS severity, status FROM logs
-- Output: {"severity": "ERROR", "status": 500}

-- Quoted identifiers with special characters
SELECT "@timestamp" AS ts, "my column" AS col FROM logs

-- Backtick-quoted identifiers
SELECT `@timestamp` AS ts FROM logs

-- Quoted alias name
SELECT level AS "my severity" FROM logs
```

:::{note}
Aliasing nested columns (e.g., `meta.latency AS lat`) extracts the value and places it at the top
level under the alias name. This differs from standard SQL, where column aliases only rename the
output column header without changing the structure. CLP flattens because its output is JSON — a
nested path like `meta.latency` would otherwise produce `{"meta": {"latency": 42}}`, and renaming
it to `lat` naturally produces `{"lat": 42}`. Empty parent objects are removed after extraction.
:::

### Mixing `*` with named columns

When `SELECT *` appears alongside named columns, the result is the **superset** — all columns are
returned:

```sql
-- These all return the full record:
SELECT * FROM logs
SELECT *, level FROM logs
SELECT level, * FROM logs

-- Full record with rename: all fields returned, but "level" is renamed to "severity"
SELECT *, level AS severity FROM logs
```

:::{note}
In standard SQL, `SELECT *, col AS alias` returns all columns **plus** an additional column with
the alias (duplicating the value). CLP does not duplicate — instead, the aliased column is renamed
in place within the full record output.
:::

## Examples

**Find all ERROR log events:**

```sql
SELECT * FROM logs WHERE level = 'ERROR'
```

**Find log events with high latency:**

```sql
SELECT * FROM logs WHERE latency_ms > 1000.0 LIMIT 100
```

**Find log events containing a substring in any column:**

```sql
SELECT * FROM logs WHERE CLP_WILDCARD_COLUMN() LIKE '%connection refused%'
```

**Select specific fields from log events within a status range:**

```sql
SELECT CLP_GET_STRING('level'), CLP_GET_INT('status'), CLP_GET_STRING('msg')
FROM logs
WHERE status BETWEEN 400 AND 599
```

**Find log events after a specific timestamp:**

```sql
SELECT * FROM logs WHERE "@timestamp" > TIMESTAMP '2024-01-15 10:30:00'
```

**Find log events matching multiple criteria:**

```sql
SELECT * FROM logs
WHERE level IN ('ERROR', 'FATAL')
  AND CLP_GET_STRING('meta.region') = 'us-east-1'
  AND latency_ms > 100
LIMIT 50
```

## Differences with standard SQL

* Only `SELECT` statements are supported (no `INSERT`, `UPDATE`, `DELETE`, etc.).
* `SELECT DISTINCT` is not supported.
* Arithmetic expressions in the `WHERE` clause are not supported (e.g., `WHERE x + 1 = 2`).
* Subqueries, `JOIN`, `GROUP BY`, `ORDER BY`, and `HAVING` are not supported.
* Aggregate functions (`COUNT`, `MIN`, `MAX`, `SUM`, `AVG`, `ARBITRARY`, `ANY_VALUE`) are supported
  in the `SELECT` clause only. They cannot be used in `WHERE` (use `HAVING` in standard SQL, which
  CLP does not support). Mixing aggregate and non-aggregate columns in `SELECT` is not allowed
  (would require `GROUP BY`). When searching across multiple archives, each archive outputs its own
  aggregate result line; merge them externally for a global result.
* Column aliases (`AS`) rename fields in the JSON output. For nested columns (e.g.,
  `meta.latency AS lat`), the value is extracted and flattened to the top level — standard SQL
  aliases only rename the column header without structural changes.
* `SELECT *, col AS alias` renames the column in place within the full record. In standard SQL,
  this would return all columns plus an additional aliased duplicate.
* The `FROM` clause is optional. CLP has no concept of tables, so the table name is ignored. You
  can use any identifier (e.g., `FROM logs`, `FROM my_data`) or omit `FROM` entirely.
* `LIKE` uses backslash (`\`) as the escape character. The `ESCAPE` clause is not supported.
* All SQL keywords are case-insensitive.
* Unquoted identifiers that match non-reserved keywords (`TIMESTAMP`, `DATE`, `ALL`, `AS`,
  `DISTINCT`, `LIMIT`, `COUNT`, `MIN`, `MAX`, `SUM`, `AVG`, `ANY_VALUE`, `ARBITRARY`) are
  case-folded to uppercase. Quote them to preserve the original case (e.g., `"timestamp"` instead
  of `timestamp`).

## Comparison with KQL

| Feature | KQL | SQL |
|---------|-----|-----|
| Equality | `key: value` | `WHERE key = 'value'` |
| Wildcard match | `key: *partial*` | `WHERE key LIKE '%partial%'` |
| Any column | `*: value` or `value` | `WHERE CLP_WILDCARD_COLUMN() = 'value'` |
| Numeric comparison | `key > 100` | `WHERE key > 100` |
| Boolean logic | `a: 1 AND (b: 2 OR c: 3)` | `WHERE a = 1 AND (b = 2 OR c = 3)` |
| Existence check | N/A | `WHERE key IS NOT NULL` |
| Range check | `key >= 10 AND key <= 20` | `WHERE key BETWEEN 10 AND 20` |
| Set membership | `key: a OR key: b` | `WHERE key IN ('a', 'b')` |
| Timestamp comparison | `timestamp("2024-01-15 10:30:00")` | `TIMESTAMP '2024-01-15 10:30:00'` |
| Column projection | N/A | `SELECT col1, col2 FROM ...` |
| Column renaming | N/A | `SELECT col AS alias FROM ...` |
| Result limit | N/A | `LIMIT n` |
| Aggregation | N/A | `SELECT COUNT(*), MIN(col) FROM ...` |
