#include "SchemaWriter.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <map>
#include <span>
#include <stdexcept>
#include <utility>
#include <vector>

#include "AdaptiveColumn.hpp"
#include "ColumnValueFilter.hpp"

namespace clp_s {
void SchemaWriter::append_column(int32_t node_id, std::unique_ptr<BaseColumnWriter> column_writer) {
    m_total_uncompressed_size += column_writer->get_total_header_size();
    m_columns.emplace_back(std::move(column_writer));
    m_column_node_ids.push_back(node_id);
}

size_t SchemaWriter::append_message(ParsedMessage& message) {
    message.sort_content();
    return append_message(message, {});
}

auto SchemaWriter::append_message(ParsedMessage& message, std::span<size_t const> value_order)
        -> size_t {
    size_t count{};
    size_t total_size{};
    auto& values{message.get_content()};
    for (size_t i{0}; i < values.size(); ++i) {
        auto const position{value_order.empty() ? i : value_order[i]};
        total_size += m_columns[count]->add_value(values[position].second);
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

auto SchemaWriter::has_adaptive_columns(SchemaTree const& tree, bool dictionary) const -> bool {
    return std::ranges::any_of(m_column_node_ids, [&](auto id) {
        auto const type{tree.get_node(id).get_type()};
        return type == NodeType::Integer || type == NodeType::Float
               || (dictionary
                   && (type == NodeType::VarString || type == NodeType::DictionaryFloat));
    });
}

auto SchemaWriter::store_separate(
        FileWriter& file,
        ZSTD_CCtx& context,
        CompressionBuffers& buffers,
        int level,
        SchemaTree const& tree,
        bool adaptive,
        bool adaptive_dictionary
) -> std::vector<ColumnFrame> {
    std::vector<ColumnFrame> frames;
    auto& raw{buffers.raw};
    auto& compressed{buffers.compressed};
    frames.reserve(m_columns.size());
    for (size_t i{0}; i < m_columns.size(); ++i) {
        raw.clear();
        m_columns[i]->store(raw);
        auto const type{tree.get_node(m_column_node_ids[i]).get_type()};
        if (raw.empty()) {
            frames.push_back({0, 0});
            continue;
        }
        if ((adaptive && (type == NodeType::Integer || type == NodeType::Float))
            || (adaptive_dictionary
                && (type == NodeType::VarString || type == NodeType::DictionaryFloat)))
        {
            auto const& frame{adaptive_column::encode(
                    raw,
                    type == NodeType::Float ? adaptive_column::Type::Float
                                            : adaptive_column::Type::Integer,
                    context,
                    level,
                    buffers.adaptive
            )};
            file.write(frame.compressed.data(), frame.compressed.size());
            frames.push_back({frame.uncompressed_size, frame.compressed.size()});
        } else {
            compressed.resize(ZSTD_compressBound(raw.size()));
            auto const size{ZSTD_compressCCtx(
                    &context,
                    compressed.data(),
                    compressed.size(),
                    raw.data(),
                    raw.size(),
                    level
            )};
            if (ZSTD_isError(size)) {
                throw std::runtime_error{ZSTD_getErrorName(size)};
            }
            file.write(compressed.data(), size);
            frames.push_back({raw.size(), size});
        }
    }
    return frames;
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
