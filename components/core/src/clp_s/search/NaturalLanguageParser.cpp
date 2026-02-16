#include "NaturalLanguageParser.hpp"

#include <memory>
#include <optional>
#include <regex>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

#include "../archive_constants.hpp"
#include "ast/AndExpr.hpp"
#include "ast/BooleanLiteral.hpp"
#include "ast/ColumnDescriptor.hpp"
#include "ast/Expression.hpp"
#include "ast/FilterExpr.hpp"
#include "ast/FilterOperation.hpp"
#include "ast/Integral.hpp"
#include "ast/OrExpr.hpp"
#include "ast/SemanticLiteral.hpp"
#include "ast/StringLiteral.hpp"
#include "FieldResolver.hpp"

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
    static std::regex const pattern{R"(\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s+(-?\d+(?:\.\d+)?)\b)"};
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

// Pattern for extracting numbers from text
auto const& get_number_pattern() {
    static std::regex const pattern{R"(-?\d+(?:\.\d+)?)"};
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

// -- Numeric type set for type-based routing --
bool is_numeric_type(NodeType type) {
    return type == NodeType::Integer || type == NodeType::Float || type == NodeType::DeltaInteger
           || type == NodeType::FormattedFloat || type == NodeType::DictionaryFloat;
}

bool is_text_type(NodeType type) {
    return type == NodeType::ClpString || type == NodeType::VarString;
}

/**
 * Creates a ColumnDescriptor for a dotted path (e.g., "auth.status_code").
 * Splits on "." into tokens and creates with the default namespace.
 */
auto create_column_for_path(std::string const& dotted_path)
        -> std::shared_ptr<ast::ColumnDescriptor> {
    std::vector<std::string> tokens;
    std::string current;
    for (char c : dotted_path) {
        if ('.' == c) {
            if (false == current.empty()) {
                tokens.push_back(current);
                current.clear();
            }
        } else {
            current += c;
        }
    }
    if (false == current.empty()) {
        tokens.push_back(current);
    }
    return ast::ColumnDescriptor::create_from_escaped_tokens(tokens, constants::cDefaultNamespace);
}

/**
 * Creates type-appropriate filter expressions for a field + phrase combination.
 * Ports the Python _synthesize_for_field() logic.
 *
 * @param field_path Dotted field path (e.g., "auth.status_code")
 * @param node_type The NodeType of the field
 * @param phrase The value phrase to match
 * @param semantic_top_k Top-K for semantic search (used for ClpString)
 * @return Vector of filter expressions (may be empty if no valid filter can be created)
 */
auto create_filter_for_field(
        std::string const& field_path,
        NodeType node_type,
        std::string const& phrase,
        std::optional<size_t> semantic_top_k
) -> std::vector<std::shared_ptr<ast::Expression>> {
    std::vector<std::shared_ptr<ast::Expression>> results;

    if (NodeType::ClpString == node_type) {
        // ClpString -> semantic("phrase", top_k)
        auto col = create_column_for_path(field_path);
        auto semantic_lit = ast::SemanticLiteral::create(phrase, semantic_top_k);
        results.push_back(
                ast::FilterExpr::create(col, ast::FilterOperation::SEMANTIC, semantic_lit)
        );
    } else if (NodeType::VarString == node_type) {
        // VarString -> wildcard match "*phrase*"
        auto col = create_column_for_path(field_path);
        auto str_lit = ast::StringLiteral::create("*" + phrase + "*");
        results.push_back(ast::FilterExpr::create(col, ast::FilterOperation::EQ, str_lit));
    } else if (is_numeric_type(node_type)) {
        // Numeric types -> try exact numeric match, else extract numbers from phrase
        auto integral_lit = ast::Integral::create_from_string(phrase);
        if (nullptr != integral_lit) {
            auto col = create_column_for_path(field_path);
            results.push_back(ast::FilterExpr::create(col, ast::FilterOperation::EQ, integral_lit));
        } else {
            // Extract all numbers from the phrase via regex
            auto const& num_pattern = get_number_pattern();
            auto begin = std::sregex_iterator(phrase.begin(), phrase.end(), num_pattern);
            auto end = std::sregex_iterator();
            for (auto it = begin; it != end; ++it) {
                auto num_lit = ast::Integral::create_from_string(it->str());
                if (nullptr != num_lit) {
                    auto col = create_column_for_path(field_path);
                    results.push_back(
                            ast::FilterExpr::create(col, ast::FilterOperation::EQ, num_lit)
                    );
                }
            }
        }
    } else if (NodeType::Boolean == node_type) {
        // Boolean -> only if phrase is "true" or "false"
        std::string lower_phrase;
        for (char c : phrase) {
            lower_phrase += static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
        }
        auto trimmed = trim(lower_phrase);
        if ("true" == trimmed || "false" == trimmed) {
            auto col = create_column_for_path(field_path);
            auto bool_lit = ast::BooleanLiteral::create_from_string(trimmed);
            if (nullptr != bool_lit) {
                results.push_back(ast::FilterExpr::create(col, ast::FilterOperation::EQ, bool_lit));
            }
        }
    }
    // DateString, Timestamp -> handled via --tge/--tle, not here

    return results;
}

/**
 * Creates a FilterExpr for a field:value pair (schema-blind fallback).
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
auto combine_with_and(std::vector<std::shared_ptr<ast::Expression>> const& exprs)
        -> std::shared_ptr<ast::Expression> {
    if (exprs.empty()) {
        return nullptr;
    }
    if (1 == exprs.size()) {
        return exprs[0];
    }

    auto root = ast::AndExpr::create();
    for (auto const& expr : exprs) {
        root->add_operand(expr);
    }
    return root;
}

/**
 * Combines multiple expressions with OR. Returns the single expression if only one exists.
 */
auto combine_with_or(std::vector<std::shared_ptr<ast::Expression>> const& exprs)
        -> std::shared_ptr<ast::Expression> {
    if (exprs.empty()) {
        return nullptr;
    }
    if (1 == exprs.size()) {
        return exprs[0];
    }

    auto root = ast::OrExpr::create();
    for (auto const& expr : exprs) {
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

/**
 * Schema-aware natural language parsing.
 * Resolves fields against the schema tree and generates type-appropriate filters.
 */
auto parse_natural_language_with_schema(
        std::string const& query,
        std::optional<size_t> semantic_top_k,
        SchemaTree const& schema_tree,
        OnnxEmbedder const* embedder
) -> std::shared_ptr<ast::Expression> {
    std::string remaining = query;

    warn_if_temporal_phrases(query);

    // Step 1: Extract explicit field:value pairs and field-number pairs
    auto field_value_pairs = extract_field_value_pairs(remaining);
    auto field_number_pairs = extract_field_number_pairs(remaining);
    remove_filler_prefix(remaining);

    // Step 2: Extract field catalog from schema
    auto const all_fields = extract_fields_from_schema(schema_tree);
    if (all_fields.empty()) {
        SPDLOG_WARN("Schema tree has no searchable leaf fields, falling back to schema-blind mode");
        return nullptr;  // Caller will fall back to schema-blind
    }

    // Build lookup maps: full_path -> FieldInfo, leaf_name -> [FieldInfo]
    std::unordered_map<std::string, FieldInfo const*> full_path_lookup;
    std::unordered_map<std::string, std::vector<FieldInfo const*>> leaf_lookup;
    for (auto const& field : all_fields) {
        full_path_lookup[field.full_path] = &field;
        auto const dot_pos = field.full_path.rfind('.');
        std::string const leaf_name = (std::string::npos != dot_pos)
                                              ? field.full_path.substr(dot_pos + 1)
                                              : field.full_path;
        leaf_lookup[leaf_name].push_back(&field);
    }

    // Step 3: Resolve explicit field:value pairs against schema
    std::vector<std::shared_ptr<ast::Expression>> fv_clauses;

    auto resolve_and_filter_pair = [&](std::string const& field_hint, std::string const& value) {
        std::vector<FieldInfo const*> matched;

        // Try exact full_path match first
        auto it = full_path_lookup.find(field_hint);
        if (it != full_path_lookup.end()) {
            matched.push_back(it->second);
        } else {
            // Try leaf name match
            auto leaf_it = leaf_lookup.find(field_hint);
            if (leaf_it != leaf_lookup.end()) {
                matched = leaf_it->second;
            }
        }

        for (auto const* field_info : matched) {
            auto filters = create_filter_for_field(
                    field_info->full_path,
                    field_info->node_type,
                    value,
                    semantic_top_k
            );
            for (auto& f : filters) {
                fv_clauses.push_back(std::move(f));
            }
        }
    };

    for (auto const& [field_name, value] : field_value_pairs) {
        resolve_and_filter_pair(field_name, value);
    }
    for (auto const& [field_name, value] : field_number_pairs) {
        resolve_and_filter_pair(field_name, value);
    }

    // Step 4: Resolve remaining text against schema fields
    auto const cleaned = trim(remaining);
    std::vector<std::shared_ptr<ast::Expression>> text_clauses;
    std::vector<std::shared_ptr<ast::Expression>> num_clauses;

    if (false == cleaned.empty()) {
        // Resolve fields using embeddings or keyword fallback
        std::vector<ResolvedField> resolved;
        if (nullptr != embedder) {
            resolved = resolve_fields(cleaned, all_fields, *embedder);
        } else {
            resolved = resolve_fields_by_keywords(cleaned, all_fields);
        }

        if (resolved.empty()) {
            SPDLOG_INFO("No fields resolved for remaining text, using wildcard semantic fallback");
            text_clauses.push_back(create_semantic_filter(cleaned, semantic_top_k));
        } else {
            for (auto const& rf : resolved) {
                auto filters = create_filter_for_field(
                        rf.field.full_path,
                        rf.field.node_type,
                        cleaned,
                        semantic_top_k
                );
                for (auto& f : filters) {
                    if (is_text_type(rf.field.node_type)) {
                        text_clauses.push_back(std::move(f));
                    } else if (is_numeric_type(rf.field.node_type)) {
                        num_clauses.push_back(std::move(f));
                    } else {
                        text_clauses.push_back(std::move(f));
                    }
                }
            }
        }
    }

    // Step 5: Clause grouping — OR within each group, AND between groups
    std::vector<std::shared_ptr<ast::Expression>> groups;

    auto fv_group = combine_with_or(fv_clauses);
    if (nullptr != fv_group) {
        groups.push_back(std::move(fv_group));
    }

    auto text_group = combine_with_or(text_clauses);
    if (nullptr != text_group) {
        groups.push_back(std::move(text_group));
    }

    auto num_group = combine_with_or(num_clauses);
    if (nullptr != num_group) {
        groups.push_back(std::move(num_group));
    }

    return combine_with_and(groups);
}
}  // namespace

auto parse_natural_language(
        std::string const& query,
        std::optional<size_t> semantic_top_k,
        SchemaTree const* schema_tree,
        OnnxEmbedder const* embedder
) -> std::shared_ptr<ast::Expression> {
    // Schema-aware path
    if (nullptr != schema_tree) {
        auto result
                = parse_natural_language_with_schema(query, semantic_top_k, *schema_tree, embedder);
        if (nullptr != result) {
            return result;
        }
        // Fall through to schema-blind path if schema-aware returned null
        SPDLOG_INFO("Schema-aware parsing returned no results, falling back to schema-blind mode");
    }

    // Schema-blind path (original behavior)
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
