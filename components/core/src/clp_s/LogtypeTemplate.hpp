#ifndef CLP_S_LOGTYPE_TEMPLATE_HPP
#define CLP_S_LOGTYPE_TEMPLATE_HPP

#include <string>
#include <string_view>

#include <clp/ir/types.hpp>

namespace clp_s {
/**
 * Appends a rendering of a logtype to a buffer, with encoded variable placeholders shown as
 * `%int%`, `%float%` and `%str%`. An escape byte is consumed and the byte after it emitted
 * literally, mirroring `LogTypeDictionaryEntry::decode_log_type`.
 * @param logtype
 * @param buffer
 * @param escape_percent Whether to double a literal '%' so it can not be mistaken for a
 * placeholder delimiter. Disable for embedding input, which does not need to be parseable.
 */
inline auto
append_logtype_template(std::string_view logtype, std::string& buffer, bool escape_percent = true)
        -> void {
    for (size_t i{0}; i < logtype.size(); ++i) {
        auto const c{logtype[i]};
        if (static_cast<char>(clp::ir::VariablePlaceholder::Escape) == c) {
            if (i + 1 < logtype.size()) {
                buffer.push_back(logtype[++i]);
            }
        } else if (static_cast<char>(clp::ir::VariablePlaceholder::Integer) == c) {
            buffer.append("%int%");
        } else if (static_cast<char>(clp::ir::VariablePlaceholder::Float) == c) {
            buffer.append("%float%");
        } else if (static_cast<char>(clp::ir::VariablePlaceholder::Dictionary) == c) {
            buffer.append("%str%");
        } else {
            buffer.push_back(c);
            if (escape_percent && '%' == c) {
                buffer.push_back(c);
            }
        }
    }
}

/**
 * Renders a logtype to a string. See `append_logtype_template`.
 * @param logtype
 * @param escape_percent
 * @return The rendered logtype.
 */
[[nodiscard]] inline auto
render_logtype_template(std::string_view logtype, bool escape_percent = true) -> std::string {
    std::string result;
    result.reserve(logtype.size());
    append_logtype_template(logtype, result, escape_percent);
    return result;
}
}  // namespace clp_s

#endif  // CLP_S_LOGTYPE_TEMPLATE_HPP
