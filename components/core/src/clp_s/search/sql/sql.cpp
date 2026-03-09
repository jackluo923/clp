#include "sql.hpp"

#include <any>
#include <cctype>
#include <cstdint>
#include <iterator>
#include <memory>
#include <optional>
#include <set>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <vector>

#include <antlr4-runtime.h>
#include <spdlog/spdlog.h>

#include <clp_s/timestamp_parser/TimestampParser.hpp>

#include "../antlr_common/ErrorListener.hpp"
#include "../ast/AndExpr.hpp"
#include "../ast/BooleanLiteral.hpp"
#include "../ast/ColumnDescriptor.hpp"
#include "../ast/Expression.hpp"
#include "../ast/FilterExpr.hpp"
#include "../ast/FilterOperation.hpp"
#include "../ast/Integral.hpp"
#include "../ast/NullLiteral.hpp"
#include "../ast/OrExpr.hpp"
#include "../ast/StringLiteral.hpp"
#include "../ast/TimestampLiteral.hpp"
#include "generated/SqlBaseVisitor.h"
#include "generated/SqlLexer.h"
#include "generated/SqlParser.h"

using namespace antlr4;
using clp_s::search::antlr_common::ErrorListener;

using clp_s::search::ast::AndExpr;
using clp_s::search::ast::BooleanLiteral;
using clp_s::search::ast::ColumnDescriptor;
using clp_s::search::ast::Expression;
using clp_s::search::ast::FilterExpr;
using clp_s::search::ast::FilterOperation;
using clp_s::search::ast::Integral;
using clp_s::search::ast::Literal;
using clp_s::search::ast::NullLiteral;
using clp_s::search::ast::OrExpr;
using clp_s::search::ast::StringLiteral;
using clp_s::search::ast::TimestampLiteral;

