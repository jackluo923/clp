// Unit tests for `clp_s::search::ShapeDecomposer`: the value-index-certified, C++-side query
// decomposition. The core check is differential: for every message a shape can produce from a set
// of values, the query matches the message (reference wildcard semantics) iff some emitted
// interpretation is satisfied by the message's placeholder values (the downstream semantics).

#include <cstddef>
#include <cstdint>
#include <functional>
#include <map>
#include <set>
#include <span>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/search/ClppShapeDecomposer.hpp>
#include <clp_s/search/ClppShapeQueryMatcher.hpp>
#include <clpp/RuleValueIndex.hpp>

using clp_s::search::build_compact_shape;
using clp_s::search::CompactShape;
using clp_s::search::DecomposeStatus;
using clp_s::search::normalize_query_tokens;
using clp_s::search::parse_shape_query;
using clp_s::search::piece_may_match;
using clp_s::search::QueryToken;
using clp_s::search::render_query_piece;
using clp_s::search::rule_id_t;
using clp_s::search::ShapeDecomposer;
using clp_s::search::ShapeInterpretation;
using clpp::RuleSignature;
using clpp::RuleValueIndex;

namespace {
#define CLPP_CHECK(condition, message) \
    do {                               \
        INFO(message);                 \
        CHECK((condition));            \
    } while (false)

/**
 * A shape plus its interned rule table and value index, ready for decomposition.
 */
struct Fixture {
    CompactShape shape;
    std::vector<std::string> rule_names;
    std::map<std::string, rule_id_t> rule_ids;
    RuleValueIndex index;
    std::vector<RuleSignature const*> signatures;
    // Per placeholder (in shape order): the rule ID.
    std::vector<rule_id_t> placeholder_rules;

    Fixture(std::string_view shape, std::map<std::string, std::vector<std::string>> const& values) {
        auto const intern = [this](std::string_view rule) -> rule_id_t {
            auto const it{rule_ids.find(std::string{rule})};
            if (it != rule_ids.end()) {
                return it->second;
            }
            auto const id{static_cast<rule_id_t>(rule_names.size())};
            rule_ids.emplace(std::string{rule}, id);
            rule_names.emplace_back(rule);
            return id;
        };
        REQUIRE(build_compact_shape(shape, intern, this->shape));
        for (auto const& [rule, rule_values] : values) {
            for (auto const& value : rule_values) {
                index.add_value(rule, value);
            }
        }
        for (auto const& name : rule_names) {
            signatures.push_back(index.find(name));
        }
        for (size_t p{0}; p < this->shape.num_placeholders(); ++p) {
            placeholder_rules.push_back(this->shape.placeholder_rule(p));
        }
    }

    /**
     * @return The message obtained by substituting `chosen` (one value per placeholder).
     */
    [[nodiscard]] auto build_message(std::vector<std::string> const& chosen) const -> std::string {
        std::string message;
        for (size_t i{0}; i < shape.num_parts(); ++i) {
            auto const p{shape.placeholder_at_or_after(i)};
            if (shape.is_placeholder(i, p)) {
                message += chosen.at(p);
            } else {
                message.push_back(shape.literal(i, p));
            }
        }
        return message;
    }

    [[nodiscard]] auto decompose(std::string_view query, std::vector<ShapeInterpretation>& out)
            -> DecomposeStatus {
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query(query, tokens));
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, signatures};
        return decomposer.decompose(shape, 1024, out);
    }
};

/**
 * Reference wildcard matcher over query tokens (`*` any run, `?` any one byte).
 * @return Whether `tokens` match all of `text`.
 */
auto tokens_match(std::span<QueryToken const> tokens, std::string_view text) -> bool {
    // dp[j][k]: tokens[j..] matches text[k..].
    size_t const m{tokens.size()};
    size_t const n{text.size()};
    std::vector<std::vector<uint8_t>> dp(m + 1, std::vector<uint8_t>(n + 1, 0));
    dp[m][n] = 1;
    for (size_t j{m}; j-- > 0;) {
        for (size_t k{n + 1}; k-- > 0;) {
            auto const& token{tokens[j]};
            bool ok{false};
            if (QueryToken::Type::AnyMany == token.type) {
                ok = 0 != dp[j + 1][k] || (k < n && 0 != dp[j][k + 1]);
            } else if (k < n) {
                bool const char_ok{QueryToken::Type::AnyOne == token.type
                                   || token.literal == text[k]};
                ok = char_ok && 0 != dp[j + 1][k + 1];
            }
            dp[j][k] = ok ? 1 : 0;
        }
    }
    return 0 != dp[0][0];
}

