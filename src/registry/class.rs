//! What a model is for, and what it handles beyond plain text.
use crate::gguf::GgufModel;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    #[default]
    Generative,
    /// Classifiers and verifiers: Laya, MiniCheck.
    Decision,
    Embedding,
    Reranker,
}

impl Class {
    pub const ALL: [Self; 4] = [Self::Generative, Self::Decision, Self::Embedding, Self::Reranker];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generative => "generative",
            Self::Decision => "decision",
            Self::Embedding => "embedding",
            Self::Reranker => "reranker",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == name)
    }

    /// From the task tags a GGUF carries in `general.tags`.
    pub fn from_tags(tags: &[String]) -> Self {
        tags.iter().find_map(|tag| match tag.as_str() {
            "text-classification" | "zero-shot-classification" => Some(Self::Decision),
            "feature-extraction" | "sentence-similarity" => Some(Self::Embedding),
            "text-ranking" => Some(Self::Reranker),
            _ => None,
        }).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Reasoning,
    Tools,
    Vision,
    Audio,
}

impl Capability {
    pub const ALL: [Self; 4] = [Self::Reasoning, Self::Tools, Self::Vision, Self::Audio];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Tools => "tools",
            Self::Vision => "vision",
            Self::Audio => "audio",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|capability| capability.as_str() == name)
    }

    pub fn names(capabilities: &[Self]) -> Vec<String> {
        capabilities.iter().map(|c| c.as_str().to_owned()).collect()
    }

    /// What the model does through `Synthesize`: thinking before answering.
    pub fn from_template(template: &str) -> Vec<Self> {
        if template.contains("<think>") { vec![Self::Reasoning] } else { Vec::new() }
    }
}

/// A native GGUF's class and capabilities.
pub fn detect(gguf: &GgufModel) -> (Class, Vec<Capability>) {
    let class = Class::from_tags(&gguf.metadata_string_array("general.tags").unwrap_or_default());
    let capabilities = gguf.metadata_str("tokenizer.chat_template").map(Capability::from_template).unwrap_or_default();
    (class, capabilities)
}

#[cfg(test)]
mod tests;
