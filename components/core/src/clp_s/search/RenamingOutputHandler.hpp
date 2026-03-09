#ifndef CLP_S_SEARCH_RENAMINGOUTPUTHANDLER_HPP
#define CLP_S_SEARCH_RENAMINGOUTPUTHANDLER_HPP

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#include <nlohmann/json.hpp>

#include "../ErrorCode.hpp"
#include "OutputHandler.hpp"

namespace clp_s::search {
/**
 * Output handler decorator that renames JSON keys based on SQL AS aliases before delegating
 * to an inner output handler.
 *
 * For top-level fields (e.g., SELECT level AS severity), the key is simply renamed.
 * For nested fields (e.g., SELECT meta.latency AS lat), the value is extracted from its
 * nested location and placed at the top level under the alias name. The original nested
 * key is removed (and empty parent objects are cleaned up).
 */
class RenamingOutputHandler : public OutputHandler {
public:
    /**
     * @param inner The wrapped output handler to delegate to after renaming.
     * @param aliases Map from original dotted column path to alias name.
     */
    RenamingOutputHandler(
            std::unique_ptr<OutputHandler> inner,
            std::unordered_map<std::string, std::string> aliases
    );

    void write(
            std::string_view message,
            epochtime_t timestamp,
            std::string_view archive_id,
            int64_t log_event_idx
    ) override;

    void write(std::string_view message) override;

    [[nodiscard]] auto flush() -> ErrorCode override;

    [[nodiscard]] auto finish() -> ErrorCode override;

private:
    std::unique_ptr<OutputHandler> m_inner;
    std::unordered_map<std::string, std::string> m_aliases;

    /**
     * Applies alias renames to a JSON record string.
     * @param json_str The original JSON record.
     * @return The renamed JSON string (or original if parsing fails or no aliases match).
     */
    [[nodiscard]] auto apply_aliases(std::string_view json_str) const -> std::string;

    /**
     * Extracts a value at a dotted path from a JSON object, removing it from the original
     * location. Empty parent objects are cleaned up after removal.
     * @param doc The JSON object (modified in place).
     * @param dotted_path The dotted path (e.g., "meta.latency").
     * @return The extracted value, or nullopt if the path does not exist.
     */
    static auto extract_and_remove(nlohmann::json& doc, std::string const& dotted_path)
            -> std::optional<nlohmann::json>;
};
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_RENAMINGOUTPUTHANDLER_HPP
