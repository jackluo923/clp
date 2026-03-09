#ifndef CLP_S_SEARCH_LOCALAGGREGATEOUTPUTHANDLER_HPP
#define CLP_S_SEARCH_LOCALAGGREGATEOUTPUTHANDLER_HPP

#include <cstdint>
#include <limits>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include <nlohmann/json.hpp>
#include <simdjson.h>

#include "../ErrorCode.hpp"
#include "OutputHandler.hpp"
#include "sql/sql.hpp"

namespace clp_s::search {
/**
 * Output handler that computes SQL aggregate functions (COUNT, MIN, MAX, SUM, AVG,
 * ARBITRARY/ANY_VALUE) over matching records and emits a single JSON result line on finish().
 *
 * For COUNT(*)-only queries, should_marshal_records() returns false — each write() call
 * represents one matching record. For queries that include MIN/MAX/SUM/AVG/COUNT(col)/ARBITRARY,
 * should_marshal_records() returns true and write() receives the serialized JSON record
 * from which the target column value is extracted via simdjson.
 */
class LocalAggregateOutputHandler : public OutputHandler {
public:
    explicit LocalAggregateOutputHandler(std::vector<sql::AggregateSpec> specs);

    void write(
            std::string_view message,
            epochtime_t timestamp,
            std::string_view archive_id,
            int64_t log_event_idx
    ) override;

    void write(std::string_view message) override;

    [[nodiscard]] auto finish() -> ErrorCode override;

private:
    struct AggState {
        sql::AggregateSpec spec;
        int64_t count{0};

        // Numeric tracking (for SUM/AVG and numeric MIN/MAX)
        double sum{0.0};
        double numeric_min{std::numeric_limits<double>::max()};
        double numeric_max{std::numeric_limits<double>::lowest()};
        bool numeric_seen{false};

        // String tracking (for string MIN/MAX)
        std::string string_min;
        std::string string_max;
        bool string_seen{false};

        // ARBITRARY/ANY_VALUE: first non-null value
        nlohmann::json arbitrary_value;
        bool arbitrary_captured{false};
    };

    simdjson::ondemand::parser m_parser;
    std::vector<AggState> m_states;
    bool m_needs_marshal;

    /**
     * @return Whether any aggregate spec requires record marshalling (i.e., anything other than
     * COUNT(*)).
     */
    static auto needs_marshal(std::vector<sql::AggregateSpec> const& specs) -> bool;

    /**
     * Walks a dotted column path (e.g. "meta.latency") in a JSON document and returns
     * the value at that path.
     * @param doc The simdjson document.
     * @param col_path The dotted column path.
     * @param[out] out_val The value at the path (only valid if true is returned).
     * @return true if the path was found, false otherwise.
     */
    static auto walk_path(
            simdjson::ondemand::document& doc,
            std::string const& col_path,
            simdjson::ondemand::value& out_val
    ) -> bool;

    /**
     * Extracts a numeric value from a JSON record at the given dotted column path.
     * @param padded The padded JSON record (shared across aggregate states).
     * @param col_path The dotted column path (e.g. "meta.latency").
     * @return The numeric value, or std::nullopt if the path is absent or non-numeric.
     */
    auto extract_numeric_value(
            simdjson::padded_string const& padded,
            std::string const& col_path
    ) -> std::optional<double>;

    /**
     * Extracts a string value from a JSON record at the given dotted column path.
     * @param padded The padded JSON record (shared across aggregate states).
     * @param col_path The dotted column path.
     * @return The string value, or std::nullopt if the path is absent or not a string.
     */
    auto extract_string_value(
            simdjson::padded_string const& padded,
            std::string const& col_path
    ) -> std::optional<std::string>;

    /**
     * Extracts the raw JSON value at the given column path as a nlohmann::json object.
     * Used by ARBITRARY/ANY_VALUE to capture values of any type (scalar types only — JSON objects
     * and arrays are not captured and return std::nullopt).
     * @param padded The padded JSON record (shared across aggregate states).
     * @param col_path The dotted column path.
     * @return The value as nlohmann::json, or std::nullopt if absent, null, or a compound type.
     */
    auto extract_raw_value(
            simdjson::padded_string const& padded,
            std::string const& col_path
    ) -> std::optional<nlohmann::json>;
};
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_LOCALAGGREGATEOUTPUTHANDLER_HPP
