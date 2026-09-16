#include "SchemaWriter.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <map>
#include <span>
#include <stdexcept>
#include <utility>
#include <vector>

#include "ColumnValueFilter.hpp"

namespace clp_s {
void SchemaWriter::append_column(int32_t node_id, std::unique_ptr<BaseColumnWriter> column_writer) {
    m_total_uncompressed_size += column_writer->get_total_header_size();
    m_columns.emplace_back(std::move(column_writer));
    m_column_node_ids.push_back(node_id);
}

size_t SchemaWriter::append_message(ParsedMessage& message) {
    int count{};
    size_t total_size{};
    for (auto& i : message.get_content()) {
        total_size += m_columns[count]->add_value(i.second);
        ++count;
    }

    for (auto& i : message.get_unordered_content()) {
        total_size += m_columns[count]->add_value(i);
        ++count;
    }

    m_num_messages++;
    m_total_uncompressed_size += total_size;
    return total_size;
}

void SchemaWriter::store(ZstdCompressor& compressor) {
    for (auto& writer : m_columns) {
        writer->store(compressor);
    }
}

auto SchemaWriter::build_value_filters() const
        -> std::vector<std::pair<int32_t, ColumnValueFilter>> {
    std::map<int32_t, std::vector<int64_t>> keys_by_node;
    for (size_t i{0}; i < m_columns.size(); ++i) {
        auto& keys{keys_by_node[m_column_node_ids[i]]};
        if (false == m_columns[i]->append_value_filter_keys(keys)) {
            keys_by_node.erase(m_column_node_ids[i]);
        }
    }

    std::vector<std::pair<int32_t, ColumnValueFilter>> filters;
    filters.reserve(keys_by_node.size());
    for (auto& [node_id, keys] : keys_by_node) {
        std::ranges::sort(keys);
        auto const [first_duplicate, last]{std::ranges::unique(keys)};
        keys.erase(first_duplicate, last);
        auto filter{ColumnValueFilter::create(std::span<int64_t const>{keys})};
        if (filter.has_error()) {
            throw std::runtime_error{"Failed to build a column value filter"};
        }
        filters.emplace_back(node_id, std::move(filter.value()));
    }
    return filters;
}
}  // namespace clp_s
