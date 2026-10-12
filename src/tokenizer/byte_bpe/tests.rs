use crate::gguf::{GgufModel, tests::Fixture};
use crate::tokenizer::Tokenizer;
use candle_core::quantized::gguf_file::Value;
use tokenizers::pre_tokenizers::byte_level::ByteLevel;

fn fixture(pre: &str) -> Fixture {
    let mut f = Fixture::new(1, 8);
    let mut alphabet: Vec<_> = ByteLevel::alphabet().into_iter().collect();
    alphabet.sort_unstable();
    let mut pieces: Vec<_> = alphabet.into_iter().map(|c| c.to_string()).collect();
    pieces.extend(
        [
            "ab",
            "bc",
            "abc",
            "12",
            "123",
            "1234",
            "<|endoftext|>",
            "<|im_start|>",
            "<|im_end|>",
        ]
        .map(str::to_owned),
    );
    let mut types = vec![Value::I32(1); pieces.len()];
    for t in &mut types[262..] {
        *t = Value::I32(3);
    }
    for (key, value) in [
        ("model", Value::String("gpt2".into())),
        ("pre", Value::String(pre.into())),
        (
            "tokens",
            Value::Array(pieces.into_iter().map(Value::String).collect()),
        ),
        ("token_type", Value::Array(types)),
        (
            "merges",
            Value::Array(
                ["a b", "ab c", "b c", "1 2", "12 3", "123 4"]
                    .map(|s| Value::String(s.into()))
                    .into(),
            ),
        ),
        ("bos_token_id", Value::U32(262)),
        ("eos_token_id", Value::U32(264)),
    ] {
        f.metadata.insert(format!("tokenizer.ggml.{key}"), value);
    }
    f
}

fn tokenizer(f: &Fixture) -> Option<Tokenizer> {
    let file = f.write();
    Tokenizer::from_gguf(&GgufModel::open(file.path()).unwrap())
}

#[test]
fn byte_bpe_preserves_merge_ranks_and_pretokenizer_boundaries() {
    let qwen = tokenizer(&fixture("qwen2")).unwrap();
    assert_eq!(qwen.encode("abc"), [258]);
    assert_eq!(qwen.encode("ab"), [256]);
    let digits = ["1", "2", "3", "4"].map(|s| qwen.piece_to_id[s]);
    assert_eq!(qwen.encode("1234"), digits);
    let glm = tokenizer(&fixture("glm4")).unwrap();
    assert_eq!(glm.encode("1234"), [260, glm.piece_to_id["4"]]);
    assert_eq!(qwen.decode(&[258]), "abc");
    assert!(qwen.encode("").is_empty());
}

#[test]
fn byte_bpe_special_tokens_keep_original_ids_without_added_spaces_or_bos() {
    let mut f = fixture("qwen2");
    let tok = tokenizer(&f).unwrap();
    assert_eq!(tok.encode("<|im_start|>abc<|im_end|>"), [263, 258, 264]);
    assert_eq!(tok.decode(&[263, 258, 264]), "<|im_start|>abc<|im_end|>");
    assert!(tok.is_eos(264));
    assert!(tok.is_eos(262));
    assert!(!tok.is_eos(263));
    f.metadata
        .insert("tokenizer.ggml.add_bos_token".into(), Value::Bool(true));
    assert_eq!(tokenizer(&f).unwrap().encode("abc"), [262, 258]);
}

#[test]
fn byte_bpe_roundtrips_unicode_whitespace_contractions_and_all_bytes() {
    for pre in ["qwen2", "qwen35", "glm4"] {
        let tok = tokenizer(&fixture(pre)).unwrap();
        for text in [
            " hello  world\r\n\n",
            "I'm WE'RE they'll",
            "é e\u{301} 中文 🦀",
            "\0\t\u{7f}",
            "1234567890",
        ] {
            assert_eq!(tok.decode(&tok.encode(text)), text, "{pre}: {text:?}");
        }
        let text: String = (0..=255).filter_map(char::from_u32).collect();
        assert_eq!(tok.decode(&tok.encode(&text)), text);
    }
}