namespace clp_s::search::sql {
using generated::SqlBaseVisitor;
using generated::SqlLexer;
using generated::SqlParser;

namespace {
/**
 * Normalizes SQL keywords to uppercase while preserving the original case of identifiers
 * (column/table names) and string literals.
 *
 * Tokenizes by word boundaries. For each unquoted word, checks if its uppercase form is a
 * known SQL or CLP keyword and replaces it; otherwise keeps the original case.
 * @param input the raw SQL string
 * @return the normalized string with keywords uppercased
 */
std::string normalize_sql_keywords(std::string const& input) {
    // NOTE: This set must be kept in sync with the lexer keyword rules in Sql.g4.
    // When adding a new keyword to the grammar, add it here as well.
    static std::set<std::string> const cSqlKeywords = {
            "ALL",
            "AND",
            "ANY_VALUE",
            "ARBITRARY",
            "AS",
            "AVG",
            "BETWEEN",
            "CLP_GET_BOOL",
            "CLP_GET_FLOAT",
            "CLP_GET_INT",
            "CLP_GET_JSON_STRING",
            "CLP_GET_STRING",
            "CLP_WILDCARD_COLUMN",
            "COUNT",
            "DATE",
            "DISTINCT",
            "FALSE",
            "FROM",
            "IN",
            "IS",
            "LIKE",
            "LIMIT",
            "MAX",
            "MIN",
            "NOT",
            "NULL",
            "OR",
            "SELECT",
            "SUM",
            "TIMESTAMP",
            "TRUE",
            "WHERE",
    };

    std::string result;
    result.reserve(input.size());

    size_t i = 0;
    while (i < input.size()) {
        char const ch = input[i];

        // Copy quoted strings as-is, handling doubled-quote escaping for all quote types.
        if ('\'' == ch || '"' == ch || '`' == ch) {
            char const quote = ch;
            result.push_back(ch);
            ++i;
            while (i < input.size()) {
                result.push_back(input[i]);
                if (input[i] == quote) {
                    // Handle doubled-quote escaping ('' for strings, "" for identifiers,
                    // `` for backticked identifiers).
                    if (i + 1 < input.size() && quote == input[i + 1]) {
                        ++i;
                        result.push_back(input[i]);
                    } else {
                        ++i;
                        break;
                    }
                }
                ++i;
            }
            continue;
        }

        // Skip SQL comments, passing them through unchanged.
        // Line comments: -- ... \n
        if ('-' == ch && i + 1 < input.size() && '-' == input[i + 1]) {
            while (i < input.size() && '\n' != input[i]) {
                result.push_back(input[i]);
                ++i;
            }
            continue;
        }
        // Block comments: /* ... */
        if ('/' == ch && i + 1 < input.size() && '*' == input[i + 1]) {
            result.push_back(input[i]);
            ++i;
            result.push_back(input[i]);
            ++i;
            while (i < input.size()) {
                if ('*' == input[i] && i + 1 < input.size() && '/' == input[i + 1]) {
                    result.push_back(input[i]);
                    ++i;
                    result.push_back(input[i]);
                    ++i;
                    break;
                }
                result.push_back(input[i]);
                ++i;
            }
            continue;
        }

        // Collect unquoted word (letters, digits, underscores, @, :)
        if (std::isalpha(static_cast<unsigned char>(ch)) || '_' == ch) {
            size_t const start = i;
            // Collect the full word including '@' and ':' which are valid IDENTIFIER
            // continuation characters in the grammar.
            while (i < input.size()
                   && (std::isalnum(static_cast<unsigned char>(input[i])) || '_' == input[i]
                       || '@' == input[i] || ':' == input[i]))
            {
                ++i;
            }
            auto const word = input.substr(start, i - start);
            std::string upper_word;
            upper_word.reserve(word.size());
            for (char c : word) {
                upper_word.push_back(
                        static_cast<char>(std::toupper(static_cast<unsigned char>(c)))
                );
            }
            if (cSqlKeywords.contains(upper_word)) {
                result += upper_word;
            } else {
                result += word;
            }
            continue;
        }

        result.push_back(ch);
        ++i;
    }
    return result;
}

/**
 * Extracts the text from an identifier context, removing surrounding quotes and unescaping
 * doubled-quote characters inside quoted identifiers.
 * @param ctx the identifier context
 * @return the unquoted identifier text
 */
std::string get_identifier_text(SqlParser::IdentifierContext* ctx) {
    if (nullptr == ctx) {
        return {};
    }
    auto text = ctx->getText();
    if (text.size() >= 2) {
        char const front = text.front();
        char const back = text.back();
        if (('"' == front && '"' == back) || ('`' == front && '`' == back)) {
            // Strip surrounding quotes and unescape doubled quotes
            std::string unescaped;
            unescaped.reserve(text.size());
            for (size_t i = 1; i + 1 < text.size(); ++i) {
                if (text[i] == front && i + 1 < text.size() - 1 && text[i + 1] == front) {
                    unescaped.push_back(front);
                    ++i;
                } else {
                    unescaped.push_back(text[i]);
                }
            }
            return unescaped;
        }
    }
    return text;
}

/**
 * Extracts a SQL string literal value, removing surrounding single quotes and unescaping
 * doubled single-quote characters.
 * @param text the raw token text (e.g. "'it''s'")
 * @return the unescaped string value (e.g. "it's")
 */
std::string unquote_sql_string(std::string const& text) {
    if (text.size() < 2 || '\'' != text.front() || '\'' != text.back()) {
        return text;
    }
    std::string result;
    result.reserve(text.size());
    for (size_t i = 1; i + 1 < text.size(); ++i) {
        if ('\'' == text[i] && i + 1 < text.size() - 1 && '\'' == text[i + 1]) {
            result.push_back('\'');
            ++i;
        } else {
            result.push_back(text[i]);
        }
    }
    return result;
}

/**
 * Converts a SQL LIKE pattern to a clp-s wildcard pattern.
 *
 * SQL LIKE wildcards are converted to clp-s equivalents:
 *   SQL '%'  -> clp-s '*' (match any sequence)
 *   SQL '_'  -> clp-s '?' (match single char)
 *
 * SQL LIKE escape sequences produce literal characters:
 *   SQL '\%' -> literal '%'
 *   SQL '\_' -> literal '_'
 *   SQL '\\' -> clp-s '\\' (literal backslash, escaped for clp-s)
 *
 * Characters that are clp-s wildcards but not SQL wildcards are escaped:
 *   literal '*' -> clp-s '\*'
 *   literal '?' -> clp-s '\?'
 *
 * @param like_pattern the SQL LIKE pattern (already unquoted)
 * @return the equivalent clp-s wildcard pattern
 */
std::string convert_like_pattern(std::string const& like_pattern) {
    std::string result;
    result.reserve(like_pattern.size());
    for (size_t i = 0; i < like_pattern.size(); ++i) {
        char const c = like_pattern[i];
        if ('\\' == c && i + 1 < like_pattern.size()) {
            char const next = like_pattern[i + 1];
            if ('%' == next || '_' == next) {
                // SQL escape: \% -> literal %, \_ -> literal _
                result.push_back(next);
                ++i;
                continue;
            }
            if ('\\' == next) {
                // SQL escape: \\ -> literal backslash.
                // Must output '\\' so clp-s also treats it as a literal backslash.
                result.push_back('\\');
                result.push_back('\\');
                ++i;
                continue;
            }
        }
        if ('%' == c) {
            result.push_back('*');
        } else if ('_' == c) {
            result.push_back('?');
        } else if ('*' == c) {
            // Escape literal '*' so clp-s does not treat it as a wildcard
            result.push_back('\\');
            result.push_back('*');
        } else if ('?' == c) {
            // Escape literal '?' so clp-s does not treat it as a single-char wildcard
            result.push_back('\\');
            result.push_back('?');
        } else {
            result.push_back(c);
        }
    }
    return result;
}

/**
 * Parses a timestamp string using default (non-quoted) timestamp patterns and returns a
 * TimestampLiteral. If `append_midnight` is true, appends " 00:00:00" before parsing (for DATE
 * literals). Otherwise, tries default patterns first, then retries with " 00:00:00" appended as a
 * date-only fallback.
 * @param timestamp_str the unquoted timestamp string
 * @param append_midnight if true, always append " 00:00:00" before parsing
 * @return a TimestampLiteral on success
 * @throws std::runtime_error on parse failure
 */
std::shared_ptr<Literal>
create_timestamp_literal_from_string(std::string const& timestamp_str, bool append_midnight) {
    auto const patterns_result = timestamp_parser::get_all_default_timestamp_patterns();
    if (patterns_result.has_error()) {
        throw std::runtime_error{"Failed to load default timestamp patterns"};
    }
    auto const& patterns = patterns_result.value();
    std::string generated_pattern;

    if (append_midnight) {
        auto const with_time = timestamp_str + " 00:00:00";
        auto const result = timestamp_parser::search_known_timestamp_patterns(
                with_time,
                patterns,
                false,
                generated_pattern
        );
        if (result.has_value()) {
            return TimestampLiteral::create(result->first);
        }
        throw std::runtime_error{
                "Failed to parse date literal '" + timestamp_str + "' as a timestamp"
        };
    }

    auto const result = timestamp_parser::search_known_timestamp_patterns(
            timestamp_str,
            patterns,
            false,
            generated_pattern
    );
    if (result.has_value()) {
        return TimestampLiteral::create(result->first);
    }

    // Retry with " 00:00:00" appended for date-only strings
    auto const with_time = timestamp_str + " 00:00:00";
    auto const retry_result = timestamp_parser::search_known_timestamp_patterns(
            with_time,
            patterns,
            false,
            generated_pattern
    );
    if (retry_result.has_value()) {
        return TimestampLiteral::create(retry_result->first);
    }

    throw std::runtime_error{
            "Failed to parse timestamp literal '" + timestamp_str + "' as a timestamp"
    };
}

/**
 * Builds a ColumnDescriptor from a single identifier.
 * @param ctx the identifier context
 * @return a ColumnDescriptor for the identifier
 */
std::shared_ptr<ColumnDescriptor> build_column_descriptor(SqlParser::IdentifierContext* ctx) {
    auto const name = get_identifier_text(ctx);
    return ColumnDescriptor::create_from_escaped_tokens({name}, "");
}

/**
 * Maps a SQL comparison operator to a CLP FilterOperation.
 * @param ctx the comparison operator context
 * @return the corresponding FilterOperation
 */
FilterOperation map_comparison_op(SqlParser::ComparisonOperatorContext* ctx) {
    if (ctx->EQ()) {
        return FilterOperation::EQ;
    }
    if (ctx->NEQ()) {
        return FilterOperation::NEQ;
    }
    if (ctx->LT()) {
        return FilterOperation::LT;
    }
    if (ctx->LTE()) {
        return FilterOperation::LTE;
    }
    if (ctx->GT()) {
        return FilterOperation::GT;
    }
    if (ctx->GTE()) {
        return FilterOperation::GTE;
    }
    throw std::runtime_error{"Unknown comparison operator"};
}

/**
 * Collects dotted identifier tokens from a dereference chain (e.g., a.b.c).
 * @param ctx the dereference context
 * @param[out] tokens output vector to append tokens to
 */
void collect_dereference_tokens(
        SqlParser::DereferenceContext* ctx,
        std::vector<std::string>& tokens
) {
    auto* base = ctx->base;
    if (auto* nested_deref = dynamic_cast<SqlParser::DereferenceContext*>(base)) {
        collect_dereference_tokens(nested_deref, tokens);
    } else if (auto* col_ref = dynamic_cast<SqlParser::ColumnReferenceContext*>(base)) {
        tokens.push_back(get_identifier_text(col_ref->identifier()));
    } else {
        throw std::runtime_error{
                "Unsupported expression in dereference chain: only column references are supported"
        };
    }
    tokens.push_back(get_identifier_text(ctx->fieldName));
}

/**
 * Joins a list of tokens with '.' as separator.
 * @param tokens the tokens to join
 * @return the joined string
 */
std::string join_tokens_with_dot(std::vector<std::string> const& tokens) {
    std::string result;
    for (auto const& t : tokens) {
        if (false == result.empty()) {
            result.push_back('.');
        }
        result += t;
    }
    return result;
}

/**
 * Extracts a field name (possibly dotted) from an expression used as a CLP_GET_* argument.
 * Supports string literal ('field.name'), identifier (field), and dereference (field.name).
 * @param expr_ctx the expression context containing the field name
 * @return the extracted field name
 */
std::string extract_field_name_from_expression(SqlParser::ExpressionContext* expr_ctx) {
    auto* bool_expr = expr_ctx->booleanExpression();
    auto* predicated = dynamic_cast<SqlParser::PredicatedContext*>(bool_expr);
    if (nullptr == predicated || nullptr == predicated->valueExpression()) {
        throw std::runtime_error{"Expected field name in CLP function argument"};
    }

    auto* val_default
            = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(predicated->valueExpression());
    if (nullptr == val_default) {
        throw std::runtime_error{"Expected field name in CLP function argument"};
    }

    auto* primary = val_default->primaryExpression();

    // String literal: CLP_GET_INT('field.name')
    if (auto* str_lit = dynamic_cast<SqlParser::StringLiteralContext*>(primary)) {
        auto* basic = dynamic_cast<SqlParser::BasicStringLiteralContext*>(str_lit->string());
        if (nullptr != basic) {
            return unquote_sql_string(basic->STRING()->getText());
        }
        throw std::runtime_error{"Only basic string literals are supported for field names"};
    }

    // Column reference: CLP_GET_INT(field)
    if (auto* col_ref = dynamic_cast<SqlParser::ColumnReferenceContext*>(primary)) {
        return get_identifier_text(col_ref->identifier());
    }

    // Dereference: CLP_GET_INT(field.subfield)
    if (auto* deref = dynamic_cast<SqlParser::DereferenceContext*>(primary)) {
        std::vector<std::string> tokens;
        collect_dereference_tokens(deref, tokens);
        return join_tokens_with_dot(tokens);
    }

    throw std::runtime_error{"Expected field name (string literal or identifier) in CLP function"};
}

/**
 * Splits a field name on unescaped '.' into tokens for ColumnDescriptor.
 *
 * Escaping rules (applied during splitting):
 *   \.  -> literal '.' (does not split)
 *   \\  -> pass-through as '\\' for ColumnDescriptor to interpret as literal '\'
 *   \*  -> pass-through as '\*' for ColumnDescriptor to interpret as literal '*'
 *
 * An unescaped '*' is passed through as-is so that ColumnDescriptor treats it as a wildcard.
 * @param field_name the field name to split (may contain escape sequences)
 * @return the split tokens
 */
std::vector<std::string> split_field_name(std::string const& field_name) {
    std::vector<std::string> tokens;
    std::string current;
    for (size_t i = 0; i < field_name.size(); ++i) {
        char const c = field_name[i];
        if ('\\' == c && i + 1 < field_name.size()) {
            char const next = field_name[i + 1];
            if ('.' == next) {
                // Escaped dot: not a separator. ColumnDescriptor does not interpret '.',
                // so we just append the literal dot character.
                current.push_back('.');
                ++i;
                continue;
            }
            // For \\ and \*, pass through the escape sequence so that
            // ColumnDescriptor::create_from_escaped_tokens can interpret it.
            if ('\\' == next || '*' == next) {
                current.push_back('\\');
                current.push_back(next);
                ++i;
                continue;
            }
        }
        if ('\\' == c) {
            // Backslash not followed by a recognized escape target (. \\ *)
            throw std::runtime_error{
                    "Invalid escape sequence in field name: only '\\.' '\\\\"
                    "' '\\*' are supported"
            };
        }
        if ('.' == c) {
            if (false == current.empty()) {
                tokens.push_back(current);
                current.clear();
            }
        } else {
            current.push_back(c);
        }
    }
    if (false == current.empty()) {
        tokens.push_back(current);
    }
    return tokens;
}

/**
 * Builds a ColumnDescriptor from a field name string (may contain dots for nested fields).
 * Supports backslash escaping (\. for literal dot, \\ for literal backslash)
 * and * wildcards for matching any field at a given level.
 * @param field_name the field name to build a ColumnDescriptor from
 * @return the constructed ColumnDescriptor
 */
std::shared_ptr<ColumnDescriptor> build_column_from_field_name(std::string const& field_name) {
    auto tokens = split_field_name(field_name);
    if (tokens.empty()) {
        throw std::runtime_error{"Empty field name in CLP function"};
    }
    return ColumnDescriptor::create_from_escaped_tokens(tokens, "");
}

/**
 * Extracts the CLP_GET_* field name from a PrimaryExpressionContext, if it is a CLP_GET_*
 * function call. Returns std::nullopt if the primary expression is not a CLP_GET_* function.
 *
 * For CLP_GET_JSON_STRING() with no arguments, returns an empty string (meaning "select all").
 * @param primary the primary expression context
 * @return the field name, empty string for no-arg CLP_GET_JSON_STRING, or std::nullopt
 */
std::optional<std::string>
try_extract_clp_get_field(SqlParser::PrimaryExpressionContext* primary) {
    if (auto* ctx = dynamic_cast<SqlParser::ClpGetIntContext*>(primary)) {
        return extract_field_name_from_expression(ctx->expression());
    }
    if (auto* ctx = dynamic_cast<SqlParser::ClpGetFloatContext*>(primary)) {
        return extract_field_name_from_expression(ctx->expression());
    }
    if (auto* ctx = dynamic_cast<SqlParser::ClpGetStringContext*>(primary)) {
        return extract_field_name_from_expression(ctx->expression());
    }
    if (auto* ctx = dynamic_cast<SqlParser::ClpGetBoolContext*>(primary)) {
        return extract_field_name_from_expression(ctx->expression());
    }
    if (auto* ctx = dynamic_cast<SqlParser::ClpGetJsonStringContext*>(primary)) {
        if (nullptr != ctx->expression()) {
            return extract_field_name_from_expression(ctx->expression());
        }
        // CLP_GET_JSON_STRING() with no args means select all
        return std::string{};
    }
    return std::nullopt;
}

class ParseTreeVisitor : public SqlBaseVisitor {
public:
    // ── Entry point ──────────────────────────────────────────────────────

