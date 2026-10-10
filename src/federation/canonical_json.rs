//! Matrix-style canonical JSON: the encoding
//! `doc/federation/identity-and-catalog-v1.md` calls "Matrix v1.19's
//! canonical JSON definition" (https://spec.matrix.org/v1.9/appendices/#canonical-json).
//!
//! Deliberately not `serde_json::Value`: canonical JSON rejects duplicate
//! object keys and non-integer/out-of-range numbers that `serde_json`
//! silently accepts (last-key-wins on duplicate keys; arbitrary `f64`
//! numbers), and its encoder must sort keys by Unicode code point and use
//! minimal string escaping, which `serde_json::to_string` does not
//! guarantee. This module owns both strict parsing and canonical encoding
//! so a verifier can check a received `body_json` byte-for-byte against
//! the re-encoding of its parsed value, per that doc's "`body_json` MUST
//! equal the canonical encoding of `B`" requirement.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// 2^53 - 1: the largest safe integer, per the canonical JSON number
/// restriction (and this doc's "integer fields are restricted to
/// `0..9007199254740991`").
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
pub const MIN_SAFE_INTEGER: i64 = -MAX_SAFE_INTEGER;

/// A parsed or constructed canonical JSON value. Object keys are kept in a
/// `BTreeMap`, whose iteration order for `String` keys already matches
/// Unicode code point order (UTF-8 byte-wise comparison preserves code
/// point ordering), which is exactly the canonical key order.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid UTF-8")]
    InvalidUtf8,
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected byte {0:#04x} at offset {1}")]
    UnexpectedByte(u8, usize),
    #[error("duplicate object key {0:?}")]
    DuplicateKey(String),
    #[error("non-integer number is not permitted in canonical JSON")]
    NonInteger,
    #[error("integer {0} is outside the canonical safe range")]
    IntegerOutOfRange(String),
    #[error("invalid Unicode escape (lone surrogate)")]
    LoneSurrogate,
    #[error("control character must be escaped")]
    UnescapedControl,
    #[error("maximum nesting depth {0} exceeded")]
    TooDeep(usize),
    #[error("trailing data after JSON value")]
    TrailingData,
}

impl Value {
    /// Builds an object from field pairs, rejecting duplicate keys. Useful
    /// when callers assemble a body from a fixed field list and want the
    /// same duplicate-key discipline as parsed input.
    pub fn object<I: IntoIterator<Item = (String, Self)>>(iter: I) -> Result<Self, Error> {
        let mut map = BTreeMap::new();
        for (k, v) in iter {
            if map.insert(k.clone(), v).is_some() {
                return Err(Error::DuplicateKey(k));
            }
        }
        Ok(Self::Object(map))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Encodes in canonical form: code-point-sorted keys, compact `,`/`:`
    /// separators and minimal string escaping (only `"`, `\` and control
    /// characters; no escaping of non-ASCII or `/`).
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Integer(n) => out.push_str(&n.to_string()),
            Self::String(s) => write_string(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).expect("writing to a String cannot fail");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parses `input` under strict canonical JSON rules: rejects duplicate
/// object keys, non-integer or out-of-safe-range numbers, lone surrogates
/// and unescaped control characters inside strings. `max_depth` bounds
/// array/object nesting (the catalog spec caps this at 16).
pub fn parse(input: &[u8], max_depth: usize) -> Result<Value, Error> {
    let text = std::str::from_utf8(input).map_err(|_| Error::InvalidUtf8)?;
    let mut p = Parser { bytes: text.as_bytes(), pos: 0, depth: 0, max_depth };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(Error::TrailingData);
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    max_depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), Error> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            match self.peek() {
                Some(got) => Err(Error::UnexpectedByte(got, self.pos)),
                None => Err(Error::UnexpectedEof),
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, Error> {
        match self.peek().ok_or(Error::UnexpectedEof)? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => Ok(Value::String(self.parse_string()?)),
            b't' => {
                self.expect_literal("true")?;
                Ok(Value::Bool(true))
            }
            b'f' => {
                self.expect_literal("false")?;
                Ok(Value::Bool(false))
            }
            b'n' => {
                self.expect_literal("null")?;
                Ok(Value::Null)
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            got => Err(Error::UnexpectedByte(got, self.pos)),
        }
    }

    fn expect_literal(&mut self, lit: &str) -> Result<(), Error> {
        let bytes = lit.as_bytes();
        if self.bytes[self.pos..].starts_with(bytes) {
            self.pos += bytes.len();
            Ok(())
        } else {
            Err(Error::UnexpectedByte(self.peek().unwrap_or(0), self.pos))
        }
    }

    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(Error::TooDeep(self.max_depth));
        }
        Ok(())
    }

    fn parse_object(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            if map.insert(key.clone(), value).is_some() {
                return Err(Error::DuplicateKey(key));
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(got) => return Err(Error::UnexpectedByte(got, self.pos)),
                None => return Err(Error::UnexpectedEof),
            }
        }
        self.depth -= 1;
        Ok(Value::Object(map))
    }

    fn parse_array(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(got) => return Err(Error::UnexpectedByte(got, self.pos)),
                None => return Err(Error::UnexpectedEof),
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, Error> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            let b = self.peek().ok_or(Error::UnexpectedEof)?;
            match b {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or(Error::UnexpectedEof)?;
                    self.pos += 1;
                    match esc {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0C}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.peek() != Some(b'\\') {
                                    return Err(Error::LoneSurrogate);
                                }
                                self.pos += 1;
                                if self.peek() != Some(b'u') {
                                    return Err(Error::LoneSurrogate);
                                }
                                self.pos += 1;
                                let low = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(Error::LoneSurrogate);
                                }
                                let c = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                s.push(char::from_u32(c).ok_or(Error::LoneSurrogate)?);
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                return Err(Error::LoneSurrogate);
                            } else {
                                s.push(char::from_u32(cp).ok_or(Error::LoneSurrogate)?);
                            }
                        }
                        _ => return Err(Error::UnexpectedByte(esc, self.pos - 1)),
                    }
                }
                0x00..=0x1F => return Err(Error::UnescapedControl),
                _ => {
                    let start = self.pos;
                    let end = start + utf8_char_len(self.bytes[start]);
                    let slice = std::str::from_utf8(&self.bytes[start..end]).map_err(|_| Error::InvalidUtf8)?;
                    s.push_str(slice);
                    self.pos = end;
                }
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32, Error> {
        if self.pos + 4 > self.bytes.len() {
            return Err(Error::UnexpectedEof);
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4]).map_err(|_| Error::InvalidUtf8)?;
        let v = u32::from_str_radix(hex, 16).map_err(|_| Error::UnexpectedByte(self.bytes[self.pos], self.pos))?;
        self.pos += 4;
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Value, Error> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(Error::UnexpectedByte(self.peek().unwrap_or(0), self.pos)),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).expect("ASCII digits only");
        if is_float {
            return Err(Error::NonInteger);
        }
        let n: i64 = text.parse().map_err(|_| Error::IntegerOutOfRange(text.to_string()))?;
        if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&n) {
            return Err(Error::IntegerOutOfRange(text.to_string()));
        }
        Ok(Value::Integer(n))
    }
}

fn utf8_char_len(byte: u8) -> usize {
    if byte & 0x80 == 0 {
        1
    } else if byte & 0xE0 == 0xC0 {
        2
    } else if byte & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}
