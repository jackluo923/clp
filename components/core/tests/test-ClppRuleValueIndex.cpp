// Unit tests for `clpp::RuleValueIndex`: the k-gram / anchoring soundness contract and the archive
// serialization round trip.

#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <map>
#include <string>
#include <string_view>
#include <vector>

#include <catch2/catch_test_macros.hpp>

#include <clp_s/FileWriter.hpp>
#include <clp_s/ZstdCompressor.hpp>
#include <clp_s/ZstdDecompressor.hpp>
#include <clpp/RuleValueIndex.hpp>

using clpp::RuleSignature;
using clpp::RuleValueIndex;

namespace {
#define CLPP_CHECK(condition, message) \
    do {                               \
        INFO(message);                 \
        CHECK((condition));            \
    } while (false)

auto test_index_basics() -> void {
    // Unknown rules are never pruned.
    {
        RuleValueIndex index;
        CLPP_CHECK(index.may_contain("nope", "anything"), "index: unknown rule never pruned");
        CLPP_CHECK(index.may_equal("nope", "zzzzzzzzzz"), "index: unknown rule may_equal");
        CLPP_CHECK(nullptr == index.find("nope"), "index: unknown rule has no signature");
    }

    // Unbounded rules are never pruned, even after values are added.
    {
        RuleValueIndex index;
        index.mark_unbounded("u");
        index.add_value("u", "abc");
        CLPP_CHECK(index.may_contain("u", "zzzzzzzzzz"), "index: unbounded never pruned");
        CLPP_CHECK(index.may_equal("u", "zzz"), "index: unbounded may_equal");
        CLPP_CHECK(index.find("u")->may_be_empty(), "index: unbounded may be empty");
    }

    // Substring queries.
    {
        RuleValueIndex index;
        index.add_value("clazz", "org.apache.hadoop.hdfs.server.datanode.DataNode");
        CLPP_CHECK(
                false == index.may_contain("clazz", "blk_1073746491_5667"),
                "index: rejects a span with absent k-grams"
        );
        CLPP_CHECK(index.may_contain("clazz", "apache"), "index: accepts a real substring");
        CLPP_CHECK(index.may_contain("clazz", ""), "index: empty span always contained");
    }

    // Length bounds.
    {
        RuleValueIndex index;
        index.add_value("r", "abc");
        index.add_value("r", "abcde");
        CLPP_CHECK(false == index.may_contain("r", "abcdef"), "index: longer than max rejected");
        CLPP_CHECK(index.may_contain("r", "abcde"), "index: exact max length accepted");
        auto const& sig{*index.find("r")};
        CLPP_CHECK(3 == sig.min_value_len && 5 == sig.max_value_len, "index: min/max lengths");
        CLPP_CHECK(sig.may_have_length(3) && sig.may_have_length(5), "index: seen lengths");
        CLPP_CHECK(false == sig.may_have_length(4), "index: unseen length rejected");
        CLPP_CHECK(false == sig.may_be_empty(), "index: empty value not seen");
        CLPP_CHECK(false == index.may_equal("r", "abcd"), "index: may_equal rejects unseen length");
        CLPP_CHECK(index.may_equal("r", "abc"), "index: may_equal accepts a value");
    }

    // Lengths at or above the exact cap collapse into one bit.
    {
        RuleValueIndex index;
        std::string const long_value(RuleSignature::cMaxExactLength + 5, 'x');
        index.add_value("r", long_value);
        auto const& sig{*index.find("r")};
        CLPP_CHECK(
                sig.may_have_length(RuleSignature::cMaxExactLength + 5),
                "index: long length seen"
        );
        CLPP_CHECK(
                false == sig.may_have_length(RuleSignature::cMaxExactLength + 6),
                "index: longer than max still rejected"
        );
        CLPP_CHECK(
                false == sig.may_have_length(RuleSignature::cMaxExactLength - 1),
                "index: shorter exact length rejected"
        );
    }

    // k-gram membership for k = 1, 2, 3.
    {
        RuleValueIndex index;
        index.add_value("r", "abc");
        CLPP_CHECK(index.may_contain("r", "a"), "index: unigram present");
        CLPP_CHECK(false == index.may_contain("r", "z"), "index: unigram absent");
        CLPP_CHECK(index.may_contain("r", "ab"), "index: bigram present");
        CLPP_CHECK(index.may_contain("r", "bc"), "index: bigram present (2)");
        CLPP_CHECK(false == index.may_contain("r", "ba"), "index: bigram absent");
        CLPP_CHECK(index.may_contain("r", "abc"), "index: trigram present");
        CLPP_CHECK(false == index.may_contain("r", "abd"), "index: trigram absent");
    }

    // Anchored queries.
    {
        RuleValueIndex index;
        index.add_value("r", "hello");
        index.add_value("r", "help");
        CLPP_CHECK(index.may_start_with("r", "hel"), "index: start present");
        CLPP_CHECK(false == index.may_start_with("r", "ell"), "index: interior span is not a start");
        CLPP_CHECK(false == index.may_start_with("r", "l"), "index: interior byte is not a start");
        CLPP_CHECK(index.may_end_with("r", "llo"), "index: end present");
        CLPP_CHECK(index.may_end_with("r", "lp"), "index: end present (2)");
        CLPP_CHECK(false == index.may_end_with("r", "hel"), "index: start is not an end");
        CLPP_CHECK(false == index.may_end_with("r", "h"), "index: first byte is not an end");
        CLPP_CHECK(index.may_equal("r", "help"), "index: equal present");
        CLPP_CHECK(false == index.may_equal("r", "hell"), "index: prefix of a value is not equal");
        CLPP_CHECK(false == index.may_equal("r", "ello"), "index: suffix of a value is not equal");
        CLPP_CHECK(index.may_start_with("r", ""), "index: empty prefix");
        CLPP_CHECK(index.may_end_with("r", ""), "index: empty suffix");
    }

    // Single-byte values and empty values.
    {
        RuleValueIndex index;
        index.add_value("r", "");
        index.add_value("r", "x");
        auto const& sig{*index.find("r")};
        CLPP_CHECK(sig.may_be_empty(), "index: empty value seen");
        CLPP_CHECK(index.may_equal("r", ""), "index: may_equal empty");
        CLPP_CHECK(index.may_equal("r", "x"), "index: may_equal single byte");
        CLPP_CHECK(index.may_start_with("r", "x") && index.may_end_with("r", "x"), "index: anchors");
        CLPP_CHECK(false == index.may_contain("r", "xx"), "index: no bigrams recorded");
    }

    // Non-ASCII bytes fold into one flag; bigrams stay exact.
    {
        RuleValueIndex ascii;
        ascii.add_value("r", "abc");
        CLPP_CHECK(false == ascii.may_contain("r", std::string_view("\xFF", 1)), "index: non-ascii absent");
        CLPP_CHECK(false == ascii.may_contain("r", std::string_view("\xC3\xA9", 2)), "index: non-ascii bigram absent");

        RuleValueIndex utf8;
        utf8.add_value("r", "caf\xC3\xA9");
        CLPP_CHECK(utf8.may_contain("r", std::string_view("\xC3", 1)), "index: non-ascii unigram present");
        CLPP_CHECK(utf8.may_contain("r", std::string_view("\xC3\xA9", 2)), "index: non-ascii bigram present");
        CLPP_CHECK(false == utf8.may_contain("r", std::string_view("\xC3\xA8", 2)), "index: different non-ascii bigram absent");
        CLPP_CHECK(utf8.may_end_with("r", std::string_view("\xA9", 1)), "index: non-ascii end");
    }

    // Repeated values are deduplicated but still indexed once.
    {
        RuleValueIndex index;
        index.add_value("r", "same");
        index.add_value("r", "same");
        index.add_value("r", "same");
        CLPP_CHECK(index.may_equal("r", "same"), "index: deduped value still indexed");
        index.clear_dedup_state();
        index.add_value("r", "other");
        CLPP_CHECK(index.may_equal("r", "other"), "index: indexing continues after dedup reset");
    }
}

/**
 * Exhaustively checks that every query the index could be asked about a value it indexed is
 * accepted (the soundness direction), across every substring, prefix, suffix, and the value itself.
 */
auto test_soundness() -> void {
    std::vector<std::string> const values{
            "",
            "a",
            "ab",
            "abc",
            "blk_1073746491_5667",
            "org.apache.hadoop.hdfs.server.datanode.DataNode",
            "10.0.0.1:50010",
            "caf\xC3\xA9 \xE2\x98\x83",
            std::string(RuleSignature::cMaxExactLength + 3, 'q'),
    };
    RuleValueIndex index;
    for (auto const& value : values) {
        index.add_value("r", value);
    }
    size_t checked{0};
    for (auto const& value : values) {
        CLPP_CHECK(index.may_equal("r", value), "soundness: may_equal " + value);
        for (size_t start{0}; start <= value.size(); ++start) {
            for (size_t len{0}; start + len <= value.size(); ++len) {
                std::string_view const span{value.data() + start, len};
                ++checked;
                CLPP_CHECK(index.may_contain("r", span), "soundness: may_contain " + std::string{span});
                if (0 == start) {
                    CLPP_CHECK(index.may_start_with("r", span), "soundness: may_start_with " + std::string{span});
                }
                if (start + len == value.size()) {
                    CLPP_CHECK(index.may_end_with("r", span), "soundness: may_end_with " + std::string{span});
                }
            }
        }
    }
    CLPP_CHECK(checked > 1000, "soundness: exercised");
}

auto test_serialization_round_trip() -> void {
    RuleValueIndex index;
    index.add_value("blockID.blockNum", "1073746491");
    index.add_value("blockID.blockNum", "7");
    index.add_value("prefix.class", "org.apache.hadoop.hdfs.server.datanode.DataNode");
    index.add_value("key_value.key", "");
    index.add_value("key_value.key", "type");
    index.add_value("short", "x");
    index.mark_unbounded("lossy.float");

    auto const path{std::filesystem::temp_directory_path()
                    / "clpp-rule-value-index-round-trip.zst"};
    {
        clp_s::FileWriter writer;
        writer.open(path.string(), clp_s::FileWriter::OpenMode::CreateForWriting);
        clp_s::ZstdCompressor compressor;
        compressor.open(writer);
        auto const result{index.compress(compressor)};
        CLPP_CHECK(false == result.has_error(), "round trip: compress succeeds");
        compressor.close();
        writer.close();
    }
    clp_s::ZstdDecompressor decompressor;
    CLPP_CHECK(clp_s::ErrorCodeSuccess == decompressor.open(path.string()), "round trip: open");
    auto result{RuleValueIndex::decompress(decompressor)};
    decompressor.close();
    std::filesystem::remove(path);
    REQUIRE(false == result.has_error());
    auto const& read{result.value()};

    CLPP_CHECK(index.size() == read.size(), "round trip: rule count");
    CLPP_CHECK(read.may_equal("blockID.blockNum", "1073746491"), "round trip: exact value");
    CLPP_CHECK(read.may_equal("blockID.blockNum", "7"), "round trip: single byte value");
    CLPP_CHECK(false == read.may_equal("blockID.blockNum", "1073746492"), "round trip: absent value");
    CLPP_CHECK(false == read.may_contain("blockID.blockNum", "a"), "round trip: absent unigram");
    CLPP_CHECK(read.may_contain("prefix.class", "hadoop"), "round trip: trigrams");
    CLPP_CHECK(read.may_start_with("prefix.class", "org") && false == read.may_start_with("prefix.class", "rg"), "round trip: start bigrams");
    CLPP_CHECK(read.may_end_with("prefix.class", "Node") && false == read.may_end_with("prefix.class", "Nod"), "round trip: end bigrams");
    CLPP_CHECK(read.find("key_value.key")->may_be_empty(), "round trip: empty value");
    CLPP_CHECK(false == read.find("short")->may_be_empty(), "round trip: no empty value");
    CLPP_CHECK(read.find("short")->bigram_bits.empty(), "round trip: unallocated bitsets stay empty");
    CLPP_CHECK(read.find("lossy.float")->unbounded, "round trip: unbounded flag");
    CLPP_CHECK(read.may_equal("lossy.float", "anything"), "round trip: unbounded never pruned");
    CLPP_CHECK(nullptr == read.find("missing"), "round trip: absent rule");

    // Field-by-field equality for every rule.
    for (auto const& name : {"blockID.blockNum", "prefix.class", "key_value.key", "short"}) {
        auto const& a{*index.find(name)};
        auto const& b{*read.find(name)};
        CLPP_CHECK(a.min_value_len == b.min_value_len && a.max_value_len == b.max_value_len, std::string{"round trip: lengths "} + name);
        CLPP_CHECK(a.length_bits == b.length_bits, std::string{"round trip: length bits "} + name);
        CLPP_CHECK(a.unigrams.bits == b.unigrams.bits && a.first_unigrams.bits == b.first_unigrams.bits && a.last_unigrams.bits == b.last_unigrams.bits, std::string{"round trip: unigrams "} + name);
        CLPP_CHECK(a.bigram_bits == b.bigram_bits && a.first_bigram_bits == b.first_bigram_bits && a.last_bigram_bits == b.last_bigram_bits, std::string{"round trip: bigrams "} + name);
        CLPP_CHECK(a.trigram_bits == b.trigram_bits, std::string{"round trip: trigrams "} + name);
    }
}
}  // namespace

TEST_CASE("clpp_rule_value_index", "[clpp][ClppRuleValueIndex]") {
    test_index_basics();
}

TEST_CASE("clpp_rule_value_index_soundness", "[clpp][ClppRuleValueIndex]") {
    test_soundness();
}

TEST_CASE("clpp_rule_value_index_round_trip", "[clpp][ClppRuleValueIndex]") {
    test_serialization_round_trip();
}
