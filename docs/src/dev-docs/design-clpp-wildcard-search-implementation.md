# Optimizing CLP+ wildcard search: changes, implementation, and impact

## Overview

**These optimizations allow CLP+ to answer wildcard queries faster by reducing query translation work, skipping compressed data that cannot match, and evaluating the remaining records more efficiently.**

The changes span two layers:

* **Log-surgeon (Rust):** reduce parser startup time, reject impossible string shapes (templates) earlier, reuse state machines, and make their intersection cheaper.
* **CLP (C++):** use summaries of stored values to translate wildcards directly into column conditions, skip encoded record tables (ERTs) that cannot match, and accelerate column scans.

Together, the changes reduce the complete query time for `message: *blk_1073746491_5667*` from **23.3 seconds to 145 milliseconds** on a 6.86-million-record Hive archive. The tradeoff is **3.55% more archive storage** and **11% slower ingest** against the shipped baseline. These are count-only query timings; see the [full benchmarks](design-clpp-wildcard-search.md#3-measurements).

Search proceeds through three steps:

| Step | Operation | Before | After |
|---|---|---|---|
| 1 | Translate the wildcard into column conditions | Use log-surgeon for every string shape | Decompose directly in C++ where supported; use the optimized log-surgeon engine where needed |
| 2 | Choose ERTs using those conditions | Read candidate ERTs to check their values | Check column summaries; skip an ERT if no alternative can match |
| 3 | Evaluate the remaining records | Evaluate numeric wildcards per record | Evaluate numeric wildcards through bulk column scans |

The implementation discussion starts with how log-surgeon makes decomposition cheaper, then explains how C++ uses archive summaries to avoid more work. At execution time, C++ handles supported string shapes directly and delegates the others to log-surgeon; this is **engine fallback**. Queries on composite extracted values also use log-surgeon directly.

## From a wildcard to column conditions

Three storage concepts explain how CLP+ translates a wildcard:

* A **parsing rule** defines a named part to extract from a string, such as `blockNum` or `genStamp` within a BlockID.
* A **string shape** preserves the fixed text and replaces extracted values with named placeholders: `blk_%blockNum%_%genStamp%`.
* An **ERT** stores records sharing the same schema in compressed columns—for example, separate columns for the components of BlockIDs.

The query `message: *blk_1073746491_5667*` asks for strings containing `blk_1073746491_5667`. To search the stored columns, CLP+ translates this pattern into conditions determined by each string's shape.

**Example 1: separate BlockID components.** The numbers occupy separate columns; `blk_` and `_` are fixed text.

```text
String:    Receiving blk_1073746491_5667
Shape:     Receiving blk_%blockID.blockNum%_%blockID.genStamp%
Condition: blockID.blockNum = 1073746491 AND blockID.genStamp matches "5667*"
```

The condition retains `5667*` because the query allows additional text after `5667`.

**Example 2: a BlockID inside a file path.** The entire path occupies one column.

```text
String:    Opening /data/blk_1073746491_5667
Shape:     Opening %filePath%
Condition: filePath matches "*blk_1073746491_5667*"
```

Both strings must be found. Checking only the BlockID component columns would miss the second record.

Each way to match the pattern against a shape is an **interpretation**. Search generates the possible interpretations and evaluates their column conditions. A shape can have several; satisfying any one is enough.

### Preparation depends on what the query searches

| What the query searches | Example | Preparation |
|---|---|---|
| String shape | `shape(message): "*Receiving blk_*"` | Compare the pattern with stored string shapes |
| Single extracted value (leaf) | `message.blockID.blockNum: 1073746491` | Find the corresponding columns |
| Composite extracted value (parent rule) | `message.blockID: *_5667` | Use log-surgeon to translate the pattern into conditions on the BlockID components |
| Entire string | `message: *blk_1073746491_5667*` | Generate column conditions for each possible string shape, as illustrated above |

Queries over the entire string benefit most from C++ decomposition. Queries on a parent rule mainly benefit from lower log-surgeon startup costs. Leaf and shape queries were already fast.

### Reading the performance measurements

The comparisons below distinguish three scopes:

* **Complete query time:** elapsed time for a count-only query, including startup, query preparation, archive reads, and evaluation.
* **Engine-path query time:** complete query time with log-surgeon performing decomposition. It includes C++ preparation and scanning as well as Rust work.
* **Component time:** time spent in a named operation, such as parser construction or local decomposition.

Each comparison identifies the configurations being compared. They are separate measurements, not successive reductions from one run; their savings should not be added together. The [benchmark tables](design-clpp-wildcard-search.md#3-measurements) give the query results and configuration details.

## Log-surgeon: reduce the cost of engine decomposition

Log-surgeon represents the query and parsing rules as **automata**, state machines describing the strings they accept. For a string shape, it combines the fixed text with the placeholder rules, then **intersects** that automaton with the query automaton to find their common matches. It converts those matches into interpretations for C++ to evaluate.

This work can be expensive even before any record is read: the parser must initialize, shape automata must be built, and intersection must explore pairs of states. The Rust changes reduce each of these costs. They apply when log-surgeon performs decomposition, including on archives without the new value summaries.

### 1. Remove JIT compilation from parser startup

**Why:** With the default just-in-time compilation (JIT) feature, `Parser::new` compiles the parsing state machine into machine code. In the benchmark, that costs about 7 s per process even with a cached parsing specification. A CLI process answering one query pays this cost before decomposition begins.

**Change:** CLP builds log-surgeon with `cargo build --release --no-default-features`, disabling the existing JIT feature. This is a dependency build setting, separate from the Rust source changes.

**Component time:** With the cached specification, disabling JIT reduces `Parser::new` from **6.99 s to 38–45 µs**. This measures the constructor alone; specification loading and preparation are separate costs.

**Complete query time:** The parent-rule query `message.blockPoolID: BP-1121897155-*` takes **7.29 s in the shipped baseline and 166 ms in the optimized build**. This comparison includes all changes in the optimized build; the constructor measurement identifies JIT as the dominant startup cost removed.

Cached-specification loading also avoids an unnecessary copy: `ParsingSpecBuilder` deserializes the cached automaton directly from the remaining specification text, instead of assembling another string line by line. This removes copying and temporary allocation; its individual timing contribution is not isolated.

### 2. Reject shapes before building and intersecting their automata

**Why:** Many shapes cannot produce the fixed text required by a wildcard. Building their full automata just to discover that impossibility wastes work.

**Change:** [The Rust prefilter][rust-prefilter] derives a conservative character set from each parsing rule's regular expression. It represents a shape as fixed characters and placeholders with those character sets. It then checks whether every uninterrupted fragment of query text could occur contiguously somewhere in the shape, including across placeholder boundaries.

For example, `*type=LAST_IN_PIPELINE*` requires the fragment `type=LAST_IN_PIPELINE`. A shape whose fixed text has no `=` and whose placeholders cannot produce `=` is rejected before its automaton is built. Checking fragments independently is sufficient for rejection; passing the checks does not establish a match.

**Effect and limits:** The filter is especially useful for distinctive punctuation. Ordinary letters and digits are harder to exclude: rules accepting broad strings can produce them. For queries such as `*Exception`, `*.log`, and `*blk_…*`, the measured filter rejected only **34–40 of 1,243 shapes**. A query such as `*?????*` has no fixed text to test and cannot benefit from this filter.

Unknown rules or unmodelled regular-expression constructs allow every character, and malformed shape profiles bypass filtering. These cases retain engine evaluation rather than risking a false negative.

### 3. Reuse the query automaton and cache shape preparation

**Why:** Every shape is compared with the same query, and a parser may encounter the same shapes across multiple searches. Rebuilding those automata repeats unchanged work.

**Change:** [SearchString::search_by_log_shapes][rust-search] constructs the query automaton once per call, outside the shape loop. A parser-owned `SearchCaches` stores each shape's automaton and its profile of fixed characters and placeholder character sets.

**Effect:** Query construction is paid once even when a call searches thousands of shapes. Shape construction is paid on the first cache miss and reused while that parser remains alive. Separate CLI processes do not share the cache, so repeated-query cache benefits require parser reuse. Retaining the cached structures also consumes memory; the cache has no eviction policy.

The benefit depends on how many shapes the prefilter rejects and whether cached shapes are reused. Broad queries can still require expensive intersections, motivating the next optimization.

### 4. Make each remaining intersection cheaper

**Why:** Broad queries survive filtering and can still require large intersections. Each intersection repeatedly looks up pairs of states, compares path edges, and discards states that cannot lead to a match. Small costs in these operations multiply across shapes.

**Change:** The [automaton implementation][rust-nfa] reduces work in these operations:

| Operation | Implementation change | Why it helps |
|---|---|---|
| Find an already visited pair of states | Use `(NfaIdx, NfaIdx)` keys in a hash map with a lightweight hasher, replacing the ordered map | Avoid repeated tree traversal and state-pair ordering |
| Compare decomposition path edges | Compare edge kinds and capture identifiers instead of recursively comparing regular expressions | Avoid traversing rule structures during ordering |
| Find states that can reach a match | Walk transitions backward from accepting states using flat adjacency storage | Avoid building strongly connected components solely to determine reachability |
| Combine automata | Adjust state and transition indices in place during concatenation and alternation | Avoid cloning states and their transition storage |
| Handle transitions in frequently executed code | Remove string formatting | Avoid repeated temporary string allocation |

**Engine-path query time:** Compare the no-JIT engine with prefiltering and automaton reuse against the same engine changes plus the intersection and construction improvements. Both configurations use C++ structural filtering and Rust decomposition. The BlockID query improves from **17.1 s to 4.9 s**, `*a*b*` from **215 s to 81 s**, and `*?????*` from **547 s to 280 s**. These measure complete queries and do not isolate each internal change. See the [configuration comparison](design-clpp-wildcard-search.md#31-stage-by-stage).

### How the Rust and C++ changes work together

The Rust prefilter reasons about what parsing rules **allow**. A digits-only rule excludes `=`, but permits `5667` regardless of whether that sequence occurs in the archive. The C++ summaries add knowledge of what values the archive **contains**, allowing more candidates to be rejected and column conditions to be generated directly. The following section explains those changes.

Parent-rule queries continue to use log-surgeon and benefit from cheaper startup and shared automaton operations. They do not use `search_by_log_shapes`' per-shape filter or caches. Queries over the entire string use the C++ decomposer where possible and the optimized Rust path for shapes it cannot fully handle. The C++/Rust interface remains unchanged.

## C++: use archive summaries and reduce scanning

The C++ changes extend optimization from parsing-rule structure to stored values. They first reduce decomposition work, then use the resulting column conditions to avoid reads and accelerate evaluation.

### 1. Use k-grams to rule out impossible query fragments

**Why:** A parsing rule's regular expression describes what it could extract, often much more than actually occurs. A rule accepting arbitrary text, for example, cannot rule out `5667` even if none of its stored values contains it.

**Change:** During ingest, [RuleValueIndex][rule-index] collects **k-grams**—consecutive byte sequences of length *k*—for lengths 1, 2, and 3, along with value lengths and the sequences appearing at value boundaries. This summary is called a **rule signature**. There is one per fully qualified rule name, such as `blockID.genStamp`, covering its values across all shapes, fields, and ERTs in the archive.

For example, `5667` contains the 3-grams `566` and `667`. If either is absent from the `genStamp` signature, no value extracted as `genStamp` can contain `5667`. The decomposer can therefore rule out placing that query fragment inside a `genStamp` placeholder.

The signature can prove that a query fragment is absent, but cannot prove a match: its byte sequences may come from different values or hash collisions. Matches that split a fragment across placeholders and fixed text are considered separately during decomposition.

**Effect:** Search rejects impossible uses of a placeholder without scanning its values or consulting log-surgeon. The index occupies about **147 KB** for 124 rules in the benchmark.

### 2. Translate wildcards using the archive's value summaries

**Why:** Filtering shapes still leaves log-surgeon to combine the query and shape automata—state machines describing what each can match. The new decomposer uses fixed text and rule signatures to generate column conditions directly, avoiding that automaton work.

**Change:** [ShapeDecomposer][decomposer] walks the query and string shape together:

1. Match query text against the shape's fixed text: `*` consumes zero or more characters, and `?` consumes one.
2. At each placeholder, use its rule's summary to check which query fragments its values could match.
3. Turn the surviving combinations into column conditions. `SchemaMatch` combines them into the expression used to search the relevant ERTs.

**Why the C++ layer:** Log-surgeon's current interface provides the query, string shapes, and parsing specification, but no archive value summaries. The C++ `ClppMatcher` already loads `RuleValueIndex` and connects shapes to schemas. Keeping decomposition there gives it direct access to the summaries without changing the C++/Rust interface.

**Moving this into Rust:** Log-surgeon could implement the same algorithm if we extended the interface to pass the rule summaries, or provided a callback to test query fragments against them. Rust would return interpretations; schema mapping, ERT selection, and scanning could stay in C++. The performance benefit comes from using archive-specific information to avoid automaton intersections; it does not depend on choosing C++ over Rust.

**Complete query time:** For the BlockID query, the optimized Rust decomposition configuration takes **4.9 s**, compared with **about 0.52 s** for the C++ decomposition configuration before column filters and subsequent scan optimizations.

**Component time:** In the final optimized configuration, local decomposition takes **about 17 ms**. Reused dynamic-programming tables, cached signature checks, and skipping impossible literal positions reduce this work. The 17 ms is part of the final query time, not a measurement from the 0.52 s configuration.

**Fallback:** Search delegates to log-surgeon when a shape is malformed, the local query syntax is unsupported, a generated condition uses a rule without a usable summary, or enumeration exceeds **64 interpretations**. Partial local results are discarded. Without the index, a simpler filter checks fixed text while allowing placeholders to contain any string, then sends the surviving shapes to log-surgeon.

### 3. Skip ERTs that cannot satisfy the column conditions

**Why:** Knowing a value might occur somewhere in the archive does not identify which ERT contains it. After decomposition, the BlockID query still qualifies for 367 ERTs containing 5.19 million records.

**Change:** At archive close, `SchemaWriter` builds filters for integer and variable-string columns in each ERT. Each filter stores min/max and a **Bloom filter**, a compact membership test that can prove a value is absent, over integer values or string dictionary IDs. Before reading ERT data, [QueryRunner][query-runner] checks these summaries and simplifies conditions whose result is already known.

**For the BlockID example:** decomposition produces `blockNum = 1073746491 AND genStamp matches "5667*"`.

* The exact `blockNum` condition can probe the range and Bloom filter. If that value is absent, this interpretation cannot match the ERT.
* The `5667*` condition can check whether the stored numeric range overlaps 5667, 56670–56679, 566700–566799, and so on.

The filters summarize individual columns, but skip the **entire ERT** only when every interpretation is ruled out. If another alternative could match, the ERT is still searched. Eliminating one condition does not by itself avoid reading that column.

For string columns, search first finds matching dictionary IDs, then probes those IDs when there are at most 256 matches. Arbitrary numeric substring conditions such as `*5667*` cannot use these ERT filters directly; the remaining values must be scanned.

**Complete query time:** Using the same optimized binary, compare archives without and with column filters. The BlockID query improves from **379 ms to 139 ms**, with reads falling from **367 ERTs to 12 ERTs** and record evaluations from **5.19 million to 154,338**. The 139 ms result is from this filter comparison; the overview's 145 ms is from the overall benchmark. The filter section adds **1.37 MB**. A Bloom hit still requires evaluation; only definite absence permits pruning.

### 4. Evaluate numeric wildcards in the column scan

**Why:** Conditions such as `genStamp matches "5667*"` previously forced search to use the per-record `QueryRunner` evaluator instead of `ColumnScan`.

**Change:** [ColumnScan][column-scan] now evaluates wildcard equality and inequality conditions on integer and double columns, recording matches in a bitmap. It formats values into a stack buffer and uses prefix/suffix and substring checks for simple numeric patterns. Other patterns use the general wildcard matcher.

**Complete query time:** Without column filters, `message: *5667*` takes about **2.6 s** in the C++ decomposition configuration before scan optimizations and **0.5 s** in the optimized scanning configuration. This comparison also includes decomposition refinements, so it does not isolate the column scan alone. With the same final binary, adding column filters reduces this query from **507 ms to 393 ms**.

Values are still formatted and matched during bulk evaluation; wildcard range comparisons still fall back.

### 5. Prepare only what the query uses

**Why:** Loading rule signatures and building compact shapes costs about 35 ms. Leaf, shape, and parent-rule queries do not use those structures.

**Change:** `ClppMatcher::prepare_shapes` runs only when decomposition against string shapes is first needed. `ensure_parser` creates the engine parser on demand and shares it between decomposition paths. `SchemaMatch` gathers dictionary requirements and requests each required dictionary once.

**Complete query time:** Across the measured leaf and shape queries, making shape preparation lazy reduces the range from **96–166 ms to 65–140 ms**, near shipped-baseline performance. These are ranges across queries, not uncertainty intervals. Parser consolidation and dictionary-request consolidation have no independently measured speedup.

**Component time:** For the parent-rule BlockPoolID query, the `clpp_parser_init` scope takes **about 89 ms within a 165 ms complete query**: approximately 15 ms to read the parsing specification and 70 ms to build its structures. This scope includes specification preparation as well as the now-microsecond `Parser::new` call. Reusing that preparation across queries is a remaining opportunity.

## Costs, compatibility, and limits

* **Ingest:** 31.2 → 34.7 s against the shipped baseline. With both versions using the same no-JIT engine, the comparison is **26.7 → 34.7 s (+30%)**. The latter isolates the index overhead from the JIT improvement.
* **Storage and memory:** archive size grows **3.55%**. Ingest also holds deduplication sets and temporary column keys; compressed index size does not bound memory usage. Peak-memory impact is not measured.
* **Existing archives:** both index sections are optional. Without the rule index, search uses structural filtering and log-surgeon. Without column filters, search skips the new ERT-pruning step. The sections are written only for experimental archives.
* **Build:** enable `CLP_BUILD_CLPP_DECOMPOSITION=ON` for parent-rule queries and engine fallback. It defaults to OFF. Query syntax is unchanged.
* **Correctness:** indexes must cover every value or treat the rule as unbounded, meaning it cannot be pruned using a signature. If a shape repeats a rule, separate occurrences may satisfy different column conditions; current tests permit extra matches in that case. Some parent-rule patterns ending in a delimiter followed by `*` also return zero results in both baseline and optimized builds.

The 43-query comparisons agree between engine/local decomposition and between archives without/with column filters. The focused final C++ subset passes 30 cases and 633,553 assertions; the full suite on `4a0b3b61` passes 164 of 165 cases, with a grep failure involving the engine dependency combination. See [validation details](design-clpp-wildcard-search.md#6-verification).

## Appendix: implementation reference

### Index and decomposition details

| Component | Details relevant to changes and extensions |
|---|---|
| Rule signature | Byte, byte-pair, and hashed byte-triple sets; first/last-byte and pair sets; length bounds and mask. Stored in `/rule_value_index`. |
| Complete coverage | Deduplication remembers up to `2^21` strings per rule; reaching that limit does **not** stop indexing new values. Missing rules and rules stored as bare doubles without retained formatting are unbounded. |
| Query semantics | Prefix-anchored, matching the engine. The local normalizer appends a trailing `*`. Use `*text*` for contains comparisons with regular clp-s. |
| Local enumeration | `reach` checks feasibility; `enumerate` emits alternatives; `reach_leafless` detects matches requiring no leaf predicates. Generation stamps reuse reachability tables across shapes. |
| Expression handling | Interpretations form an OR of ANDs; stored type variants form additional ORs. Non-inverted CLP+ filters with no decomposition result fold to empty, avoiding repeated decomposition. |
| Column keys | Integer values or variable-string dictionary IDs. Repeated occurrences of a node in one ERT contribute their union. Other column types receive no filter. Stored in `/column_value_filters`. |
| Filter probes | Resolved EQ/NEQ predicates: exact integers, `<digits>*` integer-prefix ranges, or up to 256 matching string dictionary IDs. Inversion is accounted for when folding. |
| Bloom sizing | Target 1% false positives; nominal `2^22`-bit budget. The false-positive rate is capped at 50%, so above roughly 2.9 million distinct keys the allocation can exceed that budget. |
| Section loading | Column filters are preloaded before the ERT data stream opens because only one section reader can be checked out. Single-file section ordering can also force earlier rule-index reads. |
| Numeric rendering | Doubles use the existing 17-significant-digit representation. Patterns containing only digits, `-`, `.`, and `*` use the specialized matcher; others use general wildcard matching. |
| Resource bounds | Rule/span memo size scales with rule count × query length². Reachability tables scale with shape length × query length. The interpretation cap does not bound these allocations. |

### Query-path selection

| Condition | Behavior for a query over the entire string |
|---|---|
| Index present, supported query and valid shape | Attempt local decomposition |
| No local alignment | Reject shape |
| More than 64 interpretations, or emitted leaf uses an unbounded rule | Discard local output for that shape and use the engine |
| Malformed shape | Use engine |
| Case-insensitive query or local tokenization failure | Delegate to engine; its support and errors still apply |
| No rule index | Skeleton filter, then engine for survivors |

Parent-rule value queries use log-surgeon to decompose against the named rule. A corrupt index produces an error rather than the missing-section fallback.

### Code map

Paths below are relative to `components/core/src` in the [source snapshot][source-tree].

| Commit | Main code | Change |
|---|---|---|
| `8cd04511` | `clp_s/clp-s.cpp`, `clp_s/search/{Output,SchemaMatch}.cpp` | Profiling and consolidated dictionary requests |
| `f4eae169` | `clpp/RuleValueIndex.{hpp,cpp}`, `clp_s/JsonParser.cpp`, archive reader/writer | Rule signatures |
| `f787c380` | `clp_s/search/ClppShapeQueryMatcher.hpp` | Skeleton pruning |
| `bcaaffd7` | `clp_s/search/{ClppShapeDecomposer.hpp,ClppMatcher.cpp,SchemaMatch.cpp}` | Local decomposition and expression integration |
| `5a9375bf` | `clp_s/ColumnValueFilter.{hpp,cpp}`, writers, `clp_s/search/QueryRunner.cpp` | Column filters and ERT pruning |
| `2c9b2b26` | `clp_s/search/ColumnScan.cpp` | Numeric wildcard scan |
| `9f66d6db` | Repository root: `taskfiles/deps/main.yaml` | Engine pin and no-JIT build |
| `4a0b3b61` | `clp_s/search/ClppMatcher.{hpp,cpp}` | Lazy shape preparation |
| `90fb2a04` | `clp_s/search/ClppMatcher.{hpp,cpp}` | Shared parser initialization |

Supporting changes cover section recognition, CMake dependencies, telemetry, and tests under `components/core/tests`: `test-ClppRuleValueIndex.cpp`, `test-ClppShapeQueryMatcher.cpp`, `test-ClppShapeDecomposer.cpp`, and `test-ColumnValueFilter.cpp`.

CLP pins `jackluo923/log-surgeon@96a4fb4`, incorporating the following two commits on top of `bb41b06`. Paths are relative to `rust/src` in the [Rust source snapshot][rust-source]:

| Commit | Main code | Change |
|---|---|---|
| `bc54b07` | `prefilter.rs`, `search.rs`, `parser.rs`, `c_interface.rs` | Character-set filtering, query automaton reuse, and parser-owned shape caches |
| `96a4fb4` | `nfa.rs`, `nfa/search_decomposition.rs`, `nfa/regex_construction.rs`, `parsing_spec/spec_file.rs` | Cheaper automaton construction and intersection, and cached-specification loading |

Rust validation covers 49 library tests and 2 integration tests. The prefilter check covers 8 shapes × 317 queries with no false negatives; patched and unpatched engines also produce identical output in the C++/Rust interface comparison. See the [validation details](design-clpp-wildcard-search.md#6-verification).

### Build and inspect

With the repository's C++ dependencies installed:

```sh
cmake -S components/core -B build/core \
    -DCMAKE_BUILD_TYPE=Release \
    -DCLP_BUILD_CLPP_DECOMPOSITION=ON \
    -DCLP_ENABLE_PROFILING=1
cmake --build build/core --target clp-s

CLP_TELEMETRY_ENDPOINT=ostream build/core/clp-s --experimental s --enable-telemetry \
    <archive> 'message: *blk_1073746491_5667*' --count
```

Use a cached parsing specification compatible with the engine revision for benchmark comparisons. Runtime telemetry needs compile-time profiling support to emit detailed scope timings. See [benchmark setup](design-clpp-wildcard-search.md#4-how-to-build-and-run) for ingest and dependency instructions.

| Inspect | Telemetry |
|---|---|
| Local versus engine decomposition | `num_clpp_shapes_decomposed_locally`, `num_clpp_shapes_decomposed_by_engine` |
| ERT pruning | `num_schemas_scanned`, `num_messages_evaluated` |
| Evaluator fallback | `num_column_scan_filters`, `num_query_runner_filters` |
| Preparation and I/O | `clpp_prepare_shapes`, `clpp_parser_init`, `read_schema_table` scopes |

Scope timings can be nested; do not sum them as independent wall-time contributions.

[source-tree]: https://github.com/jackluo923/clp/tree/5606a4338ba2330176823fb1a136d881f77dfa70/components/core/src
[rule-index]: https://github.com/jackluo923/clp/blob/5606a4338ba2330176823fb1a136d881f77dfa70/components/core/src/clpp/RuleValueIndex.hpp
[decomposer]: https://github.com/jackluo923/clp/blob/5606a4338ba2330176823fb1a136d881f77dfa70/components/core/src/clp_s/search/ClppShapeDecomposer.hpp
[query-runner]: https://github.com/jackluo923/clp/blob/5606a4338ba2330176823fb1a136d881f77dfa70/components/core/src/clp_s/search/QueryRunner.cpp
[column-scan]: https://github.com/jackluo923/clp/blob/5606a4338ba2330176823fb1a136d881f77dfa70/components/core/src/clp_s/search/ColumnScan.cpp
[rust-source]: https://github.com/jackluo923/log-surgeon/tree/96a4fb4/rust/src
[rust-prefilter]: https://github.com/jackluo923/log-surgeon/blob/96a4fb4/rust/src/prefilter.rs
[rust-search]: https://github.com/jackluo923/log-surgeon/blob/96a4fb4/rust/src/search.rs
[rust-nfa]: https://github.com/jackluo923/log-surgeon/blob/96a4fb4/rust/src/nfa.rs
