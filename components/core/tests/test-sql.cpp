#include <cstdint>
#include <iostream>
#include <memory>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>
#include <nlohmann/json.hpp>

#include "../src/clp_s/search/ast/AndExpr.hpp"
#include "../src/clp_s/search/ast/ColumnDescriptor.hpp"
#include "../src/clp_s/search/ast/FilterExpr.hpp"
#include "../src/clp_s/search/ast/FilterOperation.hpp"
#include "../src/clp_s/search/ast/OrExpr.hpp"
#include "../src/clp_s/search/LocalAggregateOutputHandler.hpp"
#include "../src/clp_s/search/RenamingOutputHandler.hpp"
#include "../src/clp_s/search/sql/sql.hpp"
#include "LogSuppressor.hpp"

using clp_s::search::ast::AndExpr;
using clp_s::search::ast::ColumnDescriptor;
using clp_s::search::ast::DescriptorToken;
using clp_s::search::ast::Expression;
using clp_s::search::ast::FilterExpr;
using clp_s::search::ast::FilterOperation;
using clp_s::search::ast::OrExpr;
using clp_s::ErrorCode;
using clp_s::search::LocalAggregateOutputHandler;
using clp_s::search::RenamingOutputHandler;
using clp_s::search::sql::AggregateKind;
using clp_s::search::sql::AggregateSpec;
using clp_s::search::sql::parse_sql_expression;
using clp_s::search::sql::parse_sql_query;
using clp_s::search::sql::SqlQuerySpec;
using std::string;
using std::stringstream;
using std::vector;

namespace {
/**
 * Helper to parse a SQL query and return the SqlQuerySpec.
 * @param query the SQL query string
 * @return the parsed SqlQuerySpec, or std::nullopt on failure
 */
auto parse_query(string const& query) -> std::optional<SqlQuerySpec> {
    stringstream ss{query};
    return parse_sql_query(ss);
}

/**
 * Helper to parse a SQL query and return the WHERE clause as a FilterExpr.
 * @param query the SQL query string
 * @return the FilterExpr, or nullptr if parsing failed or result is not a FilterExpr
 */
auto parse_as_filter(string const& query) -> std::shared_ptr<FilterExpr> {
    auto spec = parse_query(query);
    if (false == spec.has_value()) {
        return nullptr;
    }
    return std::dynamic_pointer_cast<FilterExpr>(spec->where_expr);
}

/**
 * Extracts the string value from a FilterExpr's literal operand.
 * @param filter the FilterExpr to extract from
 * @return the string value, or empty string on failure
 */
auto get_operand_string(std::shared_ptr<FilterExpr> const& filter) -> string {
    string value;
    if (nullptr != filter->get_operand()) {
        filter->get_operand()->as_var_string(value, filter->get_operation());
    }
    return value;
}

/**
 * Extracts the int64_t value from a FilterExpr's literal operand.
 * @param filter the FilterExpr to extract from
 * @param[out] ret the extracted value
 * @return true if extraction succeeded
 */
auto get_operand_int(std::shared_ptr<FilterExpr> const& filter, int64_t& ret) -> bool {
    if (nullptr == filter->get_operand()) {
        return false;
    }
    return filter->get_operand()->as_int(ret, filter->get_operation());
}

/**
 * Extracts the double value from a FilterExpr's literal operand.
 * @param filter the FilterExpr to extract from
 * @param[out] ret the extracted value
 * @return true if extraction succeeded
 */
auto get_operand_float(std::shared_ptr<FilterExpr> const& filter, double& ret) -> bool {
    if (nullptr == filter->get_operand()) {
        return false;
    }
    return filter->get_operand()->as_float(ret, filter->get_operation());
}

/**
 * Extracts the bool value from a FilterExpr's literal operand.
 * @param filter the FilterExpr to extract from
 * @param[out] ret the extracted value
 * @return true if extraction succeeded
 */
auto get_operand_bool(std::shared_ptr<FilterExpr> const& filter, bool& ret) -> bool {
    if (nullptr == filter->get_operand()) {
        return false;
    }
    return filter->get_operand()->as_bool(ret, filter->get_operation());
}
}  // namespace

