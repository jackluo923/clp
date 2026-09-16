#include <string>
#include <string_view>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/search/ClppShapeQueryMatcher.hpp>

using clp_s::search::build_shape_skeleton;
using clp_s::search::parse_shape_query;
using clp_s::search::QueryToken;
using clp_s::search::ShapeToken;
using clp_s::search::shape_may_match;

namespace {
/**
 * Builds the skeleton for `shape` and the tokens for `query`, then returns whether the shape could
 * possibly match the query. Fails the test if either cannot be tokenized.
 */
auto may_match(std::string_view shape, std::string_view query) -> bool {
    std::vector<ShapeToken> skeleton;
    REQUIRE(build_shape_skeleton(shape, skeleton));
    std::vector<QueryToken> tokens;
    REQUIRE(parse_shape_query(query, tokens));
    return shape_may_match(skeleton, tokens);
}
}  // namespace

TEST_CASE("clpp_shape_skeleton_tokenization", "[clpp][ClppShapeQueryMatcher]") {
    std::vector<ShapeToken> skeleton;

    // Plain literal text becomes one literal token per character.
    REQUIRE(build_shape_skeleton("abc", skeleton));
    REQUIRE(3 == skeleton.size());
    for (size_t i{0}; i < skeleton.size(); ++i) {
        REQUIRE(ShapeToken::Type::Literal == skeleton.at(i).type);
    }
    REQUIRE('a' == skeleton.at(0).literal);
    REQUIRE('c' == skeleton.at(2).literal);

    // A `%name%` reference becomes a single placeholder token.
    REQUIRE(build_shape_skeleton("a%x.b%b", skeleton));
    REQUIRE(3 == skeleton.size());
    REQUIRE(ShapeToken::Type::Literal == skeleton.at(0).type);
    REQUIRE(ShapeToken::Type::Placeholder == skeleton.at(1).type);
    REQUIRE(ShapeToken::Type::Literal == skeleton.at(2).type);

    // `%%` is an escaped literal percent.
    REQUIRE(build_shape_skeleton("a%%b", skeleton));
    REQUIRE(3 == skeleton.size());
    REQUIRE(ShapeToken::Type::Literal == skeleton.at(1).type);
    REQUIRE('%' == skeleton.at(1).literal);

    // Adjacent references are two placeholders, not an escaped percent (mirrors automata_for_shape).
    REQUIRE(build_shape_skeleton("%a%%b%", skeleton));
    REQUIRE(2 == skeleton.size());
    REQUIRE(ShapeToken::Type::Placeholder == skeleton.at(0).type);
    REQUIRE(ShapeToken::Type::Placeholder == skeleton.at(1).type);

    // A dangling `%` is malformed.
    REQUIRE_FALSE(build_shape_skeleton("%abc", skeleton));
}

TEST_CASE("clpp_shape_query_parsing", "[clpp][ClppShapeQueryMatcher]") {
    std::vector<QueryToken> tokens;

    REQUIRE(parse_shape_query("a?c*", tokens));
    REQUIRE(4 == tokens.size());
    REQUIRE(QueryToken::Type::Literal == tokens.at(0).type);
    REQUIRE(QueryToken::Type::AnyOne == tokens.at(1).type);
    REQUIRE(QueryToken::Type::Literal == tokens.at(2).type);
    REQUIRE(QueryToken::Type::AnyMany == tokens.at(3).type);

    // Escaped wildcards are literals.
    REQUIRE(parse_shape_query("a\\*b", tokens));
    REQUIRE(3 == tokens.size());
    REQUIRE(QueryToken::Type::Literal == tokens.at(1).type);
    REQUIRE('*' == tokens.at(1).literal);

    // A dangling escape is a parse error.
    REQUIRE_FALSE(parse_shape_query("abc\\", tokens));
}

TEST_CASE("clpp_shape_query_intersection", "[clpp][ClppShapeQueryMatcher]") {
    // Prefix-anchored literal shapes (matches log-surgeon's search_by_log_shapes, verified
    // empirically).
    REQUIRE(may_match("abc", "abc"));
    REQUIRE(may_match("abc", "ab"));
    REQUIRE_FALSE(may_match("abc", "bc"));
    REQUIRE_FALSE(may_match("abc", "b"));
    REQUIRE_FALSE(may_match("abc", "abd"));
    REQUIRE(may_match("abc", "a?c"));
    REQUIRE_FALSE(may_match("abc", "a?d"));
    REQUIRE(may_match("abc", "*"));

    // Anchored prefix queries are rejected when the literal prefix cannot appear.
    REQUIRE(may_match("ERROR %msg%", "ERROR *"));
    REQUIRE_FALSE(may_match("ERROR %msg%", "WARN *"));
    REQUIRE_FALSE(may_match("ERROR %msg%", "w"));
    REQUIRE(may_match("ERROR %msg%", "ERROR w"));
    REQUIRE(may_match("ERROR %msg%", "*ERROR*"));
    REQUIRE(may_match("ERROR %msg%", "*w*"));

    // A placeholder can absorb arbitrary text, including none.
    REQUIRE(may_match("%msg%", "anything"));
    REQUIRE(may_match("a%x%b", "ab"));
    REQUIRE(may_match("a%x%b", "aXXXb"));

    // Escaped percent.
    REQUIRE(may_match("a%%b", "a%b"));
    REQUIRE_FALSE(may_match("a%%b", "aXb"));

    // Empty and match-all queries.
    REQUIRE(may_match("abc", ""));
    REQUIRE(may_match("abc", "*"));

    // Matching is case-sensitive.
    REQUIRE_FALSE(may_match("ABC", "abc"));
    REQUIRE(may_match("ABC", "ABC"));
}
