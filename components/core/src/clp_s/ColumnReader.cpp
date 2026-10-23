#include "ColumnReader.hpp"

#include <bit>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <string>
#include <variant>

#include <fmt/format.h>
#include <spdlog/spdlog.h>
#include <ystdlib/error_handling/Result.hpp>

#include <clp/EncodedVariableInterpreter.hpp>
#include <clp/ir/types.hpp>
#include <clp/LogTypeDictionaryEntryReq.hpp>
#include <clp/type_utils.hpp>
#include <clp_s/BufferViewReader.hpp>
#include <clp_s/ColumnWriter.hpp>
#include <clp_s/Defs.hpp>
#include <clp_s/FloatFormatEncoding.hpp>
#include <clp_s/SchemaTree.hpp>
#include <clp_s/Utils.hpp>

namespace clp_s {
auto Int64ColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_encoded.reset();
    m_values = reader.read_unaligned_span_u64<int64_t>(num_messages);
}

auto Int64ColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_encoded ? std::bit_cast<int64_t>(m_encoded->bits_at(cur_message))
                     : m_values[cur_message];
}

auto DeltaEncodedInt64ColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_values = reader.read_unaligned_span_u64<int64_t>(num_messages);
    if (num_messages > 0) {
        m_cur_idx = 0;
        m_cur_value = m_values[0];
    }
}

auto DeltaEncodedInt64ColumnReader::get_value_at_idx(size_t idx) -> int64_t {
    if (m_cur_idx == idx) {
        return m_cur_value;
    }
    if (idx > m_cur_idx) {
        for (; m_cur_idx < idx; ++m_cur_idx) {
            m_cur_value += m_values[m_cur_idx + 1];
        }
        return m_cur_value;
    }
    for (; m_cur_idx > idx; --m_cur_idx) {
        m_cur_value -= m_values[m_cur_idx];
    }
    return m_cur_value;
}

auto DeltaEncodedInt64ColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return get_value_at_idx(cur_message);
}

auto FloatColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_encoded.reset();
    m_values = reader.read_unaligned_span_u64<double>(num_messages);
}

auto FormattedFloatColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_values = reader.read_unaligned_span_u64<double>(num_messages);
    m_formats = reader.read_unaligned_span_u64<float_format_t>(num_messages);
}

auto Int64ColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    buffer.append(
            std::to_string(
                    m_encoded ? std::bit_cast<int64_t>(m_encoded->bits_at(cur_message))
                              : m_values[cur_message]
            )
    );
}

auto DeltaEncodedInt64ColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(std::to_string(get_value_at_idx(cur_message)));
}

auto FloatColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_encoded ? std::bit_cast<double>(m_encoded->bits_at(cur_message))
                     : m_values[cur_message];
}

auto FormattedFloatColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_values[cur_message];
}

auto BooleanColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_values = reader.read_unaligned_span_u64<uint8_t>(num_messages);
}

auto FloatColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    buffer.append(
            std::to_string(
                    m_encoded ? std::bit_cast<double>(m_encoded->bits_at(cur_message))
                              : m_values[cur_message]
            )
    );
}

auto FormattedFloatColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(restore_encoded_float(m_values[cur_message], m_formats[cur_message]).value());
}

auto BooleanColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_values[cur_message];
}

auto DictionaryIdColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_encoded_values.reset();
    m_validate_ids = false;
    m_variables = reader.read_unaligned_span_u64<uint64_t>(num_messages);
}

auto DictionaryIdColumnReader::validate_ids() -> void {
    if (!m_validate_ids) {
        return;
    }
    auto const size{m_var_dict->get_entries().size()};
    if (m_encoded_values) {
        m_encoded_values->validate_unsigned_range(size);
    } else {
        for (size_t i{0}; i < m_variables.size(); ++i) {
            if (m_variables[i] >= size) {
                throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
            }
        }
    }
    m_validate_ids = false;
}

auto DictionaryIdColumnReader::get_variable_id(uint64_t row) -> uint64_t {
    validate_ids();
    return m_encoded_values ? m_encoded_values->bits_at(row) : m_variables[row];
}

auto DictionaryIdColumnReader::maximum_variable_id() const -> std::optional<uint64_t> {
    if (m_encoded_values) {
        return m_encoded_values->maximum_unsigned();
    }
    if (m_variables.size() == 0) {
        return std::nullopt;
    }
    uint64_t maximum{};
    for (size_t i{0}; i < m_variables.size(); ++i) {
        maximum = std::max(maximum, m_variables[i]);
    }
    return maximum;
}

auto DictionaryFloatColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return value_for_id(get_variable_id(cur_message));
}

auto DictionaryFloatColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(m_var_dict->get_value(get_variable_id(cur_message)));
}

auto ClpStringColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_logtypes = reader.read_unaligned_span_u64<uint64_t>(num_messages);
    auto const encoded_vars_length{reader.read_value<uint64_t>()};
    m_encoded_vars = reader.read_unaligned_span_u64<int64_t>(encoded_vars_length);
}

auto
BooleanColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    buffer.append(0 == m_values[cur_message] ? "false" : "true");
}

auto ClpStringColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    std::string message;
    extract_string_value_into_buffer(cur_message, message);
    return message;
}

auto
ClpStringColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    auto const value{m_logtypes[cur_message]};
    auto const logtype_id{ClpStringColumnWriter::get_encoded_log_dict_id(value)};
    auto& entry{m_log_dict->get_entry(logtype_id)};

    if (false == entry.initialized()) {
        entry.decode_log_type();
    }

    auto const encoded_vars_offset{ClpStringColumnWriter::get_encoded_offset(value)};
    auto encoded_vars{m_encoded_vars.sub_span(encoded_vars_offset, entry.get_num_variables())};

    clp::EncodedVariableInterpreter::decode_variables_into_message(
            entry,
            *m_var_dict,
            encoded_vars,
            buffer
    );
}

