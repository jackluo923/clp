#include "ColumnReader.hpp"

#include <cstddef>
#include <cstdint>
#include <string>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

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
#include <clp_s/LogtypeTemplate.hpp>
#include <clp_s/SchemaTree.hpp>
#include <clp_s/Utils.hpp>

namespace clp_s {
auto Int64ColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_values = reader.read_unaligned_span_u64<int64_t>(num_messages);
}

auto Int64ColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_values[cur_message];
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
    m_values = reader.read_unaligned_span_u64<double>(num_messages);
}

auto FormattedFloatColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_values = reader.read_unaligned_span_u64<double>(num_messages);
    m_formats = reader.read_unaligned_span_u64<float_format_t>(num_messages);
}

auto Int64ColumnReader::extract_string_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    buffer.append(std::to_string(m_values[cur_message]));
}

auto DeltaEncodedInt64ColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(std::to_string(get_value_at_idx(cur_message)));
}

auto FloatColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_values[cur_message];
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
    buffer.append(std::to_string(m_values[cur_message]));
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

auto DictionaryFloatColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_var_dict_ids = reader.read_unaligned_span_u64<variable_dictionary_id_t>(num_messages);
}

auto DictionaryFloatColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return std::stod(m_var_dict->get_value(m_var_dict_ids[cur_message]));
}

auto DictionaryFloatColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(m_var_dict->get_value(m_var_dict_ids[cur_message]));
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

auto
ClpStringColumnReader::extract_logtype_value_into_buffer(uint64_t cur_message, std::string& buffer)
        -> void {
    auto const value{m_logtypes[cur_message]};
    auto const logtype_id{ClpStringColumnWriter::get_encoded_log_dict_id(value)};
    auto& entry{m_log_dict->get_entry(logtype_id)};

    if (false == entry.initialized()) {
        entry.decode_log_type();
    }

    append_logtype_template(entry.get_value(), buffer);
}

auto ClpStringColumnReader::extract_escaped_logtype_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer,
        SimdJsonStringEscaper& escaper
) -> void {
    // Unlike a value, a logtype template is always emitted as a JSON string, so it is escaped even
    // for an array, whose template contains the quotes of its elements.
    std::string tmp;
    extract_logtype_value_into_buffer(cur_message, tmp);
    escaper.escape(buffer, tmp);
}

