#include "SemanticSearchUtils.hpp"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

#include "../../clp/ir/types.hpp"
#include "EmbeddingCache.hpp"
#include "OnnxEmbedder.hpp"

namespace clp_s::search {
std::string clean_logtype_for_embedding(std::string const& logtype) {
    std::string cleaned;
    cleaned.reserve(logtype.size());

    for (size_t i = 0; i < logtype.size(); ++i) {
        auto const c = logtype[i];
        if (static_cast<char>(clp::ir::VariablePlaceholder::Escape) == c) {
            // Skip escape backslash, output the next character as-is
            if (i + 1 < logtype.size()) {
                ++i;
                cleaned += logtype[i];
            }
        } else if (static_cast<char>(clp::ir::VariablePlaceholder::Integer) == c
                   || static_cast<char>(clp::ir::VariablePlaceholder::Dictionary) == c
                   || static_cast<char>(clp::ir::VariablePlaceholder::Float) == c)
        {
            cleaned += "<*>";
        } else {
            cleaned += c;
        }
    }
    return cleaned;
}

double cosine_similarity(std::vector<float> const& vec_a, std::vector<float> const& vec_b) {
    if (vec_a.size() != vec_b.size()) {
        SPDLOG_WARN(
                "cosine_similarity: vector size mismatch ({} vs {})",
                vec_a.size(),
                vec_b.size()
        );
        return 0.0;
    }
    if (vec_a.empty()) {
        return 0.0;
    }

    double dot = 0.0;
    double norm_a = 0.0;
    double norm_b = 0.0;
    for (size_t i = 0; i < vec_a.size(); ++i) {
        dot += static_cast<double>(vec_a[i]) * static_cast<double>(vec_b[i]);
        norm_a += static_cast<double>(vec_a[i]) * static_cast<double>(vec_a[i]);
        norm_b += static_cast<double>(vec_b[i]) * static_cast<double>(vec_b[i]);
    }

    double const denom = std::sqrt(norm_a) * std::sqrt(norm_b);
    if (0.0 == denom) {
        return 0.0;
    }
    return dot / denom;
}

SemanticMatchResult match_logtypes_semantically(
        std::string const& query,
        LogTypeDictionaryReader const& log_dict,
        OnnxEmbedder const& embedder,
        size_t top_k,
        double threshold,
        std::string const& model_name
) {
    if (query.empty() || 0 == top_k) {
        SPDLOG_WARN("Semantic search skipped: empty query or top_k=0");
        return {};
    }

    auto const& entries = log_dict.get_entries();
    if (entries.empty()) {
        return {};
    }

    // Step 1: Collect and clean all logtypes
    std::vector<std::string> cleaned_logtypes;
    cleaned_logtypes.reserve(entries.size());
    for (auto const& entry : entries) {
        cleaned_logtypes.push_back(clean_logtype_for_embedding(entry.get_value()));
    }

    // Step 2: Compute per-logtype hashes
    std::vector<uint64_t> logtype_hashes;
    logtype_hashes.reserve(cleaned_logtypes.size());
    for (auto const& lt : cleaned_logtypes) {
        logtype_hashes.push_back(fnv1a_hash(lt));
    }

    // Step 3: Batch cache lookup
    EmbeddingCache cache(model_name);
    auto logtype_embeddings = cache.try_load_batch(logtype_hashes, embedder.embedding_dim());

    // Step 4: Collect miss indices
    std::vector<size_t> miss_indices;
    for (size_t i = 0; i < logtype_embeddings.size(); ++i) {
        if (logtype_embeddings[i].empty()) {
            miss_indices.push_back(i);
        }
    }

    if (false == miss_indices.empty()) {
        // Step 5a: Embed only the missed logtypes
        std::vector<std::string> missed_logtypes;
        missed_logtypes.reserve(miss_indices.size());
        for (size_t idx : miss_indices) {
            missed_logtypes.push_back(cleaned_logtypes[idx]);
        }

        SPDLOG_INFO(
                "Embedding cache: {}/{} hits, computing {} missed logtype embeddings",
                entries.size() - miss_indices.size(),
                entries.size(),
                miss_indices.size()
        );

        auto const missed_embeddings = embedder.embed(missed_logtypes);
        if (missed_embeddings.size() != miss_indices.size()) {
            throw std::runtime_error("Embedder returned unexpected number of embeddings");
        }

        // Step 5b: Store missed embeddings in cache
        std::vector<uint64_t> missed_hashes;
        missed_hashes.reserve(miss_indices.size());
        for (size_t idx : miss_indices) {
            missed_hashes.push_back(logtype_hashes[idx]);
        }
        cache.store_batch(missed_hashes, missed_embeddings);

        // Step 5c: Merge into result vector
        for (size_t i = 0; i < miss_indices.size(); ++i) {
            logtype_embeddings[miss_indices[i]] = missed_embeddings[i];
        }
    }

    // Step 6: Always embed query fresh
    auto const query_embeddings = embedder.embed({query});
    if (query_embeddings.empty()) {
        throw std::runtime_error("Embedder returned no embedding for query");
    }
    auto const& query_embedding = query_embeddings[0];

    // Step 7: Compute similarity for all logtypes
    std::vector<std::pair<uint64_t, double>> scored;
    scored.reserve(entries.size());
    for (size_t i = 0; i < entries.size(); ++i) {
        double const score = cosine_similarity(query_embedding, logtype_embeddings[i]);
        if (score >= threshold) {
            scored.emplace_back(static_cast<uint64_t>(i), score);
        }
    }

    // Partial sort: only need the top K elements, not a full sort
    size_t const n = std::min(top_k, scored.size());
    auto score_cmp = [](auto const& a, auto const& b) { return a.second > b.second; };
    if (n < scored.size()) {
        std::partial_sort(scored.begin(), scored.begin() + n, scored.end(), score_cmp);
    } else {
        std::sort(scored.begin(), scored.end(), score_cmp);
    }

    SemanticMatchResult result;
    for (size_t i = 0; i < n; ++i) {
        result.matched_logtype_scores[scored[i].first] = scored[i].second;
    }

    SPDLOG_DEBUG(
            "Semantic search matched top {} of {} logtypes (threshold floor {})",
            result.matched_logtype_scores.size(),
            entries.size(),
            threshold
    );
    return result;
}
}  // namespace clp_s::search
