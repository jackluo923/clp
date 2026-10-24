#include <algorithm>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/ParsedMessage.hpp>
#include <clp_s/RecordLayoutCache.hpp>
#include <clp_s/Schema.hpp>
#include <clp_s/SchemaMap.hpp>
#include <clp_s/SchemaTree.hpp>

namespace {
using namespace clp_s;

auto compare_trees(SchemaTree const& actual, SchemaTree const& expected) -> void {
    REQUIRE(actual.get_nodes().size() == expected.get_nodes().size());
    for (auto const& node : expected.get_nodes()) {
        auto const& other{actual.get_node(node.get_id())};
        REQUIRE(other.get_parent_id() == node.get_parent_id());
        REQUIRE(other.get_key_name() == node.get_key_name());
        REQUIRE(other.get_type() == node.get_type());
        REQUIRE(other.get_count() == node.get_count());
    }
}
}  // namespace

TEST_CASE(
        "Record layouts preserve interleaved fields, types, parents and column order",
        "[clp-s][RecordLayoutCache]"
) {
    RecordLayoutCache cache;
    SchemaTree tree;
    SchemaTree reference;
    SchemaMap schemas;
    ParsedMessage message;
    for (int row{0}; row < 60; ++row) {
        int const mode{row % 5};
        cache.begin_record();
        message.clear();
        std::vector<int32_t> ordered;
        auto visit = [&](int32_t parent, NodeType type, std::string_view key) {
            auto const id{cache.add_node(tree, parent, type, key)};
            REQUIRE(id == reference.add_node(parent, type, key));
            return id;
        };
        auto const root{visit(-1, NodeType::Object, "")};
        auto leaf = [&](int32_t parent, NodeType type, std::string_view key) {
            auto const id{visit(parent, type, key)};
            ordered.push_back(id);
            message.add_value(id, int64_t{row * 100 + id});
        };
        if (mode == 1) {
            leaf(root, NodeType::Integer, "b");
        }
        auto const nested{visit(root, NodeType::Object, "nested")};
        leaf(mode == 4 ? root : nested, mode == 3 ? NodeType::VarString : NodeType::Integer, "x");
        if (mode != 1) {
            leaf(mode == 4 ? nested : root, NodeType::Integer, "b");
        }
        if (mode == 2) {
            ordered.push_back(visit(nested, NodeType::NullValue, "empty"));
        }
        Schema schema;
        for (auto id : ordered) {
            schema.insert_ordered(id);
        }
        auto const schema_id{schemas.add_schema(schema)};
        auto const* layout{cache.find(ordered, {}, message, 0)};
        if (row >= 5) {
            REQUIRE(layout != nullptr);
            REQUIRE(layout->schema_id == schema_id);
            auto expected{message.get_content()};
            std::ranges::sort(expected, {}, &ParsedMessage::Content::value_type::first);
            for (size_t i{0}; i < expected.size(); ++i) {
                auto const& actual{message.get_content()[layout->value_order[i]]};
                REQUIRE(actual.first == expected[i].first);
                REQUIRE(std::get<int64_t>(actual.second) == std::get<int64_t>(expected[i].second));
            }
        } else {
            REQUIRE(layout == nullptr);
            cache.remember(schema_id, ordered, {}, message, 0);
        }
        cache.end_record();
        compare_trees(tree, reference);
    }
}

TEST_CASE("Record layout hits require complete schemas and values", "[clp-s][RecordLayoutCache]") {
    RecordLayoutCache cache;
    SchemaTree tree;
    ParsedMessage message;
    auto begin = [&]() {
        cache.begin_record();
        message.clear();
        auto const root{cache.add_node(tree, -1, NodeType::Object, "")};
        auto const value{cache.add_node(tree, root, NodeType::Integer, "value")};
        message.add_value(value, int64_t{7});
        return value;
    };
    auto const value{begin()};
    std::vector<int32_t> ordered{value};
    Schema shape;
    auto const start{shape.start_log_message(value)};
    shape.insert_unordered(value);
    shape.end_log_message(start, 10);
    auto const shape_entries{std::span<int32_t const>{shape.data(), shape.size()}};
    cache.remember(1, ordered, shape_entries, message, 1);
    cache.end_record();

    begin();
    REQUIRE(cache.find(ordered, shape_entries, message, 1) != nullptr);
    shape.data()[start + 1] = 11;
    REQUIRE(cache.find(ordered, shape_entries, message, 1) == nullptr);
    cache.remember(2, ordered, shape_entries, message, 1);
    cache.end_record();
    begin();
    shape.data()[start + 1] = 10;
    auto const* layout{cache.find(ordered, shape_entries, message, 1)};
    REQUIRE(layout != nullptr);
    REQUIRE(layout->schema_id == 1);
    REQUIRE(cache.find(ordered, shape_entries, message, 2) == nullptr);
    REQUIRE(cache.find({}, shape_entries, message, 1) == nullptr);
    message.add_value(value + 1, int64_t{8});
    REQUIRE(cache.find(ordered, shape_entries, message, 1) == nullptr);
    cache.end_record();

    // A shorter record sharing a cached prefix must not reuse the longer record's ERT.
    cache.begin_record();
    cache.add_node(tree, -1, NodeType::Object, "");
    message.clear();
    REQUIRE(cache.find({}, {}, message, 0) == nullptr);
    cache.end_record();

    // Archive splits invalidate IDs even when the next archive immediately reuses them.
    cache.clear();
    tree.clear();
    begin();
    REQUIRE(cache.find(ordered, shape_entries, message, 1) == nullptr);
}

