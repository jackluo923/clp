#ifndef CLP_S_SEARCH_ONNXEMBEDDER_HPP
#define CLP_S_SEARCH_ONNXEMBEDDER_HPP

#include <cstddef>
#include <memory>
#include <string>
#include <vector>

#include <onnxruntime_cxx_api.h>

namespace clp_s::search {
/**
 * In-process text embedding using ONNX Runtime with bge-small-en-v1.5.
 *
 * Uses two ONNX models:
 *  - A tokenizer model (with BertTokenizer custom op from onnxruntime-extensions)
 *    that converts raw text strings to input_ids, token_type_ids, attention_mask
 *  - An encoder model that produces last_hidden_state
 *
 * Mean pooling and L2 normalization are performed in C++.
 */
class OnnxEmbedder {
public:
    /**
     * @param model_dir Directory containing tokenizer.onnx, encoder.onnx, and libortextensions.so
     */
    explicit OnnxEmbedder(std::string const& model_dir);

    OnnxEmbedder(OnnxEmbedder const&) = delete;
    OnnxEmbedder& operator=(OnnxEmbedder const&) = delete;
    OnnxEmbedder(OnnxEmbedder&&) = delete;
    OnnxEmbedder& operator=(OnnxEmbedder&&) = delete;

    /**
     * Generates normalized embeddings for the given texts.
     * Each text is tokenized, encoded, mean-pooled, and L2-normalized.
     * @param texts Vector of input strings to embed
     * @return Vector of embedding vectors (one per input text), each of dimension embedding_dim()
     */
    [[nodiscard]] auto embed(std::vector<std::string> const& texts) const
            -> std::vector<std::vector<float>>;

    /**
     * @return The embedding dimensionality produced by the model (e.g. 384 for bge-small-en-v1.5)
     */
    [[nodiscard]] auto embedding_dim() const -> size_t { return m_embedding_dim; }

private:
    Ort::Env m_env;
    Ort::SessionOptions m_tokenizer_session_options;
    Ort::SessionOptions m_encoder_session_options;
    std::unique_ptr<Ort::Session> m_tokenizer_session;
    std::unique_ptr<Ort::Session> m_encoder_session;
    size_t m_embedding_dim{0};
};
}  // namespace clp_s::search

#endif  // CLP_S_SEARCH_ONNXEMBEDDER_HPP
