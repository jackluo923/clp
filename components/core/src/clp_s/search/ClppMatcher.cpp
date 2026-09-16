#include "ClppMatcher.hpp"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <numeric>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <unordered_set>
#include <utility>
#include <vector>

#include <log_surgeon/log_surgeon.hpp>
#include <ystdlib/error_handling/Result.hpp>

#include <clp/string_utils/string_utils.hpp>
#include <clp_s/ArchiveReader.hpp>
#include <clp_s/search/ClppShapeDecomposer.hpp>
#include <clp_s/search/ClppShapeQueryMatcher.hpp>
#include <clpp/Defs.hpp>
#include <clpp/Interpretation.hpp>
#include <clpp/RuleValueIndex.hpp>
#include <clpp/TextShape.hpp>
#include <utils/profiling/ScopedProfiler.hpp>

namespace clp_s::search {
ClppMatcher::ClppMatcher(ArchiveReader* archive_reader, bool case_sensitive)
        : m_archive_reader{archive_reader},
          m_case_sensitive{case_sensitive} {
    if (false == m_archive_reader->experimental()) {
        return;
    }
    PROFILE_SCOPE("clpp_matcher_init");
    auto const log_shape_dict{m_archive_reader->get_log_shape_dictionary()};
    if (nullptr == log_shape_dict) {
        throw std::runtime_error{"ClppMatcher got a null log shape dictionary"};
    }

    m_schemas_by_log_shape.resize(log_shape_dict->get_entries().size());
    for (auto const& [schema_id, schema] : *m_archive_reader->get_schema_map()) {
        if (auto const log_shape_id{schema.get_view().find_log_shape_id()}) {
            if (*log_shape_id >= m_schemas_by_log_shape.size()) {
                throw std::runtime_error{
                        "ClppMatcher found a schema referencing an unknown log shape ID"
                };
            }
            m_schemas_by_log_shape.at(*log_shape_id).emplace(schema_id);
        }
    }
}

auto ClppMatcher::prepare_shapes() -> void {
    if (m_shapes_prepared) {
        return;
    }
    m_shapes_prepared = true;
    PROFILE_SCOPE("clpp_prepare_shapes");
    auto const& shape_entries{m_archive_reader->get_log_shape_dictionary()->get_entries()};
    m_shape_ok.assign(shape_entries.size(), false);
    {
        PROFILE_SCOPE("rule_value_index_read");
        m_rule_value_index = m_archive_reader->get_rule_value_index();
    }
    if (nullptr == m_rule_value_index) {
        // Without an index every shape is decomposed by the engine; the skeletons only prune the
        // shapes handed to it.
        PROFILE_SCOPE("build_shape_skeletons");
        m_shape_skeletons.resize(shape_entries.size());
        for (size_t shape_id{0}; shape_id < shape_entries.size(); ++shape_id) {
            m_shape_ok.at(shape_id) = build_shape_skeleton(
                    shape_entries.at(shape_id).get_value(),
                    m_shape_skeletons.at(shape_id)
            );
        }
        return;
    }

    PROFILE_SCOPE("build_compact_shapes");
    m_compact_shapes.resize(shape_entries.size());
    for (size_t shape_id{0}; shape_id < shape_entries.size(); ++shape_id) {
        m_shape_ok.at(shape_id) = build_compact_shape(
                shape_entries.at(shape_id).get_value(),
                [this](std::string_view rule) -> rule_id_t { return intern_rule(rule); },
                m_compact_shapes.at(shape_id)
        );
    }
}

auto ClppMatcher::intern_rule(std::string_view rule) -> rule_id_t {
    if (auto const it{m_rule_ids.find(std::string{rule})}; it != m_rule_ids.end()) {
        return it->second;
    }
    auto const id{static_cast<rule_id_t>(m_rule_names.size())};
    m_rule_ids.emplace(std::string{rule}, id);
    m_rule_names.emplace_back(rule);
    // An absent rule is unbounded (nullptr); `find` also returns the signature of a rule that was
    // explicitly marked unbounded, which `piece_may_match` treats the same way.
    m_rule_signatures.push_back(m_rule_value_index->find(rule));
    return id;
}

auto ClppMatcher::select_candidate_shapes(std::string_view query) const
        -> std::vector<clpp::log_shape_id_t> {
    std::vector<clpp::log_shape_id_t> candidates;
    std::vector<QueryToken> query_tokens;
    if (m_shape_skeletons.empty() || false == m_case_sensitive
        || false == parse_shape_query(query, query_tokens))
    {
        // Without skeletons the local decomposer is the filter. Otherwise: the predicate is
        // case-sensitive, so a case-insensitive match could drop matching shapes; and an
        // unparsable query cannot be reasoned about. In all cases keep every shape.
        candidates.resize(m_shape_ok.size());
        std::iota(candidates.begin(), candidates.end(), clpp::log_shape_id_t{0});
        return candidates;
    }

    for (clpp::log_shape_id_t shape_id{0}; shape_id < m_shape_skeletons.size(); ++shape_id) {
        if (false == m_shape_ok.at(shape_id)
            || shape_may_match(m_shape_skeletons.at(shape_id), query_tokens))
        {
            candidates.push_back(shape_id);
        }
    }
    return candidates;
}

auto ClppMatcher::find_matching_schemas(
        std::string_view rule_name,
        std::optional<std::string_view> shape_query
) const -> std::unordered_set<int32_t> {
    if (shape_query.has_value()) {
        return schemas_for_matching_shapes(rule_name, [&](std::string_view shape) -> bool {
            return clp::string_utils::wildcard_match_unsafe(shape, *shape_query, m_case_sensitive);
        });
    }
    return schemas_for_matching_shapes(rule_name, [](std::string_view) -> bool { return true; });
}

auto ClppMatcher::decompose_query(std::string_view query, std::string_view rule_name)
        -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>> {
    m_stats = {};
    if (rule_name.empty()) {
        return decompose_by_log_shapes(query);
    }
    return decompose_by_rule_name(query, rule_name);
}

auto ClppMatcher::to_interpretation(ShapeInterpretation const& interpretation) const
        -> clpp::Interpretation {
    clpp::TextShape<std::string> shape_query;
    for (auto const& segment : interpretation.segments) {
        if (segment.is_placeholder) {
            shape_query.append_placeholder(m_rule_names.at(segment.rule_id));
        } else {
            shape_query.escape_and_append(segment.text);
        }
    }
    std::vector<clpp::LeafQuery> leaf_queries;
    leaf_queries.reserve(interpretation.leaves.size());
    for (auto const& leaf : interpretation.leaves) {
        leaf_queries.emplace_back(m_rule_names.at(leaf.rule_id), leaf.query);
    }
    return {std::move(shape_query), std::move(leaf_queries)};
}

auto ClppMatcher::decompose_by_log_shapes(std::string_view query)
        -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>> {
    PROFILE_SCOPE("clpp_decompose_by_log_shapes");
    prepare_shapes();
    // Without an index, only hand the shapes that could possibly match to the engine; each dropped
    // shape would otherwise pay a per-shape query intersection and an NFA rebuild.
    auto const candidate_ids{select_candidate_shapes(query)};
    m_stats.num_candidate_shapes = candidate_ids.size();

    std::vector<InterpretationMatch> matches;
    std::vector<QueryToken> tokens;
    if (nullptr == m_rule_value_index || false == m_case_sensitive
        || false == parse_shape_query(query, tokens))
    {
        YSTDLIB_ERROR_HANDLING_TRYV(decompose_by_engine(query, candidate_ids, matches));
        return matches;
    }

    std::vector<clpp::log_shape_id_t> deferred_ids;
    {
        PROFILE_SCOPE("clpp_decompose_locally");
        normalize_query_tokens(tokens);
        ShapeDecomposer decomposer{tokens, m_rule_signatures};
        std::vector<ShapeInterpretation> interpretations;
        for (auto const shape_id : candidate_ids) {
            if (false == m_shape_ok.at(shape_id)) {
                deferred_ids.push_back(shape_id);
                continue;
            }
            auto const status{decomposer.decompose(
                    m_compact_shapes.at(shape_id),
                    cMaxLocalInterpretationsPerShape,
                    interpretations
            )};
            if (DecomposeStatus::NoMatch == status) {
                continue;
            }
            if (DecomposeStatus::Capped == status || decomposer.used_unbounded_rule()) {
                // The engine's regex-based decomposition is tighter than what the index can
                // certify here.
                deferred_ids.push_back(shape_id);
                continue;
            }
            ++m_stats.num_shapes_decomposed_locally;
            for (auto const& interpretation : interpretations) {
                matches.push_back(
                        {.schema_ids{m_schemas_by_log_shape.at(shape_id)},
                         .interpretation{to_interpretation(interpretation)}}
                );
            }
        }
    }
    if (false == deferred_ids.empty()) {
        YSTDLIB_ERROR_HANDLING_TRYV(decompose_by_engine(query, deferred_ids, matches));
    }
    return matches;
}

auto ClppMatcher::decompose_by_engine(
        std::string_view query,
        std::vector<clpp::log_shape_id_t> const& log_shape_ids,
        std::vector<InterpretationMatch>& matches
) -> ystdlib::error_handling::Result<void> {
    PROFILE_SCOPE("clpp_decompose_by_engine");
    m_stats.num_shapes_decomposed_by_engine += log_shape_ids.size();
    if (log_shape_ids.empty()) {
        return ystdlib::error_handling::success();
    }
    auto const& entries{m_archive_reader->get_log_shape_dictionary()->get_entries()};
    std::vector<std::string_view> log_shapes;
    log_shapes.reserve(log_shape_ids.size());
    for (auto const shape_id : log_shape_ids) {
        log_shapes.emplace_back(entries.at(shape_id).get_value());
    }

    if (nullptr == m_parser) {
        PROFILE_SCOPE("clpp_parser_init");
        m_parser = std::make_unique<log_surgeon::Parser>(log_surgeon::ParsingSpecBuilder{
                YSTDLIB_ERROR_HANDLING_TRYX(m_archive_reader->read_parsing_spec())
        }
                                                                 .build());
    }
    auto interpretations_by_shape{clpp::decompose_by_log_shapes(*m_parser, query, log_shapes)};
    for (size_t local_id{0}; local_id < interpretations_by_shape.size(); ++local_id) {
        auto const log_shape_id{log_shape_ids.at(local_id)};
        for (auto& interpretation : interpretations_by_shape.at(local_id)) {
            matches.push_back(
                    {.schema_ids{m_schemas_by_log_shape.at(log_shape_id)},
                     .interpretation{std::move(interpretation)}}
            );
        }
    }
    return ystdlib::error_handling::success();
}

auto ClppMatcher::decompose_by_rule_name(std::string_view query, std::string_view rule_name)
        -> ystdlib::error_handling::Result<std::vector<InterpretationMatch>> {
    if (nullptr == m_parser) {
        m_parser = std::make_unique<log_surgeon::Parser>(log_surgeon::ParsingSpecBuilder{
                YSTDLIB_ERROR_HANDLING_TRYX(m_archive_reader->read_parsing_spec())
        }
                                                                 .build());
    }
    auto interpretations{clpp::decompose_by_rule_name(*m_parser, query, rule_name)};
    std::vector<InterpretationMatch> matches;
    matches.reserve(interpretations.size());
    for (auto& interpretation : interpretations) {
        auto schema_ids{schemas_for_matching_shapes(rule_name, [&](std::string_view shape) -> bool {
            return clp::string_utils::wildcard_match_unsafe(
                    shape,
                    interpretation.m_shape_query.view(),
                    m_case_sensitive
            );
        })};
        if (schema_ids.empty()) {
            continue;
        }
        matches.push_back(
                {.schema_ids{std::move(schema_ids)}, .interpretation{std::move(interpretation)}}
        );
    }
    return matches;
}

auto ClppMatcher::get_shapes(clpp::log_shape_id_t log_shape_id, std::string_view rule_name) const
        -> std::vector<std::string_view> {
    std::string_view const log_shape{
            m_archive_reader->get_log_shape_dictionary()->get_entries().at(log_shape_id).get_value()
    };
    if (rule_name.empty()) {
        return {log_shape};
    }
    std::vector<std::string_view> shapes;
    for (auto const& parent_match :
         m_archive_reader->get_parent_rule_shapes().at(log_shape_id).get())
    {
        if (rule_name == parent_match.m_name) {
            shapes.emplace_back(log_shape.substr(parent_match.m_start, parent_match.m_size));
        }
    }
    return shapes;
}
}  // namespace clp_s::search
