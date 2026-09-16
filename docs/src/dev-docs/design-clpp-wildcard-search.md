# clpp wildcard search: from 20–40 s to ~150 ms per query

Branch `clpp-wildcard-search` (`jackluo923/clp`): eight code commits on top of the oss baseline
`772db1bb` — one per clp-s optimization plus a dependency bump (§5) — and this document. The Rust engine changes live in
`jackluo923/log-surgeon`, branch `clpp-search-prefilter` (`bb41b06` + two commits), which the bump
pins. Build and run instructions: §4.

## 0. Summary

A whole-message wildcard query (`message: *blk_1073746491_5667*`) on a clpp archive used to spend
~23 s building and intersecting automata for every log shape, then decompress most of the archive
to find 26 hits. It now answers in 145 ms — faster than regular (non-CLP+) clp-s on the
same data — by

1. writing two small indexes into the archive at ingest: a per-rule **k-gram value signature**
   (`/rule_value_index`, 147 KB) and per-column **Bloom + range filters** (`/column_value_filters`,
   1.37 MB);
2. decomposing the query against every log shape **in C++** with a DP that the value signatures
   certify, so the Rust engine is only a fallback;
3. using the column filters to **skip whole schema tables** before they are decompressed, and
   vectorising numeric wildcard leaves in `ColumnScan`.

Hive 24-hour log, 6,864,523 records, 1,270 log shapes, 1,283 schemas; every cell is the median of
3 wall-clock runs of `clp-s s … --count` unless footnoted. A clpp wildcard can target three
different things (§A.6), and each takes a different path through the code, so the results are
grouped by target:

1. **a log shape or a leaf value** — `shape(message): "*Receiving BP-*"`,
   `message.blockID.blockNum: 1073746491`. Dictionary and column lookups; oss clpp was already fast
   here, so the bar is "no regression".
2. **a parent variable, with the wildcard inside it** — `message.blockPoolID: BP-1121897155-*`.
   The engine decomposes the wildcard against that one rule's sub-patterns
   (`decompose_by_rule_name`); oss clpp paid the ~7 s engine JIT on every such query.
3. **the whole message field** — `message: *blk_1073746491_5667*`. The query is decomposed against
   every log shape; 20–40 s in oss clpp, and what §§1–3 are about.

### Category 1 — log shape and leaf values (already fast in oss clpp)

| query | count | clp-s¹ | oss clpp | **optimized clpp** | vs clp-s¹ | vs oss clpp |
|---|---|---|---|---|---|---|
| `message.blockID.blockNum: 1073746491` | 23 | 2.44 s | 0.070 s | **0.071 s** | 34× | 0.99× |
| `message.blockID.blockNum: 10737464*` | 2,608 | 2.43 s | 0.171 s | **0.082 s** | 30× | 2.1× |
| `message.blockID.blockNum: 1073746491 AND message.blockID.genStamp: 5667` | 23 | 0.63 s | 0.069 s | **0.073 s** | 8.6× | 0.95× |
| `message.blockPoolID.poolID: 1121897155` | 109,484 | 2.39 s | 0.068 s | **0.071 s** | 34× | 0.96× |
| `message.containerID.appSeq: 0099` | 114,918 | 0.76 s | 0.116 s | **0.072 s** | 11× | 1.6× |
| `message.attemptID.type: m` | 980,424 | 1.33 s | 0.148 s | **0.139 s** | 9.6× | 1.06× |
| `message.hostPort.host: 172.31.17.31` | 159 | 0.64 s | 0.135 s | **0.064 s** | 10× | 2.1× |
| `shape(message): "*Receiving BP-*"` | 20,500 | 0.60 s | 0.062 s | **0.067 s** | 9.0× | 0.93× |
| `shape(message): "*PacketResponder*" AND message.blockID.blockNum: 1073746491` | 3 | 1.68 s | 0.064 s | **0.065 s** | 26× | 0.98× |
| `shape(message): "*%blockID.blockNum%*" AND message.hostPort.host: 172.31.17.31` | 104 | 1.78 s | 0.061 s | **0.066 s** | 27× | 0.92× |
| **mean speedup (geometric, 10 queries)** | | | | | **17×** | **1.2×** |

Both clpp builds sit at the ~60–70 ms process floor; the optimized build wins where a leaf value
is a wildcard or a string (§2.3, §2.4) and is within ±6 ms (noise) elsewhere. The first optimized
build *had* regressed this category to 96–166 ms because it read the value index and built the
compact shapes for every query; commit `4a0b3b61` builds them only on the first whole-message
decomposition (`ClppMatcher::prepare_shapes`).

### Category 2 — parent variable, wildcard inside it

| query | count | clp-s¹ | oss clpp | **optimized clpp** | vs clp-s¹ | vs oss clpp |
|---|---|---|---|---|---|---|
| `message.blockPoolID: BP-1121897155-*` | 109,484 | 0.64 s | 7.29 s | **0.166 s** | 3.9× | 44× |
| `message.blockPoolID: *-1427088167814` | 109,484 | 2.33 s | 7.30 s | **0.163 s** | 14× | 45× |
| `message.containerID: container_1427088391284_0099_*` | 114,917 | 0.69 s | 7.34 s | **0.165 s** | 4.2× | 44× |
| `message.containerID: *_0099_01_000001` | 114,918 | 0.67 s | 7.41 s | **0.164 s** | 4.1× | 45× |
| `message.attemptID: attempt_1427088391284_0059_*` | 33,187 | 0.73 s | 7.13 s | **0.255 s** | 2.9× | 28× |
| `message.attemptID: *_m_000001_0` | 13,587 | 0.76 s | 7.15 s | **0.242 s** | 3.1× | 30× |
| `message.jobID: job_1427088391284_0*` | 44,824 | 0.93 s | 7.41 s | **0.171 s** | 5.4× | 43× |
| `message.yarnAppID: *_0036` | 2,070 | 0.80 s | 7.38 s | **0.164 s** | 4.9× | 45× |
| `message.taskID: *_m_000001` | 540 | 0.79 s | 7.42 s | **0.156 s** | 5.1× | 48× |
| `message.blockID: *_5667` | 23 | 0.62 s | 7.03 s | **0.164 s** | 3.8× | 43× |
| `message.blockID: blk_1073746491_?667` | 23 | 1.75 s | 7.47 s | **0.171 s** | 10× | 44× |
| `message.prefix: "INFO org.apache.hadoop.hdfs.*"` | 160,501 | 0.63 s | 7.43 s | **0.181 s** | 3.5× | 41× |
| `message.prefix: "WARN *"` | 15,544 | 0.60 s | 7.42 s | **0.161 s** | 3.8× | 46× |
| `message.byteSize: *MB` | 207 | 0.60 s | 7.33 s | **0.153 s** | 3.9× | 48× |
| **mean speedup (geometric, 14 queries)** | | | | | **4.7×** | **42×** |

The 7 s is the engine JIT in `Parser::new` (§A.2), paid once per process. Without it the parser
build is 85–90 ms of the optimized build's ~165 ms — 15 ms to read the 279 KB `parsing_specification`
section and ~70 ms in `ParsingSpecBuilder::build` (rule regexes → per-rule automata, search NFA,
cached-DFA setup; `telemetry-cat2-final.log`) — the decomposition itself is under a millisecond,
and the rest is the same dictionary read and schema scan the other categories pay. That parser
build is the next lever for this category (§7). The `attemptID` rows are slower because the
schemas that hold `attempt_…` values are large: 1.7 M records across 30 schemas for the first row
(109 K across 45 for the `blockPoolID` rows), so `read_schema_table` costs 80 ms instead of 8.
One quirk inherited from oss and left alone: a `*` that has to cover
exactly one trailing sub-pattern after a delimiter (`message.jobID: job_1427088391284_*`,
`message.blockID: blk_1073746491_*`) returns 0 in **both** builds, while `job_1427088391284_0*`
returns 44,824 and `blk_1073746491_?667` 23 — see §A.6.