    auto visitSingleStatement(SqlParser::SingleStatementContext* ctx) -> std::any override {
        return ctx->statement()->accept(this);
    }

    auto visitStatementDefault(SqlParser::StatementDefaultContext* ctx) -> std::any override {
        return ctx->query()->accept(this);
    }

    auto visitQuery(SqlParser::QueryContext* ctx) -> std::any override {
        if (ctx->limit) {
            try {
                m_limit = std::stoll(ctx->limit->getText());
            } catch (std::out_of_range const&) {
                throw std::runtime_error{
                        "LIMIT value out of range: " + ctx->limit->getText()
                };
            }
        }
        return ctx->querySpecification()->accept(this);
    }

    // ── SELECT ... FROM ... WHERE ────────────────────────────────────────

    auto visitQuerySpecification(SqlParser::QuerySpecificationContext* ctx) -> std::any override {
        // Reject SELECT DISTINCT — requires materializing and deduplicating all results.
        if (ctx->setQuantifier() && nullptr != ctx->setQuantifier()->DISTINCT()) {
            throw std::runtime_error{"SELECT DISTINCT is not supported"};
        }

        for (auto* item : ctx->selectItem()) {
            item->accept(this);
        }

        // Extract FROM table name
        if (ctx->FROM() && nullptr != ctx->qualifiedName()) {
            auto* qn = ctx->qualifiedName();
            std::string table_name;
            for (auto* id : qn->identifier()) {
                if (false == table_name.empty()) {
                    table_name.push_back('.');
                }
                table_name += get_identifier_text(id);
            }
            m_from_table = table_name;
        }

        // Extract WHERE expression
        if (ctx->where) {
            return ctx->where->accept(this);
        }

        // No WHERE clause — match everything via wildcard filter: *:*
        auto descriptor = ColumnDescriptor::create_from_escaped_tokens({"*"}, "");
        auto literal = StringLiteral::create("*");
        return std::dynamic_pointer_cast<Expression>(
                FilterExpr::create(descriptor, FilterOperation::EQ, literal)
        );
    }