/**
 * @return Whether `query` (matched against a prefix of the message, like the engine) matches
 * `message`.
 */
auto reference_matches(std::string_view query, std::string_view message) -> bool {
    std::vector<QueryToken> tokens;
    REQUIRE(parse_shape_query(query, tokens));
    normalize_query_tokens(tokens);
    return tokens_match(tokens, message);
}

/**
 * @return Whether `value` satisfies the leaf query `leaf` under the downstream semantics: an exact
 * comparison when the leaf has no wildcards, a whole-value wildcard match otherwise.
 */
auto leaf_satisfied(std::string_view leaf, std::string_view value) -> bool {
    std::vector<QueryToken> tokens;
    REQUIRE(parse_shape_query(leaf, tokens));
    return tokens_match(tokens, value);
}

/**
 * @return Whether some interpretation is satisfied by `chosen` (the value per placeholder), where a
 * leaf on rule `R` is satisfied if any placeholder of rule `R` hosts a satisfying value (the
 * downstream filter is per column, not per placeholder position).
 */
auto interpretations_match(
        Fixture const& fixture,
        std::vector<ShapeInterpretation> const& interpretations,
        std::vector<std::string> const& chosen
) -> bool {
    for (auto const& interpretation : interpretations) {
        bool all_leaves{true};
        for (auto const& leaf : interpretation.leaves) {
            bool some_value{false};
            for (size_t p{0}; p < chosen.size(); ++p) {
                if (fixture.placeholder_rules.at(p) == leaf.rule_id
                    && leaf_satisfied(leaf.query, chosen.at(p)))
                {
                    some_value = true;
                    break;
                }
            }
            if (false == some_value) {
                all_leaves = false;
                break;
            }
        }
        if (all_leaves) {
            return true;
        }
    }
    return false;
}

/**
 * Runs the differential check for one shape over every query in `queries` and every message the
 * shape produces from `values`.
 * @param exact Whether to also assert that no interpretation matches a non-matching message (only
 * valid when placeholder rule names are distinct).
 */
auto differential(
        std::string_view shape,
        std::map<std::string, std::vector<std::string>> const& values,
        std::vector<std::string> const& queries,
        bool exact
) -> void {
    Fixture fixture{shape, values};
    auto const num_placeholders{fixture.placeholder_rules.size()};

    // Enumerate the messages.
    std::vector<std::pair<std::string, std::vector<std::string>>> messages;
    std::vector<std::string> chosen(num_placeholders);
    std::function<void(size_t)> recurse = [&](size_t depth) {
        if (depth < num_placeholders) {
            auto const& rule_values{
                    values.at(fixture.rule_names.at(fixture.placeholder_rules.at(depth)))
            };
            for (auto const& value : rule_values) {
                chosen.at(depth) = value;
                recurse(depth + 1);
            }
            return;
        }
        messages.emplace_back(fixture.build_message(chosen), chosen);
    };
    recurse(0);

    size_t num_matching_messages{0};
    for (auto const& query : queries) {
        std::vector<ShapeInterpretation> interpretations;
        auto const status{fixture.decompose(query, interpretations)};
        CLPP_CHECK(DecomposeStatus::Capped != status, "differential: not capped " + query);
        CLPP_CHECK(
                (DecomposeStatus::NoMatch == status) == interpretations.empty(),
                "differential: status agrees with output " + query
        );
        for (auto const& [message, message_values] : messages) {
            bool const expected{reference_matches(query, message)};
            bool const actual{interpretations_match(fixture, interpretations, message_values)};
            if (expected) {
                ++num_matching_messages;
            }
            if (expected && false == actual) {
                CLPP_CHECK(
                        false,
                        "UNSOUND shape=" + std::string{shape} + " query=" + query
                                + " message=" + message
                );
            }
            if (exact && actual && false == expected) {
                CLPP_CHECK(
                        false,
                        "OVER-APPROXIMATE shape=" + std::string{shape} + " query=" + query
                                + " message=" + message
                );
            }
        }
    }
    CLPP_CHECK(num_matching_messages > 0, "differential: exercised " + std::string{shape});
}

