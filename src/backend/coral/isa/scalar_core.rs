//! The scalar core's compute instructions, as edgetpu_compiler emits them when it
//! runs SOFTMAX on the scalar core. Every word is a 0x20 bundle in the `WORD`
//! field view with these slots:
//!   scalar ALU    `s_op` (bit 5 = immediate form, `& 0x1f` the operation: 0x01-0x0f
//!                 integer add sub and or xor shl shr asr eq ne gt lt ge ges mov,
//!                 0x10-0x16 float32 i2f f2i fadd fsub fmul fmax fmin), `s_x`, `s_y`,
//!                 the immediate; i2f / f2i take the source register in the immediate.
//!   memory        `enable_vector` 2: load `s[vs_reg_v1] = smem32[s[imm_size]]`, 3: the
//!                 same with a post-increment; `v_op` 4: store `smem32[s[v_offset]] = s[vs_reg]`.
//!   special unit  `v_op_2` 2 (reciprocal) / 3 (exp2), source `vs_reg_w`, destination `unk_3`.
//!   0x23 loop     repeat the next `body` words `count` times (one word).
//!   0x22 branch   one word: a relative word offset, taken while p0 is set.

use super::f32_bits;
use super::fields;
use super::scalar::WORD;

pub const ADD: i64 = 0x01;
pub const SUB: i64 = 0x02;
pub const AND: i64 = 0x03;
pub const OR: i64 = 0x04;
pub const SHL: i64 = 0x06;
pub const LT: i64 = 0x0c;
pub const MOV: i64 = 0x0f;
pub const I2F: i64 = 0x10;
pub const F2I: i64 = 0x11;
pub const FADD: i64 = 0x12;
pub const FSUB: i64 = 0x13;
pub const FMUL: i64 = 0x14;
pub const FMAX: i64 = 0x15;
pub const FMIN: i64 = 0x16;

pub const RECIP: i64 = 2;
pub const EXP2: i64 = 3;

fn word(f: &[(&str, i64)]) -> u128 {
    WORD.encode(&fields(f).set("enable_scalar", 1))[0]
}

/// `s_x = s_y <op> imm`.
pub fn alu_imm(op: i64, x: i64, y: i64, imm: i64) -> u128 {
    word(&[
        ("s_op", op | 0x20),
        ("s_x", x),
        ("s_y", y),
        ("imm_scalar", imm & 0xffff_ffff),
    ])
}

/// `s_x = s_y <op> s_reg`.
pub fn alu_reg(op: i64, x: i64, y: i64, reg: i64) -> u128 {
    word(&[("s_op", op), ("s_x", x), ("s_y", y), ("imm_scalar", reg)])
}

pub fn mov(x: i64, reg: i64) -> u128 {
    alu_reg(MOV, x, 0, reg)
}

pub fn movi(x: i64, imm: i64) -> u128 {
    alu_imm(MOV, x, 0, imm)
}

/// A float op with an f32 immediate.
pub fn fimm(op: i64, x: i64, y: i64, v: f32) -> u128 {
    alu_imm(op, x, y, f32_bits(v))
}

pub fn i2f(x: i64, src: i64) -> u128 {
    alu_reg(I2F, x, 0, src)
}

pub fn f2i(x: i64, src: i64) -> u128 {
    alu_reg(F2I, x, 0, src)
}

/// `s_x = smem32[s_addr]`.
pub fn ld(x: i64, addr: i64) -> u128 {
    word(&[("enable_vector", 2), ("vs_reg_v1", x), ("imm_size", addr)])
}

/// `smem32[s_addr] = s_val`.
pub fn st(addr: i64, val: i64) -> u128 {
    word(&[("v_op", 4), ("v_offset", addr), ("vs_reg", val)])
}

/// `s_x = f(s_src)`: 2 reciprocal, 3 exp2.
pub fn sfu(op: i64, x: i64, src: i64) -> u128 {
    word(&[("v_op_2", op), ("vs_reg_w", src), ("unk_3", x)])
}

pub fn nop() -> u128 {
    word(&[])
}

/// Repeats the next `body` words `count` times.
pub fn repeat(count: i64, body: usize) -> u128 {
    assert!(
        (1..1 << 16).contains(&count) && (1..1 << 10).contains(&body),
        "loop of {count} x {body}"
    );
    0x8c0 | (count as u128) << 12 | (body as u128) << 28
}

/// Jumps by `off` words (relative to this instruction) while p0.
pub fn branch(off: i64) -> u128 {
    0x880 | 0x11 << 12 | ((off & 0x7fff) as u128) << 17 | 1 << 33
}