TEST_CASE("SQL query metadata parsing", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("SELECT * with no WHERE produces match-all filter") {
        auto spec = parse_query("SELECT * FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
        REQUIRE("logs" == spec->from_table);
        REQUIRE(false == spec->limit.has_value());
        // WHERE is a wildcard match-all: *:*
        auto filter = std::dynamic_pointer_cast<FilterExpr>(spec->where_expr);
        REQUIRE(nullptr != filter);
        REQUIRE(filter->get_column()->is_pure_wildcard());
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("*" == get_operand_string(filter));
    }

    SECTION("SELECT specific columns are extracted") {
        auto spec = parse_query("SELECT level, status, msg FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(3 == spec->select_columns.size());
        REQUIRE("level" == spec->select_columns[0]);
        REQUIRE("status" == spec->select_columns[1]);
        REQUIRE("msg" == spec->select_columns[2]);
    }

    SECTION("SELECT with CLP_GET_* extracts field names") {
        auto spec = parse_query(
                "SELECT CLP_GET_INT('status'), CLP_GET_STRING('meta.region') FROM logs"
        );
        REQUIRE(spec.has_value());
        REQUIRE(2 == spec->select_columns.size());
        REQUIRE("status" == spec->select_columns[0]);
        REQUIRE("meta.region" == spec->select_columns[1]);
    }

    SECTION("SELECT CLP_WILDCARD_COLUMN() behaves like SELECT *") {
        auto spec = parse_query("SELECT CLP_WILDCARD_COLUMN() FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("SELECT CLP_GET_JSON_STRING() with no args produces empty column name") {
        auto spec = parse_query("SELECT CLP_GET_JSON_STRING() FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("FROM clause extracts table name") {
        auto query = GENERATE(
                "SELECT * FROM logs",
                "SELECT * FROM my_dataset",
                "select * from logs"
        );
        auto spec = parse_query(query);
        REQUIRE(spec.has_value());
        REQUIRE(false == spec->from_table.empty());
    }

    SECTION("LIMIT clause is extracted") {
        auto spec = parse_query("SELECT * FROM logs LIMIT 100");
        REQUIRE(spec.has_value());
        REQUIRE(spec->limit.has_value());
        REQUIRE(100 == spec->limit.value());
    }

    SECTION("FROM clause is optional") {
        auto spec = parse_query("SELECT * WHERE level = 'ERROR'");
        REQUIRE(spec.has_value());
        REQUIRE(spec->from_table.empty());
        auto filter = std::dynamic_pointer_cast<FilterExpr>(spec->where_expr);
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("ERROR" == get_operand_string(filter));
    }

    SECTION("No LIMIT clause yields nullopt") {
        auto spec = parse_query("SELECT * FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(false == spec->limit.has_value());
    }
}

TEST_CASE("SQL case-insensitive keywords", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("Keywords are case-insensitive") {
        auto query = GENERATE(
                "SELECT * FROM logs WHERE level = 'ERROR'",
                "select * from logs where level = 'ERROR'",
                "Select * From logs Where level = 'ERROR'",
                "sElEcT * fRoM logs wHeRe level = 'ERROR'"
        );
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("ERROR" == get_operand_string(filter));
    }

    SECTION("CLP_GET_* keywords are case-insensitive") {
        auto query = GENERATE(
                "SELECT * FROM logs WHERE CLP_GET_INT('status') = 200",
                "SELECT * FROM logs WHERE clp_get_int('status') = 200",
                "SELECT * FROM logs WHERE Clp_Get_Int('status') = 200"
        );
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(200 == value);
    }
}

TEST_CASE("SQL comparison operators", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("All comparison operators are parsed correctly") {
        auto const [query, expected_op] = GENERATE(table<string, FilterOperation>({
                {"SELECT * FROM t WHERE x = 1", FilterOperation::EQ},
                {"SELECT * FROM t WHERE x != 1", FilterOperation::NEQ},
                {"SELECT * FROM t WHERE x <> 1", FilterOperation::NEQ},
                {"SELECT * FROM t WHERE x < 1", FilterOperation::LT},
                {"SELECT * FROM t WHERE x <= 1", FilterOperation::LTE},
                {"SELECT * FROM t WHERE x > 1", FilterOperation::GT},
                {"SELECT * FROM t WHERE x >= 1", FilterOperation::GTE},
        }));
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        REQUIRE(expected_op == filter->get_operation());
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(1 == value);
    }
}

TEST_CASE("SQL literal types", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("String literal") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = 'hello'");
        REQUIRE(nullptr != filter);
        REQUIRE("hello" == get_operand_string(filter));
    }

    SECTION("String literal with escaped single quotes") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = 'it''s'");
        REQUIRE(nullptr != filter);
        REQUIRE("it's" == get_operand_string(filter));
    }

    SECTION("Integer literal") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = 42");
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(42 == value);
    }

    SECTION("Negative integer literal") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = -42");
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(-42 == value);
    }

    SECTION("Decimal literal") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = 3.14");
        REQUIRE(nullptr != filter);
        double value{};
        REQUIRE(get_operand_float(filter, value));
        REQUIRE(3.14 == value);
    }

    SECTION("Negative decimal literal") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = -3.14");
        REQUIRE(nullptr != filter);
        double value{};
        REQUIRE(get_operand_float(filter, value));
        REQUIRE(-3.14 == value);
    }

    SECTION("Boolean literal true") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = TRUE");
        REQUIRE(nullptr != filter);
        bool value{};
        REQUIRE(get_operand_bool(filter, value));
        REQUIRE(true == value);
    }

    SECTION("Boolean literal false") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col = FALSE");
        REQUIRE(nullptr != filter);
        bool value{};
        REQUIRE(get_operand_bool(filter, value));
        REQUIRE(false == value);
    }

    SECTION("NULL literal with IS NULL") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col IS NULL");
        REQUIRE(nullptr != filter);
        // IS NULL is implemented as an inverted EXISTS check
        REQUIRE(FilterOperation::EXISTS == filter->get_operation());
        REQUIRE(true == filter->is_inverted());
    }

    SECTION("IS NOT NULL") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col IS NOT NULL");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EXISTS == filter->get_operation());
        REQUIRE(false == filter->is_inverted());
    }
}

TEST_CASE("SQL column references", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("Simple column name") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE status = 200");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
        auto const token = DescriptorToken::create_descriptor_from_escaped_token("status");
        REQUIRE(token == *filter->get_column()->descriptor_begin());
    }

    SECTION("Dotted column name (dereference)") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE meta.region = 'us'");
        REQUIRE(nullptr != filter);
        REQUIRE(2 == filter->get_column()->get_descriptor_list().size());
        auto it = filter->get_column()->descriptor_begin();
        auto const meta_token = DescriptorToken::create_descriptor_from_escaped_token("meta");
        REQUIRE(meta_token == *it++);
        auto const region_token = DescriptorToken::create_descriptor_from_escaped_token("region");
        REQUIRE(region_token == *it++);
    }

    SECTION("CLP_GET_* with string literal field name") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING('level') = 'ERROR'");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
        auto const token = DescriptorToken::create_descriptor_from_escaped_token("level");
        REQUIRE(token == *filter->get_column()->descriptor_begin());
    }

    SECTION("CLP_GET_* with nested field name via string literal") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING('meta.region') = 'us'");
        REQUIRE(nullptr != filter);
        REQUIRE(2 == filter->get_column()->get_descriptor_list().size());
        auto it = filter->get_column()->descriptor_begin();
        auto const meta_token = DescriptorToken::create_descriptor_from_escaped_token("meta");
        REQUIRE(meta_token == *it++);
        auto const region_token = DescriptorToken::create_descriptor_from_escaped_token("region");
        REQUIRE(region_token == *it++);
    }

    SECTION("CLP_GET_* with dereference field name") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING(meta.region) = 'us'");
        REQUIRE(nullptr != filter);
        REQUIRE(2 == filter->get_column()->get_descriptor_list().size());
    }

    SECTION("Wildcard field path via CLP_GET_*") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING('meta.*') = 'us'");
        REQUIRE(nullptr != filter);
        REQUIRE(2 == filter->get_column()->get_descriptor_list().size());
        auto it = filter->get_column()->descriptor_begin();
        auto const meta_token = DescriptorToken::create_descriptor_from_escaped_token("meta");
        REQUIRE(meta_token == *it++);
        // Second token should be a wildcard
        REQUIRE(it->wildcard());
    }

    SECTION("Pure wildcard field path") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING('*') = 'ERROR'");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->get_column()->is_pure_wildcard());
    }

    SECTION("Escaped dot in field name produces single-level token with literal dot") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE CLP_GET_STRING('app\\.name') = 'web'");
        REQUIRE(nullptr != filter);
        // Escaped dot should NOT split into two tokens
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
        auto const token = DescriptorToken::create_descriptor_from_escaped_token("app.name");
        REQUIRE(token == *filter->get_column()->descriptor_begin());
    }

    SECTION("Quoted identifier") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE \"my-col\" = 'val'");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
    }

    SECTION("Quoted identifier starting with @") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE \"@timestamp\" = 'val'");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
    }

    SECTION("Quoted identifier with doubled quotes") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE \"col\"\"name\" = 'val'");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
    }

    SECTION("Identifier with @ and : continuation characters") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE col@attr = 'val'");
        REQUIRE(nullptr != filter);
        REQUIRE(1 == filter->get_column()->get_descriptor_list().size());
        auto const token = DescriptorToken::create_descriptor_from_escaped_token("col@attr");
        REQUIRE(token == *filter->get_column()->descriptor_begin());
    }

    SECTION("Deeply chained dereference (a.b.c.d)") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE a.b.c.d = 'val'");
        REQUIRE(nullptr != filter);
        vector<string> const expected_tokens = {"a", "b", "c", "d"};
        REQUIRE(expected_tokens.size() == filter->get_column()->get_descriptor_list().size());
        auto it = filter->get_column()->descriptor_begin();
        for (auto const& name : expected_tokens) {
            auto const token = DescriptorToken::create_descriptor_from_escaped_token(name);
            REQUIRE(token == *it++);
        }
    }
}