TEST_CASE(
        "Record layout memory is bounded and duplicate values retain the first value",
        "[clp-s][RecordLayoutCache]"
) {
    RecordLayoutCache cache;
    SchemaTree tree;
    ParsedMessage message;
    for (size_t row{0}; row <= RecordLayoutCache::cMaxLayouts; ++row) {
        cache.begin_record();
        auto const id{cache.add_node(tree, -1, NodeType::Integer, std::to_string(row))};
        message.clear();
        message.add_value(id, int64_t{1});
        cache.remember(static_cast<int32_t>(row), {&id, 1}, {}, message, 0);
        cache.end_record();
    }
    cache.begin_record();
    auto const id{cache.add_node(tree, -1, NodeType::Integer, "0")};
    message.clear();
    message.add_value(id, int64_t{1});
    REQUIRE(cache.find({&id, 1}, {}, message, 0) == nullptr);
    cache.end_record();

    cache.begin_record();
    std::vector<int32_t> ordered;
    message.clear();
    for (size_t i{0}; i < RecordLayoutCache::cMaxLayoutBytes; ++i) {
        ordered.push_back(cache.add_node(tree, -1, NodeType::Integer, std::to_string(i)));
    }
    cache.remember(99, ordered, {}, message, 0);
    REQUIRE(cache.find(ordered, {}, message, 0) == nullptr);
    cache.end_record();

    // A path may fit by itself while its schema and column permutation exceed the budget.
    for (size_t repeat{0}; repeat < 2; ++repeat) {
        cache.begin_record();
        std::vector<int32_t> fields;
        message.clear();
        for (size_t i{0}; i < 600; ++i) {
            auto const field{cache.add_node(tree, -1, NodeType::Integer, std::to_string(i))};
            fields.push_back(field);
            message.add_value(field, int64_t{1});
        }
        REQUIRE(cache.find(fields, {}, message, 0) == nullptr);
        cache.remember(99, fields, {}, message, 0);
        cache.end_record();
    }

    cache.clear();
    cache.begin_record();
    cache.add_node(tree, -1, NodeType::Integer, "0");
    message.clear();
    message.add_value(id + 1, int64_t{3});
    message.add_value(id, int64_t{1});
    message.add_value(id + 1, int64_t{4});
    message.add_value(id, int64_t{2});
    cache.remember(99, {&id, 1}, {}, message, 0);
    REQUIRE(cache.find({&id, 1}, {}, message, 0) == nullptr);
    message.sort_content();
    auto const& values{message.get_content()};
    REQUIRE(values.size() == 2);
    REQUIRE(values[0].first == id);
    REQUIRE(std::get<int64_t>(values[0].second) == 1);
    REQUIRE(values[1].first == id + 1);
    REQUIRE(std::get<int64_t>(values[1].second) == 3);
}

TEST_CASE(
        "Batch ordered nodes preserve unordered delimiters and string shape IDs",
        "[clp-s][RecordLayoutCache]"
) {
    Schema incremental;
    Schema batched;
    std::vector<int32_t> nodes{9, 2, 17, 3};
    for (auto* schema : {&incremental, &batched}) {
        auto const start{schema->start_log_message(21)};
        schema->insert_unordered(22);
        auto const nested{schema->start_unordered_object(NodeType::StructuredArray)};
        schema->insert_unordered(23);
        schema->insert_unordered(23);
        schema->end_unordered_object(nested);
        schema->end_log_message(start, 42);
    }
    for (auto id : nodes) {
        incremental.insert_ordered(id);
    }
    batched.insert_ordered(nodes);
    REQUIRE(incremental == batched);
    REQUIRE(incremental.get_num_ordered() == batched.get_num_ordered());
    REQUIRE(batched.get_view().find_log_shape_id() == 42);
}

TEST_CASE(
        "Record layout caching recovers after a phase with no locality",
        "[clp-s][RecordLayoutCache]"
) {
    RecordLayoutCache cache;
    SchemaTree tree;
    SchemaTree reference;
    size_t hits{};
    for (size_t row{0}; row < 400; ++row) {
        cache.begin_record();
        auto const key{row < 200 ? std::to_string(row % 32) : "stable"};
        auto const id{cache.add_node(tree, -1, NodeType::Integer, key)};
        REQUIRE(id == reference.add_node(-1, NodeType::Integer, key));
        ParsedMessage message;
        message.add_value(id, int64_t{1});
        auto const* layout{cache.find({&id, 1}, {}, message, 0)};
        if (row < 200) {
            REQUIRE(layout == nullptr);
        }
        if (layout != nullptr) {
            ++hits;
            REQUIRE(layout->schema_id == id);
        }
        cache.end_record();
        if (layout == nullptr) {
            cache.remember(id, {&id, 1}, {}, message, 0);
        }
    }
    REQUIRE(hits > 100);
    compare_trees(tree, reference);
}