auto leaves_of(ShapeInterpretation const& interpretation, Fixture const& fixture) -> std::string {
    std::string out;
    for (auto const& leaf : interpretation.leaves) {
        out += fixture.rule_names.at(leaf.rule_id) + "=" + leaf.query + ";";
    }
    return out;
}

auto leaf_sets(std::vector<ShapeInterpretation> const& interpretations, Fixture const& fixture)
        -> std::set<std::string> {
    std::set<std::string> out;
    for (auto const& interpretation : interpretations) {
        out.insert(leaves_of(interpretation, fixture));
    }
    return out;
}

std::vector<std::string> const cQueries{
        "*",
        "",
        "a",
        "*a*",
        "*a",
        "a*",
        "?",
        "*?*",
        "a?b",
        "*ab*",
        "*b*a*",
        "*-*",
        "pre*",
        "*suf",
        "x*y",
        "*\\**",
        "\\?",
        "*a*b*c*",
        "??",
        "ab",
        "*ab",
        "*ba*",
        "c",
        "*c*d*",
        "abc",
        "*=*",
        "* *",
        "?=?",
        "a*b",
        "*a?",
        "?a*",
        "lit",
        "li",
        "*it",
        "?it*",
        "**a**",
        "a**b",
};

auto test_differential() -> void {
    differential("lit", {}, cQueries, true);
    differential("%x%", {{"x", {"", "a", "ab", "ba", "abc", "*", "a?"}}}, cQueries, true);
    differential("a%x%b", {{"x", {"", "q", "ab", "Zq9", "a"}}}, cQueries, true);
    differential("pre%x%suf", {{"x", {"", "X", "m", "a"}}}, cQueries, true);
    differential(
            "a%x%c%y%d",
            {{"x", {"", "b", "xy", "a"}}, {"y", {"", "c", "mn", "b"}}},
            cQueries,
            true
    );
    differential(
            "%x%-%y%",
            {{"x", {"", "a", "ab", "b"}}, {"y", {"", "b", "cd", "a"}}},
            cQueries,
            true
    );
    differential("x%a%%b%y", {{"a", {"", "zz", "a"}}, {"b", {"", "q", "b"}}}, cQueries, true);
    // Duplicate placeholder names: sound, but the per-column downstream semantics over-approximate.
    differential(
            "%k%=%v% %k%=%v%",
            {{"k", {"a", "b", ""}}, {"v", {"1", "a", ""}}},
            cQueries,
            false
    );
}

