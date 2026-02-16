#ifndef CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP
#define CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP

#include <cstddef>
#include <memory>
#include <optional>
#include <string>

#include "../SchemaTree.hpp"
#include "ast/Expression.hpp"
#include "OnnxEmbedder.hpp"

namespace clp_s::search::nl {
/**
 * Parses a natural language query string into a search expression tree.
 *
 * When schema_tree is provided, performs schema-aware field resolution:
 * 1. Extracts field:value pairs and implicit field-number pairs
 * 2. Matches extracted fields against the schema tree by full_path or leaf name
 * 3. Resolves remaining text against schema fields using embeddings (if embedder available)
 *    or keyword fallback
 * 4. Generates type-appropriate filters: semantic() for ClpString, wildcard for VarString,
 *    exact match for numeric types, boolean for Boolean
 * 5. Groups: OR within each type group (text, numeric, field-value), AND between groups
 *
 * When schema_tree is null, falls back to the original behavior: wraps leftover text in a
 * wildcard semantic(*, "text") without schema knowledge.
 *
 * If the query appears to contain temporal phrases (e.g., "last 2 hours"), a warning is logged
 * suggesting the use of --tge/--tle flags instead.
 *
 * @param query The natural language query string
 * @param semantic_top_k Optional top-K for semantic nearest neighbor search
 * @param schema_tree Optional schema tree for field resolution (null = schema-blind)
 * @param embedder Optional ONNX embedder for embedding-based field resolution
 * @return The expression tree, or nullptr if nothing could be parsed
 */
[[nodiscard]] auto parse_natural_language(
        std::string const& query,
        std::optional<size_t> semantic_top_k = std::nullopt,
        SchemaTree const* schema_tree = nullptr,
        OnnxEmbedder const* embedder = nullptr
) -> std::shared_ptr<ast::Expression>;
}  // namespace clp_s::search::nl

#endif  // CLP_S_SEARCH_NATURALLANGUAGEPARSER_HPP
