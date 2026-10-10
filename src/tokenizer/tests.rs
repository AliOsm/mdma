use super::Tokenizer;
use crate::backend::test_support::fixture;
use crate::gguf::GgufModel;

pub(crate) fn tiny_tokenizer() -> Tokenizer {
    tiny_tokenizer_with_eos(None)
}

pub(crate) fn tiny_tokenizer_with_stop(stop_id: u32) -> Tokenizer {
    let mut tokenizer = tiny_tokenizer();
    tokenizer.stop_ids.push(stop_id);
    tokenizer
}

pub(crate) fn tiny_tokenizer_with_decode_piece(id: u32, piece: String) -> Tokenizer {
    let mut tokenizer = tiny_tokenizer();
    // Deliberately adversarial decode table for oversized native event tests.
    // Encoding still uses the original byte-to-ID map.
    tokenizer.pieces[id as usize] = piece;
    tokenizer
}

pub(crate) fn tiny_tokenizer_with_eos(eos_id: Option<u32>) -> Tokenizer {
    let pieces: Vec<String> = (0u8..128).map(|b| (b as char).to_string()).collect();
    let piece_to_id = pieces.iter().enumerate().map(|(id, piece)| (piece.clone(), id as u32)).collect();
    Tokenizer {
        pieces, piece_to_id, scores: vec![0.0; 128],
        bos_id: None, eos_id, unk_id: None,
        add_bos: false, add_space_prefix: false,
        merge_ranks: None, specials: Vec::new(), stop_ids: Vec::new(), byte_bpe: None,
    }
}

fn load() -> Tokenizer {
    let model = GgufModel::open(fixture()).expect("open gguf");
    Tokenizer::from_gguf(&model).expect("build tokenizer")
}

fn strip_bos(tok: &Tokenizer, ids: Vec<u32>) -> Vec<u32> {
    if ids.first().copied() == tok.bos_id { ids[1..].to_vec() } else { ids }
}

fn gemma_tokenizer(merges: &[&str]) -> Option<Tokenizer> {
    use crate::gguf::tests::Fixture;
    use candle_core::quantized::gguf_file::Value;

    let pieces = ["a", "b", "c", "ab", "ac", "bc", "abc", "é", "🦀", "é🦀"];
    let mut fixture = Fixture::new(1, 8);
    for (key, value) in [
        ("model", Value::String("gemma4".into())),
        ("tokens", Value::Array(pieces.map(|piece| Value::String(piece.into())).into())),
        ("scores", Value::Array(vec![Value::F32(0.0); pieces.len()])),
        ("merges", Value::Array(merges.iter().map(|pair| Value::String((*pair).into())).collect())),
        ("add_bos_token", Value::Bool(false)),
        ("add_space_prefix", Value::Bool(false)),
    ] {
        fixture.metadata.insert(format!("tokenizer.ggml.{key}"), value);
    }
    let file = fixture.write();
    Tokenizer::from_gguf(&GgufModel::open(file.path()).unwrap())
}

#[test]
fn gemma_merge_lookup_preserves_pairs_ranks_and_duplicate_order() {
    let tokenizer = gemma_tokenizer(&["a b", "b c", "a c", "ab c", "a b", "é 🦀"]).unwrap();
    assert_eq!(tokenizer.encode("ab"), [3]);
    assert_eq!(tokenizer.encode("ac"), [4]);
    assert_eq!(tokenizer.encode("bc"), [5]);
    // The repeated "a b" has rank 4, so "b c" wins; "a bc" is not a merge.
    assert_eq!(tokenizer.encode("abc"), [0, 5]);
    assert_eq!(tokenizer.encode("é🦀"), [9]);
    let tokenizer = gemma_tokenizer(&["a b", "ab c", "b c"]).unwrap();
    assert_eq!(tokenizer.encode("abc"), [6]);
}

#[test]
fn gemma_missing_merges_do_not_fall_back_to_sentencepiece_scores() {
    let tokenizer = gemma_tokenizer(&[]).unwrap();
    assert_eq!(tokenizer.encode("abc"), [0, 1, 2]);
    assert!(gemma_tokenizer(&["missing-separator"]).is_none());
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn encode_decode_roundtrip_ascii() {
    let tok = load();
    let text = "The quick brown fox jumps over the lazy dog.";
    let ids = strip_bos(&tok, tok.encode(text));
    assert!(!ids.is_empty());
    assert_eq!(tok.decode(&ids), text);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn matches_llama_cpp_reference_ids() {
    let tok = load();
    let ids = tok.encode("The quick brown fox jumps over the lazy dog.");
    let expected: Vec<u32> = vec![1, 918, 4131, 14018, 38648, 34256, 1053, 410, 15810, 5718, 281];
    assert_eq!(ids, expected);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn unknown_bytes_fall_back_to_byte_tokens() {
    let tok = load();
    let text = "😀";
    let ids = strip_bos(&tok, tok.encode(text));
    assert!(!ids.is_empty());
    assert_eq!(tok.decode(&ids), text);
}
