#include "RenamingOutputHandler.hpp"

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <vector>

#include <nlohmann/json.hpp>

#include "../ErrorCode.hpp"
#include "OutputHandler.hpp"

namespace clp_s::search {
RenamingOutputHandler::RenamingOutputHandler(
        std::unique_ptr<OutputHandler> inner,
        std::unordered_map<std::string, std::string> aliases
)
        : OutputHandler(inner->should_output_metadata(), inner->should_marshal_records()),
          m_inner{std::move(inner)},
          m_aliases{std::move(aliases)} {}

void RenamingOutputHandler::write(
        std::string_view message,
        epochtime_t timestamp,
        std::string_view archive_id,
        int64_t log_event_idx
) {
    auto renamed = apply_aliases(message);
    m_inner->write(renamed, timestamp, archive_id, log_event_idx);
}

void RenamingOutputHandler::write(std::string_view message) {
    auto renamed = apply_aliases(message);
    m_inner->write(renamed);
}

auto RenamingOutputHandler::flush() -> ErrorCode {
    return m_inner->flush();
}

auto RenamingOutputHandler::finish() -> ErrorCode {
    return m_inner->finish();
}

auto RenamingOutputHandler::apply_aliases(std::string_view json_str) const -> std::string {
    nlohmann::json doc;
    try {
        doc = nlohmann::json::parse(json_str);
    } catch (...) {
        return std::string{json_str};
    }

    if (false == doc.is_object()) {
        return std::string{json_str};
    }

    bool any_applied = false;
    for (auto const& [col_path, alias] : m_aliases) {
        auto extracted = extract_and_remove(doc, col_path);
        if (extracted.has_value()) {
            doc[alias] = std::move(extracted.value());
            any_applied = true;
        }
    }

    if (false == any_applied) {
        return std::string{json_str};
    }
    return doc.dump();
}

auto RenamingOutputHandler::extract_and_remove(
        nlohmann::json& doc,
        std::string const& dotted_path
) -> std::optional<nlohmann::json> {
    // Split path into segments
    std::vector<std::string> segments;
    size_t start = 0;
    while (start < dotted_path.size()) {
        auto dot_pos = dotted_path.find('.', start);
        if (std::string::npos == dot_pos) {
            segments.emplace_back(dotted_path.substr(start));
            break;
        }
        segments.emplace_back(dotted_path.substr(start, dot_pos - start));
        start = dot_pos + 1;
    }

    if (segments.empty()) {
        return std::nullopt;
    }

    // Walk to the parent of the target key
    nlohmann::json* current = &doc;
    for (size_t i = 0; i + 1 < segments.size(); ++i) {
        auto it = current->find(segments[i]);
        if (it == current->end() || false == it->is_object()) {
            return std::nullopt;
        }
        current = &(*it);
    }

    // Extract the target value
    auto const& last_key = segments.back();
    auto it = current->find(last_key);
    if (it == current->end()) {
        return std::nullopt;
    }
    nlohmann::json extracted = std::move(*it);
    current->erase(it);

    // Clean up empty parent objects by re-walking from root
    for (size_t depth = segments.size() - 1; depth > 0; --depth) {
        nlohmann::json* parent = &doc;
        for (size_t i = 0; i + 1 < depth; ++i) {
            parent = &(*parent)[segments[i]];
        }
        auto child_it = parent->find(segments[depth - 1]);
        if (child_it != parent->end() && child_it->is_object() && child_it->empty()) {
            parent->erase(child_it);
        } else {
            break;
        }
    }

    return extracted;
}
}  // namespace clp_s::search
