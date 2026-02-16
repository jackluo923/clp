#ifndef CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP
#define CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP

#include <cstddef>
#include <memory>
#include <optional>
#include <string>

#include "ast/Expression.hpp"

namespace clp_s::search::nl {
/**
 * Parses a natural language query string into a search expression tree.
 *
 * Extracts:
 * 1. Field:value pairs (e.g., "status_code:403", field:"quoted value")
 * 2. Implicit field-number pairs (e.g., "status_code 403" where field name contains '_' or '.')
 * 3. Remaining text becomes a semantic() search expression with wildcard column
 *
 * All extracted sub-expressions are combined with AND.
 *
 * If the query appears to contain temporal phrases (e.g., "last 2 hours"), a warning is logged
 * suggesting the use of --tge/--tle flags instead.
 *
 * @param query The natural language query string
 * @param semantic_top_k Optional top-K for semantic nearest neighbor search
 * @return The expression tree, or nullptr if nothing could be parsed
 */
auto parse_natural_language(
        std::string const& query,
        std::optional<size_t> semantic_top_k = std::nullopt
) -> std::shared_ptr<ast::Expression>;
}  // namespace clp_s::search::nl

#endif  // CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP
