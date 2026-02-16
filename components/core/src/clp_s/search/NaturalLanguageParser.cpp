#include "NaturalLanguageParser.hpp"

#include <memory>
#include <optional>
#include <regex>
#include <string>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

#include "../archive_constants.hpp"
#include "ast/AndExpr.hpp"
#include "ast/ColumnDescriptor.hpp"
#include "ast/Expression.hpp"
#include "ast/FilterExpr.hpp"
#include "ast/FilterOperation.hpp"
#include "ast/Integral.hpp"
#include "ast/SemanticLiteral.hpp"
#include "ast/StringLiteral.hpp"

namespace clp_s::search::nl {
namespace {
// Lazy-initialized regex accessors to avoid static initialization order issues and defer
// construction cost until the NL code path is actually taken.

// Explicit field-value pairs: field:value or field:"quoted value"
// Only ':' is used as separator (not '=') to avoid ambiguity in natural language.
// Unquoted values are restricted to word chars, '.', and '*' to avoid consuming punctuation.
auto const& get_field_value_pattern() {
    static std::regex const pattern{
            R"re(\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s*:\s*(?:"([^"]+)"|([\w.*]+)))re"
    };
    return pattern;
}

// Implicit field-number pairs: field_name 403 (field must contain '_' or '.')
auto const& get_field_number_pattern() {
    static std::regex const pattern{
            R"(\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s+(-?\d+(?:\.\d+)?)\b)"
    };
    return pattern;
}

// Filler words at the start of a query
auto const& get_filler_pattern() {
    static std::regex const pattern{
            R"(^(?:search\s+for|show\s+me|find\s+me|give\s+me|find|show|get|search)\s+)",
            std::regex_constants::icase
    };
    return pattern;
}

// Temporal-looking phrases (used only for the warning)
auto const& get_temporal_hint_pattern() {
    static std::regex const pattern{
            R"(\b(?:last\s+\d+\s+(?:hours?|minutes?|mins?|days?|hrs?)|last\s+(?:hour|minute|min|day|hr)|yesterday|since\s+\d{4}-\d{2}-\d{2})\b)",
            std::regex_constants::icase
    };
    return pattern;
}

/**
 * Strips leading and trailing whitespace and punctuation from a string.
 */
auto trim(std::string const& s) -> std::string {
    auto const start = s.find_first_not_of(" \t,;");
    if (std::string::npos == start) {
        return "";
    }
    auto const end = s.find_last_not_of(" \t,;");
    return s.substr(start, end - start + 1);
}

/**
 * Creates a FilterExpr for a field:value pair.
 * Attempts to parse the value as an integer first, falling back to string.
 */
auto create_field_value_filter(std::string const& field_name, std::string const& value)
        -> std::shared_ptr<ast::Expression> {
    auto col = ast::ColumnDescriptor::create_from_escaped_tokens(
            {field_name},
            constants::cDefaultNamespace
    );

    auto integral_lit = ast::Integral::create_from_string(value);
    if (nullptr != integral_lit) {
        return ast::FilterExpr::create(col, ast::FilterOperation::EQ, integral_lit);
    }

    auto str_lit = ast::StringLiteral::create(value);
    return ast::FilterExpr::create(col, ast::FilterOperation::EQ, str_lit);
}

/**
 * Creates a semantic() FilterExpr with wildcard column for the given text.
 */
auto create_semantic_filter(std::string const& text, std::optional<size_t> top_k)
        -> std::shared_ptr<ast::Expression> {
    auto semantic_lit = ast::SemanticLiteral::create(text, top_k);
    auto wildcard_col = ast::ColumnDescriptor::create_from_escaped_tokens(
            {"*"},
            constants::cDefaultNamespace
    );
    return ast::FilterExpr::create(wildcard_col, ast::FilterOperation::SEMANTIC, semantic_lit);
}

/**
 * Combines multiple expressions with AND. Returns the single expression if only one exists.
 */
auto combine_with_and(std::vector<std::shared_ptr<ast::Expression>>& exprs)
        -> std::shared_ptr<ast::Expression> {
    if (exprs.empty()) {
        return nullptr;
    }
    if (1 == exprs.size()) {
        return exprs[0];
    }

    auto root = ast::AndExpr::create();
    for (auto& expr : exprs) {
        root->add_operand(expr);
    }
    return root;
}

/**
 * Extracts explicit field:value pairs from the query string.
 * Removes matched text from remaining.
 */
auto extract_field_value_pairs(std::string& remaining)
        -> std::vector<std::pair<std::string, std::string>> {
    std::vector<std::pair<std::string, std::string>> pairs;
    std::smatch match;
    auto const& pattern = get_field_value_pattern();

    std::string working = remaining;
    std::string result_str;
    while (std::regex_search(working, match, pattern)) {
        std::string const field_name = match[1].str();
        std::string const value = match[2].matched ? match[2].str() : match[3].str();
        pairs.emplace_back(field_name, value);
        result_str += match.prefix().str() + " ";
        working = match.suffix().str();
    }
    result_str += working;

    if (false == pairs.empty()) {
        remaining = result_str;
    }
    return pairs;
}

/**
 * Extracts implicit field-number pairs (e.g., "status_code 403") from the query string.
 * Only matches if the field name contains '_' or '.'.
 * Removes matched text from remaining.
 *
 * Note: This heuristic can false-positive on phrases like "job_queue had 3 entries" where
 * the word before the number happens to contain '_'. This is acceptable for a prototype.
 */
auto extract_field_number_pairs(std::string& remaining)
        -> std::vector<std::pair<std::string, std::string>> {
    std::vector<std::pair<std::string, std::string>> pairs;
    std::smatch match;
    auto const& pattern = get_field_number_pattern();

    std::string working = remaining;
    std::string result_str;
    while (std::regex_search(working, match, pattern)) {
        std::string const field_name = match[1].str();
        std::string const value = match[2].str();
        bool const has_separator = field_name.find('_') != std::string::npos
                                   || field_name.find('.') != std::string::npos;
        if (has_separator) {
            pairs.emplace_back(field_name, value);
            result_str += match.prefix().str() + " ";
        } else {
            result_str += match.prefix().str() + match[0].str();
        }
        working = match.suffix().str();
    }
    result_str += working;

    if (false == pairs.empty()) {
        remaining = result_str;
    }
    return pairs;
}

/**
 * Removes filler words from the start of the query (e.g., "show me", "find", "search for").
 */
void remove_filler_prefix(std::string& remaining) {
    remaining = std::regex_replace(remaining, get_filler_pattern(), "");
}

/**
 * Logs a warning if the query appears to contain temporal phrases.
 */
void warn_if_temporal_phrases(std::string const& query) {
    if (std::regex_search(query, get_temporal_hint_pattern())) {
        SPDLOG_WARN(
                "Query appears to contain time-range language (e.g., \"last 2 hours\"). "
                "Time filtering is not extracted from natural language queries. "
                "Use --tge/--tle flags to specify a time range."
        );
    }
}
}  // namespace

auto parse_natural_language(
        std::string const& query,
        std::optional<size_t> semantic_top_k
) -> std::shared_ptr<ast::Expression> {
    std::string remaining = query;

    warn_if_temporal_phrases(query);

    auto field_value_pairs = extract_field_value_pairs(remaining);
    auto field_number_pairs = extract_field_number_pairs(remaining);
    remove_filler_prefix(remaining);

    std::vector<std::shared_ptr<ast::Expression>> sub_exprs;

    for (auto const& [field_name, value] : field_value_pairs) {
        sub_exprs.push_back(create_field_value_filter(field_name, value));
    }
    for (auto const& [field_name, value] : field_number_pairs) {
        sub_exprs.push_back(create_field_value_filter(field_name, value));
    }

    auto const cleaned = trim(remaining);
    if (false == cleaned.empty()) {
        sub_exprs.push_back(create_semantic_filter(cleaned, semantic_top_k));
    }

    return combine_with_and(sub_exprs);
}
}  // namespace clp_s::search::nl