#[test]
fn byte_bpe_streaming_defers_split_utf8_without_corrupting_text() {
    let tok = tokenizer(&fixture("qwen2")).unwrap();
    let text = "é 中文 🦀 <|im_start|>abc";
    let mut decode = tok.stream_decoder();
    let chunks: Vec<_> = tok
        .try_encode(text)
        .unwrap()
        .into_iter()
        .map(|id| decode(id).unwrap())
        .collect();
    assert!(chunks.iter().any(String::is_empty));
    assert_eq!(chunks.concat(), text);
    let mut decode = tok.stream_decoder();
    let ids = tok.encode("🦀");
    assert_eq!(ids.len(), 4);
    for &id in &ids[..3] {
        assert_eq!(decode(id).unwrap(), "");
    }
    assert_eq!(decode(ids[3]).unwrap(), "🦀");
}

#[test]
fn byte_bpe_rejects_unknown_pre_incomplete_alphabet_duplicates_and_bad_merges() {
    assert!(tokenizer(&fixture("unknown")).is_none());
    for key in ["tokenizer.ggml.pre", "tokenizer.ggml.merges"] {
        let mut f = fixture("qwen2");
        f.metadata.remove(key);
        assert!(tokenizer(&f).is_none());
    }
    let mut f = fixture("qwen2");
    if let Value::Array(pieces) = f.metadata.get_mut("tokenizer.ggml.tokens").unwrap() {
        pieces[0] = Value::String("duplicate".into());
    }
    assert!(tokenizer(&f).is_none());
    let mut f = fixture("qwen2");
    if let Value::Array(pieces) = f.metadata.get_mut("tokenizer.ggml.tokens").unwrap() {
        pieces[256] = pieces[257].clone();
    }
    assert!(tokenizer(&f).is_none());
    for merge in ["bad", "a ", " a", "a b c", "a missing"] {
        let mut f = fixture("qwen2");
        f.metadata.insert(
            "tokenizer.ggml.merges".into(),
            Value::Array(vec![Value::String(merge.into())]),
        );
        assert!(tokenizer(&f).is_none(), "{merge:?}");
    }
    let mut f = fixture("qwen2");
    f.metadata
        .insert("tokenizer.ggml.eos_token_id".into(), Value::U32(1000));
    assert!(tokenizer(&f).is_none());
    f = fixture("qwen2");
    f.metadata.remove("tokenizer.ggml.token_type");
    assert!(tokenizer(&f).is_none());
}

#[test]
#[ignore = "requires MDMA_TEST_QWEN_GGUF"]
fn byte_bpe_real_qwen_metadata_and_roundtrip() {
    let path = std::env::var("MDMA_TEST_QWEN_GGUF").expect("set MDMA_TEST_QWEN_GGUF");
    let gguf = GgufModel::open(path).unwrap();
    let tok = Tokenizer::from_gguf(&gguf).expect("real Qwen byte BPE");
    // llama-tokenize IDs for the Qwen3-8B and NuMarkdown-8B checkpoints.
    let cases: [(&str, &[u32]); 3] = [
        ("The capital of France is", &[785, 6722, 315, 9625, 374]),
        (
            "中文 🦀 12345\n",
            &[104811, 11162, 99, 222, 220, 16, 17, 18, 19, 20, 198],
        ),
        (
            "<|im_start|>user\nHi<|im_end|>\n",
            &[151644, 872, 198, 13048, 151645, 198],
        ),
    ];
    for (text, expected) in cases {
        let ids = tok.try_encode(text).unwrap();
        assert_eq!(ids, expected, "llama.cpp tokenizer parity: {text:?}");
        assert_eq!(tok.decode(&ids), text);
        eprintln!("{text:?}: {ids:?}");
    }
}
