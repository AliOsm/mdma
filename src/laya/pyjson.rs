// Python `json.dumps` output for a serde_json::Value: `", "` / `": "`
// separators and optional ASCII escaping. The prompt embeds serialized
// state and criteria, so the bytes must match what upstream tokenizes.
use serde_json::Value;
use std::fmt::Write;

pub fn dumps(value: &Value, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ensure_ascii);
    out
}

fn write_value(out: &mut String, value: &Value, ensure_ascii: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_str(out, s, ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item, ensure_ascii);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(out, k, ensure_ascii);
                out.push_str(": ");
                write_value(out, v, ensure_ascii);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if n.is_f64() {
        // Number cannot hold NaN/infinity. Python needs a sign on exponents.
        let s = n.to_string();
        match s.split_once('e') {
            Some((m, e)) if !e.starts_with(['-', '+']) => {
                write!(out, "{m}e+{e}").expect("writing to a String cannot fail");
            }
            _ => out.push_str(&s),
        }
    } else {
        write!(out, "{n}").expect("writing to a String cannot fail");
    }
}

fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).expect("writing to a String cannot fail");
            }
            c if ensure_ascii && !c.is_ascii() => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    write!(out, "\\u{unit:04x}").expect("writing to a String cannot fail");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
