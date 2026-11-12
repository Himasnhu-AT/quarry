//! One cell. Values from a database are hostile: engines cap text and bytes to
//! [`crate::limits::CELL_CAP_BYTES`] / [`crate::limits::BYTES_PREVIEW`] before building a page.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One cell of a result.
///
/// Engines map their types onto these; anything without a faithful mapping (arrays, ranges,
/// geometry, custom types) is [`Value::Other`] with the server's text form (PLAN §10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Text up to the cell cap.
    Text(Arc<str>),
    /// Binary data: its full length and the first bytes.
    Bytes {
        len: u64,
        preview: Arc<[u8]>,
    },
    /// JSON / JSONB as the server's text (not parsed: it can be huge or deep).
    Json(Arc<str>),
    /// Date, time, timestamp or interval as ISO-8601 text (`2024-05-01T12:00:00Z`).
    Time(Arc<str>),
    /// Text or JSON longer than the cell cap: the first part and the full length in bytes. The
    /// grid's cell viewer fetches the rest on demand.
    Clipped {
        preview: Arc<str>,
        len: u64,
    },
    /// A sensitive column's value withheld from an agent (SECURITY.md, layer D); the length in
    /// bytes is kept. Never produced for the human's grid.
    Redacted {
        len: u64,
    },
    /// Any other type, in the server's text form.
    Other(Arc<str>),
}

/// Largest integer a JSON number carries exactly in every agent client (2^53).
const JSON_SAFE_INT: i64 = 1 << 53;

impl Value {
    pub fn text(s: impl Into<Arc<str>>) -> Self {
        Value::Text(s.into())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The agent-facing JSON of a cell (contract 3): JSON scalars where they are exact, strings for
    /// text-like types, and `$`-objects for the rest:
    ///
    /// | Value | JSON |
    /// |---|---|
    /// | `Null` / `Bool` | `null` / `true` |
    /// | `Int` within ±2^53 | number, else string `"9007199254740993"` |
    /// | `Float` finite | number, else string `"NaN"`, `"inf"`, `"-inf"` |
    /// | `Text`, `Json`, `Time`, `Other` | string |
    /// | `Bytes` | `{"$bytes": len, "$hex": "00ff…"}` (preview) |
    /// | `Clipped` | `{"$clipped": len, "$preview": "…"}` |
    /// | `Redacted` | `{"$redacted": len}` |
    pub fn to_agent_json(&self) -> serde_json::Value {
        use serde_json::{Value as J, json};
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Int(i) if (-JSON_SAFE_INT..=JSON_SAFE_INT).contains(i) => json!(i),
            Value::Int(i) => J::String(i.to_string()),
            Value::Float(f) if f.is_finite() => json!(f),
            Value::Float(f) if f.is_nan() => J::String("NaN".into()),
            Value::Float(f) => J::String(if *f > 0.0 { "inf" } else { "-inf" }.into()),
            Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => J::String(s.to_string()),
            Value::Bytes { len, preview } => json!({"$bytes": len, "$hex": hex(preview)}),
            Value::Clipped { preview, len } => json!({"$clipped": len, "$preview": &**preview}),
            Value::Redacted { len } => json!({"$redacted": len}),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_json_of_every_variant() {
        let cases = [
            (Value::Null, json!(null)),
            (Value::Bool(true), json!(true)),
            (Value::Int(42), json!(42)),
            (Value::Int(i64::MAX), json!("9223372036854775807")),
            (Value::Float(1.5), json!(1.5)),
            (Value::Float(f64::NAN), json!("NaN")),
            (Value::Float(f64::NEG_INFINITY), json!("-inf")),
            (Value::text("hi"), json!("hi")),
            (Value::Json("{\"a\":1}".into()), json!("{\"a\":1}")),
            (Value::Time("2024-05-01T12:00:00Z".into()), json!("2024-05-01T12:00:00Z")),
            (Value::Other("{1,2}".into()), json!("{1,2}")),
            (Value::Bytes { len: 300, preview: Arc::from(&[0u8, 255][..]) }, json!({"$bytes": 300, "$hex": "00ff"})),
            (Value::Clipped { preview: "abc".into(), len: 70_000 }, json!({"$clipped": 70_000, "$preview": "abc"})),
            (Value::Redacted { len: 12 }, json!({"$redacted": 12})),
        ];
        for (v, want) in cases {
            assert_eq!(v.to_agent_json(), want, "{v:?}");
        }
    }

    #[test]
    fn internal_serde_round_trips() {
        for v in
            [Value::Null, Value::Int(-3), Value::text("x"), Value::Bytes { len: 2, preview: Arc::from(&[1u8, 2][..]) }, Value::Redacted { len: 3 }]
        {
            let s = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&s).unwrap(), v, "{s}");
        }
    }
}