auto ClpStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    if (false == m_is_array) {
        // TODO: escape while decoding instead of after.
        std::string tmp;
        extract_string_value_into_buffer(cur_message, tmp);
        StringUtils::escape_json_string(buffer, tmp);
    } else {
        extract_string_value_into_buffer(cur_message, buffer);
    }
}

auto ClpStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer,
        SimdJsonStringEscaper& escaper
) -> void {
    if (false == m_is_array) {
        std::string tmp;
        extract_string_value_into_buffer(cur_message, tmp);
        escaper.escape(buffer, tmp);
    } else {
        extract_string_value_into_buffer(cur_message, buffer);
    }
}

auto ClpStringColumnReader::get_encoded_id(uint64_t cur_message) -> int64_t {
    auto value = m_logtypes[cur_message];
    return ClpStringColumnWriter::get_encoded_log_dict_id(value);
}

auto ClpStringColumnReader::maximum_variable_id() -> std::optional<uint64_t> {
    std::optional<uint64_t> maximum;
    uint64_t cached_id{std::numeric_limits<uint64_t>::max()};
    size_t variable_count{};
    std::vector<size_t> dictionary_positions;
    for (size_t row{0}; row < m_logtypes.size(); ++row) {
        auto const descriptor{m_logtypes[row]};
        auto const id{ClpStringColumnWriter::get_encoded_log_dict_id(descriptor)};
        if (id != cached_id) {
            auto& entry{m_log_dict->get_entry(id)};
            if (!entry.initialized()) {
                entry.decode_log_type();
            }
            variable_count = entry.get_num_variables();
            dictionary_positions.clear();
            size_t variable{};
            for (size_t i{0}; i < entry.get_num_placeholders(); ++i) {
                clp::ir::VariablePlaceholder kind{};
                entry.get_placeholder_info(i, kind);
                if (kind == clp::ir::VariablePlaceholder::Dictionary) {
                    dictionary_positions.push_back(variable);
                }
                if (kind != clp::ir::VariablePlaceholder::Escape) {
                    ++variable;
                }
            }
            cached_id = id;
        }
        auto const offset{ClpStringColumnWriter::get_encoded_offset(descriptor)};
        if (offset > m_encoded_vars.size() || variable_count > m_encoded_vars.size() - offset) {
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
        for (auto const position : dictionary_positions) {
            auto const variable_id{clp::EncodedVariableInterpreter::decode_var_dict_id(
                    m_encoded_vars[offset + position]
            )};
            maximum = std::max(maximum.value_or(0), variable_id);
        }
    }
    return maximum;
}

auto ClpStringColumnReader::get_encoded_vars(uint64_t cur_message) -> UnalignedMemSpan<int64_t> {
    auto value = m_logtypes[cur_message];
    auto logtype_id = ClpStringColumnWriter::get_encoded_log_dict_id(value);
    auto& entry = m_log_dict->get_entry(logtype_id);

    // It should be initialized before because we are searching on this field
    if (false == entry.initialized()) {
        entry.decode_log_type();
    }

    auto encoded_vars_offset{ClpStringColumnWriter::get_encoded_offset(value)};

    return m_encoded_vars.sub_span(encoded_vars_offset, entry.get_num_variables());
}

auto VariableStringColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_var_dict->get_value(get_variable_id(cur_message));
}

auto VariableStringColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(m_var_dict->get_value(get_variable_id(cur_message)));
}

auto VariableStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    StringUtils::escape_json_string(buffer, m_var_dict->get_value(get_variable_id(cur_message)));
}

auto VariableStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer,
        SimdJsonStringEscaper& escaper
) -> void {
    escaper.escape(buffer, m_var_dict->get_value(get_variable_id(cur_message)));
}

auto DeprecatedDateStringColumnReader::load(BufferViewReader& reader, uint64_t num_messages)
        -> void {
    m_timestamps = reader.read_unaligned_span_u64<int64_t>(num_messages);
    m_timestamp_encodings = reader.read_unaligned_span_u64<int64_t>(num_messages);
}

auto DeprecatedDateStringColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_timestamp_dict->get_deprecated_timestamp_string_encoding(
            m_timestamps[cur_message],
            m_timestamp_encodings[cur_message]
    );
}

auto DeprecatedDateStringColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(m_timestamp_dict->get_deprecated_timestamp_string_encoding(
            m_timestamps[cur_message],
            m_timestamp_encodings[cur_message]
    ));
}

auto DeprecatedDateStringColumnReader::get_encoded_time(uint64_t cur_message) -> epochtime_t {
    return m_timestamps[cur_message];
}

auto TimestampColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_timestamps.load(reader, num_messages);
    m_timestamp_encodings = reader.read_unaligned_span_u64<uint64_t>(num_messages);
}

auto TimestampColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    std::string ret;
    m_timestamp_dict->append_timestamp_to_buffer(
            m_timestamps.get_value_at_idx(cur_message),
            m_timestamp_encodings[cur_message],
            ret
    );
    return ret;
}

auto
TimestampColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    m_timestamp_dict->append_timestamp_to_buffer(
            m_timestamps.get_value_at_idx(cur_message),
            m_timestamp_encodings[cur_message],
            buffer
    );
}

auto TimestampColumnReader::get_encoded_time(uint64_t cur_message) -> epochtime_t {
    return m_timestamps.get_value_at_idx(cur_message);
}
}  // namespace clp_s
