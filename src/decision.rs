//! What a decision model accepts through `Decide`, and how it answers.
use crate::gguf::GgufModel;
use crate::registry::Class;
use crate::tokenizer::{ChatFormat, Tokenizer};

pub mod minicheck;

/// The input a decision model takes and the answer it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Contract {
    /// A state and typed questions; a distribution per question.
    Laya,
    /// A document and claims; the probability each claim is supported.
    MiniCheck,
}

impl Contract {
    pub const ALL: [Self; 2] = [Self::Laya, Self::MiniCheck];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Laya => "laya",
            Self::MiniCheck => "minicheck",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|contract| contract.as_str() == name)
    }

    /// A native GGUF's contract: Bespoke-MiniCheck's, when its header names
    /// it, it is classed `decision`, and its tokenizer has ChatML and `yes`.
    pub fn detect(gguf: &GgufModel, tokenizer: &Tokenizer, class: Class) -> Option<Self> {
        let named = gguf
            .metadata_str("general.basename")
            .is_some_and(|name| name.eq_ignore_ascii_case("bespoke-minicheck"));
        (named
            && class == Class::Decision
            && tokenizer.chat_format() == Some(ChatFormat::ChatMl)
            && !minicheck::yes_ids(tokenizer).is_empty())
        .then_some(Self::MiniCheck)
    }
}
