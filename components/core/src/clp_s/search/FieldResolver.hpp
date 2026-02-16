#ifndef CLP_S_SEARCH_FIELDRESOLVER_HPP
#define CLP_S_SEARCH_FIELDRESOLVER_HPP

#include <cstdint>
#include <string>
#include <vector>

#include "../SchemaTree.hpp"
#include "OnnxEmbedder.hpp"

namespace clp_s::search {
/**
 * A searchable leaf field from the schema tree.
 */
struct FieldInfo {
    int32_t node_id;
    std::string full_path;  // e.g. "auth.status_code"
    NodeType node_type;  // e.g. NodeType::Integer
    std::string parent_context;  // e.g. "auth"

    /**
     * Formats the field for embedding — includes hierarchy context and keyword expansion.
     * @return A rich text description suitable for embedding or keyword matching
     */
    [[nodiscard]] auto rich_description() const -> std::string;
};

/**
 * A field matched by similarity to the query.
 */
struct ResolvedField {
    FieldInfo field;
    double similarity;
};

/**
 * Extracts all searchable leaf fields from the schema tree.
 * Skips container types (Object, Metadata, StructuredArray, UnstructuredArray)
 * and nodes under the metadata subtree.
 * @param tree The schema tree to extract fields from
 * @return A vector of FieldInfo describing each leaf field
 */
[[nodiscard]] auto extract_fields_from_schema(SchemaTree const& tree) -> std::vector<FieldInfo>;

/**
 * Resolves the most relevant fields for a query using embedding-based cosine similarity.
 * @param query The query text
 * @param fields The field catalog to search
 * @param embedder The ONNX embedder for generating text embeddings
 * @param top_k Maximum number of fields to return
 * @param threshold Minimum cosine similarity threshold
 * @return Fields sorted by descending similarity, filtered by threshold
 */
[[nodiscard]] auto resolve_fields(
        std::string const& query,
        std::vector<FieldInfo> const& fields,
        OnnxEmbedder const& embedder,
        size_t top_k = 5,
        double threshold = 0.3
) -> std::vector<ResolvedField>;

/**
 * Resolves the most relevant fields for a query using keyword token overlap with synonym expansion.
 * Used as fallback when no embedder is available.
 * @param query The query text
 * @param fields The field catalog to search
 * @param top_k Maximum number of fields to return
 * @return Fields sorted by descending keyword overlap score
 */
[[nodiscard]] auto resolve_fields_by_keywords(
        std::string const& query,
        std::vector<FieldInfo> const& fields,
        size_t top_k = 5
) -> std::vector<ResolvedField>;
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_FIELDRESOLVER_HPP