### Category 3 — the whole message field

| query | count | clp-s | oss clpp | **optimized clpp** | vs clp-s | vs oss clpp |
|---|---|---|---|---|---|---|
| `*blk_1073746491_5667*` | 26 | 0.68 s | 23.3 s | **0.145 s** | 4.7× | 161× |
| `*blk_1073746491_*` | 26 | 0.66 s | 23.3 s | **0.136 s** | 4.9× | 171× |
| `*blk_*_5667*` | 26 | 1.75 s | 40.3 s | **0.158 s** | 11× | 255× |
| `*blk_10737464??_*` | 2,865 | 1.75 s | 22.7 s | **0.214 s** | 8.2× | 106× |
| `*5667*` | 808 | 2.43 s | 30.8 s | **0.39 s** | 6.2× | 79× |
| `*1073746491*` | 26 | 2.52 s | 31.9 s | **0.23 s** | 11× | 139× |
| `*Exception*` | 123,309 | 0.60 s | 23.5 s | **0.127 s** | 4.7× | 185× |
| `*.log*` | 35,198 | 0.66 s | 23.7 s | **0.164 s** | 4.0× | 145× |
| `*type=LAST_IN_PIPELINE*` | 5,920 | 0.61 s | 22.0 s | **0.126 s** | 4.8× | 175× |
| `*DataNode*` | 67,412 | 0.63 s | 21.8 s | **0.146 s** | 4.3× | 149× |
| `*INFO*` | 6,706,343 | 0.64 s | 21.7 s | **0.45 s** | 1.4× | 48× |
| `*a*b*` | 2,946,453 | 1.64 s | 215 s³ | **0.67 s** | 2.4× | 321× |
| `*?????*` | 6,863,835 | 1.43 s | 475 s² ³ | **0.89 s** | 1.6× | 534× |
| `*zzzz*` (no match) | 0 | 0.025 s | 22.7 s | **0.065 s** | 0.38× | 349× |
| **mean speedup (geometric, 14 queries)** | | | | | **3.8×** | **170×** |

¹ clp-s (non-CLP+) stores `message` as one string and cannot address a leaf or a parent variable
at all, so the `clp-s` cell in categories 1–2 is the *nearest full-message substring query*
(`message: *1121897155*` for `message.blockPoolID.poolID: 1121897155`, `message: *BP-1121897155-*`
for `message.blockPoolID: BP-1121897155-*`), which usually over-matches — 129,079 instead of
109,484 in both of those. The exact substitute query and its count for every row are listed in
§A.6; "vs clp-s" in those two tables therefore compares a precise clpp query against a looser
clp-s one, and is a lower bound on the gap.
² oss clpp returns **6,864,518** here, not 6,863,835: it over-matches the 683 records whose message
is ` ` or ` OK` (2–4 characters). The raw JSONL has exactly 6,863,835 records with a message of
≥ 5 characters, which is what clp-s, the optimized build, and the optimized build's engine
fallback path (`diff-2.log`) all return — so the over-match is confined to the oss build (unpatched
engine + oss matcher) and was not investigated further.
³ Single runs (`bench-pristine.log`, heavy block); every other cell is a median of 3. oss clpp
values for the first block-ID, `*Exception*` and `*.log*` rows were re-measured on an idle machine
(`bench-pristine-three.log`) after an earlier single run under load had given 31.4/28.6/29.3 s.

*clp-s* = the same `clp-s` binary without `--experimental` (regular, non-CLP+ mode), archive `hive-24hr-plain`;
*oss clpp* = branch HEAD `772db1bb` with the unpatched engine, archive `hive-24hr-clpp-cached`;
*optimized clpp* = branch head `90fb2a04` with the patched no-jit engine (§4.1), archive
`hive-24hr-clpp-cvf`. Categories 1–2 were measured on `4a0b3b61` (`bench-cat-final.log`,
`bench-cat-oss.log`, `bench-cat-nearest-clps.log`, `bench-nearest2-clps.log`); category 3 on
`2c9b2b26` and re-measured on `4a0b3b61` within ±6 ms of every cell (`bench-final-cvf-lazy.log`),
since the lazy build does not touch the whole-message path; `90fb2a04` only adds profiling
scopes (two category-2 rows re-checked: 165/163 ms).
Every count agrees between oss clpp and the optimized build in all three tables except `*?????*`
(footnote ²); clp-s agrees on category 3; a 43-query differential and a decode-and-grep of the raw
log agree too (§6).

Costs: the archive grows 3.6 % (42.59 → 44.10 MB, still 20 % smaller than clp-s's
55.38 MB) and single-threaded ingest goes from 31.2 s to 34.7 s (+11 % against shipped oss clpp;
+30 % against the oss code on the same no-jit engine, 26.7 s — §3.3). Both sections are
written only for `--experimental` archives; a clpp archive without them still searches through the
old engine path.

### 0.1 The optimizations in plain terms

Pipeline order; impact figures are for the block-ID query unless stated (23.3 s → 0.145 s overall).

