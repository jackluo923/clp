#include "SemanticSearchUtils.hpp"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <stdexcept>
#include <string>
#include <utility>

#include <spdlog/spdlog.h>

#include "../../clp/ir/types.hpp"
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
        double threshold
) {
    if (query.empty() || 0 == top_k) {
        SPDLOG_WARN("Semantic search skipped: empty query or top_k=0");
        return {};
    }

    auto const& entries = log_dict.get_entries();
    if (entries.empty()) {
        return {};
    }

    // Build batch: [query, cleaned_logtype_0, cleaned_logtype_1, ...]
    std::vector<std::string> texts;
    texts.reserve(1 + entries.size());
    texts.push_back(query);
    for (auto const& entry : entries) {
        texts.push_back(clean_logtype_for_embedding(entry.get_value()));
    }

    SPDLOG_INFO("Computing embeddings for query and {} logtypes", entries.size());

    auto const embeddings = embedder.embed(texts);
    if (embeddings.size() != texts.size()) {
        throw std::runtime_error("Embedder returned unexpected number of embeddings");
    }

    // Compute similarity for all logtypes
    auto const& query_embedding = embeddings[0];
    std::vector<std::pair<uint64_t, double>> scored;
    scored.reserve(entries.size());
    for (size_t i = 0; i < entries.size(); ++i) {
        double const score = cosine_similarity(query_embedding, embeddings[i + 1]);
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

    SPDLOG_INFO(
            "Semantic search matched top {} of {} logtypes (threshold floor {})",
            result.matched_logtype_scores.size(),
            entries.size(),
            threshold
    );
    return result;
}
}  // namespace clp_s::search
