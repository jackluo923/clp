#include "LocalAggregateOutputHandler.hpp"

#include <algorithm>
#include <cstdint>
#include <iostream>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <nlohmann/json.hpp>
#include <simdjson.h>

#include "../ErrorCode.hpp"
#include "sql/sql.hpp"

namespace clp_s::search {
LocalAggregateOutputHandler::LocalAggregateOutputHandler(
        std::vector<sql::AggregateSpec> specs
)
        : OutputHandler(false, needs_marshal(specs)),
          m_needs_marshal{should_marshal_records()} {
    m_states.reserve(specs.size());
    for (auto& s : specs) {
        m_states.push_back(AggState{std::move(s)});
    }
}

void LocalAggregateOutputHandler::write(
        std::string_view message,
        epochtime_t /*timestamp*/,
        std::string_view /*archive_id*/,
        int64_t /*log_event_idx*/
) {
    // Forward to single-param write() so aggregate counting still works
    // even if this variant is called unexpectedly.
    write(message);
}

void LocalAggregateOutputHandler::write(std::string_view message) {
    // For COUNT(*)-only queries, should_marshal_records() returns false, and message is empty.
    // Skip the padded_string allocation in that case.
    simdjson::padded_string padded;
    if (m_needs_marshal) {
        padded = simdjson::padded_string{message};
    }

    for (auto& state : m_states) {
        switch (state.spec.kind) {
            case sql::AggregateKind::CountStar:
                ++state.count;
                break;
            case sql::AggregateKind::Count: {
                auto doc_result = m_parser.iterate(padded);
                if (doc_result.error()) {
                    break;
                }
                auto doc = std::move(doc_result).value();
                simdjson::ondemand::value val;
                if (walk_path(doc, state.spec.column, val)) {
                    auto is_null_result = val.is_null();
                    if (false == is_null_result.error()
                        && false == is_null_result.value())
                    {
                        ++state.count;
                    }
                }
                break;
            }
            case sql::AggregateKind::Sum:
            case sql::AggregateKind::Avg: {
                auto val = extract_numeric_value(padded, state.spec.column);
                if (false == val.has_value()) {
                    break;
                }
                state.numeric_seen = true;
                ++state.count;
                state.sum += val.value();
                break;
            }
            case sql::AggregateKind::Min:
            case sql::AggregateKind::Max: {
                // MIN/MAX: numeric values take precedence over string values.
                // Once any numeric value is seen, only numeric comparisons are used.
                auto num_val = extract_numeric_value(padded, state.spec.column);
                if (num_val.has_value()) {
                    state.numeric_seen = true;
                    state.numeric_min = std::min(state.numeric_min, num_val.value());
                    state.numeric_max = std::max(state.numeric_max, num_val.value());
                    break;
                }
                auto str_val = extract_string_value(padded, state.spec.column);
                if (str_val.has_value()) {
                    auto const& sv = str_val.value();
                    if (false == state.string_seen) {
                        state.string_min = sv;
                        state.string_max = sv;
                        state.string_seen = true;
                    } else {
                        if (sv < state.string_min) {
                            state.string_min = sv;
                        }
                        if (sv > state.string_max) {
                            state.string_max = sv;
                        }
                    }
                }
                break;
            }
            case sql::AggregateKind::Arbitrary: {
                if (false == state.arbitrary_captured) {
                    auto raw = extract_raw_value(padded, state.spec.column);
                    if (raw.has_value()) {
                        state.arbitrary_value = std::move(raw.value());
                        state.arbitrary_captured = true;
                    }
                }
                break;
            }
        }
    }
}

auto LocalAggregateOutputHandler::finish() -> ErrorCode {
    nlohmann::json result = nlohmann::json::object();
    for (auto const& state : m_states) {
        auto const& alias = state.spec.alias;
        switch (state.spec.kind) {
            case sql::AggregateKind::CountStar:
            case sql::AggregateKind::Count:
                result[alias] = state.count;
                break;
            case sql::AggregateKind::Min:
                if (state.numeric_seen) {
                    result[alias] = state.numeric_min;
                } else if (state.string_seen) {
                    result[alias] = state.string_min;
                } else {
                    result[alias] = nullptr;
                }
                break;
            case sql::AggregateKind::Max:
                if (state.numeric_seen) {
                    result[alias] = state.numeric_max;
                } else if (state.string_seen) {
                    result[alias] = state.string_max;
                } else {
                    result[alias] = nullptr;
                }
                break;
            case sql::AggregateKind::Sum:
                if (state.numeric_seen) {
                    result[alias] = state.sum;
                } else {
                    result[alias] = nullptr;
                }
                break;
            case sql::AggregateKind::Avg:
                if (0 == state.count) {
                    result[alias] = nullptr;
                } else {
                    result[alias] = state.sum / static_cast<double>(state.count);
                }
                break;
            case sql::AggregateKind::Arbitrary:
                if (state.arbitrary_captured) {
                    result[alias] = state.arbitrary_value;
                } else {
                    result[alias] = nullptr;
                }
                break;
        }
    }
    std::cout << result.dump() << "\n";
    return ErrorCode::ErrorCodeSuccess;
}

auto LocalAggregateOutputHandler::needs_marshal(
        std::vector<sql::AggregateSpec> const& specs
) -> bool {
    return std::any_of(specs.begin(), specs.end(), [](auto const& s) {
        return s.kind != sql::AggregateKind::CountStar;
    });
}

auto LocalAggregateOutputHandler::walk_path(
        simdjson::ondemand::document& doc,
        std::string const& col_path,
        simdjson::ondemand::value& out_val
) -> bool {
    size_t start = 0;
    bool first = true;

    while (start < col_path.size()) {
        auto dot_pos = col_path.find('.', start);
        std::string_view key;
        if (std::string::npos == dot_pos) {
            key = std::string_view(col_path.data() + start, col_path.size() - start);
            start = col_path.size();
        } else {
            key = std::string_view(col_path.data() + start, dot_pos - start);
            start = dot_pos + 1;
        }

        if (first) {
            auto field_result = doc.find_field(key);
            if (field_result.error()) {
                return false;
            }
            out_val = field_result.value();
            first = false;
        } else {
            auto field_result = out_val.find_field(key);
            if (field_result.error()) {
                return false;
            }
            out_val = field_result.value();
        }
    }
    return false == first;
}

auto LocalAggregateOutputHandler::extract_numeric_value(
        simdjson::padded_string const& padded,
        std::string const& col_path
) -> std::optional<double> {
    auto doc_result = m_parser.iterate(padded);
    if (doc_result.error()) {
        return std::nullopt;
    }
    auto doc = std::move(doc_result).value();

    simdjson::ondemand::value val;
    if (false == walk_path(doc, col_path, val)) {
        return std::nullopt;
    }

    {
        auto double_result = val.get_double();
        if (false == double_result.error()) {
            return double_result.value();
        }
    }
    {
        auto int_result = val.get_int64();
        if (false == int_result.error()) {
            return static_cast<double>(int_result.value());
        }
    }
    {
        auto uint_result = val.get_uint64();
        if (false == uint_result.error()) {
            return static_cast<double>(uint_result.value());
        }
    }
    return std::nullopt;
}

auto LocalAggregateOutputHandler::extract_string_value(
        simdjson::padded_string const& padded,
        std::string const& col_path
) -> std::optional<std::string> {
    auto doc_result = m_parser.iterate(padded);
    if (doc_result.error()) {
        return std::nullopt;
    }
    auto doc = std::move(doc_result).value();

    simdjson::ondemand::value val;
    if (false == walk_path(doc, col_path, val)) {
        return std::nullopt;
    }

    auto str_result = val.get_string();
    if (str_result.error()) {
        return std::nullopt;
    }
    return std::string{str_result.value()};
}

auto LocalAggregateOutputHandler::extract_raw_value(
        simdjson::padded_string const& padded,
        std::string const& col_path
) -> std::optional<nlohmann::json> {
    auto doc_result = m_parser.iterate(padded);
    if (doc_result.error()) {
        return std::nullopt;
    }
    auto doc = std::move(doc_result).value();

    simdjson::ondemand::value val;
    if (false == walk_path(doc, col_path, val)) {
        return std::nullopt;
    }

    // Check null
    {
        auto is_null_result = val.is_null();
        if (false == is_null_result.error() && is_null_result.value()) {
            return std::nullopt;
        }
    }

    // Try each scalar type. Objects and arrays are intentionally not captured —
    // ARBITRARY/ANY_VALUE returns the first non-null scalar value.
    {
        auto str_result = val.get_string();
        if (false == str_result.error()) {
            return nlohmann::json(std::string{str_result.value()});
        }
    }
    {
        auto int_result = val.get_int64();
        if (false == int_result.error()) {
            return nlohmann::json(int_result.value());
        }
    }
    {
        auto uint_result = val.get_uint64();
        if (false == uint_result.error()) {
            return nlohmann::json(uint_result.value());
        }
    }
    {
        auto double_result = val.get_double();
        if (false == double_result.error()) {
            return nlohmann::json(double_result.value());
        }
    }
    {
        auto bool_result = val.get_bool();
        if (false == bool_result.error()) {
            return nlohmann::json(bool_result.value());
        }
    }
    return std::nullopt;
}
}  // namespace clp_s::search