    auto visitSelectSingle(SqlParser::SelectSingleContext* ctx) -> std::any override {
        auto* expr = ctx->expression();
        if (nullptr == expr) {
            return {};
        }

        // Check if this select item is an aggregate function
        auto agg_spec = try_extract_aggregate(expr);
        if (agg_spec.has_value()) {
            // Read optional AS alias
            if (nullptr != ctx->identifier()) {
                agg_spec->alias = get_identifier_text(ctx->identifier());
            }
            m_aggregations.push_back(std::move(agg_spec.value()));
            return {};
        }

        auto const col_name = extract_column_name_from_expression(expr);
        if (false == col_name.empty()) {
            m_select_columns.push_back(col_name);

            // Track AS alias for column renaming in output
            if (nullptr != ctx->identifier()) {
                m_column_aliases[col_name] = get_identifier_text(ctx->identifier());
            }
        }
        return {};
    }

    auto visitSelectAll(SqlParser::SelectAllContext* /*ctx*/) -> std::any override {
        // SELECT * — mark that all columns should be returned.
        // If mixed with named columns (e.g., SELECT a, *), the superset (all columns) wins.
        m_select_all = true;
        return {};
    }

    // ── Boolean logic ────────────────────────────────────────────────────

    auto visitLogicalBinary(SqlParser::LogicalBinaryContext* ctx) -> std::any override {
        auto left = std::any_cast<std::shared_ptr<Expression>>(ctx->left->accept(this));
        auto right = std::any_cast<std::shared_ptr<Expression>>(ctx->right->accept(this));

        if (SqlParser::AND == ctx->operator_->getType()) {
            return std::dynamic_pointer_cast<Expression>(AndExpr::create(left, right));
        }
        return std::dynamic_pointer_cast<Expression>(OrExpr::create(left, right));
    }

