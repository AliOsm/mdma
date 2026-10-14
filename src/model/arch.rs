//! Model-specific tensor layouts, configuration, and layer execution.
pub(super) mod gemma4;
pub(super) mod glm4;
pub(super) mod llama;
pub mod modernbert;
pub(super) mod qwen;
pub(super) mod qwen35;

pub(super) enum Decoder<B: crate::backend::Backend> {
    Llama(llama::Llama<B>),
    Gemma4(gemma4::Gemma<B>),
    Qwen(qwen::Qwen<B>),
    Qwen35(qwen35::Qwen35<B>),
    Glm4(glm4::Glm4<B>),
}