auto test_hive_shape() -> void {
    std::string const shape{
            " %prefix.logLevel% %prefix.class%: PacketResponder: BP-%blockPoolID.poolID%-"
            "%blockPoolID.poolIP%-%blockPoolID.poolTimestamp%:blk_%blockID.blockNum%_"
            "%blockID.genStamp%, %key_value.key%=%key_value.str_value% terminating"
    };
    std::map<std::string, std::vector<std::string>> const values{
            {"prefix.logLevel", {"INFO", "WARN"}},
            {"prefix.class",
             {"org.apache.hadoop.hdfs.server.datanode.DataNode",
              "org.apache.hadoop.hdfs.server.datanode.DataNode.clienttrace"}},
            {"blockPoolID.poolID", {"1073741", "108"}},
            {"blockPoolID.poolIP", {"10.0.0.1", "10.0.0.2"}},
            {"blockPoolID.poolTimestamp", {"1400000000000", "1400000000001"}},
            {"blockID.blockNum", {"1073746491", "1073746492"}},
            {"blockID.genStamp", {"5667", "56678"}},
            {"key_value.key", {"type", "op"}},
            {"key_value.str_value", {"LAST_IN_PIPELINE", "HAS_DOWNSTREAM_IN_PIPELINE"}},
    };
    Fixture fixture{shape, values};

    // The block-ID query resolves to exactly the literal-anchored interpretation; the value index
    // rules out hosting the whole run inside a free-text placeholder.
    {
        std::vector<ShapeInterpretation> interpretations;
        auto const status{fixture.decompose("*blk_1073746491_5667*", interpretations)};
        CLPP_CHECK(DecomposeStatus::Complete == status, "hive: complete");
        auto const sets{leaf_sets(interpretations, fixture)};
        CLPP_CHECK(
                sets == std::set<std::string>{"blockID.blockNum=1073746491;blockID.genStamp=5667*;"},
                "hive: block-ID query yields the anchored interpretation only"
        );
    }
    // A run that no value hosts and no literal contains prunes the shape.
    {
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::NoMatch == fixture.decompose("*Exception*", interpretations),
                "hive: absent run prunes"
        );
    }
    // Anchoring: real messages start with a space, so an unanchored-looking query is rejected.
    {
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::NoMatch == fixture.decompose("INFO *", interpretations),
                "hive: prefix anchoring rejects `INFO *`"
        );
        CLPP_CHECK(
                DecomposeStatus::Complete == fixture.decompose(" INFO *", interpretations),
                "hive: prefix anchoring accepts ` INFO *`"
        );
        CLPP_CHECK(
                leaf_sets(interpretations, fixture)
                        == std::set<std::string>{"prefix.logLevel=INFO;"},
                "hive: ` INFO *` constrains the level only"
        );
    }
    // Literal-only queries produce the leafless interpretation and nothing else.
    {
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == fixture.decompose("*PacketResponder*", interpretations),
                "hive: literal query complete"
        );
        CLPP_CHECK(
                1 == interpretations.size() && interpretations.front().leaves.empty(),
                "hive: literal query is leafless"
        );
    }
    // A run straddling a placeholder end and a literal.
    {
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == fixture.decompose("*Node: Pack*", interpretations),
                "hive: straddling run complete"
        );
        CLPP_CHECK(
                leaf_sets(interpretations, fixture) == std::set<std::string>{"prefix.class=*Node;"},
                "hive: straddling run anchors the class suffix"
        );
    }
    // Multi-run queries can spread over several placeholders.
    {
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == fixture.decompose("*5667*LAST*", interpretations),
                "hive: multi-run complete"
        );
        CLPP_CHECK(
                leaf_sets(interpretations, fixture)
                        == std::set<std::string>{
                                "blockID.genStamp=*5667*;key_value.str_value=*LAST*;"
                        },
                "hive: multi-run spreads over placeholders"
        );
    }
    differential(
            shape,
            values,
            {"*blk_1073746491_5667*",
             "*5667*",
             "*_5667*",
             "*5667",
             "*Node: Pack*",
             " INFO *",
             "*=LAST*",
             "*type=*",
             "*10.0.0.?-*",
             "*a*b*",
             "*DataNode*terminating"},
            true
    );
}

