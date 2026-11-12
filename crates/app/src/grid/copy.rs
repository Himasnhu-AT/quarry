//! Copy selected cells as TSV, CSV, JSON or SQL `INSERT`s (`grid.copy`), and SQL literals shared
//! with the pending-edit statements.

use std::fmt::Write as _;

use quarry_core::{EngineKind, TableRef, Value};

use super::cells::{self, plain};

/// Most rows one copy takes (the rest is left out, and the result says so).
pub const MAX_COPY_ROWS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Tsv,
    Csv,
    Json,
    Insert,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tsv" => Self::Tsv,
            "csv" => Self::Csv,
            "json" => Self::Json,
            "insert" => Self::Insert,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Tsv => "tsv",
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Insert => "insert",
        }
    }
}

/// `"name"`, quotes doubled.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `"schema"."table"` or `"table"`.
pub fn table_ident(t: &TableRef) -> String {
    match &t.schema {
        Some(s) => format!("{}.{}", ident(s), ident(&t.name)),
        None => ident(&t.name),
    }
}

/// `'text'`, quotes doubled.
pub fn string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A SQL literal for `v`, or `None` when the grid holds only part of the value (clipped text,
/// a byte preview, a redacted cell): such a value is never written back.
pub fn literal(v: &Value, dialect: EngineKind) -> Option<String> {
    Some(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Float(f) => string(&f.to_string()),
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => string(s),
        Value::Bytes { .. } if !cells::complete(v) => return None,
        Value::Bytes { preview, .. } => match dialect {
            EngineKind::Postgres => format!("'\\x{}'::bytea", cells::hex(preview)),
            EngineKind::Sqlite => format!("X'{}'", cells::hex(preview)),
        },
        Value::Clipped { .. } | Value::Redacted { .. } => return None,
    })
}

