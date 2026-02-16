#include "FieldResolver.hpp"

#include <algorithm>
#include <cstdint>
#include <set>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

#include "SemanticSearchUtils.hpp"

namespace clp_s::search {
namespace {
// NodeType -> human-readable description (ported from Python _NODE_TYPE_DESCRIPTIONS)
std::unordered_map<NodeType, std::string> const cNodeTypeDescriptions{
        {NodeType::ClpString, "text string field with mixed static and variable content"},
        {NodeType::VarString, "short variable string field like identifiers or enum values"},
        {NodeType::Integer, "integer numeric field"},
        {NodeType::Float, "floating point numeric field"},
        {NodeType::DeltaInteger, "delta-encoded integer numeric field"},
        {NodeType::FormattedFloat, "formatted floating point numeric field"},
        {NodeType::DictionaryFloat, "dictionary-encoded floating point numeric field"},
        {NodeType::Boolean, "boolean true/false field"},
        {NodeType::DateString, "date/time string field"},
        {NodeType::Timestamp, "timestamp field recording when an event occurred"},
};

// Field name token -> keyword expansion (ported from Python _FIELD_NAME_KEYWORDS)
std::unordered_map<std::string, std::string> const cFieldNameKeywords{
        {"message", "log message text body content"},
        {"msg", "log message text body content"},
        {"body", "log message text body content"},
        {"error", "error failure exception fault crash"},
        {"err", "error failure exception fault crash"},
        {"exception", "error failure exception traceback stack"},
        {"status", "status code http response state"},
        {"status_code", "status code http response"},
        {"code", "status code error return"},
        {"level", "log level severity debug info warn error fatal"},
        {"severity", "log level severity debug info warn error fatal"},
        {"service", "service application component microservice"},
        {"host", "host hostname server machine ip address"},
        {"hostname", "host hostname server machine ip address"},
        {"ip", "ip address network source destination"},
        {"url", "url path endpoint uri route request"},
        {"path", "url path file route endpoint"},
        {"method", "http method get post put delete request"},
        {"user", "user username account identity principal"},
        {"username", "user username account identity login"},
        {"auth", "authentication authorization login credential token"},
        {"token", "authentication token jwt session credential"},
        {"timestamp", "timestamp time date when occurred"},
        {"time", "timestamp time date when occurred"},
        {"duration", "duration elapsed time latency response"},
        {"latency", "duration elapsed time latency response slow"},
        {"request", "request http incoming call invocation"},
        {"response", "response http outgoing reply result"},
        {"pid", "process id pid worker thread"},
        {"thread", "thread id worker process concurrent"},
        {"trace", "trace id distributed tracing span correlation"},
        {"span", "span id distributed tracing trace"},
};

// Query token -> synonym expansion (ported from Python _QUERY_SYNONYMS)
std::unordered_map<std::string, std::unordered_set<std::string>> const cQuerySynonyms{
        {"auth", {"authentication", "authorization", "login", "credential", "token"}},
        {"authentication", {"auth", "login", "credential", "token"}},
        {"authorization", {"auth", "login", "credential", "token"}},
        {"login", {"auth", "authentication", "credential", "user"}},
        {"error", {"failure", "fault", "crash", "exception", "fatal", "error"}},
        {"errors", {"failure", "fault", "crash", "exception", "fatal", "error"}},
        {"failure", {"error", "fault", "crash", "exception", "fatal"}},
        {"failures", {"error", "fault", "crash", "exception", "fatal"}},
        {"crash", {"error", "failure", "fault", "exception", "fatal"}},
        {"slow", {"latency", "duration", "timeout", "response"}},
        {"timeout", {"latency", "duration", "slow", "response"}},
        {"http", {"status", "code", "request", "response", "url", "method"}},
        {"request", {"http", "url", "method", "path", "endpoint"}},
        {"response", {"http", "status", "code", "latency", "duration"}},
        {"log", {"message", "text", "body", "content"}},
        {"message", {"log", "text", "body", "content"}},
        {"user", {"username", "account", "identity", "principal"}},
        {"server", {"host", "hostname", "machine", "ip"}},
        {"ip", {"address", "host", "network", "source", "destination"}},
};

// Node types that are not searchable leaf fields (containers and null)
bool is_non_searchable_type(NodeType type) {
    return type == NodeType::Object || type == NodeType::Metadata
           || type == NodeType::StructuredArray || type == NodeType::UnstructuredArray
           || type == NodeType::NullValue;
}

/**
 * Tokenizes text into lowercase alphanumeric tokens.
 */
auto tokenize(std::string const& text) -> std::set<std::string> {
    std::set<std::string> tokens;
    std::string current;
    for (char c : text) {
        if (std::isalnum(static_cast<unsigned char>(c))) {
            current += static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
        } else {
            if (false == current.empty()) {
                tokens.insert(current);
                current.clear();
            }
        }
    }
    if (false == current.empty()) {
        tokens.insert(current);
    }
    return tokens;
}

/**
 * Expands a token set with synonyms from cQuerySynonyms.
 */
auto expand_tokens(std::set<std::string> const& tokens) -> std::set<std::string> {
    std::set<std::string> expanded = tokens;
    for (auto const& token : tokens) {
        auto it = cQuerySynonyms.find(token);
        if (it != cQuerySynonyms.end()) {
            for (auto const& synonym : it->second) {
                expanded.insert(synonym);
            }
        }
    }
    return expanded;
}
}  // namespace

auto FieldInfo::rich_description() const -> std::string {
    // Get type description
    std::string type_desc;
    auto it = cNodeTypeDescriptions.find(node_type);
    if (it != cNodeTypeDescriptions.end()) {
        type_desc = it->second;
    } else {
        type_desc = "field";
    }

    // Extract leaf name and split into parts for keyword expansion
    auto const dot_pos = full_path.rfind('.');
    std::string const leaf_name
            = (std::string::npos != dot_pos) ? full_path.substr(dot_pos + 1) : full_path;

    // Split leaf name on '_' and '-' to get name parts
    std::vector<std::string> name_parts;
    std::string part;
    for (char c : leaf_name) {
        if ('_' == c || '-' == c) {
            if (false == part.empty()) {
                // Convert to lowercase
                std::string lower_part;
                for (char pc : part) {
                    lower_part += static_cast<char>(std::tolower(static_cast<unsigned char>(pc)));
                }
                name_parts.push_back(lower_part);
                part.clear();
            }
        } else {
            part += c;
        }
    }
    if (false == part.empty()) {
        std::string lower_part;
        for (char pc : part) {
            lower_part += static_cast<char>(std::tolower(static_cast<unsigned char>(pc)));
        }
        name_parts.push_back(lower_part);
    }

    // Collect keywords from name parts
    std::vector<std::string> keywords;
    for (auto const& name_part : name_parts) {
        auto kw_it = cFieldNameKeywords.find(name_part);
        if (kw_it != cFieldNameKeywords.end()) {
            keywords.push_back(kw_it->second);
        }
    }
    // Also try the full leaf name (e.g. "status_code")
    {
        std::string lower_leaf;
        for (char c : leaf_name) {
            lower_leaf += static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
        }
        auto kw_it = cFieldNameKeywords.find(lower_leaf);
        if (kw_it != cFieldNameKeywords.end()) {
            // Only add if not already present
            bool found = false;
            for (auto const& kw : keywords) {
                if (kw == kw_it->second) {
                    found = true;
                    break;
                }
            }
            if (false == found) {
                keywords.push_back(kw_it->second);
            }
        }
    }

    // Build the description string
    std::string result = full_path + " — " + type_desc;
    if (false == parent_context.empty()) {
        result += ", under " + parent_context;
    }
    if (false == keywords.empty()) {
        result += ". Related: ";
        for (size_t i = 0; i < keywords.size(); ++i) {
            if (i > 0) {
                result += "; ";
            }
            result += keywords[i];
        }
    }
    return result;
}

auto extract_fields_from_schema(SchemaTree const& tree) -> std::vector<FieldInfo> {
    auto const& nodes = tree.get_nodes();
    if (nodes.empty()) {
        return {};
    }

    // Find metadata subtree root(s) to exclude
    std::unordered_set<int32_t> metadata_roots;
    for (auto const& node : nodes) {
        if (-1 == node.get_parent_id() && NodeType::Metadata == node.get_type()) {
            metadata_roots.insert(node.get_id());
        }
    }

    // Check if a node is under the metadata subtree
    auto is_under_metadata = [&](int32_t node_id) -> bool {
        int32_t nid = node_id;
        while (nid >= 0) {
            if (metadata_roots.contains(nid)) {
                return true;
            }
            nid = nodes[nid].get_parent_id();
        }
        return false;
    };

    // Build full path for a node by walking parent chain
    auto build_path = [&](int32_t node_id) -> std::string {
        std::vector<std::string> parts;
        int32_t nid = node_id;
        while (nid >= 0) {
            auto const& node = nodes[nid];
            auto key = node.get_key_name();
            if (false == key.empty()) {
                parts.emplace_back(key);
            }
            nid = node.get_parent_id();
        }
        std::reverse(parts.begin(), parts.end());
        std::string path;
        for (size_t i = 0; i < parts.size(); ++i) {
            if (i > 0) {
                path += ".";
            }
            path += parts[i];
        }
        return path;
    };

    std::vector<FieldInfo> fields;
    for (auto const& node : nodes) {
        // Skip container types
        if (is_non_searchable_type(node.get_type())) {
            continue;
        }

        // Skip metadata subtree
        if (is_under_metadata(node.get_id())) {
            continue;
        }

        auto full_path = build_path(node.get_id());
        if (full_path.empty()) {
            continue;
        }

        // Get parent context
        std::string parent_context;
        if (node.get_parent_id() >= 0) {
            parent_context = std::string(nodes[node.get_parent_id()].get_key_name());
        }

        fields.push_back(
                FieldInfo{
                        node.get_id(),
                        std::move(full_path),
                        node.get_type(),
                        std::move(parent_context)
                }
        );
    }

    return fields;
}

auto resolve_fields(
        std::string const& query,
        std::vector<FieldInfo> const& fields,
        OnnxEmbedder const& embedder,
        size_t top_k,
        double threshold
) -> std::vector<ResolvedField> {
    if (fields.empty() || query.empty()) {
        return {};
    }

    // Build texts: [query, field0.rich_description(), field1.rich_description(), ...]
    std::vector<std::string> texts;
    texts.reserve(1 + fields.size());
    texts.push_back(query);
    for (auto const& field : fields) {
        texts.push_back(field.rich_description());
    }

    SPDLOG_INFO("Computing field resolution embeddings for query and {} fields", fields.size());

    auto const embeddings = embedder.embed(texts);
    if (embeddings.size() != texts.size()) {
        SPDLOG_WARN("Embedder returned unexpected number of embeddings for field resolution");
        return {};
    }

    auto const& query_embedding = embeddings[0];
    std::vector<ResolvedField> scored;
    scored.reserve(fields.size());
    for (size_t i = 0; i < fields.size(); ++i) {
        double const score = cosine_similarity(query_embedding, embeddings[i + 1]);
        if (score >= threshold) {
            scored.push_back(ResolvedField{fields[i], score});
        }
    }

    // Partial sort: only need the top K elements
    size_t const n = std::min(top_k, scored.size());
    auto score_cmp = [](auto const& a, auto const& b) { return a.similarity > b.similarity; };
    if (n < scored.size()) {
        std::partial_sort(scored.begin(), scored.begin() + n, scored.end(), score_cmp);
    } else {
        std::sort(scored.begin(), scored.end(), score_cmp);
    }
    scored.resize(n);

    SPDLOG_INFO(
            "Field resolution matched top {} of {} fields (threshold {})",
            scored.size(),
            fields.size(),
            threshold
    );
    return scored;
}

auto resolve_fields_by_keywords(
        std::string const& query,
        std::vector<FieldInfo> const& fields,
        size_t top_k
) -> std::vector<ResolvedField> {
    if (fields.empty() || query.empty()) {
        return {};
    }

    auto const query_tokens = expand_tokens(tokenize(query));

    std::vector<ResolvedField> scored;
    for (auto const& field : fields) {
        auto const desc_tokens = tokenize(field.rich_description());
        if (desc_tokens.empty()) {
            continue;
        }

        // Count overlap
        size_t overlap = 0;
        for (auto const& qt : query_tokens) {
            if (desc_tokens.contains(qt)) {
                ++overlap;
            }
        }

        if (overlap > 0) {
            double const sim
                    = static_cast<double>(overlap) / static_cast<double>(desc_tokens.size());
            scored.push_back(ResolvedField{field, sim});
        }
    }

    std::sort(scored.begin(), scored.end(), [](auto const& a, auto const& b) {
        return a.similarity > b.similarity;
    });

    if (scored.size() > top_k) {
        scored.resize(top_k);
    }

    return scored;
}
}  // namespace clp_s::search