auto test_unbounded_and_cap() -> void {
    // An unbounded rule certifies every piece and is flagged so callers can defer to the engine.
    {
        Fixture fixture{"a%x%b", {{"x", {"q"}}}};
        fixture.index.mark_unbounded("x");
        fixture.signatures.at(0) = fixture.index.find("x");
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query("azzb", tokens));
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, fixture.signatures};
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == decomposer.decompose(fixture.shape, 16, interpretations),
                "unbounded: decomposes"
        );
        CLPP_CHECK(decomposer.used_unbounded_rule(), "unbounded: flagged");
        // The trailing `*` lets the value also run past the literal `b`.
        std::set<std::string> const expected{"x=zz;", "x=zzb*;"};
        CLPP_CHECK(leaf_sets(interpretations, fixture) == expected, "unbounded: leaves emitted");
    }
    // A rule missing from the index behaves the same way.
    {
        Fixture fixture{"a%x%b", {}};
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query("a*zb", tokens));
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, fixture.signatures};
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == decomposer.decompose(fixture.shape, 16, interpretations),
                "missing: decomposes"
        );
        CLPP_CHECK(decomposer.used_unbounded_rule(), "missing: flagged");
    }
    // A leafless interpretation never flags an unbounded rule.
    {
        Fixture fixture{"a%x%b", {}};
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query("a*", tokens));
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, fixture.signatures};
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Complete == decomposer.decompose(fixture.shape, 16, interpretations),
                "leafless: decomposes"
        );
        CLPP_CHECK(false == decomposer.used_unbounded_rule(), "leafless: not flagged");
        CLPP_CHECK(1 == interpretations.size() && interpretations.front().leaves.empty(), "leafless: one leafless interpretation");
    }
    // The cap stops enumeration.
    {
        Fixture fixture{"%a%%b%%c%", {{"a", {"x", "y"}}, {"b", {"x", "y"}}, {"c", {"x", "y"}}}};
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query("*x*y*", tokens));
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, fixture.signatures};
        std::vector<ShapeInterpretation> interpretations;
        CLPP_CHECK(
                DecomposeStatus::Capped == decomposer.decompose(fixture.shape, 1, interpretations),
                "cap: hit"
        );
        CLPP_CHECK(1 == interpretations.size(), "cap: one emitted");
        CLPP_CHECK(
                DecomposeStatus::Complete == decomposer.decompose(fixture.shape, 1024, interpretations),
                "cap: complete with a large cap"
        );
        CLPP_CHECK(interpretations.size() > 1, "cap: several interpretations");
    }
}

auto test_piece_may_match() -> void {
    RuleValueIndex index;
    index.add_value("r", "hello");
    index.add_value("r", "hi");
    auto const* sig{index.find("r")};
    auto const piece = [](std::string_view text) -> std::vector<QueryToken> {
        std::vector<QueryToken> tokens;
        REQUIRE(parse_shape_query(text, tokens));
        return tokens;
    };
    CLPP_CHECK(piece_may_match(nullptr, piece("anything")), "piece: unbounded");
    CLPP_CHECK(piece_may_match(sig, piece("hello")), "piece: exact value");
    CLPP_CHECK(false == piece_may_match(sig, piece("hell")), "piece: exact prefix rejected");
    CLPP_CHECK(piece_may_match(sig, piece("hell*")), "piece: open prefix");
    CLPP_CHECK(piece_may_match(sig, piece("*llo")), "piece: open suffix");
    CLPP_CHECK(false == piece_may_match(sig, piece("*ll")), "piece: suffix must end a value");
    CLPP_CHECK(piece_may_match(sig, piece("*ll*")), "piece: contained");
    CLPP_CHECK(piece_may_match(sig, piece("h?")), "piece: ? with exact length");
    CLPP_CHECK(false == piece_may_match(sig, piece("h??")), "piece: ? with unseen length");
    CLPP_CHECK(piece_may_match(sig, piece("h?llo")), "piece: ? inside");
    CLPP_CHECK(false == piece_may_match(sig, piece("h?lla")), "piece: ? inside, absent end");
    CLPP_CHECK(false == piece_may_match(sig, piece("?")), "piece: length one unseen");
    CLPP_CHECK(piece_may_match(sig, piece("??")), "piece: length two seen");
    CLPP_CHECK(false == piece_may_match(sig, piece("")), "piece: empty unseen");
    CLPP_CHECK(piece_may_match(sig, piece("*")), "piece: star");
    CLPP_CHECK(false == piece_may_match(sig, piece("*hello*world*")), "piece: too long");
    CLPP_CHECK("a\\*b\\?c\\\\d" == render_query_piece(piece("a\\*b\\?c\\\\d")), "render: escapes literals");
    CLPP_CHECK("*a?" == render_query_piece(piece("*a?")), "render: keeps wildcards");
}
}  // namespace

TEST_CASE("clpp_shape_decomposer_piece", "[clpp][ClppShapeDecomposer]") {
    test_piece_may_match();
}

TEST_CASE("clpp_shape_decomposer_differential", "[clpp][ClppShapeDecomposer]") {
    test_differential();
}

TEST_CASE("clpp_shape_decomposer_hive", "[clpp][ClppShapeDecomposer]") {
    test_hive_shape();
}

TEST_CASE("clpp_shape_decomposer_unbounded_and_cap", "[clpp][ClppShapeDecomposer]") {
    test_unbounded_and_cap();
}
