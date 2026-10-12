use super::*;

#[test]
fn ttu_rewinds_inner_levels() {
    let f = ttu("in_", &[1, 72, 72, 0], &[72, 1, 1, 3], 8, "cnt");
    assert_eq!(f.get("in_inc0"), Some(1));
    assert_eq!(f.get("in_cnt0"), Some(71));
    assert_eq!(f.get("in_inc1"), Some(72 - 71));
    assert_eq!(f.get("in_inc3"), Some(0 - 71));
    assert_eq!(f.get("in_cnt3"), Some(2));
    assert_eq!(f.get("in_inc7"), Some(-71));
    assert_eq!(f.get("in_cnt7"), Some(0));
    let lim = ttu("w_", &[1], &[3], 4, "lim");
    assert_eq!(lim.get("w_lim0"), Some(2));
    assert_eq!(lim.get("w_lim3"), Some(0));
}

#[test]
fn layouts_round_trip_and_fill_gaps() {
    let layout = Layout::new("t", &[0x21], 2, vec![spec("code", 14, 5), sspec("delta", 130, 17), spec("flag", 255, 1)], Fields::new().set("flag", 1));
    let names: Vec<&str> = layout.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["gate", "pred_reg", "pred_pol", "rsv5", "opcode", "rsv12", "code", "rsv19", "delta", "rsv147", "flag"]);
    let words = layout.encode(&Fields::new().set("code", 4).set("delta", -3));
    assert_eq!(opcode(words[0]), 0x21);
    assert_eq!(words[0] >> 14 & 0x1f, 4);
    assert_eq!(words[1] >> 2 & 0x1ffff, (1 << 17) - 3);
    assert_eq!(words[1] >> 127, 1, "default flag");
    let decoded = layout.decode(&words);
    assert_eq!(decoded.get("delta"), Some(-3));
    assert_eq!(decoded.get("code"), Some(4));
    assert_eq!(decoded.get("flag"), Some(1));
    assert_eq!(layout.encode(&decoded), words, "encode(decode) is the identity");
}

#[test]
#[should_panic(expected = "t.code=32 does not fit 5 bits")]
fn values_must_fit() {
    Layout::new("t", &[0x21], 1, vec![spec("code", 14, 5)], Fields::new()).encode(&Fields::new().set("code", 32));
}

#[test]
fn words_and_bytes() {
    let words = vec![0x0123_4567_89ab_cdef_u128, u128::MAX];
    assert_eq!(bytes_to_words(&words_to_bytes(&words)), words);
    assert_eq!(neg(5, 14), (1 << 14) - 5);
    assert_eq!(neg(0, 14), 0);
    assert_eq!(f32_bits(1.0), 0x3f80_0000);
    assert_eq!(length(0x01), 17);
    assert_eq!(length(0x16), 6);
    assert_eq!(length(0x3e), 1);
}
