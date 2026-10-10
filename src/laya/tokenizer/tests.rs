use super::*;
use serde_json::json;

pub(crate) fn checkpoint() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let tokens = ["[UNK]", "[PAD]", "[CLS]", "[SEP]", "[MASK]"];
    let added: Vec<_> = tokens
        .iter()
        .enumerate()
        .map(|(id, token)| {
            json!({
                "id": id, "content": token, "single_word": false, "lstrip": false,
                "rstrip": false, "normalized": false, "special": true
            })
        })
        .collect();
    let tokenizer = json!({
        "version": "1.0",
        "truncation": {"direction": "Right", "max_length": 3,
            "strategy": "LongestFirst", "stride": 0},
        "padding": {"strategy": {"Fixed": 10}, "direction": "Right",
            "pad_to_multiple_of": null, "pad_id": 1, "pad_type_id": 0, "pad_token": "[PAD]"},
        "added_tokens": added,
        "normalizer": {"type": "BertNormalizer", "clean_text": true,
            "handle_chinese_chars": true, "strip_accents": null, "lowercase": true},
        "pre_tokenizer": {"type": "BertPreTokenizer"},
        "post_processor": {"type": "BertProcessing", "sep": ["[SEP]", 3], "cls": ["[CLS]", 2]},
        "decoder": {"type": "WordPiece", "prefix": "##", "cleanup": true},
        "model": {"type": "WordPiece", "unk_token": "[UNK]",
            "continuing_subword_prefix": "##", "max_input_chars_per_word": 100,
            "vocab": {"[UNK]": 0, "[PAD]": 1, "[CLS]": 2, "[SEP]": 3, "[MASK]": 4,
                "hello": 5, "world": 6, "!": 7}}
    });
    std::fs::write(dir.path().join("tokenizer.json"), tokenizer.to_string()).unwrap();
    std::fs::write(
        dir.path().join("tokenizer_config.json"),
        json!({
            "cls_token": "[CLS]", "sep_token": "[SEP]", "pad_token": "[PAD]",
            "mask_token": {"content": "[MASK]"}
        })
        .to_string(),
    )
    .unwrap();
    dir
}

#[test]
fn legacy_checkpoint_converts_without_writing_or_padding_truncation_and_special_frames() {
    let dir = checkpoint();
    let path = dir.path().join("tokenizer.json");
    let original = std::fs::read(&path).unwrap();
    let tokenizer = Tokenizer::open(dir.path()).unwrap();
    assert_eq!(
        (
            tokenizer.cls_id,
            tokenizer.sep_id,
            tokenizer.pad_id,
            tokenizer.mask_id
        ),
        (2, 3, 1, 4)
    );
    assert_eq!(
        tokenizer.encode("HELLO world! hello world!").unwrap(),
        [5, 6, 7, 5, 6, 7]
    );
    assert_eq!(tokenizer.encode("[MASK]").unwrap(), [4]);
    assert!(tokenizer.encode("").unwrap().is_empty());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn valid_small_head_budgets_do_not_underflow_during_prompt_preparation() {
    let dir = checkpoint();
    let tokenizer = Tokenizer::open(dir.path()).unwrap();
    let question = crate::laya::prompt::Question::from_json(&json!({
        "type": "choice", "instructions": "hello", "criteria": ["hello", "world"]
    })).unwrap();
    for head_max_len in 5..16 {
        let sequence = crate::laya::prompt::build_sequence(
            &tokenizer, &json!({}), &question, 16, head_max_len,
        ).unwrap();
        assert!(sequence.ids.len() <= 16);
        assert_eq!(sequence.markers.len(), 2);
    }
}

#[test]
fn canonical_checkpoint_has_the_same_ids() {
    let dir = checkpoint();
    let path = dir.path().join("tokenizer.json");
    let canonical = tokenizers::canonicalize_file(&path).unwrap();
    std::fs::write(&path, &canonical).unwrap();
    let tokenizer = Tokenizer::open(dir.path()).unwrap();
    assert_eq!(tokenizer.encode("Hello world!").unwrap(), [5, 6, 7]);
    assert_eq!(std::fs::read_to_string(path).unwrap(), canonical);
}

#[test]
fn special_ids_can_come_from_the_model_vocabulary() {
    let dir = checkpoint();
    let path = dir.path().join("tokenizer.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["added_tokens"] = json!([]);
    std::fs::write(path, config.to_string()).unwrap();
    let tokenizer = Tokenizer::open(dir.path()).unwrap();
    assert_eq!(
        (
            tokenizer.cls_id,
            tokenizer.sep_id,
            tokenizer.pad_id,
            tokenizer.mask_id
        ),
        (2, 3, 1, 4)
    );
}

#[test]
fn missing_special_and_invalid_checkpoint_are_errors() {
    let dir = checkpoint();
    let path = dir.path().join("tokenizer_config.json");
    std::fs::write(path, r#"{"cls_token":"[CLS]"}"#).unwrap();
    assert!(Tokenizer::open(dir.path()).is_err());
    std::fs::write(dir.path().join("tokenizer.json"), "{}").unwrap();
    assert!(Tokenizer::open(dir.path()).is_err());
}
