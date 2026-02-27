#ifndef CLP_S_SEARCH_SEMANTICSEARCHUTILS_HPP
#define CLP_S_SEARCH_SEMANTICSEARCHUTILS_HPP

#include <cstdint>
#include <string>
#include <unordered_map>
#include <vector>

#include "../DictionaryReader.hpp"
#include "OnnxEmbedder.hpp"

namespace clp_s::search {
struct SemanticMatchResult {
    // Map from logtype dictionary entry index to similarity score
    std::unordered_map<uint64_t, double> matched_logtype_scores;
};

/**
 * Cleans a logtype string for embedding by replacing placeholder characters with "<*>" and
 * stripping escape backslashes.
 * @param logtype The raw logtype string
 * @return The cleaned string suitable for embedding
 */
std::string clean_logtype_for_embedding(std::string const& logtype);

/**
 * Computes cosine similarity between two vectors.
 * @param vec_a First vector
 * @param vec_b Second vector
 * @return Cosine similarity in [-1, 1]
 */
double cosine_similarity(std::vector<float> const& vec_a, std::vector<float> const& vec_b);

/**
 * Matches logtypes semantically against a query string using in-process ONNX embeddings.
 * Returns the top_k most similar logtypes, optionally filtered by a minimum threshold.
 * Uses an on-disk SQLite cache in /tmp/clp/{model_name}/ to avoid recomputing
 * logtype embeddings across queries and archives.
 * @param query The query text
 * @param log_dict The logtype dictionary reader (entries must already be read)
 * @param embedder The ONNX embedder for generating text embeddings
 * @param top_k Number of top logtypes to return
 * @param threshold Minimum cosine similarity floor (0.0 to disable)
 * @param model_name Model name used for the cache directory and content hash
 * @return SemanticMatchResult containing matched logtype indices and their scores
 */
SemanticMatchResult match_logtypes_semantically(
        std::string const& query,
        LogTypeDictionaryReader const& log_dict,
        OnnxEmbedder const& embedder,
        size_t top_k,
        double threshold,
        std::string const& model_name
);
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_SEMANTICSEARCHUTILS_HPP