    auto visitLogicalNot(SqlParser::LogicalNotContext* ctx) -> std::any override {
        auto expr = std::any_cast<std::shared_ptr<Expression>>(
                ctx->booleanExpression()->accept(this)
        );
        expr->invert();
        return expr;
    }

    auto visitPredicated(SqlParser::PredicatedContext* ctx) -> std::any override {
        if (nullptr != ctx->predicate()) {
            // Store the LHS for use by the predicate visitor methods. This is safe because
            // ANTLR visits sequentially and predicates do not nest. Reset after use to
            // catch accidental reuse if the grammar evolves.
            m_current_lhs_value_expr = ctx->valueExpression();
            auto result = ctx->predicate()->accept(this);
            m_current_lhs_value_expr = nullptr;
            return result;
        }
        return ctx->valueExpression()->accept(this);
    }

    // ── Predicates ───────────────────────────────────────────────────────

    auto visitComparison(SqlParser::ComparisonContext* ctx) -> std::any override {
        auto column = build_column_from_value_expr(m_current_lhs_value_expr);
        auto op = map_comparison_op(ctx->comparisonOperator());
        auto literal = build_literal_from_value_expr(ctx->right);
        return std::dynamic_pointer_cast<Expression>(FilterExpr::create(column, op, literal));
    }

    auto visitBetween(SqlParser::BetweenContext* ctx) -> std::any override {
        auto column = build_column_from_value_expr(m_current_lhs_value_expr);
        auto lower_lit = build_literal_from_value_expr(ctx->lower);
        auto upper_lit = build_literal_from_value_expr(ctx->upper);

        auto gte_expr = FilterExpr::create(column, FilterOperation::GTE, lower_lit);
        auto lte_expr = FilterExpr::create(column, FilterOperation::LTE, upper_lit);
        std::shared_ptr<Expression> result = AndExpr::create(gte_expr, lte_expr);

        if (nullptr != ctx->NOT()) {
            result->invert();
        }
        return result;
    }

    auto visitInList(SqlParser::InListContext* ctx) -> std::any override {
        auto column = build_column_from_value_expr(m_current_lhs_value_expr);
        auto or_expr = OrExpr::create();

        for (auto* expr_ctx : ctx->expression()) {
            auto* bool_expr = expr_ctx->booleanExpression();
            auto* predicated = dynamic_cast<SqlParser::PredicatedContext*>(bool_expr);
            if (nullptr == predicated || nullptr == predicated->valueExpression()) {
                throw std::runtime_error{
                        "Unsupported expression in IN list: only literal values are supported"
                };
            }
            auto literal = build_literal_from_value_expr(predicated->valueExpression());
            or_expr->add_operand(FilterExpr::create(column, FilterOperation::EQ, literal));
        }

        std::shared_ptr<Expression> result = or_expr;
        if (nullptr != ctx->NOT()) {
            result->invert();
        }
        return result;
    }

    auto visitLike(SqlParser::LikeContext* ctx) -> std::any override {
        auto column = build_column_from_value_expr(m_current_lhs_value_expr);

        auto* pattern_val
                = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(ctx->pattern);
        if (nullptr == pattern_val) {
            throw std::runtime_error{"Expected string literal for LIKE pattern"};
        }
        auto* str_lit
                = dynamic_cast<SqlParser::StringLiteralContext*>(pattern_val->primaryExpression());
        if (nullptr == str_lit) {
            throw std::runtime_error{"Expected string literal for LIKE pattern"};
        }
        auto* basic = dynamic_cast<SqlParser::BasicStringLiteralContext*>(str_lit->string());
        if (nullptr == basic) {
            throw std::runtime_error{"Only basic string literals are supported for LIKE"};
        }
        auto const sql_pattern = unquote_sql_string(basic->STRING()->getText());
        auto const wildcard_pattern = convert_like_pattern(sql_pattern);

        // LIKE is implemented as an equality match against a clp-s wildcard pattern.
        // clp-s interprets '*' and '?' in string literals as wildcards during query execution.
        auto literal = StringLiteral::create(wildcard_pattern);
        std::shared_ptr<Expression> result
                = FilterExpr::create(column, FilterOperation::EQ, literal);

        if (nullptr != ctx->NOT()) {
            result->invert();
        }
        return result;
    }

    auto visitNullPredicate(SqlParser::NullPredicateContext* ctx) -> std::any override {
        auto column = build_column_from_value_expr(m_current_lhs_value_expr);
        std::shared_ptr<Expression> result
                = FilterExpr::create(column, FilterOperation::EXISTS);
        // IS NULL means the column does NOT exist (inverted exists check)
        // IS NOT NULL means the column exists
        if (nullptr == ctx->NOT()) {
            result->invert();
        }
        return result;
    }