TEST_CASE("SQL CLP_WILDCARD_COLUMN", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("CLP_WILDCARD_COLUMN() creates pure wildcard column in WHERE") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE CLP_WILDCARD_COLUMN() = 'error'",
                "SELECT * FROM t WHERE clp_wildcard_column() = 'error'",
                "SELECT * FROM t WHERE Clp_Wildcard_Column() = 'error'"
        );
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->get_column()->is_pure_wildcard());
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("error" == get_operand_string(filter));
    }

    SECTION("CLP_WILDCARD_COLUMN() with comparison operators") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE CLP_WILDCARD_COLUMN() > 100");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->get_column()->is_pure_wildcard());
        REQUIRE(FilterOperation::GT == filter->get_operation());
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(100 == value);
    }

    SECTION("CLP_WILDCARD_COLUMN() with LIKE") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE CLP_WILDCARD_COLUMN() LIKE '%error%'");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->get_column()->is_pure_wildcard());
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("*error*" == get_operand_string(filter));
    }

    SECTION("CLP_WILDCARD_COLUMN() with IS NULL") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE CLP_WILDCARD_COLUMN() IS NULL");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->get_column()->is_pure_wildcard());
        REQUIRE(FilterOperation::EXISTS == filter->get_operation());
        REQUIRE(true == filter->is_inverted());
    }

    SECTION("CLP_WILDCARD_COLUMN() in boolean logic") {
        auto spec = parse_query(
                "SELECT * FROM t WHERE CLP_WILDCARD_COLUMN() = 'error' AND status = 500"
        );
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(2 == and_expr->get_num_operands());
    }
}

TEST_CASE("SQL boolean logic", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("AND expression") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE a = 1 AND b = 2",
                "select * from t where a = 1 and b = 2"
        );
        auto spec = parse_query(query);
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(false == and_expr->is_inverted());
        REQUIRE(2 == and_expr->get_num_operands());
    }

    SECTION("OR expression") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE a = 1 OR b = 2",
                "select * from t where a = 1 or b = 2"
        );
        auto spec = parse_query(query);
        REQUIRE(spec.has_value());
        auto or_expr = std::dynamic_pointer_cast<OrExpr>(spec->where_expr);
        REQUIRE(nullptr != or_expr);
        REQUIRE(false == or_expr->is_inverted());
        REQUIRE(2 == or_expr->get_num_operands());
    }

    SECTION("NOT inverts the expression") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE NOT col = 'val'");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->is_inverted());
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("Parenthesized expression") {
        auto spec = parse_query("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3");
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(2 == and_expr->get_num_operands());
        // First operand should be an OR expression
        auto it = and_expr->op_begin();
        auto or_child = std::dynamic_pointer_cast<OrExpr>(*it);
        REQUIRE(nullptr != or_child);
        REQUIRE(2 == or_child->get_num_operands());
    }
}

TEST_CASE("SQL BETWEEN predicate", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("BETWEEN produces AND of GTE and LTE") {
        auto spec = parse_query("SELECT * FROM t WHERE x BETWEEN 10 AND 20");
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(false == and_expr->is_inverted());
        REQUIRE(2 == and_expr->get_num_operands());

        auto it = and_expr->op_begin();
        auto gte_filter = std::dynamic_pointer_cast<FilterExpr>(*it++);
        auto lte_filter = std::dynamic_pointer_cast<FilterExpr>(*it++);
        REQUIRE(nullptr != gte_filter);
        REQUIRE(nullptr != lte_filter);
        REQUIRE(FilterOperation::GTE == gte_filter->get_operation());
        REQUIRE(FilterOperation::LTE == lte_filter->get_operation());

        int64_t lower{};
        int64_t upper{};
        REQUIRE(get_operand_int(gte_filter, lower));
        REQUIRE(get_operand_int(lte_filter, upper));
        REQUIRE(10 == lower);
        REQUIRE(20 == upper);
    }

    SECTION("NOT BETWEEN inverts the expression") {
        auto spec = parse_query("SELECT * FROM t WHERE x NOT BETWEEN 10 AND 20");
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(true == and_expr->is_inverted());
    }
}

TEST_CASE("SQL IN predicate", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("IN list produces OR of EQ filters") {
        auto spec = parse_query("SELECT * FROM t WHERE level IN ('A', 'B', 'C')");
        REQUIRE(spec.has_value());
        auto or_expr = std::dynamic_pointer_cast<OrExpr>(spec->where_expr);
        REQUIRE(nullptr != or_expr);
        REQUIRE(false == or_expr->is_inverted());
        REQUIRE(3 == or_expr->get_num_operands());

        vector<string> const expected_values = {"A", "B", "C"};
        size_t idx = 0;
        for (auto const& operand : or_expr->get_op_list()) {
            auto filter = std::dynamic_pointer_cast<FilterExpr>(operand);
            REQUIRE(nullptr != filter);
            REQUIRE(FilterOperation::EQ == filter->get_operation());
            REQUIRE(expected_values[idx] == get_operand_string(filter));
            ++idx;
        }
    }

    SECTION("NOT IN inverts the expression") {
        auto spec = parse_query("SELECT * FROM t WHERE level NOT IN ('A', 'B')");
        REQUIRE(spec.has_value());
        auto or_expr = std::dynamic_pointer_cast<OrExpr>(spec->where_expr);
        REQUIRE(nullptr != or_expr);
        REQUIRE(true == or_expr->is_inverted());
    }
}

