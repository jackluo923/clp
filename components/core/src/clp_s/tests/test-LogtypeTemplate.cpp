#include <string>
#include <string_view>

#include <catch2/catch_test_macros.hpp>

#include <clp/ir/types.hpp>

#include "../src/clp_s/LogtypeTemplate.hpp"

using clp_s::append_logtype_template;
using clp_s::render_logtype_template;

namespace {
auto ph(clp::ir::VariablePlaceholder p) -> char {
    return static_cast<char>(p);
}
}  // namespace

TEST_CASE("logtype-template-typed-placeholders", "[clp-s][logtype-template]") {
    REQUIRE(render_logtype_template(std::string{ph(clp::ir::VariablePlaceholder::Integer)})
            == "%int%");
    REQUIRE(render_logtype_template(std::string{ph(clp::ir::VariablePlaceholder::Float)})
            == "%float%");
    REQUIRE(render_logtype_template(std::string{ph(clp::ir::VariablePlaceholder::Dictionary)})
            == "%str%");
}

TEST_CASE("logtype-template-escape-branch", "[clp-s][logtype-template]") {
    // An escaped placeholder byte is emitted literally, not as a typed placeholder.
    std::string escaped_int;
    escaped_int.push_back(ph(clp::ir::VariablePlaceholder::Escape));
    escaped_int.push_back(ph(clp::ir::VariablePlaceholder::Integer));
    REQUIRE(render_logtype_template(escaped_int)
            == std::string(1, ph(clp::ir::VariablePlaceholder::Integer)));

    // A trailing escape byte is dropped rather than read past the end.
    std::string trailing_escape;
    trailing_escape.push_back(ph(clp::ir::VariablePlaceholder::Escape));
    REQUIRE(render_logtype_template(trailing_escape).empty());
}

TEST_CASE("logtype-template-percent-escaping", "[clp-s][logtype-template]") {
    // A literal '%' doubles so it is unambiguous next to placeholder delimiters.
    REQUIRE(render_logtype_template("a%b") == "a%%b");
    REQUIRE(render_logtype_template("a%b", true) == "a%%b");

    REQUIRE(render_logtype_template("a%b", false) == "a%b");

    std::string combined{"CPU at "};
    combined.push_back(ph(clp::ir::VariablePlaceholder::Integer));
    combined += "% load";
    REQUIRE(render_logtype_template(combined, true) == "CPU at %int%%% load");
    REQUIRE(render_logtype_template(combined, false) == "CPU at %int%% load");
}

TEST_CASE("logtype-template-append-variant", "[clp-s][logtype-template]") {
    std::string combined{"CPU at "};
    combined.push_back(ph(clp::ir::VariablePlaceholder::Integer));
    combined += "% load";

    std::string buffer{"pre:"};
    append_logtype_template(combined, buffer);
    REQUIRE(buffer == "pre:CPU at %int%%% load");
}