    // ── Literals ─────────────────────────────────────────────────────────

    auto visitStringLiteral(SqlParser::StringLiteralContext* ctx) -> std::any override {
        auto* basic = dynamic_cast<SqlParser::BasicStringLiteralContext*>(ctx->string());
        if (nullptr == basic) {
            throw std::runtime_error{"Only basic string literals are supported"};
        }
        auto const value = unquote_sql_string(basic->STRING()->getText());
        return std::dynamic_pointer_cast<Literal>(StringLiteral::create(value));
    }

    auto visitIntegerLiteral(SqlParser::IntegerLiteralContext* ctx) -> std::any override {
        auto const& text = ctx->INTEGER_VALUE()->getText();
        try {
            auto const value = std::stoll(text);
            return std::dynamic_pointer_cast<Literal>(Integral::create_from_int(value));
        } catch (std::out_of_range const&) {
            throw std::runtime_error{"Integer literal out of range: " + text};
        }
    }

    auto visitDecimalLiteral(SqlParser::DecimalLiteralContext* ctx) -> std::any override {
        auto const& text = ctx->DECIMAL_VALUE()->getText();
        try {
            auto const value = std::stod(text);
            return std::dynamic_pointer_cast<Literal>(Integral::create_from_float(value));
        } catch (std::out_of_range const&) {
            throw std::runtime_error{"Decimal literal out of range: " + text};
        }
    }

    auto visitDoubleLiteral(SqlParser::DoubleLiteralContext* ctx) -> std::any override {
        auto const& text = ctx->DOUBLE_VALUE()->getText();
        try {
            auto const value = std::stod(text);
            return std::dynamic_pointer_cast<Literal>(Integral::create_from_float(value));
        } catch (std::out_of_range const&) {
            throw std::runtime_error{"Double literal out of range: " + text};
        }
    }

    auto visitBooleanLiteral(SqlParser::BooleanLiteralContext* ctx) -> std::any override {
        auto* bv = ctx->booleanValue();
        bool const val = (nullptr != bv->TRUE());
        return std::dynamic_pointer_cast<Literal>(BooleanLiteral::create_from_bool(val));
    }

    auto visitNullLiteral(SqlParser::NullLiteralContext* /*ctx*/) -> std::any override {
        return std::dynamic_pointer_cast<Literal>(NullLiteral::create());
    }

    auto visitTimestampLiteral(SqlParser::TimestampLiteralContext* ctx) -> std::any override {
        auto* basic = dynamic_cast<SqlParser::BasicStringLiteralContext*>(ctx->string());
        if (nullptr == basic) {
            throw std::runtime_error{"Only basic string literals are supported for TIMESTAMP"};
        }
        auto const value = unquote_sql_string(basic->STRING()->getText());
        return std::dynamic_pointer_cast<Literal>(
                create_timestamp_literal_from_string(value, false)
        );
    }

    auto visitDateLiteral(SqlParser::DateLiteralContext* ctx) -> std::any override {
        auto* basic = dynamic_cast<SqlParser::BasicStringLiteralContext*>(ctx->string());
        if (nullptr == basic) {
            throw std::runtime_error{"Only basic string literals are supported for DATE"};
        }
        auto const value = unquote_sql_string(basic->STRING()->getText());
        return std::dynamic_pointer_cast<Literal>(
                create_timestamp_literal_from_string(value, true)
        );
    }

    // ── Parenthesized expressions ───────────────────────────────────────

    auto visitParenthesizedExpression(SqlParser::ParenthesizedExpressionContext* ctx)
            -> std::any override {
        return ctx->expression()->accept(this);
    }

    // ── Unsupported grammar rules (arithmetic) ──────────────────────────

    auto visitArithmeticUnary(SqlParser::ArithmeticUnaryContext* /*ctx*/) -> std::any override {
        throw std::runtime_error{"Arithmetic expressions are not supported in SQL queries"};
    }

    auto visitArithmeticBinary(SqlParser::ArithmeticBinaryContext* /*ctx*/) -> std::any override {
        throw std::runtime_error{"Arithmetic expressions are not supported in SQL queries"};
    }

    // ── Accessors for collected metadata ─────────────────────────────────

    [[nodiscard]] auto get_select_columns() const -> std::vector<std::string> const& {
        return m_select_columns;
    }

    [[nodiscard]] auto is_select_all() const -> bool { return m_select_all; }

    [[nodiscard]] auto get_column_aliases() const
            -> std::unordered_map<std::string, std::string> const& {
        return m_column_aliases;
    }

    [[nodiscard]] auto get_from_table() const -> std::string const& { return m_from_table; }

    [[nodiscard]] auto get_limit() const -> std::optional<int64_t> const& { return m_limit; }