| # | optimization | how it works | impact |
|---|---|---|---|
| 1 | Build the engine without JIT | The Rust engine JIT-compiled the parsing automaton at every process start (~7 s); a one-query process never earns that back. Build flag only (`cargo build --release --no-default-features`, §4). | ≈ −7 s on every query (`Parser::new` 6.99 s → µs, §A.2; 23.3 → 17.1 s with the prefilter/hoist/caches included); compression 31 → 26 s |
| 2 | Engine patch: prefilters, build-once, caches, hot loops | Drop shapes that provably cannot produce the query's literal text before any automaton is built; build the query automaton once instead of once per shape; cache per-shape automata; cheaper hot loops (integer keys, no string formatting, no state cloning). | Engine path 17 → 4.9 s (3.5×) from the hot-loop fixes; prefilter/hoist/caches alone gave only 1.1–1.3× on ordinary queries, 20–35× on queries with rare punctuation and 76–234× on anchored prefixes (§A.1). Now the fallback only |
| 3 | Rule value index in the archive | At ingest, for each rule, remember which byte pairs/triples and lengths its values actually contained (147 KB). At search, "could any value of rule X contain `1073746491`?" is answered in nanoseconds, and "no" is a proof. | As a filter alone: 20 → 1.8 s (11×). In the final design it is what makes #4 possible |
| 4 | Decompose the query in C++, not in the engine | Slide the query along each shape's text: literal text must match exactly; a placeholder may swallow a piece of the query only if #3 says some value of that rule could contain it. Every valid alignment becomes an interpretation (`blockNum = 1073746491`, `genStamp = 5667*`) — the same output the engine produced. | 4.9 s → 0.5 s (9×) against the fully patched engine, 17 s → 0.5 s against the prefilter/hoist/caches-only one; `*a*b*` 81 s → 0.75 s; `*?????*` 280 s → 1 s. No shape falls back to the engine on this data |
| 5 | Column value filters in the archive | At ingest, for every column of every schema table, store min/max and a Bloom filter of its values (1.37 MB). At search, before decompressing a table, check whether the leaf value could be in it; skip the table if not. | Tables read 367 → 12, messages scanned 5.19 M → 154 K; 0.38 → 0.14 s (2.7×); `*Exception*` 356 → 126 ms; `*.log*` 462 → 165 ms |
| 6 | Vectorised numeric wildcards | `5667*` on an integer column used to force the slow per-message evaluator; now the column is scanned in bulk with a digit-pattern matcher. | `*5667*` 2.6 → 0.5 s (5×) |
| 7 | Decomposer (dynamic-programming) micro-optimizations | Reuse the alignment memo table across shapes with generation stamps instead of clearing it; when a `*` slides over literal text, only probe positions where the next literal could start. | Decomposition 49 → 17 ms (`telemetry-final.log`; the 49 ms profile is in the session transcript only) — ~12 % of the final 145 ms |
| 8 | Build the shape tables only for whole-message queries | The value index read and the compact-shape build (#3, #4; ~35 ms) ran in the matcher's constructor, i.e. for every `--experimental` query. Leaf, shape and parent-variable queries never decompose against the log shapes, so they now skip it (`ClppMatcher::prepare_shapes`, on first use). | Category 1: 96–166 → 65–140 ms, back to parity with oss clpp; category 2: 195–281 → 153–255 ms |

---

## 1. Where oss clpp's time went

`ClppMatcher::decompose_by_log_shapes` handles an unrestricted wildcard query by handing every log
shape to log-surgeon's `SearchString::search_by_log_shapes`, which per shape builds the shape's
NFA (`ParsingSpec::automata_for_shape`), builds the query's NFA, and intersects them. Profiled on
the block-ID query (callgrind + gdb sampling on 2026-09-15, no-jit engine; the 5.19 M figure is the
final binary's `num_messages_evaluated` on the archive without column filters, `telemetry-final.log`):

| phase | share of 1.78 s (index prototype) | share of ~23 s (oss) |
|---|---|---|
| engine `search_by_log_shapes` | 53 % | ~100 % (`Tnfa::intersect`, SCC/live-state computation, allocation) |
| ERT scan (`Output::filter`): 5.19 M of 6.86 M messages decompressed for 26 hits | 22 % | hidden under the engine |
| matcher construction + candidate DP (prototype side file) | 18 % | — |
| spec deserialisation | 4 % | — |

Two engine facts make the shape work avoidable:

- **The engine is prefix-anchored.** `Tnfa::intersect::<true, false>` accepts as soon as the query
  automaton accepts, so `*foo*` means "the message contains `foo`", and any match contains every
  literal run of the query contiguously.
- **A shape is literals interleaved with leaf-rule placeholders** (`%timestamp% %level% %blockID%`),
  each placeholder standing for the language of its rule — and, more usefully, for the finite set
  of values that rule actually took in this archive.

And one fact about the downstream: every leaf kind is evaluated exactly by clp-s afterwards
(numeric wildcard leaves are formatted and wildcard-matched), so emitting a **superset** of the
engine's interpretations is always correct, only slower. That licenses replacing the engine with
any sound approximation.

Two earlier dead ends worth recording: the shipped **character-set prefilter** (bounding a
placeholder by its rule's regex alphabet) prunes almost nothing on real shapes because
`%prefix.class%` appears in 825 of 1,243 shapes and its alphabet covers any identifier-like run
(§A.1); and the engine's **Cranelift JIT** costs ~7 s per process in `Parser::new`, so the engine
must be built with `--no-default-features` (§A.2).

## 2. Architecture

```
ingest  (JsonParser::parse_str_field, --experimental only)
  ├─ every (rule, lexeme) ───────────► clpp::RuleValueIndex ──► section /rule_value_index
  └─ every schema table, per column ─► clp_s::ColumnValueFilter ► section /column_value_filters

search  (message: <wildcard>)
  ClppMatcher              prepare_shapes (first whole-message query only): read /rule_value_index;
                           intern rule names; one CompactShape per log shape
  ShapeDecomposer          per shape: DP over (shape position, query token), every placeholder piece
                           certified by its RuleSignature → interpretations (segments + leaves)
                           engine fallback: capped shapes, leaves on unbounded rules, malformed shapes,
                           case-insensitive or unparsable queries, archives without an index
  SchemaMatch              OR over interpretations of AND over leaves (one FilterExpr per type variant)
  QueryRunner::schema_init constant_propagate → evaluate_with_column_value_filter per leaf:
                             integer  value → Bloom + [min,max];  digits* → prefix ranges vs [min,max]
                             string   value → its dictionary IDs (≤ 256) probed against the Bloom
                           → schema table skipped when every interpretation folds to False
  ColumnScan               numeric wildcard leaves (5667*, *5667*) on the vectorised path
```

The two indexes have different scopes, which is why both exist:

| | rule value index (§2.1) | column value filters (§2.3) |
|---|---|---|
| keyed by | fully-qualified rule name (`blockID.blockNum`; 124 on hive) | (schema table, node id) — every int / var-string column of every table |
| granularity | **one signature per rule for the whole archive**, regardless of which log shape, field or table the value landed in | per table, per column |
| answers | "could *any* value this rule ever matched contain / start with / end with this text?" | "does *this table's* column hold this exact value?" |
| used for | deciding which placeholder may swallow which piece of a `message: *…*` query, per shape | skipping schema tables before decompression |
| size | 147 KB | 1.37 MB |

A rule that appears in many shapes and fields (`prefix.class`, `key_value.value`) accumulates a
wide signature and prunes less; and the rule index can say nothing table-specific, so after
decomposition the block-ID query still touched 367 tables until the column filters cut that to 12.

### 2.1 Rule value index — `components/core/src/clpp/RuleValueIndex.{hpp,cpp}`

At ingest `JsonParser` sees every `(rule_name, lexeme)` it parses, numeric or not, and feeds it to a
per-rule `RuleSignature` (`ArchiveWriter::add_rule_value(qualified_name, lexeme)`, one entry per
rule name archive-wide): a 128-bit ASCII byte set (+ non-ASCII flag) of all bytes, first bytes and
last bytes; exact 2^16-bit bigram sets (all, first, last); a 2^21-bit folded trigram set; and a
64-bit length mask (exact lengths 0–62, one bit for ≥ 63). Bitsets are allocated lazily. Queries:
`may_contain`, `may_start_with`, `may_end_with`, `may_equal`, `may_have_length`. A rule can be
`mark_unbounded` (every query answers true); a rule absent from the index is unbounded too.

Soundness contract (header comment): every k-gram, anchored k-gram and length of every added value
sets its bit, so `false` proves no indexed value satisfies the query; collisions and cross-value
k-grams only ever set more bits. On the hive archive the index holds all 124 rules its shapes reference with nothing
capped and costs 146,778 bytes (`ls -l` of the section; `verify-misc.log`). `ArchiveWriter::close_rule_value_index` writes it;
`ArchiveReader::get_rule_value_index` reads it lazily (it is one more zstd section, listed in the
archive's file list and in `Utils.cpp`'s known-section list like the others).

### 2.2 Shape decomposer — `components/core/src/clp_s/search/ClppShapeDecomposer.hpp`

`ClppMatcher` builds a `CompactShape` per log shape once per query (one byte per literal
character, placeholder positions and interned `rule_id_t`s in side arrays) and runs
`ShapeDecomposer` over it with the normalized query tokens (`literal`, `?`, `*`).

- `reach` / `reach_leafless` are reachability DPs over `(shape position, query token)`: a literal
  must match character-for-character (`?` any one), a `*` absorbs any run, and a placeholder
  absorbs a contiguous piece of the query which becomes its leaf query — closed (`piece`) or open on
  the side(s) where it shares a `*` with a neighbour (`*piece`, `piece*`, `*piece*`). A piece is
  followed only if the placeholder's signature cannot rule it out (`may_equal`/`may_start_with`/
  `may_end_with`/`may_contain` per shape). `*` over a literal run is handled iteratively, and
  positions that cannot start the next literal are not probed.
- Memo cells are generation-stamped `uint32_t`s reused across all 1,270 shapes (no per-shape
  allocation); `piece_may_match` results are memoised per `(rule, span)`.
- `enumerate` emits every certified alignment as `ShapeInterpretation` (text segments + leaves),
  capped at `cMaxLocalInterpretationsPerShape = 64`. A leafless interpretation (every placeholder
  unconstrained) matches every message of the shape and is the only one emitted.
- `DecomposeStatus::Capped`, or an emitted leaf on an unbounded rule (`used_unbounded_rule`), sends
  that shape to the engine (`decompose_by_engine` → the patched `search_by_log_shapes`); malformed
  shapes always go there, and case-insensitive or unparsable queries skip the local path entirely.
  On the hive archive every rule is bounded and no shape hits the cap, so nothing reaches the engine:
  `num_clpp_shapes_decomposed_by_engine = 0` for the block-ID, `*5667*` and `*Exception*` queries
  (`telemetry-final.log`), 360/672/422 shapes decomposed locally.

The old skeleton (structural) filter, `ClppShapeQueryMatcher.hpp`, is still built and applied
when an archive has no value index — there it is the only pruning before the engine.

### 2.3 Column value filters — `components/core/src/clp_s/ColumnValueFilter.{hpp,cpp}`

At `ArchiveWriter::close`, `SchemaWriter::build_value_filters` collects, per schema table and per
node id, the distinct keys of its `Int64ColumnWriter` (the values) and `VariableStringColumnWriter`
(the dictionary IDs, stable at read time) columns. A node id that appears in several columns of one
schema (a rule repeated in the shape) gets **one** filter over the union — the first cut kept only
the last column's and silently dropped 5 `SocketTimeoutException` records; the reader now rejects
duplicate `(schema, node)` entries as corrupt. Other column kinds (delta-encoded ints, floats,
arrays, …) get no filter and are never pruned.

`ColumnValueFilter` = `num_keys` + `[min_key, max_key]` + a `clp_s::filter::BloomFilter` over the
8-byte keys, sized for a 1 % false-positive rate within a 2^22-bit budget: past the budget the rate
is raised instead (clamped at 50 %, so a column with more than ~2.9 M distinct keys still gets a
larger array). `may_contain(key)` checks
the range then the Bloom; `may_contain_in_range(lo, hi)` is the range overlap only.
`ArchiveWriter::store_column_value_filters` writes `/column_value_filters` (1,366,179 bytes here,
`verify-misc.log`);
`ArchiveReader::get_column_value_filter(schema_id, node_id)` serves it from a lazily loaded map,
preloaded in `open_packed_streams()` because only one section reader may be checked out at a time.

Consultation: `QueryRunner::constant_propagate` runs per schema on a copy of the expression before
any table is read. `evaluate_with_column_value_filter` handles EQ/NEQ leaves on a resolved column:
an integer leaf probes the value (or, for a `<digits>*` pattern, the ranges `[D·10^k, D·10^k+10^k−1]`
against `[min,max]`); a string leaf takes the dictionary IDs `global_init` already matched for the
query string and probes each (skipped above `cMaxColumnValueFilterProbes = 256`). A leaf proven
absent folds to `False` (NEQ/inverted to `True`) and the AND/OR folding skips the schema entirely.
Block-ID query: 367 → 12 schema tables read, 5,189,089 → 154,338 messages evaluated
(`telemetry-final.log`, archive without vs with the section).

### 2.4 `ColumnScan` numeric wildcards — `components/core/src/clp_s/search/ColumnScan.cpp`

Integer/float leaves with wildcards (`5667*`, `*5667*`) previously disqualified their schemas from
the bitmap fast path and fell to the per-message `QueryRunner` (`as_var_string` + `fmt` +
case-insensitive `wildcard_match_unsafe`). `build_numeric_wildcard_filter` now formats each value
with `fmt::format_to_n` and matches it with `NumericPatternMatcher`: for patterns made only of
digits, `-`, `.` and `*` it does an anchored prefix/suffix check plus ordered substring search; any
other pattern uses the general matcher. `*5667*` no longer falls to `QueryRunner` at all (`num_query_runner_filters = 0`,
`telemetry-final.log`): 2.6 s → 0.5 s before the column filters.

### 2.5 Engine fallback and the Rust patch

The engine still answers capped shapes, unbounded rules, case-insensitive and unparsable queries,
so the `log-surgeon` pin moves from `modelconsumer/log-surgeon@bb41b06` to
`jackluo923/log-surgeon@96a4fb4` = `bb41b06` + `bc54b07` (prefilter, hoist, caches) +
`96a4fb4` (hot loops) — `git diff bb41b06 96a4fb4` in that repository (12 files:
`src/prefilter.rs`, `tests/anchor_probe.rs` new; `c_interface.rs`, `interval_tree.rs`, `lib.rs`,
`nfa.rs`, `nfa/regex_construction.rs`, `nfa/search_decomposition.rs`, `parser.rs`,
`parsing_spec.rs`, `parsing_spec/spec_file.rs`, `search.rs` modified; FFI signature unchanged):

- **Character-set prefilter** (`prefilter.rs`): a conservative per-rule `Charset` from the regex
  AST (unmodelled constructs widen to universal), `shape_profile` tokenisation mirroring the C++
  tokenizer, and `run_can_appear`, a DP proving a shape cannot produce some literal run of the query
  contiguously; such shapes return before any NFA is built.
- **Query-NFA hoist**: the query automaton is built once per `search_by_log_shapes` call instead of
  once per shape.
- **Per-shape caches** on the long-lived `Parser` (`SearchCaches`: shape NFAs and profiles behind
  `Mutex<BTreeMap<String, Arc<…>>>`), threaded through the FFI entry point. Not on `ParsingSpec`,
  whose `const BLANK` forbids interior mutability (E0492).
- **Hot-loop fixes**: `Tnfa::intersect` keys its visited-state map by `(NfaIdx, NfaIdx)` with a
  cheap hasher instead of a `BTreeMap` of state pairs, and `PathEdge` orders by variant rank and
  integer indices instead of comparing `Regex`es; live states by plain reverse reachability instead
  of an SCC condensation; no `format!` on the hot paths; `concat`/`or` re-base states in place
  instead of cloning `NfaState`s; the cached DFA is deserialised from a slice of the spec file.

Net effect, measured end to end on the engine path (skeleton filter, no decomposer):
prefilter + hoist + caches alone 17.1 s → all of it 4.9 s on the block-ID query, `*a*b*` 215 → 81 s,
`*?????*` 547 → 280 s (§3.1). Irrelevant for queries the decomposer handles locally, which on this
archive is all of them. The patch is built with `--no-default-features` (no JIT, §A.2).

### 2.6 Why it is sound

Every approximation over-approximates, and a candidate is dropped only on a proof it cannot match:

- the value index answers `false` only when no recorded value has the k-gram/anchor/length; an
  incompletely recorded rule is `unbounded` and never prunes;
- the decomposer enumerates every alignment the engine would (prefix-anchored, `*` shareable
  between a placeholder and its neighbours), dropping only alignments with an uncertifiable piece —
  and the downstream evaluation of every leaf is exact, so extra interpretations cost time, not
  correctness;
- a column filter says "absent" only when the key is outside `[min,max]` or the Bloom filter, which
  never reports a stored key as absent; unsupported column kinds have no filter;
- the skeleton filter treats placeholders as Σ*, the char-set prefilter widens anything unmodelled
  to universal, and both require only that some literal run be *impossible*.

## 3. Measurements

### 3.1 Stage by stage

Same queries and archive family. Columns, left to right: oss clpp (JIT engine, no patch, no
index); the engine path with the skeleton filter — first the no-JIT engine carrying only the
prefilter/hoist/cache part of the patch, on the index-less archive (`diff-1.log`/`diff-2.log` OLD
column, single runs), then the full patch including the hot-loop fixes, using the commit-`f787c380`
binary, which predates the decomposer and so takes the engine path even though the archive
(`hive-24hr-clpp-idx`) carries the index (`bench-skeleton-patched-2.log`, median of 3; the two slow
queries from `bench-skeleton-patched.log`); "items 1–4" = first local-decomposition build without
column filters (`diff-1.log`/`diff-2.log` NEW column); final = the branch head
(`bench-final-cvf.log`).

| query | oss clpp | skeleton + engine (prefilter/hoist/caches) | skeleton + engine (full patch) | items 1–4 | **items 1–5 (final)** |
|---|---|---|---|---|---|
| `*blk_1073746491_5667*` | 23.3 s | 17.1 s | 4.9 s | 0.52 s | **0.145 s** |
| `*blk_1073746491_*` | 23.3 s | 18.6 s | 4.8 s | 0.49 s | **0.136 s** |
| `*blk_*_5667*` | 40.3 s | 36.0 s | 10.3 s | 0.60 s | **0.158 s** |
| `*blk_10737464??_*` | 22.7 s | 17.4 s | 5.1 s | 0.58 s | **0.214 s** |
| `*5667*` | 30.8 s | 25.1 s | 13.9 s | 2.6 s | **0.39 s** |
| `*1073746491*` | 31.9 s | 29.2 s | 14.2 s | 1.26 s | **0.23 s** |
| `*Exception*` | 23.5 s | 16.3 s | 4.8 s | 0.45 s | **0.127 s** |
| `*.log*` | 23.7 s | 16.5 s | 4.6 s | 0.61 s | **0.164 s** |
| `*type=LAST_IN_PIPELINE*` | 22.0 s | 1.25 s | 0.65 s | 0.29 s | **0.126 s** |
| `*DataNode*` | 21.8 s | 16.0 s | 4.7 s | 0.39 s | **0.146 s** |
| `*INFO*` | 21.7 s | 15.8 s | 4.7 s | 0.58 s | **0.45 s** |
| `*a*b*` | 215 s | 215 s | 81 s | 0.75 s | **0.67 s** |
| `*?????*` | 475 s¹ | 547 s | 280 s | 1.02 s | **0.89 s** |
| `*zzzz*` (no match) | 22.7 s | 15.7 s | 4.3 s | 0.30 s | **0.065 s** |

Reading it: dropping the JIT and adding the prefilter/hoist/caches (column 2) is worth 1.1–1.45×
on ordinary queries — essentially the ~7 s JIT — and nothing on `*a*b*` (18× on
`*type=LAST_IN_PIPELINE*`, whose `=` lets the character-set prefilter reject most shapes); the engine hot-loop fixes (column 3) another 1.8–3.9×
(3.5× on the block-ID query, 2.7× on `*a*b*`, 2.0× on `*?????*`); the C++ decomposer (column 4)
is the step that removes the engine from the path — 5–17× on most rows, 2.2× on
`*type=LAST_IN_PIPELINE*` (already cheap), 110× on `*a*b*` and 270× on `*?????*`; the column
filters (column 5) are the last 2.5–3.5× on queries whose interpretations touch many schemas. Column 3 also returns the correct 6,863,835 for `*?????*`, so
the over-match in footnote ¹ is specific to the oss combination.

Item-5 ablation (same final binary, archive without → with `/column_value_filters`):
`*blk_1073746491_5667*` 379 → 139 ms, `*Exception*` 356 → 126, `*.log*` 462 → 165,
`*1073746491*` 447 → 230, `*5667*` 507 → 393, `*DataNode*` 225 → 146; queries that already touched
few schemas (`*PacketResponder*`, `*INFO*`, `*a*b*`, `*?????*`) are unchanged. The 0.52 → 0.38 s
between "items 1–4" and that ablation came from the DP literal-skip (decomposition 49 → 17 ms) and
the numeric `ColumnScan` path.

Against clp-s the optimized build is 4–5× faster on the common queries and 6–11× on bare
numbers (`*5667*`, `*1073746491*`): a bare number forces clp-s to scan every schema whose
integer, float or dictionary variables could hold it, whereas the decomposer pins it to the rules
whose values contain it and the column filters to the tables that do. Regular clp-s keeps the
zero-match case (25 ms vs 65 ms) because its dictionary lookup fails before any archive work.

### 3.2 Where the remaining 145 ms go (block-ID query)

From `telemetry-final.log` (`--enable-telemetry`, profiler scopes; `search_archive` = 119 ms of a
145 ms median wall clock):

| phase | ms | notes |
|---|---|---|
| process start, archive open, metadata | ~26 | wall − `search_archive`; shared with clp-s |
| `ClppMatcher::prepare_shapes` (the ctor before `4a0b3b61`) | 36 | value index read 15, `CompactShape` build 15, schema→shape index ~6 (the last stays in the ctor) |
| local decomposition, 1,270 shapes | 17 | 395 interpretations; the 1,263 for `*5667*` cost 11 |
| `SchemaMatch` dictionaries + rest | 33 | 28 is the variable-dictionary read for the string type variants |
| `Output::filter` | 32 | 12 schemas / 154,338 messages: `read_schema_table` 15, scan 19, `global_init` 6 |

A parent-variable query (category 2, `message.blockPoolID: BP-1121897155-*`, 143 ms
`search_archive` of a 165 ms wall clock; `telemetry-cat2-final.log`) has a different profile: no
shape tables and no decomposition to speak of, but 89 ms building the engine parser
(`clpp_parser_init`: 15 ms `parsing_spec_read` + ~70 ms `ParsingSpecBuilder::build`), 26 ms of
dictionaries, 6 ms matcher ctor, and 13 ms of `Output::filter` over 45 schemas. A leaf query
(category 1) skips all of the clpp-specific work: `schema_match` 33 ms, `Output::filter` 12 ms.

### 3.3 Costs

| | clp-s | oss clpp | + rule value index | + column value filters (final) |
|---|---|---|---|---|
| archive bytes | 55,379,378 | 42,589,517 | 42,736,318 (+0.34 %) | 44,102,522 (+3.55 %) |
| compression ratio (2,353,623,901 B input) | 42.5× | 55.3× | 55.1× | 53.4× |
| single-threaded ingest | 10 s | 31.2 s (26.7 s with the no-jit engine) | 38 s | 34.7 s |

Every other section is byte-identical across the three clpp archives except `header`, which grows by
23 B and 25 B as the new section names are listed; the two new sections are otherwise pure additions.
Ingest of the 2.35 GB input: the oss and final columns are 3 interleaved runs each
(`ingest-ab.log`: oss 31.2/31.2/31.3 s, final 34.9/34.7/34.5 s), i.e. the shipped optimized
build compresses **11 % slower** than shipped oss clpp. That understates the cost of the two new
sections, because the optimized build also dropped the engine JIT, which is worth 4.5 s at ingest:
the oss code linked against the same no-jit engine takes 26.7 s (`ingest-oss-nojit.log`,
26.7/26.6/26.7 s), so the rule value index plus the column value filters add **8.0 s (+30 %)** on
their own. The clp-s (10 s, `hive-plain-compress.log`) and index-only (38 s) columns are single
runs. If 1.37 MB of Bloom filters
matters, `ColumnValueFilter::cMaxBloomBits` (2^22) and `cTargetFalsePositiveRate` (1 %) are the
knobs — the schema skip only needs to be usually right, and the range check alone already prunes
most tables for numeric leaves.

## 4. How to build and run

Everything below was run inside the CLP core dependencies container
(`clp-core-dependencies-x86-manylinux_2_28:dev`) with the repo mounted at `/mnt/repo` and the
prebuilt dependency tree at `/mnt/repo/build/deps/cpp`; the same commands work in any environment
that can build clp-s.

### 4.1 The engine: patched log-surgeon, built without JIT

Commit `9f66d6db` (`taskfiles/deps/main.yaml`, task `log-surgeon`) re-points the dependency from
`modelconsumer/log-surgeon@bb41b06` to `jackluo923/log-surgeon@96a4fb4` (branch
`clpp-search-prefilter` = `bb41b06` + the two engine commits; SHA-256 of the GitHub tarball
`7f8bb69f…`) and changes its build line from `cargo build --release` to
`cargo build --release --no-default-features`, dropping the crate's `default = ["jit"]` feature (the
"−7 s per process" of §A.2). `task deps:core` therefore builds the right engine with no manual
step (a change to `taskfiles/deps/main.yaml` invalidates the dependency checksum files, so an
existing deps tree is refreshed on the next run).

Manual equivalent, for a checkout that has not taken the bump:

```sh
git -C <clone of jackluo923/log-surgeon> diff bb41b06 96a4fb4 -- rust > engine.patch
cd <deps>/log-surgeon-src/rust
git apply -p2 engine.patch                            # 12 files; FFI signature unchanged
cargo build --release --no-default-features
# install where clp-s's CMake config looks for it (mirrors the taskfile; the taskfile's
# toolchain env sets CARGO_TARGET_DIR=build/rust-targets, a plain cargo uses ./target)
cp target/release/liblog_surgeon.a <deps>/log-surgeon-install/lib/
cp -r cxx/log_surgeon           <deps>/log-surgeon-install/include/
```

The no-JIT build is strictly better for clp-s (search −7 s, compression 31 → 26 s); the engine
patch only matters when a query falls back to the engine (§2.2) — the clp-s commits work against
the unpatched engine, just slower on that path (§3.1, columns 2 vs 3).

During this work the patched no-JIT install was kept outside the deps tree and bind-mounted over
it (`-v $J/ls-install-nojit-v2:/mnt/repo/build/deps/cpp/log-surgeon-install`). Note that CMake
will not relink `clp-s`/`unitTest` when the library under that path changes but its mtime does
not — `rm build/core/clp-s build/core/unitTest` before rebuilding after switching engines.

### 4.2 clp-s

```sh
git switch clpp-wildcard-search
task core            # = deps:core + cmake generate (-DCLP_ENABLE_PROFILING=0) + build into build/core
# or, with build/deps/cpp already populated (CMakeLists.txt picks it up automatically):
cmake -S components/core -B build/core -DCMAKE_BUILD_TYPE=Release
cmake --build build/core --target clp-s -j 24
cmake --build build/core --target unitTest -j 24 && build/core/unitTest   # optional
```

No new CMake options: the two archive sections and all search paths are compiled unconditionally
and used only for `--experimental` archives. The profiling scopes report through the existing
telemetry (`--enable-telemetry`), so the default `CLP_ENABLE_PROFILING=0` build is enough.

### 4.3 Ingest

```sh
build/core/clp-s --experimental c --timestamp-key timestamp \
    --parsing-specification blk_id_full_log_message.spec.cached.txt \
    <archives-dir> hive-24hr-ts.jsonl
```

This is the unchanged clpp ingest command; the branch adds the `/rule_value_index` and
`/column_value_filters` sections to every `--experimental` archive automatically (§3.3 for the
cost). Use the `.cached` form of the parsing spec (spec text + serialized DFA after a `===` line,
produced by `~/.local/bin/generate_cached_parsing_spec.py` with the `log-surgeon-ffi` venv):
the raw spec costs ~236 s of DFA determinisation per process (§A.2), and the cache must come from
the same log-surgeon ref that loads it. Inputs on this host: `/home/jack/hive-24hr-ts.jsonl`
(2.35 GB), `/home/jack/blk_id_full_log_message.spec.cached.txt` (29 MB); archives under
`/home/jack/clpp-bench/archives/`.

Archives ingested by the oss build (no sections) still search, via the skeleton filter + engine
path (`hive-24hr-clpp-cached` in every table above). The reverse also works: the oss `772db1bb`
binary reads `hive-24hr-clpp-cvf` and ignores the two extra sections (`*type=LAST_IN_PIPELINE*`:
5,920 in 22.2 s, checked 2026-09-16).

### 4.4 Search

```sh
# count only
build/core/clp-s --experimental s <archive> 'message: *blk_1073746491_5667*' --count

# with the stage timings and counters quoted in this document
CLP_TELEMETRY_ENDPOINT=ostream build/core/clp-s --experimental s --enable-telemetry \
    <archive> 'message: *blk_1073746491_5667*' --count 2>&1 \
  | grep -E 'count|clp\.query\.(num_|search_archive)'
```

Whole-message queries take the `message: <wildcard>` form; use the closing `*` for a contains
query (§A.5). Leaf and parent-variable queries name the node (`message.blockID.blockNum: 1073746491`,
`message.blockPoolID: BP-1121897155-*`); a value containing a space or colon must be quoted
(`message.prefix: "WARN *"`).
Counters worth reading: `num_clpp_shapes_decomposed_locally` / `_by_engine` (a non-zero engine
count means a shape fell back — capped, unbounded rule, malformed, or a case-insensitive/unparsable
query), `num_query_runner_filters` (0 = every leaf stayed on the vectorised `ColumnScan` path),
`num_schemas_scanned` / `num_messages_evaluated` (what the column filters left), and the
`clpp_prepare_shapes.*` / `clpp_parser_init.*` / `output_filter.schema_scan.*` durations (§3.2).

Regular clp-s for comparison is the same binary without `--experimental` on an archive ingested
without `--experimental` (`hive-24hr-plain`).

### 4.5 Benchmark scripts

In `/home/jack/.claude/jobs/3c7496e2/tmp` (job scratch, not durable): `bench-final2.sh <clp-s>
<archive> <queries-file>` prints count + median/min of 3 wall-clock runs per query, prefixing each
line with `message: ` (category 3); `bench-cat.sh <clp-s> <archive> <queries-file> [flags]` is
the same with the full KQL on each line and the flags (`--experimental`) passed through
(categories 1–2 and the clp-s substitutes);
`bench-reg.sh` is the clp-s (non-CLP+) variant; `diff-run.sh <clp-s> <archive-a> <archive-b>
<queries-file>` flags count mismatches between two archives; `telemetry-final.sh` captures the
§3.2 profile; `ingest-hive.sh <out>` times an ingest, `ingest-ab.sh <runs>` interleaves oss and
optimized ingests and `ingest-one.sh <clp-s> <tag> <runs>` times one binary (`clp-s-oss-nojit` is
`772db1bb` linked against the fork engine). The `final-queries.txt` / `diff-queries.txt`
lists are the 17-query set (the 14 category-3 rows plus `*IOException*`, `*PacketResponder*`,
`*ERROR*`) and the 43-query differential set; `cat1-final.txt` / `cat2-final.txt` are the
category-1/2 rows and `cat-nearest.txt`, `nearest2.txt`, `cat2-new-nearest.txt` their clp-s
substitutes (§A.6).

## 5. Deliverables

Branch `clpp-wildcard-search` = `772db1bb` + eight code commits + this document (the two commits
after the document were added when the per-category benchmark exposed the category-1 regression);
each of the eight clp-s commits was built and unit-tested on its own (the intermediate prototype
commit `8498cfba` on branch `clpp` and its
`CLPP_RULE_VALUE_INDEX` side file are superseded and not in this history):

| commit | optimization | files |
|---|---|---|
| `8cd04511` clp-s: Add profiling scopes to the clpp search path | instrumentation (+ read each dictionary once) | `clp-s.cpp`, `Output.cpp`, `SchemaMatch.{hpp,cpp}` |
| `f4eae169` clp-s: Record per-rule value signatures at ingest and store them in CLP+ archives | §2.1 | `clpp/RuleValueIndex.{hpp,cpp}`, `JsonParser.cpp`, `ArchiveWriter`, `ArchiveReader`, `archive_constants.hpp`, `Utils.cpp`, test |
| `f787c380` clp-s: Prune log shapes structurally before handing them to the clpp engine | skeleton filter | `ClppShapeQueryMatcher.hpp`, `ClppMatcher`, test |
| `bcaaffd7` clp-s: Decompose clpp wildcard queries against log shapes in C++ using the rule value index | §2.2 (incl. the DP micro-optimizations) | `ClppShapeDecomposer.hpp`, `ClppMatcher`, `SchemaMatch`, `Output.cpp`, `SearchTelemetry`, test |
| `5a9375bf` clp-s: Store per-column value filters in CLP+ archives and skip schema tables that cannot match | §2.3 | `ColumnValueFilter.{hpp,cpp}`, `SchemaWriter`, `ColumnWriter`, `ArchiveWriter`, `ArchiveReader`, `QueryRunner`, CMake, test |
| `2c9b2b26` clp-s: Evaluate numeric wildcard leaves on the ColumnScan fast path | §2.4 | `ColumnScan.cpp` |
| `9f66d6db` deps: Bump log-surgeon to a fork with the search prefilter and intersection optimizations; build it without the JIT feature | §2.5, §A.2 | `taskfiles/deps/main.yaml` |
| `c095de9d` docs: Add the CLP+ wildcard-search optimization design doc | this document | `docs/src/dev-docs/design-clpp-wildcard-search.md`, `index.md` |
| `4a0b3b61` clp-s: Build the clpp shape tables only when a query decomposes against the log shapes | §0.1 #8 | `ClppMatcher.{hpp,cpp}` |
| `90fb2a04` clp-s: Share the engine parser initialization between the clpp decomposition paths and profile it | instrumentation for §3.2 (category 2) | `ClppMatcher.{hpp,cpp}` |

Relative to the baseline (clp-s commits only): 37 files (10 new), +4,163/−96 lines. The diff contains no reformat-only hunks;
the one mechanical change is `SchemaWriter::append_column` gaining the node id (ten call sites in
`ArchiveWriter::initialize_schema_writer`), which the column filters need to key filters per node.

- new: `clpp/RuleValueIndex.{hpp,cpp}`, `clp_s/ColumnValueFilter.{hpp,cpp}`,
  `clp_s/search/ClppShapeQueryMatcher.hpp`, `clp_s/search/ClppShapeDecomposer.hpp`; tests
  `test-ClppRuleValueIndex.cpp`, `test-ClppShapeQueryMatcher.cpp`, `test-ClppShapeDecomposer.cpp`,
  `test-ColumnValueFilter.cpp`
- modified: `ArchiveWriter`, `ArchiveReader`, `SchemaWriter`, `ColumnWriter`, `JsonParser`,
  `archive_constants.hpp`, `Utils.cpp`, `clp-s.cpp`; `search/ClppMatcher`, `ColumnScan`, `Output`,
  `QueryRunner`, `SchemaMatch`, `SearchTelemetry`; four `CMakeLists.txt` (`core`, `clp_s`,
  `clp_s/search`, `clpp`) and `cmake/Options/options.cmake` (`clp_s::filter` is now a dependency of
  the archive reader and writer)

Dependency (Rust): `jackluo923/log-surgeon` branch `clpp-search-prefilter` = `bb41b06` +
`bc54b07` "search: Prefilter log shapes by literal-run feasibility, build the query NFA once, and
cache per-shape automata on the Parser" + `96a4fb4` "nfa/search: Remove allocation and cloning from
the per-shape intersection path; deserialize cached specs from a slice" (12 files, +950/−198;
`cargo fmt --check` clean, 49 lib + 2 integration tests pass with `--no-default-features`). Only the
engine fallback needs it; the local path works against the unpatched engine, just slower when it
falls back. Upstream (`modelconsumer/log-surgeon`, branch `log-mechanic`) has since moved 13 commits
past `bb41b06` with its own work in `search.rs`/`search_decomposition.rs` (including a "filter log
shapes by boundary chars" commit), so re-basing these two commits onto it will need a merge.

Format: two new sections in experimental archives, both optional at read time. A reader without
them (or an archive without them) takes the engine path with the skeleton filter.

## 6. Verification

| check | result |
|---|---|
| 43-query differential, archive without vs with column filters, same binary (`diff-cvf-2.log`) | all counts identical |
| 43-query differential, engine path vs local decomposer (`diff-1.log`, `diff-2.log`) | all counts identical |
| clp-s vs optimized clpp, the 14 category-3 queries | all counts identical |
| oss clpp vs optimized clpp, the 10 category-1 and 14 category-2 queries (`bench-cat-oss.log`, `bench-cat-final.log`) | all counts identical |
| engine path with the full patch (commit `f787c380` binary, `bench-skeleton-patched*.log`) vs final, 17 queries | all counts identical, including 6,863,835 for `*?????*` |
| decode-and-grep of the raw 2.35 GB log (`blk_` 161,615; block ID 26; `Exception` 123,309; `.log` 35,198; `IOException` 27; `PacketReceiver` 6; `datanode` 130,410) | match |
| `[ClppShapeDecomposer]` differential vs a reference wildcard matcher (8 synthetic shapes + the real hive shape), `[ClppRuleValueIndex]` brute-force soundness, `[ColumnValueFilter]`, `[ClppShapeQueryMatcher]` (`verify-misc.log`) | 14 cases, 37,693 assertions pass |
| full `unitTest` (165 cases, `unittest-full-2.log`) | 162 pass; the 3 failures are the known environmental ones — `std::bad_alloc` in `parser.ingest()` at `clp_s_test_utils.cpp:62` and the `compress_archive` sites `test-clp_s-search.cpp:285` / `test-clp_s-end_to_end.cpp:274` — the same three that fail on unmodified HEAD in this container (2026-09-15 baseline: 151 cases, 3 failed) |
| full `unitTest` on `4a0b3b61` (165 cases) | 164 pass; the one failure is `test-GrepCore.cpp` `process_raw_query` `[dfa_search]`, which fails whenever `unitTest` is linked against a from-source log-surgeon instead of the container's prebuilt one (a version mismatch in the clp — not clp-s — grep test, unrelated to this branch); the `[ClppShapeDecomposer],[ClppShapeQueryMatcher],[ClppRuleValueIndex],[ColumnValueFilter],[clp-s]` subset passes on `90fb2a04` (30 cases, 633,553 assertions) |
| Rust: 49 log-surgeon lib unit tests (5 in `prefilter.rs`, `job2-fulltest-nodflt.log`); `prefilter_never_drops_a_match` (8 shapes × 317 queries, 0 false negatives); FFI differential patched vs unpatched engine (`ffi/out_patched.txt` = `ffi/out_pristine.txt`) | pass / identical output |
| earlier adversarial passes on the value index: 23,617 shape×query cross-check vs brute-force substitution (`crosscheck_kgram.out`), exhaustive small-shape DP proof and ASan/UBSan runs (session transcript only) | 0 unsound; a positive control reported 17,613 (`crosscheck_kgram.selftest.out`) |

Reproduce: `bench-final2.sh <clp-s> <archive> <queries>` (clpp, category 3, median of 3),
`bench-cat.sh <clp-s> <archive> <queries> [--experimental]` (full KQL per line: categories 1–2 and
the clp-s substitutes), `bench-reg.sh`
(clp-s), `telemetry-final.sh`, `ingest-hive.sh <out>` — in the job's scratch directory
(`/home/jack/.claude/jobs/3c7496e2/tmp`) alongside the logs named above.

## 7. Next levers

1. Skip the variable-dictionary read (~26 ms) when no `VarString` leaf survives the column filters
   — requires running the filter pass before `populate_string_queries`.
2. Cache the value index and `CompactShape`s across queries in a long-lived process (~29 ms per
   whole-message query); they are per-archive constants.
3. Parent-variable queries (category 2) spend 85–90 ms of ~165 ms building the engine parser
   (§3.2): cache the parser per archive in a long-lived process, or make
   `ParsingSpecBuilder::build` cheaper for a cached spec (only the parsing DFA comes from the
   cache; the rule regexes are re-parsed and the search NFA rebuilt every time — not yet profiled
   on the Rust side). Would roughly halve this category.
4. Column filters for more column kinds (delta-encoded timestamps, floats) and a per-schema
   signature for string leaves whose dictionary match set exceeds 256 IDs.
5. Upstream the two engine commits to `modelconsumer/log-surgeon` (rebase onto `log-mechanic`,
   which now carries overlapping work — §5) so the pin can return to upstream.
6. The category-2 zero-result quirk (§A.6) — an oss engine behaviour, but worth a fix while the
   fork is being upstreamed.

---

## Appendix A. History and lessons

### A.1 The character-set prefilter era (why it was not enough)

The first optimizations were a C++ structural (skeleton) filter, a Rust character-set prefilter, a
query-NFA hoist and per-shape caches (all still present, §2.5). On the Rust-side micro-benchmark
recorded in the first revision of this document they gave 20–35× on queries with rare punctuation
(`*hdfs://…*`, `*java.io.IOException: Premature EOF…*`) and 76–234× on anchored prefixes, but only
1.1–1.3× on the queries people run:
`*Exception`, `*.log`, `*blk_…*` pruned 34–40 shapes of 1,243. Measured over the distinct rules the
shapes reference, `E` is emittable by 37 rules and `.` by 25, and `%prefix.class%` (825 shapes) can
emit every character of any identifier. The lever had to be **what values a rule actually took**,
not what its regex could emit.

### A.2 The 7.8 s floor was the JIT

Sampling a zero-candidate run put every stack inside `cranelift_codegen`/`regalloc2`: with the
cached spec, `log_surgeon::Parser::new` measured 6.99 s with the `jit` feature (the Cargo default)
and tens of microseconds without (38–45 µs across runs). Building the engine with `--no-default-features` removed the floor and was faster in
both directions (compression 30.9 s → 26.4 s). The `.cached` spec is a different cost: it removes the
236 s `Tdfa::for_rules` determinisation.

### A.3 A sampled index over-prunes

An early estimate of 94 % shape rejection came from an index built on 1 M sampled records with a
20 K per-rule cap. A sampled index sets a subset of the true bits and therefore prunes *unsoundly*.
The complete index (all 6.86 M records, 0 rules capped) prunes less — 1,892 → 866 interpretations
on the block-ID query — and that is the number that matters. Hence the `unbounded` escape hatch for
any rule whose values were not all recorded.

### A.4 The side-file prototype and `*.log`

Before the decomposer, the value-aware bound only *filtered* candidates and the engine still
intersected the survivors: 1.78 s on the block-ID query (engine 53 %), and `*.log*` stayed at
17–20 s because it pruned only 6 % of interpretations while the value-aware DP cost 2.6 s more than
the skeleton DP. Two conclusions drove the final design: the engine's output is exactly the set of
alignments the DP already enumerates (`"*blk_" [blockNum="1073746491"] "_" [genStamp="5667*"] "*"`
etc.), so the DP could emit interpretations directly; and ~0.5 s of the "fixed floor" was the
prototype's own side-file parse and hashed oracle lookups, which the in-archive index and interned
rule ids removed.

### A.5 Regular clp-s semantics caveat

The clp-s (non-CLP+) engine needs the *closing* `*` for a contains query: `message: *Exception` returns
nothing there (re-checked in `verify-misc.log`) while `*Exception*` returns 123,309; clpp treats
both as contains. All comparisons use the
`*X*` form, on which the engines agree exactly. Regular clp-s cannot express the leaf-scoped or
parent-variable queries clpp exists for (`message.blockID.blockNum: *1073746491*`, 23 matches in
0.55 s) at all — the §0 category-1/2 tables use the nearest substring query instead (§A.6).

### A.6 Query categories

`SchemaMatch::build_clpp_query_filter` routes a wildcard by target: (1) log shape + leaf values
via `build_shape_match_filter` (already fast in oss), (2) a named parent variable via
`decompose_by_rule_name`, (3) the message field — the empty-`rule_name` branch from
`build_ls_rule_name`, which is what §§1–3 target. Category 1 also covers `shape(message): "…"`,
a wildcard on the log shape's text in which `%rule.leaf%` stands for a placeholder.

The clp-s substitute used for each category-1/2 row of §0 (clp-s, archive `hive-24hr-plain`,
median of 3; `bench-cat-nearest-clps.log`, `bench-nearest2-clps.log`):

| clpp query (count) | clp-s substitute | count | ms |
|---|---|---|---|
| `message.blockID.blockNum: 1073746491` (23) | `message: *1073746491*` | 26 | 2,437 |
| `message.blockID.blockNum: 10737464*` (2,608) | `message: *10737464*` | 2,865 | 2,430 |
| `…blockNum: 1073746491 AND …genStamp: 5667` (23) | `message: *1073746491_5667*` | 26 | 628 |
| `message.blockPoolID.poolID: 1121897155` (109,484) | `message: *1121897155*` | 129,079 | 2,391 |
| `message.containerID.appSeq: 0099` (114,918) | `message: *_0099_*` | 114,923 | 764 |
| `message.attemptID.type: m` (980,424) | `message: *_m_*` | 1,406,166 | 1,327 |
| `message.hostPort.host: 172.31.17.31` (159) | `message: *172.31.17.31*` | 1,621 | 636 |
| `shape(message): "*Receiving BP-*"` (20,500) | `message: "*Receiving BP-*"` | 20,500 | 600 |
| `shape(message): "*PacketResponder*" AND …blockNum: 1073746491` (3) | `message: *PacketResponder*1073746491*` | 3 | 1,680 |
| `shape(message): "*%blockID.blockNum%*" AND …host: 172.31.17.31` (104) | `message: *blk_*172.31.17.31*` | 645 | 1,784 |
| `message.blockPoolID: BP-1121897155-*` (109,484) | `message: *BP-1121897155-*` | 129,079 | 641 |
| `message.blockPoolID: *-1427088167814` (109,484) | `message: *-1427088167814*` | 129,079 | 2,332 |
| `message.containerID: container_1427088391284_0099_*` (114,917) | `message: *container_1427088391284_0099_*` | 114,919 | 691 |
| `message.containerID: *_0099_01_000001` (114,918) | `message: *_0099_01_000001*` | 114,919 | 667 |
| `message.attemptID: attempt_1427088391284_0059_*` (33,187) | `message: *attempt_1427088391284_0059_*` | 41,130 | 733 |
| `message.attemptID: *_m_000001_0` (13,587) | `message: *_m_000001_0*` | 18,516 | 757 |
| `message.jobID: job_1427088391284_0*` (44,824) | `message: *job_1427088391284_0*` | 470,231 | 930 |
| `message.yarnAppID: *_0036` (2,070) | `message: *_0036*` | 56,186 | 801 |
| `message.taskID: *_m_000001` (540) | `message: *_m_000001*` | 20,526 | 790 |
| `message.blockID: *_5667` (23) | `message: *_5667*` | 26 | 620 |
| `message.blockID: blk_1073746491_?667` (23) | `message: *blk_1073746491_?667*` | 26 | 1,754 |
| `message.prefix: "INFO org.apache.hadoop.hdfs.*"` (160,501) | `message: "*INFO org.apache.hadoop.hdfs.*"` | 160,501 | 629 |
| `message.prefix: "WARN *"` (15,544) | `message: "*WARN *"` | 19,684 | 605 |
| `message.byteSize: *MB` (207) | `message: *MB*` | 353,392 | 599 |

The substitute's count differs wherever the value also occurs in messages where it is not parsed
as that variable (`blk_…_5667` in three more messages, e.g. inside a `blockIDList`; `1121897155`
in 19,595 more, where it is not a `blockPoolID`) — which is the point of the scoped query.

**Category-2 zero-result quirk (oss behaviour, both builds).** A parent-variable wildcard whose
`*` must cover exactly one trailing sub-pattern after a delimiter returns nothing:
`message.jobID: job_1427088391284_*` 0 (but `job_1427088391284_0*` 44,824),
`message.blockID: blk_1073746491_*` 0 (but `blk_1073746491_5*` and `blk_1073746491_?667` 23),
`message.yarnAppID: application_1427088391284_*`, `message.taskID: task_1427088391284_0059_m_*`,
`message.appAttemptID: appattempt_1427088391284_0059_*`, `message.blockID: blk_*_5667`,
`message.blockPoolID: BP-*-172.31.17.135-*`, `message.hostPort: "172.31.17.31:*"` all 0.
Telemetry shows `num_clpp_interpretations: 0` — the engine's `search_by_name` produces no
interpretation, before any archive data is touched — and oss clpp (`772db1bb`, unpatched engine)
gives the same 0 for the two cases checked (`job_…_*`, `blk_…_*`; `probe-oss.txt`). Not
investigated; the §0 rows avoid the pattern.