fn csv_field(s: &str, sep: char) -> String {
    // TSV (for spreadsheets) quotes only what would break a line or a field
    let needs = if sep == '\t' {
        s.contains(['\t', '\n', '\r']) || s.starts_with('"')
    } else {
        s.contains([sep, '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ')
    };
    if needs { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn json_of(v: &Value) -> serde_json::Value {
    match v {
        // a JSON cell is copied as JSON, not as a string holding JSON
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.to_string())),
        other => other.to_agent_json(),
    }
}

/// `rows` (each one value per name in `columns`) as text in `format`. `table` names the INSERT
/// target (`result` when the grid shows a query).
pub fn copy(format: Format, columns: &[&str], rows: &[Vec<&Value>], table: Option<&TableRef>, dialect: EngineKind) -> String {
    let mut out = String::new();
    match format {
        Format::Tsv | Format::Csv => {
            let sep = if format == Format::Tsv { '\t' } else { ',' };
            let line = |out: &mut String, fields: &mut dyn Iterator<Item = String>| {
                let mut first = true;
                for f in fields {
                    if !first {
                        out.push(sep);
                    }
                    first = false;
                    out.push_str(&csv_field(&f, sep));
                }
                out.push('\n');
            };
            line(&mut out, &mut columns.iter().map(|c| c.to_string()));
            for r in rows {
                line(&mut out, &mut r.iter().map(|v| plain(v)));
            }
        }
        Format::Json => {
            let arr: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    let mut obj = serde_json::Map::new();
                    for (c, v) in columns.iter().zip(r) {
                        obj.insert((*c).to_string(), json_of(v));
                    }
                    serde_json::Value::Object(obj)
                })
                .collect();
            out = serde_json::to_string_pretty(&arr).unwrap_or_default();
            out.push('\n');
        }
        Format::Insert => {
            let target = table.map_or_else(|| ident("result"), table_ident);
            let cols = columns.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
            for r in rows {
                let vals = r
                    .iter()
                    .map(|v| {
                        literal(v, dialect).unwrap_or_else(|| {
                            let len = match v {
                                Value::Clipped { len, .. } | Value::Redacted { len } | Value::Bytes { len, .. } => *len,
                                _ => 0,
                            };
                            format!("NULL /* not fully loaded: {} */", cells::size(len))
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "INSERT INTO {target} ({cols}) VALUES ({vals});");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn sample() -> (Vec<&'static str>, Vec<Vec<Value>>) {
        (
            vec!["id", "note", "payload"],
            vec![
                vec![Value::Int(1), Value::text("a, \"quoted\"\nline"), Value::Json("{\"k\": [1, 2]}".into())],
                vec![Value::Int(2), Value::Null, Value::Bytes { len: 2, preview: Arc::from(&[0xab_u8, 0xcd][..]) }],
            ],
        )
    }

    fn refs(rows: &[Vec<Value>]) -> Vec<Vec<&Value>> {
        rows.iter().map(|r| r.iter().collect()).collect()
    }

    #[test]
    fn tsv_and_csv_quote_what_needs_it() {
        let (cols, rows) = sample();
        let tsv = copy(Format::Tsv, &cols, &refs(&rows), None, EngineKind::Postgres);
        assert_eq!(tsv, "id\tnote\tpayload\n1\t\"a, \"\"quoted\"\"\nline\"\t{\"k\": [1, 2]}\n2\t\t\\xabcd\n");
        let csv = copy(Format::Csv, &cols, &refs(&rows), None, EngineKind::Postgres);
        assert_eq!(csv, "id,note,payload\n1,\"a, \"\"quoted\"\"\nline\",\"{\"\"k\"\": [1, 2]}\"\n2,,\\xabcd\n");
    }

    #[test]
    fn json_keeps_types_and_embeds_json_cells() {
        let (cols, rows) = sample();
        let j: serde_json::Value = serde_json::from_str(&copy(Format::Json, &cols, &refs(&rows), None, EngineKind::Postgres)).unwrap();
        assert_eq!(
            j,
            serde_json::json!([
                {"id": 1, "note": "a, \"quoted\"\nline", "payload": {"k": [1, 2]}},
                {"id": 2, "note": null, "payload": {"$bytes": 2, "$hex": "abcd"}}
            ])
        );
    }

    #[test]
    fn insert_per_dialect_and_never_writes_a_preview() {
        let (cols, rows) = sample();
        let t = TableRef::new(Some("public"), "or\"ders");
        let pg = copy(Format::Insert, &cols, &refs(&rows), Some(&t), EngineKind::Postgres);
        assert_eq!(
            pg,
            "INSERT INTO \"public\".\"or\"\"ders\" (\"id\", \"note\", \"payload\") VALUES (1, 'a, \"quoted\"\nline', '{\"k\": [1, 2]}');\n\
             INSERT INTO \"public\".\"or\"\"ders\" (\"id\", \"note\", \"payload\") VALUES (2, NULL, '\\xabcd'::bytea);\n"
        );
        let lite = copy(Format::Insert, &["b"], &[vec![&rows[1][2]]], None, EngineKind::Sqlite);
        assert_eq!(lite, "INSERT INTO \"result\" (\"b\") VALUES (X'abcd');\n");
        let clipped = Value::Clipped { preview: "x".into(), len: 2048 };
        let s = copy(Format::Insert, &["c"], &[vec![&clipped]], None, EngineKind::Sqlite);
        assert_eq!(s, "INSERT INTO \"result\" (\"c\") VALUES (NULL /* not fully loaded: 2.0 KB */);\n");
        assert_eq!(literal(&Value::text("it's"), EngineKind::Sqlite).unwrap(), "'it''s'");
        assert_eq!(literal(&Value::Float(1.0), EngineKind::Sqlite).unwrap(), "1.0");
        assert_eq!(literal(&Value::Float(f64::NAN), EngineKind::Postgres).unwrap(), "'NaN'");
    }
}