TEST_CASE("SQL LIKE predicate", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("LIKE converts SQL wildcards to clp-s wildcards") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE '%error%'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        // SQL '%' should be converted to clp-s '*'
        REQUIRE("*error*" == get_operand_string(filter));
    }

    SECTION("LIKE converts _ to ?") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'a_c'");
        REQUIRE(nullptr != filter);
        REQUIRE("a?c" == get_operand_string(filter));
    }

    SECTION("LIKE escapes: \\% produces literal %") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE '100\\%'");
        REQUIRE(nullptr != filter);
        REQUIRE("100%" == get_operand_string(filter));
    }

    SECTION("LIKE escapes: \\_ produces literal _") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'a\\_b'");
        REQUIRE(nullptr != filter);
        REQUIRE("a_b" == get_operand_string(filter));
    }

    SECTION("LIKE escapes: \\\\ produces literal backslash for clp-s") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'a\\\\b'");
        REQUIRE(nullptr != filter);
        // SQL '\\' -> literal '\', output as '\\' for clp-s
        REQUIRE("a\\\\b" == get_operand_string(filter));
    }

    SECTION("LIKE escapes literal * to \\*") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'a*b'");
        REQUIRE(nullptr != filter);
        // Literal '*' in SQL LIKE should become '\\*' to prevent clp-s wildcard interpretation
        REQUIRE("a\\*b" == get_operand_string(filter));
    }

    SECTION("LIKE escapes literal ? to \\?") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'what?'");
        REQUIRE(nullptr != filter);
        // Literal '?' in SQL LIKE should become '\\?' to prevent clp-s wildcard interpretation
        REQUIRE("what\\?" == get_operand_string(filter));
    }

    SECTION("LIKE with no wildcards produces exact match") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg LIKE 'exact'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("exact" == get_operand_string(filter));
    }

    SECTION("NOT LIKE inverts the expression") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE msg NOT LIKE '%err%'");
        REQUIRE(nullptr != filter);
        REQUIRE(true == filter->is_inverted());
    }
}

TEST_CASE("SQL error handling", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("Invalid SQL returns nullopt") {
        auto query = GENERATE(
                "",
                "SELECT FROM WHERE",
                "DELETE FROM logs",
                "INSERT INTO logs VALUES (1)",
                "UPDATE logs SET x = 1"
        );
        auto spec = parse_query(query);
        REQUIRE(false == spec.has_value());
    }

    SECTION("Arithmetic expressions produce error") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE x + 1 = 2",
                "SELECT * FROM t WHERE -x = 1",
                "SELECT * FROM t WHERE x * 2 = 4"
        );
        auto spec = parse_query(query);
        REQUIRE(false == spec.has_value());
    }

    SECTION("CLP_GET_JSON_STRING() without args in WHERE produces error") {
        auto spec = parse_query("SELECT * FROM t WHERE CLP_GET_JSON_STRING() = 'x'");
        REQUIRE(false == spec.has_value());
    }
}

