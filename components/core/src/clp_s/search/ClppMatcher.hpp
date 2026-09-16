#ifndef CLP_S_SEARCH_CLPPMATCHER_HPP
#define CLP_S_SEARCH_CLPPMATCHER_HPP

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>
#include <vector>

#include <log_surgeon/log_surgeon.hpp>
#include <ystdlib/error_handling/Result.hpp>

#include <clp_s/ArchiveReader.hpp>
#include <clp_s/search/ClppShapeQueryMatcher.hpp>
#include <clpp/Defs.hpp>
#include <clpp/Interpretation.hpp>

namespace clp_s::search {
/**
 * Utility for running clpp queries.
 */
class ClppMatcher {
public:
    // Types
    /**
     * An interpretation of a clpp query and the schema IDs containing a matching shape (log shape
     * or parent rule shape).
     */
    struct InterpretationMatch {
        std::unordered_set<int32_t> schema_ids;
        clpp::Interpretation interpretation;
    };

    // Constructors
    /**
     * Builds the log shape to schemas index using `archive_reader` to enable clpp querying.
     * If the archive is not experimental construction is skipped and the object is invalid.
     * @param archive_reader
     * @param case_sensitive Whether matching is case sensitive.
     * @throw std::runtime_error if the log shape dictionary is not valid.
     */
    ClppMatcher(ArchiveReader* archive_reader, bool case_sensitive);

    // Methods
    /**
     * Finds the schemas whose log shapes (or parent rule shapes of `rule_name`) satisfy
     * `shape_query`.
     * @param rule_name A parent rule name, or empty to query the entire log shape.
     * @param shape_query A wildcard pattern, or std::nullopt to match every shape.
     * @return The matching schema IDs.
     */
    [[nodiscard]] auto find_matching_schemas(
            std::string_view rule_name,
            std::optional<std::string_view> shape_query
    ) const -> std::unordered_set<int32_t>;

    /**
     * Decomposes `query` against the parent rule `rule_name`, or against every log shape if
     * `rule_name` is empty, then returns the possible interpretations.
     * @param query
     * @param rule_name A qualified parent rule name, or empty to decompose against every log shape.
     * @return The matching interpretations (empty if no interpretation matched a shape), or an
     * error code indicating the failure:
     * - Forwards `decompose_by_log_shapes`'s return values.
     * - Forwards `decompose_by_rule_name`'s return values.
     */
    [[nodiscard]] auto decompose_query(std::string_view query, std::string_view rule_name)
            -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>>;

private:
    // Methods
    /**
     * Decomposes `query` against the log shapes returning interpretations that matched a log shape.
     * Shapes that cannot match are dropped by the skeleton filter and the rest are handed to the
     * engine.
     * @return The matching interpretations, or an error code indicating the failure:
     * - Forwards `ArchiveReader::read_parsing_spec`'s return values.
     */
    [[nodiscard]] auto decompose_by_log_shapes(std::string_view query)
            -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>>;

    /**
     * Decomposes `query` against the parent rule `rule_name`, then matches each interpretation's
     * shape query against `rule_name` in relevant log shapes.
     * @return The matching interpretations, or an error code indicating the failure:
     * - Forwards `ArchiveReader::read_parsing_spec`'s return values.
     */
    [[nodiscard]] auto decompose_by_rule_name(std::string_view query, std::string_view rule_name)
            -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>>;

    /**
     * Runs the engine on `log_shape_ids` and appends the resulting interpretations to `matches`.
     * @return A void result on success, or an error code indicating the failure:
     * - Forwards `ArchiveReader::read_parsing_spec`'s return values.
     */
    [[nodiscard]] auto decompose_by_engine(
            std::string_view query,
            std::vector<clpp::log_shape_id_t> const& log_shape_ids,
            std::vector<InterpretationMatch>& matches
    ) -> ystdlib::error_handling::Result<void>;

    /**
     * Get the parent rule shapes named `rule_name` from `log_shape_id` or the entire log shape if
     * `rule_name` is empty.
     * @throw Propagates `ArchiveReader::get_parent_rule_shapes`'s exceptions.
     */
    [[nodiscard]] auto
    get_shapes(clpp::log_shape_id_t log_shape_id, std::string_view rule_name) const
            -> std::vector<std::string_view>;

    /**
     * Finds the schemas whose log shape has a shape satisfying `shape_matches`.
     * @param rule_name A parent rule name, or empty to query the entire log shape.
     * @param shape_matches Invoked with each (log or parent) shape, returning true if matching.
     * @return The matching schema IDs.
     * @throw Propagates `get_shapes`'s exceptions.
     */
    template <typename ShapePredicate>
    [[nodiscard]] auto schemas_for_matching_shapes(
            std::string_view rule_name,
            ShapePredicate const& shape_matches
    ) const -> std::unordered_set<int32_t>;

    /**
     * Selects the shapes that could possibly match `query`, so the expensive per-shape query
     * intersection only runs on candidates.
     *
     * The test is a sound necessary condition (see `shape_may_match`): a shape is dropped only when
     * it provably cannot match. When the query cannot be reasoned about (invalid escape) or the
     * match is case-insensitive, every shape is returned.
     * @param query A wildcard pattern.
     * @return The candidate log shape IDs in ascending order.
     */
    [[nodiscard]] auto select_candidate_shapes(std::string_view query) const
            -> std::vector<clpp::log_shape_id_t>;

    // Data members
    ArchiveReader* m_archive_reader;
    bool m_case_sensitive{false};
    std::vector<std::unordered_set<int32_t>> m_schemas_by_log_shape;
    // Per log shape ID: false if the shape is malformed and must always be a candidate.
    std::vector<bool> m_shape_ok;
    // Per log shape ID: a placeholder-as-wildcard tokenization of the shape, built once.
    std::vector<std::vector<ShapeToken>> m_shape_skeletons;
    std::unique_ptr<log_surgeon::Parser> m_parser;
};

template <typename ShapePredicate>
auto ClppMatcher::schemas_for_matching_shapes(
        std::string_view rule_name,
        ShapePredicate const& shape_matches
) const -> std::unordered_set<int32_t> {
    std::unordered_set<int32_t> schema_ids;
    auto const num_log_shapes{m_archive_reader->get_log_shape_dictionary()->get_entries().size()};
    for (clpp::log_shape_id_t log_shape_id{0}; log_shape_id < num_log_shapes; ++log_shape_id) {
        auto const shapes{get_shapes(log_shape_id, rule_name)};
        if (false
            == std::ranges::any_of(
                    shapes,
                    [&](std::string_view shape) -> bool { return shape_matches(shape); }
            ))
        {
            continue;
        }
        auto const& shape_schemas{m_schemas_by_log_shape.at(log_shape_id)};
        schema_ids.insert(shape_schemas.begin(), shape_schemas.end());
    }
    return schema_ids;
}
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_CLPPMATCHER_HPP
