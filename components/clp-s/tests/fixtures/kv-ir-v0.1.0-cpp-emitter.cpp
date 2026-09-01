#include <cstdint>
#include <fstream>
#include <string>
#include <string_view>
#include <vector>

#include <msgpack.hpp>
#include <nlohmann/json.hpp>

#include <clp/ffi/ir_stream/Serializer.hpp>
#include <clp/ffi/ir_stream/protocol_constants.hpp>
#include <clp/ir/types.hpp>
#include <clp/type_utils.hpp>

namespace {
template <typename EncodedVariable>
auto emit(std::string_view path) -> bool {
    nlohmann::json const metadata{{"fixture", "rust-kv-ir-reader-v1"}};
    auto serializer_result
            = clp::ffi::ir_stream::Serializer<EncodedVariable>::create(metadata);
    if (serializer_result.has_error()) {
        return false;
    }
    auto& serializer{serializer_result.value()};
    serializer.change_utc_offset(clp::UtcOffset{3'600'000});

    nlohmann::json const auto_generated{{"level", "info"}, {"seq", 7}};
    nlohmann::json const user_generated{
            {"empty", nlohmann::json::object()},
            {"message", "task 42 done"},
            {"none", nullptr},
            {"ok", true},
            {"ratio", 1.25},
    };
    auto const auto_bytes{nlohmann::json::to_msgpack(auto_generated)};
    auto const user_bytes{nlohmann::json::to_msgpack(user_generated)};
    auto const auto_handle{msgpack::unpack(
            clp::size_checked_pointer_cast<char const>(auto_bytes.data()),
            auto_bytes.size()
    )};
    auto const user_handle{msgpack::unpack(
            clp::size_checked_pointer_cast<char const>(user_bytes.data()),
            user_bytes.size()
    )};
    auto const& auto_object{auto_handle.get()};
    auto const& user_object{user_handle.get()};
    if (serializer.serialize_msgpack_map(auto_object.via.map, user_object.via.map).has_error()) {
        return false;
    }

    auto const bytes{serializer.get_ir_buf_view()};
    std::ofstream output{std::string{path}, std::ios::binary};
    output.write(reinterpret_cast<char const*>(bytes.data()), static_cast<std::streamsize>(bytes.size()));
    output.put(static_cast<char>(clp::ffi::ir_stream::cProtocol::Eof));
    return output.good();
}
}  // namespace

auto main(int argc, char** argv) -> int {
    if (argc != 3) {
        return 2;
    }
    if (false == emit<clp::ir::four_byte_encoded_variable_t>(argv[1])) {
        return 1;
    }
    return emit<clp::ir::eight_byte_encoded_variable_t>(argv[2]) ? 0 : 1;
}
