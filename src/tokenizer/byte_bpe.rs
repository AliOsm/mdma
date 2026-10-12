//! GGUF byte-level BPE using the established tokenizers implementation.
use crate::gguf::GgufModel;
use std::collections::BTreeMap;
use tokenizers::SplitDelimiterBehavior;
use tokenizers::models::bpe::{BpeConfig, PipelineBPE, Vocab};
use tokenizers::pipeline::{
    NormalizerChain, PipelineModel, PipelinePostProcessor, PipelinePreTokenizer, PipelineTokenizer,
};
use tokenizers::pre_tokenizers::{
    byte_level::ByteLevel,
    split::{Split, SplitPattern},
};
use tokenizers::vocab::bucket_added_vocabulary::{AddedToken, AddedVocabulary};

const QWEN2: &str = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";
const QWEN35: &str = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?[\p{L}\p{M}]+|\p{N}| ?[^\s\p{L}\p{N}\p{M}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";
const GLM4: &str = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";

pub(super) fn from_gguf(
    g: &GgufModel,
    pieces: &[String],
    specials: &[(String, u32)],
) -> Option<PipelineTokenizer> {
    let pattern = match g.metadata_str("tokenizer.ggml.pre")? {
        "qwen2" => QWEN2,
        "qwen35" => QWEN35,
        "glm4" => GLM4,
        _ => return None,
    };
    let vocab: Vocab = pieces
        .iter()
        .enumerate()
        .map(|(id, p)| (p.clone(), id as u32))
        .collect();
    if vocab.len() != pieces.len()
        || pieces.iter().any(String::is_empty)
        || ByteLevel::alphabet()
            .iter()
            .any(|c| !vocab.contains_key(&c.to_string()))
    {
        return None;
    }
    let merges = g
        .metadata_string_array("tokenizer.ggml.merges")?
        .into_iter()
        .map(|pair| {
            let (a, b) = pair.split_once(' ')?;
            if a.is_empty()
                || b.is_empty()
                || b.contains(' ')
                || !vocab.contains_key(a)
                || !vocab.contains_key(b)
                || !vocab.contains_key(&format!("{a}{b}"))
            {
                return None;
            }
            Some((a.to_owned(), b.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    let mut added = AddedVocabulary::new();
    let mut special_tokens: Vec<_> = specials.iter().collect();
    special_tokens.sort_unstable_by_key(|(_, id)| *id);
    added
        .add_special_tokens(
            special_tokens
                .into_iter()
                .map(|(p, _)| AddedToken::from(p.clone(), true).normalized(false)),
            pieces.len(),
            |token| vocab.get(token).copied(),
            None::<&NormalizerChain<'_>>,
        )
        .ok()?;
    if specials
        .iter()
        .any(|(p, id)| added.token_to_id(p) != Some(*id))
    {
        return None;
    }
    let model = PipelineBPE::from_config(BpeConfig {
        vocab,
        merges,
        byte_level: true,
        ..Default::default()
    })
    .ok()?;
    let split = Split::new(
        SplitPattern::Regex(pattern.into()),
        SplitDelimiterBehavior::Isolated,
        false,
    )
    .ok()?;
    // Byte-level conversion belongs to the model in v1; only the regex
    // boundary split remains in the pre-tokenizer. Decode uses raw vocab bytes.
    Some(PipelineTokenizer::from_parts(
        added,
        Vec::new(),
        PipelinePreTokenizer::Split(split),
        PipelineModel::BPE(model),
        PipelinePostProcessor::default(),
        None,
        BTreeMap::default(),
        None,
        None,
    ))
}

#[cfg(test)]
mod tests;