TEST_CASE("SQL TIMESTAMP and DATE literals", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("TIMESTAMP with ISO-8601 datetime string") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15 10:30:00'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        // Should produce a TimestampLiteral, which supports as_int
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }

    SECTION("TIMESTAMP with milliseconds") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15 10:30:00.123'");
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }

    SECTION("TIMESTAMP is case-insensitive") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15 10:30:00'",
                "SELECT * FROM t WHERE ts = timestamp '2024-01-15 10:30:00'",
                "SELECT * FROM t WHERE ts = Timestamp '2024-01-15 10:30:00'"
        );
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }

    SECTION("DATE literal appends 00:00:00") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE ts >= DATE '2024-01-15'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::GTE == filter->get_operation());
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }

    SECTION("TIMESTAMP in BETWEEN") {
        auto spec = parse_query(
                "SELECT * FROM t WHERE ts BETWEEN TIMESTAMP '2024-01-01 00:00:00'"
                " AND TIMESTAMP '2024-12-31 23:59:59'"
        );
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(2 == and_expr->get_num_operands());

        auto it = and_expr->op_begin();
        auto gte_filter = std::dynamic_pointer_cast<FilterExpr>(*it++);
        auto lte_filter = std::dynamic_pointer_cast<FilterExpr>(*it++);
        REQUIRE(nullptr != gte_filter);
        REQUIRE(nullptr != lte_filter);
        REQUIRE(FilterOperation::GTE == gte_filter->get_operation());
        REQUIRE(FilterOperation::LTE == lte_filter->get_operation());
    }

    SECTION("TIMESTAMP in comparison operators") {
        auto const [query, expected_op] = GENERATE(table<string, FilterOperation>({
                {"SELECT * FROM t WHERE ts > TIMESTAMP '2024-01-15 10:30:00'",
                 FilterOperation::GT},
                {"SELECT * FROM t WHERE ts < TIMESTAMP '2024-01-15 10:30:00'",
                 FilterOperation::LT},
                {"SELECT * FROM t WHERE ts >= TIMESTAMP '2024-01-15 10:30:00'",
                 FilterOperation::GTE},
                {"SELECT * FROM t WHERE ts <= TIMESTAMP '2024-01-15 10:30:00'",
                 FilterOperation::LTE},
        }));
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        REQUIRE(expected_op == filter->get_operation());
    }

    SECTION("Invalid timestamp string produces error") {
        auto spec = parse_query("SELECT * FROM t WHERE ts = TIMESTAMP 'not-a-timestamp'");
        REQUIRE(false == spec.has_value());
    }

    SECTION("TIMESTAMP as column name (backward compat)") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE timestamp = 'value'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("value" == get_operand_string(filter));
    }

    SECTION("DATE as column name (backward compat)") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE date = 'value'");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        REQUIRE("value" == get_operand_string(filter));
    }

    SECTION("TIMESTAMP date-only fallback appends 00:00:00") {
        // TIMESTAMP with a date-only string should also work via the retry logic
        auto filter = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15'");
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);

        // Should produce the same value as DATE '2024-01-15'
        auto date_filter = parse_as_filter("SELECT * FROM t WHERE ts = DATE '2024-01-15'");
        REQUIRE(nullptr != date_filter);
        int64_t date_value{};
        REQUIRE(get_operand_int(date_filter, date_value));
        REQUIRE(value == date_value);
    }

    SECTION("TIMESTAMP with ISO-8601 T separator") {
        auto filter
                = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15T10:30:00'");
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }

    SECTION("Different timestamps produce different epoch values") {
        auto filter_a
                = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-01-15 10:30:00'");
        auto filter_b
                = parse_as_filter("SELECT * FROM t WHERE ts = TIMESTAMP '2024-06-20 15:00:00'");
        REQUIRE(nullptr != filter_a);
        REQUIRE(nullptr != filter_b);
        int64_t value_a{};
        int64_t value_b{};
        REQUIRE(get_operand_int(filter_a, value_a));
        REQUIRE(get_operand_int(filter_b, value_b));
        REQUIRE(value_a != value_b);
        // January is earlier than June
        REQUIRE(value_a < value_b);
    }

    SECTION("TIMESTAMP in IN list") {
        auto spec = parse_query(
                "SELECT * FROM t WHERE ts IN (TIMESTAMP '2024-01-01 00:00:00',"
                " TIMESTAMP '2024-06-15 12:00:00')"
        );
        REQUIRE(spec.has_value());
        auto or_expr = std::dynamic_pointer_cast<OrExpr>(spec->where_expr);
        REQUIRE(nullptr != or_expr);
        REQUIRE(2 == or_expr->get_num_operands());
    }

    SECTION("TIMESTAMP in boolean logic") {
        auto spec = parse_query(
                "SELECT * FROM t WHERE ts > TIMESTAMP '2024-01-01 00:00:00'"
                " AND level = 'ERROR'"
        );
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(2 == and_expr->get_num_operands());
    }

    SECTION("TIMESTAMP in NOT BETWEEN") {
        auto spec = parse_query(
                "SELECT * FROM t WHERE ts NOT BETWEEN TIMESTAMP '2024-01-01 00:00:00'"
                " AND TIMESTAMP '2024-12-31 23:59:59'"
        );
        REQUIRE(spec.has_value());
        auto and_expr = std::dynamic_pointer_cast<AndExpr>(spec->where_expr);
        REQUIRE(nullptr != and_expr);
        REQUIRE(true == and_expr->is_inverted());
        REQUIRE(2 == and_expr->get_num_operands());
    }

    SECTION("TIMESTAMP as column name in SELECT clause") {
        auto spec = parse_query("SELECT timestamp FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
        // normalize_sql_keywords() uppercases TIMESTAMP since it's a keyword
        REQUIRE("TIMESTAMP" == spec->select_columns[0]);
    }

    SECTION("Invalid DATE string produces error") {
        auto spec = parse_query("SELECT * FROM t WHERE ts = DATE 'not-a-date'");
        REQUIRE(false == spec.has_value());
    }

    SECTION("DATE is case-insensitive") {
        auto query = GENERATE(
                "SELECT * FROM t WHERE ts = DATE '2024-01-15'",
                "SELECT * FROM t WHERE ts = date '2024-01-15'",
                "SELECT * FROM t WHERE ts = Date '2024-01-15'"
        );
        auto filter = parse_as_filter(query);
        REQUIRE(nullptr != filter);
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(value != 0);
    }
}