auto ClpStringColumnReader::extract_decomposed_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    auto const value{m_logtypes[cur_message]};
    auto const logtype_id{ClpStringColumnWriter::get_encoded_log_dict_id(value)};
    auto& entry{m_log_dict->get_entry(logtype_id)};
    if (false == entry.initialized()) {
        entry.decode_log_type();
    }
    auto const encoded_vars_offset{ClpStringColumnWriter::get_encoded_offset(value)};
    auto const num_vars{entry.get_num_variables()};

    // `UnalignedMemSpan` performs no bounds checking on `sub_span` or `operator[]`, so a corrupt
    // logtype entry claiming more variables than the column holds would read out of bounds.
    if (encoded_vars_offset + num_vars > m_encoded_vars.size()) {
        SPDLOG_ERROR(
                "ClpStringColumnReader: logtype {} claims {} variables at offset {}, but the "
                "column only holds {} encoded variables.",
                logtype_id,
                num_vars,
                encoded_vars_offset,
                m_encoded_vars.size()
        );
        throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
    }
    auto encoded_vars{m_encoded_vars.sub_span(encoded_vars_offset, num_vars)};

    // Doubling literal '%' keeps it distinct from a placeholder delimiter.
    std::string shape;
    append_logtype_template(entry.get_value(), shape, true);

    // Bucket variables by type in placeholder order so the shape can be reconstructed.
    std::vector<std::string> ints;
    std::vector<std::string> floats;
    std::vector<std::string> strs;
    clp::ir::VariablePlaceholder placeholder{};
    size_t var_ix{0};
    for (size_t pix{0}; pix < entry.get_num_placeholders(); ++pix) {
        entry.get_placeholder_info(pix, placeholder);

        // Every placeholder but `Escape` consumes a variable; a corrupt logtype may declare
        // fewer than it uses.
        if (clp::ir::VariablePlaceholder::Escape != placeholder && var_ix >= num_vars) {
            SPDLOG_ERROR(
                    "ClpStringColumnReader: logtype {} has more variable placeholders than its {} "
                    "declared variables.",
                    logtype_id,
                    num_vars
            );
            throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }

        switch (placeholder) {
            case clp::ir::VariablePlaceholder::Integer:
                ints.emplace_back(std::to_string(encoded_vars[var_ix++]));
                break;
            case clp::ir::VariablePlaceholder::Float: {
                std::string float_str;
                clp::EncodedVariableInterpreter::convert_encoded_float_to_string(
                        encoded_vars[var_ix++],
                        float_str
                );
                floats.emplace_back(std::move(float_str));
                break;
            }
            case clp::ir::VariablePlaceholder::Dictionary: {
                auto const var_dict_id{
                        clp::EncodedVariableInterpreter::decode_var_dict_id(encoded_vars[var_ix++])
                };
                strs.emplace_back(m_var_dict->get_value(var_dict_id));
                break;
            }
            case clp::ir::VariablePlaceholder::Escape:
                break;
            default:
                SPDLOG_ERROR(
                        "ClpStringColumnReader: logtype {} contains unexpected variable "
                        "placeholder 0x{:x}.",
                        logtype_id,
                        static_cast<uint8_t>(placeholder)
                );
                throw OperationFailed(ErrorCodeCorrupt, __FILENAME__, __LINE__);
        }
    }

    // Ints and floats are emitted as unquoted JSON numbers, strs as quoted strings.
    buffer += '{';
    bool first{true};
    auto emit_key{[&](std::string_view key) {
        if (false == first) {
            buffer += ',';
        }
        first = false;
        buffer += '"';
        StringUtils::escape_json_string(buffer, key);
        buffer += "\":";
    }};
    auto emit_quoted{[&](std::string_view v) {
        buffer += '"';
        StringUtils::escape_json_string(buffer, v);
        buffer += '"';
    }};
    auto emit_raw_array{[&](std::string_view key, std::vector<std::string> const& values) {
        if (values.empty()) {
            return;
        }
        emit_key(key);
        buffer += '[';
        for (size_t i{0}; i < values.size(); ++i) {
            if (0 != i) {
                buffer += ',';
            }
            buffer += values[i];
        }
        buffer += ']';
    }};

    emit_key("shape");
    emit_quoted(shape);
    emit_raw_array("int", ints);
    emit_raw_array("float", floats);
    if (false == strs.empty()) {
        emit_key("str");
        buffer += '[';
        for (size_t i{0}; i < strs.size(); ++i) {
            if (0 != i) {
                buffer += ',';
            }
            emit_quoted(strs[i]);
        }
        buffer += ']';
    }
    buffer += '}';
}

auto ClpStringColumnReader::get_encoded_id(uint64_t cur_message) -> int64_t {
    auto value = m_logtypes[cur_message];
    return ClpStringColumnWriter::get_encoded_log_dict_id(value);
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

auto VariableStringColumnReader::load(BufferViewReader& reader, uint64_t num_messages) -> void {
    m_variables = reader.read_unaligned_span_u64<uint64_t>(num_messages);
}

auto VariableStringColumnReader::extract_value(uint64_t cur_message)
        -> std::variant<int64_t, double, std::string, uint8_t> {
    return m_var_dict->get_value(m_variables[cur_message]);
}

auto VariableStringColumnReader::extract_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    buffer.append(m_var_dict->get_value(m_variables[cur_message]));
}

auto VariableStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer
) -> void {
    StringUtils::escape_json_string(buffer, m_var_dict->get_value(m_variables[cur_message]));
}

auto VariableStringColumnReader::extract_escaped_string_value_into_buffer(
        uint64_t cur_message,
        std::string& buffer,
        SimdJsonStringEscaper& escaper
) -> void {
    escaper.escape(buffer, m_var_dict->get_value(m_variables[cur_message]));
}

auto VariableStringColumnReader::get_variable_id(uint64_t cur_message) -> uint64_t {
    return m_variables[cur_message];
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
