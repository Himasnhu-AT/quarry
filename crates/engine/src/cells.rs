//! Cell caps shared by both engines: text and JSON longer than [`CELL_CAP_BYTES`] become
//! [`Value::Clipped`], binary data keeps its length and a [`BYTES_PREVIEW`] preview.

use std::sync::Arc;

use quarry_core::Value;
use quarry_core::limits::{BYTES_PREVIEW, CELL_CAP_BYTES};

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
pub(crate) fn clip_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

fn capped(s: &str, make: fn(Arc<str>) -> Value) -> Value {
    if s.len() <= CELL_CAP_BYTES { make(s.into()) } else { Value::Clipped { preview: clip_str(s, CELL_CAP_BYTES).into(), len: s.len() as u64 } }
}

pub(crate) fn text(s: &str) -> Value {
    capped(s, Value::Text)
}

pub(crate) fn json(s: &str) -> Value {
    capped(s, Value::Json)
}

pub(crate) fn time(s: &str) -> Value {
    capped(s, Value::Time)
}

pub(crate) fn other(s: &str) -> Value {
    capped(s, Value::Other)
}

pub(crate) fn bytes(b: &[u8]) -> Value {
    let n = b.len().min(BYTES_PREVIEW);
    Value::Bytes { len: b.len() as u64, preview: Arc::from(b.get(..n).unwrap_or_default()) }
}

/// Text from bytes the engine could not map: printable UTF-8 as text, anything else as `\x` hex
/// (capped).
pub(crate) fn unknown(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) if !s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) => other(s),
        _ => {
            use std::fmt::Write;
            let take = b.len().min(CELL_CAP_BYTES / 2 - 1);
            let mut s = String::with_capacity(2 + take * 2);
            s.push_str("\\x");
            for byte in b.iter().take(take) {
                let _ = write!(s, "{byte:02x}");
            }
            if take < b.len() { Value::Clipped { preview: s.into(), len: (b.len() as u64).saturating_mul(2).saturating_add(2) } } else { other(&s) }
        }
    }
}

/// A cell as plain text (for array and range elements).
pub(crate) fn as_text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => s.to_string(),
        Value::Clipped { preview, .. } => preview.to_string(),
        Value::Bytes { preview, .. } => {
            use std::fmt::Write;
            let mut s = String::from("\\x");
            for b in preview.iter() {
                let _ = write!(s, "{b:02x}");
            }
            s
        }
        Value::Redacted { len } => format!("[redacted {len}]"),
    }
}

/// A cell as JSON (array elements): numbers and booleans as JSON, everything else as strings.
pub(crate) fn as_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(|| J::String(f.to_string()), J::Number),
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| J::String(s.to_string())),
        other => J::String(as_text(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_on_char_boundaries() {
        let s = "é".repeat(CELL_CAP_BYTES); // 2 bytes each
        match text(&s) {
            Value::Clipped { preview, len } => {
                assert_eq!(len, s.len() as u64);
                assert!(preview.len() <= CELL_CAP_BYTES);
                assert!(preview.chars().all(|c| c == 'é'));
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(clip_str("aé", 2), "a");
        assert_eq!(text("short"), Value::text("short"));
    }

    #[test]
    fn bytes_keep_len_and_preview() {
        let b = vec![7u8; 1000];
        assert_eq!(bytes(&b), Value::Bytes { len: 1000, preview: Arc::from(&b[..BYTES_PREVIEW]) });
        assert_eq!(unknown(&[0, 1, 255]), Value::Other("\\x0001ff".into()));
        assert_eq!(unknown(b"happy"), Value::Other("happy".into()));
    }
}