TEST_CASE("SQL parse_sql_expression convenience interface", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("Returns WHERE clause expression") {
        stringstream ss{"SELECT * FROM t WHERE level = 'ERROR'"};
        auto expr = parse_sql_expression(ss);
        REQUIRE(nullptr != expr);
        auto filter = std::dynamic_pointer_cast<FilterExpr>(expr);
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("Returns nullptr on parse failure") {
        stringstream ss{"NOT VALID SQL"};
        auto expr = parse_sql_expression(ss);
        REQUIRE(nullptr == expr);
    }
}

TEST_CASE("SQL aggregate functions", "[SQL]") {
    LogSuppressor const suppressor;

    SECTION("COUNT(*) produces correct AggregateSpec") {
        auto spec = parse_query("SELECT COUNT(*) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::CountStar == spec->aggregations[0].kind);
        REQUIRE(spec->aggregations[0].column.empty());
        REQUIRE("count(*)" == spec->aggregations[0].alias);
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("COUNT(*) AS cnt sets alias") {
        auto spec = parse_query("SELECT COUNT(*) AS cnt FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::CountStar == spec->aggregations[0].kind);
        REQUIRE("cnt" == spec->aggregations[0].alias);
    }

    SECTION("MIN(col) extracts column name") {
        auto spec = parse_query("SELECT MIN(latency) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Min == spec->aggregations[0].kind);
        REQUIRE("latency" == spec->aggregations[0].column);
        REQUIRE("min(latency)" == spec->aggregations[0].alias);
    }

    SECTION("MAX(col) extracts column name") {
        auto spec = parse_query("SELECT MAX(latency) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Max == spec->aggregations[0].kind);
        REQUIRE("latency" == spec->aggregations[0].column);
    }

    SECTION("SUM(col) extracts column name") {
        auto spec = parse_query("SELECT SUM(bytes) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Sum == spec->aggregations[0].kind);
        REQUIRE("bytes" == spec->aggregations[0].column);
    }

    SECTION("AVG(col) extracts column name") {
        auto spec = parse_query("SELECT AVG(latency) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Avg == spec->aggregations[0].kind);
        REQUIRE("latency" == spec->aggregations[0].column);
        REQUIRE("avg(latency)" == spec->aggregations[0].alias);
    }

    SECTION("Dotted column path: MIN(meta.latency)") {
        auto spec = parse_query("SELECT MIN(meta.latency) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Min == spec->aggregations[0].kind);
        REQUIRE("meta.latency" == spec->aggregations[0].column);
    }

    SECTION("Multiple aggregates in one query") {
        auto spec = parse_query(
                "SELECT MIN(latency) AS min_lat, MAX(latency) AS max_lat, COUNT(*) AS cnt "
                "FROM logs"
        );
        REQUIRE(spec.has_value());
        REQUIRE(3 == spec->aggregations.size());
        REQUIRE(AggregateKind::Min == spec->aggregations[0].kind);
        REQUIRE("min_lat" == spec->aggregations[0].alias);
        REQUIRE(AggregateKind::Max == spec->aggregations[1].kind);
        REQUIRE("max_lat" == spec->aggregations[1].alias);
        REQUIRE(AggregateKind::CountStar == spec->aggregations[2].kind);
        REQUIRE("cnt" == spec->aggregations[2].alias);
    }

    SECTION("Aggregate with WHERE clause") {
        auto spec
                = parse_query("SELECT COUNT(*) AS cnt FROM logs WHERE level = 'ERROR'");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::CountStar == spec->aggregations[0].kind);
        auto filter = std::dynamic_pointer_cast<FilterExpr>(spec->where_expr);
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("COUNT(col) extracts column name") {
        auto spec = parse_query("SELECT COUNT(status) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Count == spec->aggregations[0].kind);
        REQUIRE("status" == spec->aggregations[0].column);
        REQUIRE("count(status)" == spec->aggregations[0].alias);
    }

    SECTION("COUNT(col) AS alias sets alias") {
        auto spec = parse_query("SELECT COUNT(status) AS cnt FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Count == spec->aggregations[0].kind);
        REQUIRE("cnt" == spec->aggregations[0].alias);
    }

    SECTION("COUNT(col) with dotted path") {
        auto spec = parse_query("SELECT COUNT(meta.region) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Count == spec->aggregations[0].kind);
        REQUIRE("meta.region" == spec->aggregations[0].column);
    }

    SECTION("Multiple COUNT(col) aggregates") {
        auto spec = parse_query("SELECT COUNT(x) AS cx, COUNT(y) AS cy FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(2 == spec->aggregations.size());
        REQUIRE(AggregateKind::Count == spec->aggregations[0].kind);
        REQUIRE("x" == spec->aggregations[0].column);
        REQUIRE("cx" == spec->aggregations[0].alias);
        REQUIRE(AggregateKind::Count == spec->aggregations[1].kind);
        REQUIRE("y" == spec->aggregations[1].column);
        REQUIRE("cy" == spec->aggregations[1].alias);
    }

    SECTION("Aggregate in WHERE clause produces nullopt") {
        auto spec = parse_query("SELECT * FROM t WHERE COUNT(*) > 10");
        REQUIRE(false == spec.has_value());
    }

    SECTION("Mixed aggregate and column SELECT produces nullopt") {
        auto spec = parse_query("SELECT COUNT(*), level FROM logs");
        REQUIRE(false == spec.has_value());
    }

    SECTION("Aggregate keywords are case-insensitive") {
        auto query = GENERATE(
                "SELECT count(*) FROM logs",
                "SELECT Count(*) FROM logs",
                "SELECT COUNT(*) FROM logs"
        );
        auto spec = parse_query(query);
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::CountStar == spec->aggregations[0].kind);
    }

    SECTION("COUNT/MIN/MAX as column name backward compat (non-reserved)") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE count = 5");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
        int64_t value{};
        REQUIRE(get_operand_int(filter, value));
        REQUIRE(5 == value);
    }

    SECTION("MIN as column name backward compat") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE min = 1");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("MAX as column name backward compat") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE max = 1");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("SUM as column name backward compat") {
        auto spec = parse_query("SELECT sum FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
    }

    SECTION("AVG as column name backward compat") {
        auto spec = parse_query("SELECT avg FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
    }

    SECTION("ARBITRARY(col) extracts column name") {
        auto spec = parse_query("SELECT ARBITRARY(level) FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Arbitrary == spec->aggregations[0].kind);
        REQUIRE("level" == spec->aggregations[0].column);
        REQUIRE("arbitrary(level)" == spec->aggregations[0].alias);
    }

    SECTION("ANY_VALUE(col) is alias for ARBITRARY") {
        auto spec = parse_query("SELECT ANY_VALUE(level) AS lv FROM logs");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Arbitrary == spec->aggregations[0].kind);
        REQUIRE("level" == spec->aggregations[0].column);
        REQUIRE("lv" == spec->aggregations[0].alias);
    }

    SECTION("ARBITRARY/ANY_VALUE are case-insensitive") {
        auto query = GENERATE(
                "SELECT arbitrary(x) FROM t",
                "SELECT Arbitrary(x) FROM t",
                "SELECT any_value(x) FROM t",
                "SELECT Any_Value(x) FROM t"
        );
        auto spec = parse_query(query);
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::Arbitrary == spec->aggregations[0].kind);
    }

    SECTION("ARBITRARY as column name backward compat") {
        auto filter = parse_as_filter("SELECT * FROM t WHERE arbitrary = 1");
        REQUIRE(nullptr != filter);
        REQUIRE(FilterOperation::EQ == filter->get_operation());
    }

    SECTION("LIMIT with aggregate query — LIMIT is parsed but should be ignored at call site") {
        // The parser still parses LIMIT, but clp-s.cpp passes std::nullopt to search_archive()
        // for aggregate queries. Here we verify the parser accepts the syntax.
        auto spec = parse_query("SELECT COUNT(*) FROM logs LIMIT 10");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->aggregations.size());
        REQUIRE(AggregateKind::CountStar == spec->aggregations[0].kind);
        REQUIRE(spec->limit.has_value());
        REQUIRE(10 == spec->limit.value());
    }

    SECTION("SELECT DISTINCT is rejected") {
        auto spec = parse_query("SELECT DISTINCT level FROM logs");
        REQUIRE(false == spec.has_value());
    }

    SECTION("SELECT ALL is accepted (ALL is a no-op)") {
        auto spec = parse_query("SELECT ALL * FROM logs WHERE x = 1");
        REQUIRE(spec.has_value());
    }

    SECTION("ARBITRARY/ANY_VALUE in WHERE clause produces nullopt") {
        auto spec = parse_query("SELECT * FROM t WHERE ARBITRARY(x) > 10");
        REQUIRE(false == spec.has_value());
        spec = parse_query("SELECT * FROM t WHERE ANY_VALUE(x) > 10");
        REQUIRE(false == spec.has_value());
    }
}

TEST_CASE("LocalAggregateOutputHandler", "[SQL]") {
    SECTION("COUNT(*) counts write calls") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::CountStar, "", "cnt"}
        };
        LocalAggregateOutputHandler handler(specs);
        REQUIRE(false == handler.should_marshal_records());

        handler.write("");
        handler.write("");
        handler.write("");

        // Capture stdout
        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        REQUIRE("{\"cnt\":3}\n" == captured.str());
    }

    SECTION("MIN/MAX extracts values from JSON") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Min, "val", "min_val"},
                {AggregateKind::Max, "val", "max_val"}
        };
        LocalAggregateOutputHandler handler(specs);
        REQUIRE(true == handler.should_marshal_records());

        handler.write("{\"val\": 10}\n");
        handler.write("{\"val\": 3}\n");
        handler.write("{\"val\": 25}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(3.0 == result["min_val"].get<double>());
        REQUIRE(25.0 == result["max_val"].get<double>());
    }

    SECTION("SUM accumulates values") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Sum, "bytes", "total"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"bytes\": 100}\n");
        handler.write("{\"bytes\": 200}\n");
        handler.write("{\"bytes\": 50}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(350.0 == result["total"].get<double>());
    }

    SECTION("AVG computes average") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Avg, "latency", "avg_lat"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"latency\": 10}\n");
        handler.write("{\"latency\": 20}\n");
        handler.write("{\"latency\": 30}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(20.0 == result["avg_lat"].get<double>());
    }

    SECTION("AVG with no matching values outputs null") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Avg, "missing", "avg_val"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"other\": 10}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(result["avg_val"].is_null());
    }

    SECTION("MIN/MAX/SUM with no matching values outputs null") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Min, "missing", "min_val"},
                {AggregateKind::Max, "missing", "max_val"},
                {AggregateKind::Sum, "missing", "sum_val"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"other\": 10}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(result["min_val"].is_null());
        REQUIRE(result["max_val"].is_null());
        REQUIRE(result["sum_val"].is_null());
    }

    SECTION("Nested column path extraction") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Min, "meta.latency", "min_lat"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"meta\": {\"latency\": 42}}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(42.0 == result["min_lat"].get<double>());
    }

    SECTION("COUNT(*) with zero matches") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::CountStar, "", "cnt"}
        };
        LocalAggregateOutputHandler handler(specs);

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(0 == result["cnt"].get<int64_t>());
    }

    SECTION("Mixed COUNT and MIN/MAX") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::CountStar, "", "cnt"},
                {AggregateKind::Min, "val", "min_val"}
        };
        LocalAggregateOutputHandler handler(specs);
        // needs_marshal returns true because of MIN
        REQUIRE(true == handler.should_marshal_records());

        handler.write("{\"val\": 5}\n");
        handler.write("{\"val\": 3}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(2 == result["cnt"].get<int64_t>());
        REQUIRE(3.0 == result["min_val"].get<double>());
    }

    SECTION("COUNT(col) counts only records where column exists") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Count, "status", "cnt_status"},
                {AggregateKind::Count, "region", "cnt_region"}
        };
        LocalAggregateOutputHandler handler(specs);
        REQUIRE(true == handler.should_marshal_records());

        handler.write("{\"status\": 200, \"region\": \"us\"}\n");
        handler.write("{\"status\": 500}\n");
        handler.write("{\"region\": \"eu\"}\n");
        handler.write("{\"other\": 1}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(2 == result["cnt_status"].get<int64_t>());
        REQUIRE(2 == result["cnt_region"].get<int64_t>());
    }

    SECTION("COUNT(col) does not count null values") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Count, "val", "cnt"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"val\": 42}\n");
        handler.write("{\"val\": null}\n");
        handler.write("{\"val\": \"hello\"}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        // null should not be counted
        REQUIRE(2 == result["cnt"].get<int64_t>());
    }

    SECTION("COUNT(col) with nested path") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Count, "meta.region", "cnt"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"meta\": {\"region\": \"us\"}}\n");
        handler.write("{\"meta\": {\"other\": 1}}\n");
        handler.write("{\"meta\": {\"region\": \"eu\"}}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(2 == result["cnt"].get<int64_t>());
    }

    SECTION("COUNT(col) with zero matches") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Count, "missing", "cnt"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"other\": 1}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(0 == result["cnt"].get<int64_t>());
    }

    SECTION("MIN/MAX on string values") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Min, "level", "min_lv"},
                {AggregateKind::Max, "level", "max_lv"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"level\": \"error\"}\n");
        handler.write("{\"level\": \"warn\"}\n");
        handler.write("{\"level\": \"debug\"}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE("debug" == result["min_lv"].get<std::string>());
        REQUIRE("warn" == result["max_lv"].get<std::string>());
    }

    SECTION("MIN/MAX prefers numeric over string when both present") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Min, "val", "min_val"},
                {AggregateKind::Max, "val", "max_val"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"val\": 10}\n");
        handler.write("{\"val\": \"hello\"}\n");
        handler.write("{\"val\": 3}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        // Numeric values take precedence
        REQUIRE(3.0 == result["min_val"].get<double>());
        REQUIRE(10.0 == result["max_val"].get<double>());
    }

    SECTION("ARBITRARY captures first non-null value") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Arbitrary, "level", "lv"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"level\": \"error\"}\n");
        handler.write("{\"level\": \"warn\"}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE("error" == result["lv"].get<std::string>());
    }

    SECTION("ARBITRARY captures numeric value") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Arbitrary, "status", "s"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"status\": 200}\n");
        handler.write("{\"status\": 500}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(200 == result["s"].get<int64_t>());
    }

    SECTION("ARBITRARY with no matching values outputs null") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Arbitrary, "missing", "val"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"other\": 1}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(result["val"].is_null());
    }

    SECTION("ARBITRARY skips null values") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Arbitrary, "val", "v"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"val\": null}\n");
        handler.write("{\"val\": \"found\"}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE("found" == result["v"].get<std::string>());
    }

    SECTION("SUM/AVG skip non-numeric values") {
        std::vector<AggregateSpec> specs{
                {AggregateKind::Sum, "val", "total"},
                {AggregateKind::Avg, "val", "average"}
        };
        LocalAggregateOutputHandler handler(specs);

        handler.write("{\"val\": 10}\n");
        handler.write("{\"val\": \"not_a_number\"}\n");
        handler.write("{\"val\": 20}\n");

        std::ostringstream captured;
        auto* old_buf = std::cout.rdbuf(captured.rdbuf());
        REQUIRE(ErrorCode::ErrorCodeSuccess == handler.finish());
        std::cout.rdbuf(old_buf);

        auto result = nlohmann::json::parse(captured.str());
        REQUIRE(30.0 == result["total"].get<double>());
        REQUIRE(15.0 == result["average"].get<double>());
    }
}

