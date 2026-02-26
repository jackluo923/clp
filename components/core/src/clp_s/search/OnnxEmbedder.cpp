#include "OnnxEmbedder.hpp"

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <stdexcept>
#include <string>
#include <vector>

#include <onnxruntime_cxx_api.h>
#include <spdlog/spdlog.h>

namespace clp_s::search {
OnnxEmbedder::OnnxEmbedder(std::string const& model_dir)
        : m_env(ORT_LOGGING_LEVEL_WARNING, "clp_s_embedder") {
    auto const tokenizer_path = model_dir + "/tokenizer.onnx";
    auto const encoder_path = model_dir + "/encoder.onnx";
    auto const ortextensions_path = model_dir + "/libortextensions.so";

    // Tokenizer session needs custom ops for BertTokenizer
    m_tokenizer_session_options.SetIntraOpNumThreads(1);
    m_tokenizer_session_options.RegisterCustomOpsLibrary(ortextensions_path.c_str());

    // Encoder session uses standard ONNX ops only
    m_encoder_session_options.SetIntraOpNumThreads(1);
    m_encoder_session_options.SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);

    m_tokenizer_session
            = std::make_unique<Ort::Session>(m_env, tokenizer_path.c_str(), m_tokenizer_session_options);
    m_encoder_session
            = std::make_unique<Ort::Session>(m_env, encoder_path.c_str(), m_encoder_session_options);

    // Determine embedding dimension from encoder output shape: [batch, seq_len, hidden_dim]
    auto output_info = m_encoder_session->GetOutputTypeInfo(0);
    auto tensor_info = output_info.GetTensorTypeAndShapeInfo();
    auto shape = tensor_info.GetShape();
    if (shape.size() >= 3 && shape[2] > 0) {
        m_embedding_dim = static_cast<size_t>(shape[2]);
    } else {
        throw std::runtime_error("Cannot determine embedding dimension from encoder output shape");
    }

    SPDLOG_INFO("OnnxEmbedder initialized: model_dir={}, embedding_dim={}", model_dir, m_embedding_dim);
}

auto OnnxEmbedder::embed(std::vector<std::string> const& texts) const
        -> std::vector<std::vector<float>> {
    if (texts.empty()) {
        return {};
    }

    Ort::AllocatorWithDefaultOptions allocator;
    std::vector<std::vector<float>> result;
    result.reserve(texts.size());

    // Process each text individually (tokenizer produces variable-length sequences)
    for (auto const& text : texts) {
        // Step 1: Tokenize - input is a single string tensor
        int64_t const tok_input_shape[] = {1};
        auto tok_input = Ort::Value::CreateTensor(
                allocator,
                tok_input_shape,
                1,
                ONNX_TENSOR_ELEMENT_DATA_TYPE_STRING
        );
        tok_input.FillStringTensorElement(text.c_str(), 0);

        char const* tok_input_names[] = {"text"};
        char const* tok_output_names[] = {"input_ids", "token_type_ids", "attention_mask"};

        auto tok_outputs = m_tokenizer_session->Run(
                Ort::RunOptions{nullptr},
                tok_input_names,
                &tok_input,
                1,
                tok_output_names,
                3
        );

        // Tokenizer outputs are 1D: [seq_len]
        auto const* input_ids_data = tok_outputs[0].GetTensorData<int64_t>();
        auto const* token_type_ids_data = tok_outputs[1].GetTensorData<int64_t>();
        auto const* attention_mask_data = tok_outputs[2].GetTensorData<int64_t>();

        auto tok_shape = tok_outputs[0].GetTensorTypeAndShapeInfo().GetShape();
        int64_t const seq_len = tok_shape[0];

        // Step 2: Encode - reshape to [1, seq_len] for the encoder
        int64_t const enc_shape[] = {1, seq_len};
        auto enc_input_ids = Ort::Value::CreateTensor<int64_t>(
                allocator.GetInfo(),
                const_cast<int64_t*>(input_ids_data),
                static_cast<size_t>(seq_len),
                enc_shape,
                2
        );
        auto enc_attention_mask = Ort::Value::CreateTensor<int64_t>(
                allocator.GetInfo(),
                const_cast<int64_t*>(attention_mask_data),
                static_cast<size_t>(seq_len),
                enc_shape,
                2
        );
        auto enc_token_type_ids = Ort::Value::CreateTensor<int64_t>(
                allocator.GetInfo(),
                const_cast<int64_t*>(token_type_ids_data),
                static_cast<size_t>(seq_len),
                enc_shape,
                2
        );

        char const* enc_input_names[] = {"input_ids", "attention_mask", "token_type_ids"};
        char const* enc_output_names[] = {"last_hidden_state"};
        std::array<Ort::Value, 3> enc_inputs{
                std::move(enc_input_ids),
                std::move(enc_attention_mask),
                std::move(enc_token_type_ids)
        };

        auto enc_outputs = m_encoder_session->Run(
                Ort::RunOptions{nullptr},
                enc_input_names,
                enc_inputs.data(),
                3,
                enc_output_names,
                1
        );

        // Output shape: [1, seq_len, hidden_dim]
        auto const* hidden_data = enc_outputs[0].GetTensorData<float>();
        auto const hidden_dim = m_embedding_dim;

        // Step 3: Mean pooling (using attention mask to exclude padding)
        std::vector<float> embedding(hidden_dim, 0.0f);
        float mask_sum = 0.0f;
        for (int64_t t = 0; t < seq_len; ++t) {
            auto mask_val = static_cast<float>(attention_mask_data[t]);
            mask_sum += mask_val;
            for (size_t d = 0; d < hidden_dim; ++d) {
                embedding[d] += hidden_data[static_cast<size_t>(t) * hidden_dim + d] * mask_val;
            }
        }
        if (mask_sum > 0.0f) {
            for (size_t d = 0; d < hidden_dim; ++d) {
                embedding[d] /= mask_sum;
            }
        }

        // Step 4: L2 normalization
        float l2_sq = 0.0f;
        for (size_t d = 0; d < hidden_dim; ++d) {
            l2_sq += embedding[d] * embedding[d];
        }
        float const l2_norm = std::sqrt(l2_sq);
        if (l2_norm > 1e-12f) {
            for (size_t d = 0; d < hidden_dim; ++d) {
                embedding[d] /= l2_norm;
            }
        }

        result.push_back(std::move(embedding));
    }

    return result;
}
}  // namespace clp_s::search