    [[nodiscard]] auto get_aggregations() const -> std::vector<AggregateSpec> const& {
        return m_aggregations;
    }

private:
    /**
     * Tries to extract an AggregateSpec from an expression context, if it is an aggregate
     * function call (COUNT(*), MIN, MAX, SUM, AVG).
     * @param expr the expression context
     * @return an AggregateSpec on success, or std::nullopt if not an aggregate
     */
    static auto try_extract_aggregate(SqlParser::ExpressionContext* expr)
            -> std::optional<AggregateSpec> {
        auto* bool_expr = expr->booleanExpression();
        auto* predicated = dynamic_cast<SqlParser::PredicatedContext*>(bool_expr);
        if (nullptr == predicated || nullptr == predicated->valueExpression()) {
            return std::nullopt;
        }
        auto* val_default = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(
                predicated->valueExpression()
        );
        if (nullptr == val_default) {
            return std::nullopt;
        }
        auto* primary = val_default->primaryExpression();

        if (dynamic_cast<SqlParser::AggCountStarContext*>(primary)) {
            return AggregateSpec{AggregateKind::CountStar, "", "count(*)"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggCountContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Count, col, "count(" + col + ")"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggMinContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Min, col, "min(" + col + ")"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggMaxContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Max, col, "max(" + col + ")"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggSumContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Sum, col, "sum(" + col + ")"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggAvgContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Avg, col, "avg(" + col + ")"};
        }
        if (auto* ctx = dynamic_cast<SqlParser::AggArbitraryContext*>(primary)) {
            auto col = extract_field_name_from_expression(ctx->expression());
            return AggregateSpec{AggregateKind::Arbitrary, col, "arbitrary(" + col + ")"};
        }
        return std::nullopt;
    }

    /**
     * Extracts a column name from an expression context (for SELECT column list).
     * Handles bare identifiers, dereferences, and CLP_GET_* functions.
     * @param expr the expression context
     * @return the column name, or empty string if not extractable
     */
    static std::string extract_column_name_from_expression(SqlParser::ExpressionContext* expr) {
        auto* bool_expr = expr->booleanExpression();
        auto* predicated = dynamic_cast<SqlParser::PredicatedContext*>(bool_expr);
        if (nullptr == predicated || nullptr == predicated->valueExpression()) {
            return {};
        }

        auto* val_default = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(
                predicated->valueExpression()
        );
        if (nullptr == val_default) {
            return {};
        }

        return extract_column_name_from_primary(val_default->primaryExpression());
    }

    /**
     * Extracts a column name from a primary expression (handles identifiers,
     * dereferences, and CLP_GET_* functions).
     * @param primary the primary expression context
     * @return the column name, or empty string if not extractable
     */
    static std::string
    extract_column_name_from_primary(SqlParser::PrimaryExpressionContext* primary) {
        if (auto* col_ref = dynamic_cast<SqlParser::ColumnReferenceContext*>(primary)) {
            return get_identifier_text(col_ref->identifier());
        }
        if (auto* deref = dynamic_cast<SqlParser::DereferenceContext*>(primary)) {
            std::vector<std::string> tokens;
            collect_dereference_tokens(deref, tokens);
            return join_tokens_with_dot(tokens);
        }
        if (dynamic_cast<SqlParser::ClpWildcardColumnContext*>(primary)) {
            // CLP_WILDCARD_COLUMN() in SELECT means select all columns — same as SELECT *
            return {};
        }

        auto const clp_field = try_extract_clp_get_field(primary);
        if (clp_field.has_value()) {
            return clp_field.value();
        }

        return {};
    }

    /**
     * Builds a ColumnDescriptor from a value expression used in WHERE clause.
     * Handles identifiers, dereferences, and CLP_GET_* functions.
     * @param ctx the value expression context (expected to be a column reference)
     * @return the constructed ColumnDescriptor
     */
    static std::shared_ptr<ColumnDescriptor>
    build_column_from_value_expr(SqlParser::ValueExpressionContext* ctx) {
        auto* val_default = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(ctx);
        if (nullptr == val_default) {
            throw std::runtime_error{"Expected column reference in comparison"};
        }

        auto* primary = val_default->primaryExpression();

        if (auto* col_ref = dynamic_cast<SqlParser::ColumnReferenceContext*>(primary)) {
            return build_column_descriptor(col_ref->identifier());
        }
        if (auto* deref = dynamic_cast<SqlParser::DereferenceContext*>(primary)) {
            std::vector<std::string> tokens;
            collect_dereference_tokens(deref, tokens);
            return ColumnDescriptor::create_from_escaped_tokens(tokens, "");
        }

        if (dynamic_cast<SqlParser::ClpWildcardColumnContext*>(primary)) {
            return ColumnDescriptor::create_from_escaped_tokens({"*"}, "");
        }

        // Aggregate functions are not allowed in WHERE clause
        if (dynamic_cast<SqlParser::AggCountStarContext*>(primary)
            || dynamic_cast<SqlParser::AggCountContext*>(primary)
            || dynamic_cast<SqlParser::AggMinContext*>(primary)
            || dynamic_cast<SqlParser::AggMaxContext*>(primary)
            || dynamic_cast<SqlParser::AggSumContext*>(primary)
            || dynamic_cast<SqlParser::AggAvgContext*>(primary)
            || dynamic_cast<SqlParser::AggArbitraryContext*>(primary))
        {
            throw std::runtime_error{
                    "Aggregate functions (COUNT, MIN, MAX, SUM, AVG, ARBITRARY/ANY_VALUE) are "
                    "not allowed in WHERE clause"
            };
        }

        auto const clp_field = try_extract_clp_get_field(primary);
        if (clp_field.has_value()) {
            if (clp_field.value().empty()) {
                throw std::runtime_error{"Empty field name in CLP function in WHERE clause"};
            }
            return build_column_from_field_name(clp_field.value());
        }

        throw std::runtime_error{"Expected column reference in comparison"};
    }