TEST_CASE("SQL SELECT * mixed with columns", "[SQL]") {
    SECTION("SELECT * alone produces empty select_columns") {
        auto spec = parse_query("SELECT * FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("SELECT *, col produces empty select_columns (superset)") {
        auto spec = parse_query("SELECT *, level FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("SELECT col, * produces empty select_columns (superset)") {
        auto spec = parse_query("SELECT level, * FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("SELECT *, col AS alias preserves alias") {
        auto spec = parse_query("SELECT *, level AS severity FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
        REQUIRE(1 == spec->column_aliases.size());
        REQUIRE("severity" == spec->column_aliases.at("level"));
    }

    SECTION("Multiple * produces empty select_columns") {
        auto spec = parse_query("SELECT *, * FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(spec->select_columns.empty());
    }

    SECTION("SELECT col1, col2 without * preserves columns") {
        auto spec = parse_query("SELECT level, status FROM t WHERE x = 1");
        REQUIRE(spec.has_value());
        REQUIRE(2 == spec->select_columns.size());
        REQUIRE("level" == spec->select_columns[0]);
        REQUIRE("status" == spec->select_columns[1]);
    }
}

TEST_CASE("SQL column aliases", "[SQL]") {
    SECTION("Simple alias") {
        auto spec = parse_query("SELECT level AS severity FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
        REQUIRE("level" == spec->select_columns[0]);
        REQUIRE(1 == spec->column_aliases.size());
        REQUIRE("severity" == spec->column_aliases.at("level"));
    }

    SECTION("Multiple aliases") {
        auto spec = parse_query("SELECT level AS severity, status AS code FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(2 == spec->select_columns.size());
        REQUIRE(2 == spec->column_aliases.size());
        REQUIRE("severity" == spec->column_aliases.at("level"));
        REQUIRE("code" == spec->column_aliases.at("status"));
    }

    SECTION("Mixed aliased and non-aliased columns") {
        auto spec = parse_query("SELECT level AS severity, status FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(2 == spec->select_columns.size());
        REQUIRE(1 == spec->column_aliases.size());
        REQUIRE("severity" == spec->column_aliases.at("level"));
    }

    SECTION("Nested column alias") {
        auto spec = parse_query("SELECT meta.latency AS lat FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
        REQUIRE("meta.latency" == spec->select_columns[0]);
        REQUIRE(1 == spec->column_aliases.size());
        REQUIRE("lat" == spec->column_aliases.at("meta.latency"));
    }

    SECTION("Alias without AS keyword") {
        auto spec = parse_query("SELECT level severity FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(1 == spec->select_columns.size());
        REQUIRE("level" == spec->select_columns[0]);
        REQUIRE(1 == spec->column_aliases.size());
        REQUIRE("severity" == spec->column_aliases.at("level"));
    }

    SECTION("No alias produces empty column_aliases") {
        auto spec = parse_query("SELECT level, status FROM t");
        REQUIRE(spec.has_value());
        REQUIRE(spec->column_aliases.empty());
    }
}

// Helper OutputHandler that captures write() calls for testing RenamingOutputHandler
namespace {
class CapturingOutputHandler : public clp_s::search::OutputHandler {
public:
    CapturingOutputHandler() : OutputHandler(false, true) {}

    void write(
            std::string_view message,
            clp_s::epochtime_t /*timestamp*/,
            std::string_view /*archive_id*/,
            int64_t /*log_event_idx*/
    ) override {
        m_messages.emplace_back(message);
    }

    void write(std::string_view message) override { m_messages.emplace_back(message); }

    [[nodiscard]] auto get_messages() const -> std::vector<std::string> const& {
        return m_messages;
    }

private:
    std::vector<std::string> m_messages;
};
}  // namespace

TEST_CASE("RenamingOutputHandler", "[SQL]") {
    SECTION("Top-level field rename") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"level", "severity"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"level":"ERROR","status":500})");
        REQUIRE(1 == inner_ptr->get_messages().size());
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE("ERROR" == result["severity"].get<std::string>());
        REQUIRE(false == result.contains("level"));
        REQUIRE(500 == result["status"].get<int>());
    }

    SECTION("Nested field rename flattens to top level") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"meta.latency", "lat"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"meta":{"latency":42,"region":"us"},"status":200})");
        REQUIRE(1 == inner_ptr->get_messages().size());
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE(42 == result["lat"].get<int>());
        REQUIRE("us" == result["meta"]["region"].get<std::string>());
        REQUIRE(false == result["meta"].contains("latency"));
        REQUIRE(200 == result["status"].get<int>());
    }

    SECTION("Nested field rename cleans up empty parent objects") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"meta.latency", "lat"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"meta":{"latency":42},"status":200})");
        REQUIRE(1 == inner_ptr->get_messages().size());
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE(42 == result["lat"].get<int>());
        REQUIRE(false == result.contains("meta"));
        REQUIRE(200 == result["status"].get<int>());
    }

    SECTION("Multiple renames") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{
                {"level", "severity"},
                {"status", "code"}
        };
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"level":"ERROR","status":500})");
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE("ERROR" == result["severity"].get<std::string>());
        REQUIRE(500 == result["code"].get<int>());
        REQUIRE(false == result.contains("level"));
        REQUIRE(false == result.contains("status"));
    }

    SECTION("Non-matching alias passes through unchanged") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"nonexistent", "alias"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"level":"ERROR"})");
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE("ERROR" == result["level"].get<std::string>());
        REQUIRE(false == result.contains("alias"));
    }

    SECTION("Deeply nested field rename") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"a.b.c", "flat"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"a":{"b":{"c":99}},"x":1})");
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE(99 == result["flat"].get<int>());
        REQUIRE(false == result.contains("a"));
        REQUIRE(1 == result["x"].get<int>());
    }

    SECTION("Metadata write variant also renames") {
        auto inner = std::make_unique<CapturingOutputHandler>();
        auto* inner_ptr = inner.get();
        std::unordered_map<std::string, std::string> aliases{{"level", "sev"}};
        RenamingOutputHandler handler(std::move(inner), aliases);

        handler.write(R"({"level":"WARN"})", 1234567890, "archive-1", 42);
        auto result = nlohmann::json::parse(inner_ptr->get_messages()[0]);
        REQUIRE("WARN" == result["sev"].get<std::string>());
        REQUIRE(false == result.contains("level"));
    }
}
