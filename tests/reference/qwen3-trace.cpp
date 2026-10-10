// Optional fixture collector; see doc/architectures.md. Not linked into MDMA.
#include "llama.h"
#include "ggml-backend.h"
#include <filesystem>
#include <fstream>
#include <iostream>
#include <memory>
#include <string>
#include <vector>

struct trace_state {
    std::filesystem::path directory;
    unsigned position = 0;
};

static bool trace_tensor(ggml_tensor * tensor, bool ask, void * user) {
    const std::string name = tensor->name;
    const bool selected = name.starts_with("l_out-") || name.starts_with("attn_norm-")
        || name.starts_with("Qcur") || name.starts_with("Kcur") || name.starts_with("Vcur")
        || name.starts_with("ffn_") || name.starts_with("result_");
    const bool eligible = selected && tensor->type == GGML_TYPE_F32 && ggml_is_contiguous(tensor);
    if (ask) return eligible;
    if (!eligible) return true;
    const auto & state = *static_cast<trace_state *>(user);
    const auto directory = state.directory / std::to_string(state.position);
    std::filesystem::create_directories(directory);
    const auto path = directory / (name + "." + ggml_op_name(tensor->op) + ".f32");
    std::vector<char> bytes(ggml_nbytes(tensor));
    ggml_backend_tensor_get(tensor, bytes.data(), 0, bytes.size());
    std::ofstream file(path, std::ios::binary);
    file.write(bytes.data(), bytes.size());
    if (!file) throw std::runtime_error("cannot write " + path.string());
    return true;
}

int main(int argc, char ** argv) try {
    if (argc < 5) {
        std::cerr << "usage: qwen3-trace MODEL.gguf LLAMA_LIB_DIR OUTPUT_DIR TOKEN_ID...\n";
        return 1;
    }
    std::vector<llama_token> tokens;
    for (int i = 4; i < argc; ++i) {
        size_t consumed;
        const auto value = std::stoul(argv[i], &consumed);
        if (argv[i][consumed] || value > INT32_MAX) throw std::runtime_error("invalid token ID");
        tokens.push_back(static_cast<llama_token>(value));
    }
    if (tokens.size() > 128) throw std::runtime_error("fixture exceeds context 128");
    ggml_backend_load_all_from_path(argv[2]);
    llama_backend_init();
    auto model_params = llama_model_default_params();
    model_params.n_gpu_layers = 999;
    std::unique_ptr<llama_model, decltype(&llama_model_free)> model(
        llama_model_load_from_file(argv[1], model_params), llama_model_free);
    if (!model) throw std::runtime_error("model load failed");
    const auto * vocab = llama_model_get_vocab(model.get());
    for (const auto token : tokens) {
        if (token >= llama_vocab_n_tokens(vocab)) throw std::runtime_error("token outside vocabulary");
    }
    trace_state state{argv[3]};
    auto params = llama_context_default_params();
    params.n_ctx = 128;
    params.n_batch = 128;
    params.n_ubatch = 1;
    params.n_threads = 1;
    params.n_threads_batch = 1;
    params.type_k = GGML_TYPE_F16;
    params.type_v = GGML_TYPE_F16;
    params.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_DISABLED;
    params.cb_eval = trace_tensor;
    params.cb_eval_user_data = &state;
    std::unique_ptr<llama_context, decltype(&llama_free)> context(
        llama_init_from_model(model.get(), params), llama_free);
    if (!context) throw std::runtime_error("context allocation failed");
    for (auto & token : tokens) {
        if (llama_decode(context.get(), llama_batch_get_one(&token, 1))) {
            throw std::runtime_error("decode failed");
        }
        ++state.position;
    }
    context.reset();
    model.reset();
    llama_backend_free();
    std::cout << "saved " << tokens.size() << " sequential token traces\n";
    return 0;
} catch (const std::exception & error) {
    std::cerr << error.what() << '\n';
    return 1;
}
