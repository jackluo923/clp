#include "RecordLayoutCache.hpp"

#include <algorithm>
#include <numeric>
#include <utility>

namespace clp_s {
auto RecordLayoutCache::begin_record() -> void {
    m_path.clear();
    m_recording = m_records_to_skip == 0;
    m_cacheable = m_recording;
    m_matched = false;
    if (m_records_to_skip != 0) {
        --m_records_to_skip;
    }
    m_candidate = m_recording && false == m_layouts.empty() ? std::optional{m_layouts.size() - 1}
                                                            : std::nullopt;
}

auto RecordLayoutCache::end_record() -> void {
    if (m_recording) {
        if (m_matched) {
            m_consecutive_misses = 0;
        } else if (++m_consecutive_misses == cMissLimit) {
            // Avoid repeatedly searching and replacing layouts when the working set does not
            // fit. Probe again after a bounded interval so a stable input phase can recover.
            m_records_to_skip = cCooldownRecords;
            m_consecutive_misses = 0;
        }
    }
    m_recording = false;
}

auto RecordLayoutCache::clear() -> void {
    m_layouts.clear();
    m_path.clear();
    m_candidate.reset();
    m_recording = false;
    m_cacheable = false;
    m_matched = false;
    m_consecutive_misses = 0;
    m_records_to_skip = 0;
}

auto
RecordLayoutCache::add_node(SchemaTree& tree, int32_t parent, NodeType type, std::string_view key)
        -> int32_t {
    if (false == m_recording || false == m_cacheable) {
        return tree.add_node(parent, type, key);
    }
    auto const position{m_path.size()};
    if (position == cMaxLayoutBytes / sizeof(int32_t)) {
        m_cacheable = false;
        m_candidate.reset();
        m_path.clear();
        return tree.add_node(parent, type, key);
    }
    auto matches = [&](Layout const& layout) {
        if (position >= layout.path.size()) {
            return false;
        }
        auto const& node{tree.get_node(layout.path[position])};
        return node.get_parent_id() == parent && node.get_type() == type
               && node.get_key_name() == key;
    };
    if (m_candidate && false == matches(m_layouts[*m_candidate])) {
        m_candidate.reset();
        for (size_t i{0}; i < m_layouts.size(); ++i) {
            auto const& layout{m_layouts[i]};
            if (matches(layout) && std::equal(m_path.begin(), m_path.end(), layout.path.begin())) {
                m_candidate = i;
                break;
            }
        }
    }
    int32_t id{};
    if (m_candidate) {
        id = m_layouts[*m_candidate].path[position];
        tree.record_node_occurrence(id);
    } else {
        id = tree.add_node(parent, type, key);
    }
    m_path.push_back(id);
    return id;
}

auto RecordLayoutCache::find(
        std::span<int32_t const> ordered_nodes,
        std::span<int32_t const> unordered_schema,
        ParsedMessage const& message,
        size_t unordered_value_count
) -> Layout const* {
    if (false == m_cacheable || false == m_candidate.has_value()) {
        return nullptr;
    }
    auto const& values{message.get_content()};
    auto matches = [&](Layout const& layout) {
        return layout.path == m_path && std::ranges::equal(layout.ordered_nodes, ordered_nodes)
               && std::ranges::equal(layout.unordered_schema, unordered_schema)
               && layout.unordered_value_count == unordered_value_count
               && layout.value_nodes.size() == values.size()
               && std::equal(
                       layout.value_nodes.begin(),
                       layout.value_nodes.end(),
                       values.begin(),
                       [](auto id, auto const& value) { return id == value.first; }
               );
    };
    // Identical node paths can still have different string shapes or empty-container layouts.
    size_t selected{*m_candidate};
    if (false == matches(m_layouts[selected])) {
        auto const it{std::ranges::find_if(m_layouts, matches)};
        if (it == m_layouts.end()) {
            return nullptr;
        }
        selected = static_cast<size_t>(it - m_layouts.begin());
    }
    if (selected + 1 != m_layouts.size()) {
        std::swap(m_layouts[selected], m_layouts.back());
    }
    m_candidate = m_layouts.size() - 1;
    m_matched = true;
    return &m_layouts.back();
}

auto RecordLayoutCache::remember(
        int32_t schema_id,
        std::span<int32_t const> ordered_nodes,
        std::span<int32_t const> unordered_schema,
        ParsedMessage const& message,
        size_t unordered_value_count
) -> void {
    auto const& values{message.get_content()};
    // Bound each term before summing, including records with very large structured arrays.
    size_t remaining{cMaxLayoutBytes};
    auto fits = [&](size_t count, size_t width) {
        if (count > remaining / width) {
            return false;
        }
        remaining -= count * width;
        return true;
    };
    if (false == m_cacheable || false == fits(m_path.size(), sizeof(int32_t))
        || false == fits(ordered_nodes.size(), sizeof(int32_t))
        || false == fits(unordered_schema.size(), sizeof(int32_t))
        || false == fits(values.size(), sizeof(int32_t) + sizeof(size_t)))
    {
        return;
    }
    Layout layout;
    if (m_layouts.size() == cMaxLayouts) {
        layout = std::move(m_layouts.front());
        m_layouts.erase(m_layouts.begin());
        m_candidate.reset();
    }
    layout.path = m_path;
    layout.ordered_nodes.assign(ordered_nodes.begin(), ordered_nodes.end());
    layout.unordered_schema.assign(unordered_schema.begin(), unordered_schema.end());
    layout.value_nodes.clear();
    layout.value_nodes.reserve(values.size());
    for (auto const& [id, value] : values) {
        layout.value_nodes.push_back(id);
    }
    layout.value_order.resize(values.size());
    std::iota(layout.value_order.begin(), layout.value_order.end(), size_t{0});
    std::ranges::sort(layout.value_order, [&](auto lhs, auto rhs) {
        return layout.value_nodes[lhs] < layout.value_nodes[rhs];
    });
    for (size_t i{1}; i < layout.value_order.size(); ++i) {
        if (layout.value_nodes[layout.value_order[i - 1]]
            == layout.value_nodes[layout.value_order[i]])
        {
            return;
        }
    }
    layout.unordered_value_count = unordered_value_count;
    layout.schema_id = schema_id;
    remaining = cMaxLayoutBytes;
    if (false == fits(layout.path.capacity(), sizeof(int32_t))
        || false == fits(layout.ordered_nodes.capacity(), sizeof(int32_t))
        || false == fits(layout.unordered_schema.capacity(), sizeof(int32_t))
        || false == fits(layout.value_nodes.capacity(), sizeof(int32_t))
        || false == fits(layout.value_order.capacity(), sizeof(size_t)))
    {
        return;
    }
    m_layouts.push_back(std::move(layout));
    m_candidate = m_layouts.size() - 1;
}
}  // namespace clp_s