    /**
     * Builds a Literal from a value expression (expected to be a literal value).
     * @param ctx the value expression context
     * @return the constructed Literal
     */
    auto build_literal_from_value_expr(SqlParser::ValueExpressionContext* ctx)
            -> std::shared_ptr<Literal> {
        // Handle unary +/- for negative numeric literals (e.g., -42, -3.14)
        if (auto* unary = dynamic_cast<SqlParser::ArithmeticUnaryContext*>(ctx)) {
            auto* inner = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(
                    unary->valueExpression()
            );
            if (nullptr == inner) {
                throw std::runtime_error{
                        "Arithmetic expressions are not supported in SQL queries"
                };
            }
            auto* num_lit = dynamic_cast<SqlParser::NumericLiteralContext*>(
                    inner->primaryExpression()
            );
            if (nullptr == num_lit) {
                throw std::runtime_error{
                        "Unary +/- is only supported on numeric literals in SQL queries"
                };
            }

            bool const negate = (SqlParser::MINUS == unary->operator_->getType());
            auto* number_ctx = num_lit->number();
            auto const& text = number_ctx->getText();

            try {
                if (dynamic_cast<SqlParser::IntegerLiteralContext*>(number_ctx)) {
                    auto value = std::stoll(text);
                    return std::dynamic_pointer_cast<Literal>(
                            Integral::create_from_int(negate ? -value : value)
                    );
                }
                // Decimal or double literal
                auto value = std::stod(text);
                return std::dynamic_pointer_cast<Literal>(
                        Integral::create_from_float(negate ? -value : value)
                );
            } catch (std::out_of_range const&) {
                throw std::runtime_error{"Numeric literal out of range: " + text};
            }
        }

        auto* val_default = dynamic_cast<SqlParser::ValueExpressionDefaultContext*>(ctx);
        if (nullptr == val_default) {
            throw std::runtime_error{"Expected literal value in expression"};
        }

        auto* primary = val_default->primaryExpression();
        if (auto* str_lit = dynamic_cast<SqlParser::StringLiteralContext*>(primary)) {
            return std::any_cast<std::shared_ptr<Literal>>(str_lit->accept(this));
        }
        if (auto* num_lit = dynamic_cast<SqlParser::NumericLiteralContext*>(primary)) {
            return std::any_cast<std::shared_ptr<Literal>>(num_lit->accept(this));
        }
        if (auto* bool_lit = dynamic_cast<SqlParser::BooleanLiteralContext*>(primary)) {
            return std::any_cast<std::shared_ptr<Literal>>(bool_lit->accept(this));
        }
        if (dynamic_cast<SqlParser::NullLiteralContext*>(primary)) {
            return NullLiteral::create();
        }
        if (auto* ts_lit = dynamic_cast<SqlParser::TimestampLiteralContext*>(primary)) {
            return std::any_cast<std::shared_ptr<Literal>>(ts_lit->accept(this));
        }
        if (auto* date_lit = dynamic_cast<SqlParser::DateLiteralContext*>(primary)) {
            return std::any_cast<std::shared_ptr<Literal>>(date_lit->accept(this));
        }
        throw std::runtime_error{"Expected literal value, got column reference or expression"};
    }

    // LHS value expression for the current predicate being visited.
    // Set by visitPredicated() before dispatching to the predicate visitor.
    SqlParser::ValueExpressionContext* m_current_lhs_value_expr{nullptr};

    // Collected metadata from the query
    std::vector<std::string> m_select_columns;
    bool m_select_all{false};
    std::unordered_map<std::string, std::string> m_column_aliases;
    std::string m_from_table;
    std::optional<int64_t> m_limit;
    std::vector<AggregateSpec> m_aggregations;
};
}  // namespace

auto parse_sql_query(std::istream& in) -> std::optional<SqlQuerySpec> {
    // Normalize only SQL keywords to uppercase (the grammar requires uppercase keywords).
    // Column names, table names, and string literals preserve their original case.
    std::string const raw_input{std::istreambuf_iterator<char>(in), {}};
    auto const normalized = normalize_sql_keywords(raw_input);

    ErrorListener lexer_error_listener;
    ErrorListener parser_error_listener;

    ANTLRInputStream input{normalized};
    SqlLexer lexer{&input};
    lexer.removeErrorListeners();
    lexer.addErrorListener(&lexer_error_listener);
    CommonTokenStream tokens{&lexer};
    SqlParser parser(&tokens);
    parser.removeErrorListeners();
    parser.addErrorListener(&parser_error_listener);
    SqlParser::SingleStatementContext* tree{parser.singleStatement()};

    if (lexer_error_listener.error()) {
        SPDLOG_ERROR("SQL syntax error: {}", lexer_error_listener.message());
        return std::nullopt;
    }
    if (parser_error_listener.error()) {
        SPDLOG_ERROR("SQL syntax error: {}", parser_error_listener.message());
        return std::nullopt;
    }

    ParseTreeVisitor visitor;
    try {
        auto expr = std::any_cast<std::shared_ptr<Expression>>(visitor.visitSingleStatement(tree));
        auto const& aggregations = visitor.get_aggregations();
        auto const& select_columns = visitor.get_select_columns();

        // Validate: mixing aggregate and non-aggregate items in SELECT is not supported
        if (false == aggregations.empty() && false == select_columns.empty()) {
            SPDLOG_ERROR(
                    "SQL parse error: cannot mix aggregate functions and column references in "
                    "SELECT"
            );
            return std::nullopt;
        }

        // If SELECT * appears (even mixed with named columns), return all columns.
        // Named columns' aliases are still preserved for renaming in the output.
        auto effective_columns
                = visitor.is_select_all() ? std::vector<std::string>{} : select_columns;

        return SqlQuerySpec{
                .where_expr = expr,
                .select_columns = std::move(effective_columns),
                .from_table = visitor.get_from_table(),
                .limit = visitor.get_limit(),
                .column_aliases = visitor.get_column_aliases(),
                .aggregations = aggregations,
        };
    } catch (std::exception const& e) {
        SPDLOG_ERROR("SQL parse error: {}", e.what());
        return std::nullopt;
    }
}

auto parse_sql_expression(std::istream& in) -> std::shared_ptr<Expression> {
    auto result = parse_sql_query(in);
    if (false == result.has_value()) {
        return nullptr;
    }
    return result->where_expr;
}
}  // namespace clp_s::search::sql
