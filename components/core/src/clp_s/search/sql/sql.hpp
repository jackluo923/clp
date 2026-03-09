#ifndef CLP_S_SEARCH_SQL_SQL_HPP
#define CLP_S_SEARCH_SQL_SQL_HPP

#include <cstdint>
#include <istream>
#include <memory>
#include <optional>
#include <string>
#include <unordered_map>
#include <vector>

#include "../ast/Expression.hpp"

namespace clp_s::search::sql {
/**
 * The kind of SQL aggregate function.
 */
enum class AggregateKind { CountStar, Count, Min, Max, Sum, Avg, Arbitrary };

/**
 * Specification of a single aggregate function call in a SQL SELECT clause.
 */
struct AggregateSpec {
    AggregateKind kind;
    std::string column;  // empty for CountStar; dotted path for others (e.g. "meta.latency")
    std::string alias;   // from AS alias, or default: "count(*)", "min(col)", etc.
};

/**
 * Structured result of parsing a SQL SELECT query.
 */
struct SqlQuerySpec {
    // The WHERE clause expression (match-all wildcard filter if no WHERE clause)
    std::shared_ptr<clp_s::search::ast::Expression> where_expr;

    // Column names from the SELECT clause (empty means SELECT *)
    std::vector<std::string> select_columns;

    // Table name from the FROM clause (empty if FROM is omitted; currently unused)
    std::string from_table;

    // LIMIT value (nullopt means no limit)
    std::optional<int64_t> limit;

    // Column aliases from AS clauses (original dotted path → alias name)
    // Only populated for non-aggregate queries. For aggregate queries, aliases are stored in
    // AggregateSpec::alias instead.
    std::unordered_map<std::string, std::string> column_aliases;

    // Aggregate functions (non-empty means this is an aggregation query)
    std::vector<AggregateSpec> aggregations;
};

/**
 * Parses a SQL SELECT query from the given stream.
 * @param in Input stream containing a SQL query followed by EOF
 * @return A SqlQuerySpec on success, std::nullopt otherwise
 */
[[nodiscard]] auto parse_sql_query(std::istream& in) -> std::optional<SqlQuerySpec>;

/**
 * Parses a SQL SELECT query and returns only the WHERE clause as a search AST.
 * This is a convenience wrapper around parse_sql_query() that matches the KQL parser interface.
 * @param in Input stream containing a SQL query followed by EOF
 * @return a search AST on success, nullptr otherwise
 */
[[nodiscard]] auto parse_sql_expression(std::istream& in)
        -> std::shared_ptr<clp_s::search::ast::Expression>;
}  // namespace clp_s::search::sql

#endif  // CLP_S_SEARCH_SQL_SQL_HPP
