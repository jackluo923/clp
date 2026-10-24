#ifndef CLP_S_RECORDLAYOUTCACHE_HPP
#define CLP_S_RECORDLAYOUTCACHE_HPP

#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <string_view>
#include <vector>

#include "ParsedMessage.hpp"
#include "SchemaTree.hpp"

namespace clp_s {
/** Bounded, archive-local scratch for recurring JSON traversal and column layouts. */
class RecordLayoutCache {
public:
    static constexpr size_t cMaxLayouts{8};
    static constexpr size_t cMaxLayoutBytes{8192};

    struct Layout {
        std::vector<int32_t> path;
        std::vector<int32_t> ordered_nodes;
        std::vector<int32_t> unordered_schema;
        std::vector<int32_t> value_nodes;
        std::vector<size_t> value_order;
        size_t unordered_value_count{};
        int32_t schema_id{};
    };

    auto begin_record() -> void;

    auto end_record() -> void;

    auto clear() -> void;

    /** Validates cached IDs against the actual parent, key and type, and preserves node counts. */
    auto add_node(SchemaTree& tree, int32_t parent, NodeType type, std::string_view key) -> int32_t;

    /**
     * Requires the entire traversal, ordered fields, unordered schema (including string shape
     * IDs), and value layout to match. A prefix match alone never selects an ERT.
     */
    auto find(
            std::span<int32_t const> ordered_nodes,
            std::span<int32_t const> unordered_schema,
            ParsedMessage const& message,
            size_t unordered_value_count
    ) -> Layout const*;

    /** Stores only bounded layouts without duplicate ordered values. */
    auto remember(
            int32_t schema_id,
            std::span<int32_t const> ordered_nodes,
            std::span<int32_t const> unordered_schema,
            ParsedMessage const& message,
            size_t unordered_value_count
    ) -> void;

private:
    static constexpr size_t cMissLimit{2 * cMaxLayouts};
    static constexpr size_t cCooldownRecords{64};

    std::vector<Layout> m_layouts;
    std::vector<int32_t> m_path;
    std::optional<size_t> m_candidate;
    bool m_recording{false};
    bool m_cacheable{false};
    bool m_matched{false};
    size_t m_consecutive_misses{};
    size_t m_records_to_skip{};
};
}  // namespace clp_s
#endif  // CLP_S_RECORDLAYOUTCACHE_HPP
