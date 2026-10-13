# Laya typed decisions (CPU)

`mdma-laya` runs a [Laya](https://huggingface.co/convaiinnovations/laya)
checkpoint the way [laya-mlx](https://github.com/mizorewww/laya-mlx) does:
a ModernBERT-large encoder plus decision heads that score one `[MASK]`
marker per option, so a request is a state and a set of `choice` /
`score` / `noul` questions and the reply is a distribution per question.
No tokens are generated. Prompt construction, the [0.5, 5.0] temperature
clamp, confidence and the output shape follow laya-mlx. Laya is one of the
`Decide` contracts; MiniCheck's is in [serving](serving.md#grpc-api).

**Known issue:** the published `convaiinnovations/laya` checkpoint does not
currently load. `tokenizers` 1.0.0-rc.2 requires all 256 ByteLevel byte atoms
and rejects its vocabulary (`Byte atom 0xC0 not found in the vocabulary`), so
the checkpoint tests below fail at load. Generated conformance checkpoints
exercise the runtime but do not qualify real checkpoints; validate tokenizer
compatibility before deployment.

```sh
MDMA_LAYA_PATH=~/.cache/huggingface/hub/models--convaiinnovations--laya/snapshots/<rev> \
cargo run --release --bin mdma-laya -- request.json
# request.json: {"state": <string|object|array>, "questions": {"<id>": {"type", "instructions", "criteria"}}}
```

The checkpoint directory holds `model.safetensors`, `rl_agent_config.json`,
`encoder/config.json` and `tokenizer/`; the tokenizer goes through the
`tokenizers` crate, the weights through `safetensors`. CPU backend only for
now: the encoder ops (`layernorm`, `gelu`, `rope_neox`, `attn_bidir`, ...)
are `Backend` methods that default to `NotImplemented` on the GPU backends.

The reusable encoder is `model::arch::modernbert::ModernBert`; Laya owns
the decision heads, prompting, and calibration. `laya::EncoderConfig` and
`laya::weights` re-export the shared configuration and safetensors loader.
Their errors convert into `laya::Error` with `?`.

## Checkpoint tests

```sh
# convaiinnovations/laya, revision 1c5edc17a7acd8701df6fc341c0d179f1c62c982
MDMA_LAYA_PATH="$HOME/models/laya" \
  cargo test --release --lib laya::tests -- --ignored --test-threads=1
```

Laya gates fail if the checkpoint is missing; they do not silently skip.
The stored fixtures hold 16 reference cases and 63 questions covering tokenizer
IDs, marker/action logits, calibrated answers, batching, padding, multilingual
prompts, and 512-token truncation. They currently fail at checkpoint load; see
the known issue above.
