//! Pending table edits → the statements a commit would run and the diff the person reviews.
//!
//! The grid only builds these; committing is the human-only `edit.commit`
//! (`crate::human::HumanAction::CommitEdits`), which runs [`statements`] in one transaction.

use std::fmt::Write as _;

use ashlar_diff::FileDiff;
use quarry_core::{EngineKind, PendingEdits, RowChange, Value};

use super::cells;
use super::copy::{ident, literal, table_ident};

/// The statements that apply `e`, in order: deletes, then updates (one per row), then inserts.
/// Refused when a key or a new value is only partly loaded (it would write the wrong row or a
/// truncated value).
pub fn statements(e: &PendingEdits, dialect: EngineKind) -> Result<Vec<String>, String> {
    if e.key.is_empty() {
        return Err("the table has no primary key: its rows cannot be edited".into());
    }
    let table = table_ident(&e.table);
    let lit = |v: &Value| literal(v, dialect).ok_or_else(|| format!("a value is only partly loaded ({})", cells::shown(v).text));
    let where_of = |key: &[Value]| -> Result<String, String> {
        if key.len() != e.key.len() {
            return Err("a row's key does not match the table's primary key".into());
        }
        let parts = e.key.iter().zip(key).map(|(c, v)| Ok(format!("{} = {}", ident(c), lit(v)?))).collect::<Result<Vec<_>, String>>()?;
        Ok(parts.join(" AND "))
    };
    let mut out = Vec::new();
    for c in &e.changes {
        if let RowChange::Delete { key, .. } = c {
            out.push(format!("DELETE FROM {table} WHERE {};", where_of(key)?));
        }
    }
    // updates grouped by row, rows in the order first edited
    let mut rows: Vec<(&Vec<Value>, Vec<(&str, &Value)>)> = Vec::new();
    for c in &e.changes {
        if let RowChange::Update { key, column, new, .. } = c {
            match rows.iter_mut().find(|(k, _)| *k == key) {
                Some((_, sets)) => sets.push((column, new)),
                None => rows.push((key, vec![(column, new)])),
            }
        }
    }
    for (key, sets) in rows {
        let sets = sets.iter().map(|(c, v)| Ok(format!("{} = {}", ident(c), lit(v)?))).collect::<Result<Vec<_>, String>>()?;
        out.push(format!("UPDATE {table} SET {} WHERE {};", sets.join(", "), where_of(key)?));
    }
    for values in e.inserts() {
        if values.is_empty() {
            out.push(format!("INSERT INTO {table} DEFAULT VALUES;"));
            continue;
        }
        let cols = values.iter().map(|(c, _)| ident(c)).collect::<Vec<_>>().join(", ");
        let vals = values.iter().map(|(_, v)| lit(v)).collect::<Result<Vec<_>, String>>()?.join(", ");
        out.push(format!("INSERT INTO {table} ({cols}) VALUES ({vals});"));
    }
    Ok(out)
}

fn cell(v: &Value) -> String {
    let s = cells::shown(v);
    match v {
        Value::Text(_) | Value::Time(_) | Value::Json(_) | Value::Other(_) => format!("'{}'", s.text),
        _ => s.text,
    }
}

/// The rows `e` touches, before and after, one line per row (`id=5  status: 'paid'`), as a diff.
pub fn diff(e: &PendingEdits) -> FileDiff {
    let key_text = |key: &[Value]| e.key.iter().zip(key).map(|(c, v)| format!("{c}={}", cell(v))).collect::<Vec<_>>().join(" ");
    let (mut old, mut new) = (String::new(), String::new());
    let mut seen: Vec<&Vec<Value>> = Vec::new();
    for c in &e.changes {
        match c {
            RowChange::Update { key, .. } if !seen.contains(&key) => {
                seen.push(key);
                let (mut o, mut n) = (key_text(key), key_text(key));
                for c in &e.changes {
                    if let RowChange::Update { key: k, column, old: ov, new: nv } = c
                        && k == key
                    {
                        let _ = write!(o, "  {column}: {}", cell(ov));
                        let _ = write!(n, "  {column}: {}", cell(nv));
                    }
                }
                let _ = writeln!(old, "{o}");
                let _ = writeln!(new, "{n}");
            }
            RowChange::Delete { key, before } => {
                let mut o = key_text(key);
                for (c, v) in before.iter().filter(|(c, _)| !e.key.contains(c)) {
                    let _ = write!(o, "  {c}: {}", cell(v));
                }
                let _ = writeln!(old, "{o}");
            }
            RowChange::Insert { values } => {
                let line = values.iter().map(|(c, v)| format!("{c}: {}", cell(v))).collect::<Vec<_>>().join("  ");
                let _ = writeln!(new, "new row  {line}");
            }
            RowChange::Update { .. } => {}
        }
    }
    let name = e.table.to_string();
    FileDiff::from_texts(&old, &new).with_paths(&format!("{name} (now)"), &format!("{name} (pending)"))
}

/// The value the person typed into a cell: `NULL` is NULL; otherwise parsed like the cell's old
/// value (or its column type when the old value is NULL); anything that does not parse stays
/// text, for the database to accept or refuse at commit.
pub fn parse_input(text: &str, type_name: &str, old: &Value) -> Value {
    if text == "NULL" {
        return Value::Null;
    }
    let t = type_name.to_ascii_lowercase();
    let int_type = ["int", "serial"].iter().any(|k| t.contains(k));
    let float_type = ["real", "double", "float"].iter().any(|k| t.contains(k));
    let parsed = match old {
        Value::Int(_) => text.trim().parse().ok().map(Value::Int),
        Value::Float(_) => text.trim().parse().ok().map(Value::Float),
        Value::Bool(_) => parse_bool(text).map(Value::Bool),
        Value::Json(_) => Some(Value::Json(text.into())),
        Value::Time(_) => Some(Value::Time(text.into())),
        Value::Null if int_type => text.trim().parse().ok().map(Value::Int),
        Value::Null if float_type => text.trim().parse().ok().map(Value::Float),
        Value::Null if t.starts_with("bool") => parse_bool(text).map(Value::Bool),
        Value::Null if t.starts_with("json") => Some(Value::Json(text.into())),
        _ => None,
    };
    parsed.unwrap_or_else(|| Value::text(text))
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" => Some(true),
        "false" | "f" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// What the person may type to edit `v` (the plain text, `NULL` for NULL); `None` for a value the
/// grid holds only part of.
pub fn edit_text(v: &Value) -> Option<String> {
    if !cells::complete(v) || matches!(v, Value::Bytes { .. }) {
        return None;
    }
    Some(if v.is_null() { "NULL".into() } else { cells::plain(v) })
}

#[cfg(test)]
mod tests {
    use quarry_core::{TabId, TableRef};

    use super::*;

    fn edits() -> PendingEdits {
        let mut e = PendingEdits::new(TabId(1), TableRef::new(Some("public"), "orders"), vec!["id".into()]);
        e.set(vec![Value::Int(5)], "status", Value::text("paid"), Value::text("shipped"));
        e.set(vec![Value::Int(7)], "note", Value::Null, Value::text("it's late"));
        e.set(vec![Value::Int(5)], "note", Value::Null, Value::Null);
        e.set(vec![Value::Int(5)], "total_cents", Value::Int(100), Value::Int(120));
        e.delete(vec![Value::Int(9)], vec![("id".into(), Value::Int(9)), ("status".into(), Value::text("pending"))]);
        e.insert(vec![("customer_id".into(), Value::Int(3)), ("status".into(), Value::text("pending"))]);
        e
    }

    #[test]
    fn statements_group_updates_and_quote_everything() {
        let s = statements(&edits(), EngineKind::Postgres).unwrap();
        assert_eq!(
            s,
            vec![
                "DELETE FROM \"public\".\"orders\" WHERE \"id\" = 9;",
                "UPDATE \"public\".\"orders\" SET \"status\" = 'shipped', \"total_cents\" = 120 WHERE \"id\" = 5;",
                "UPDATE \"public\".\"orders\" SET \"note\" = 'it''s late' WHERE \"id\" = 7;",
                "INSERT INTO \"public\".\"orders\" (\"customer_id\", \"status\") VALUES (3, 'pending');",
            ]
        );
    }

    #[test]
    fn partial_values_and_keyless_tables_are_refused() {
        let mut e = edits();
        e.set(vec![Value::Int(8)], "note", Value::Null, Value::Clipped { preview: "x".into(), len: 99_999 });
        assert!(statements(&e, EngineKind::Sqlite).unwrap_err().contains("partly loaded"));
        let mut e = edits();
        e.key.clear();
        assert!(statements(&e, EngineKind::Sqlite).is_err());
    }

    #[test]
    fn diff_shows_rows_before_and_after() {
        let d = diff(&edits());
        let (added, removed) = d.stats();
        assert_eq!((added, removed), (3, 3), "{}", d.to_patch());
        let patch = d.to_patch();
        assert!(patch.contains("-id=5  status: 'paid'  total_cents: 100"), "{patch}");
        assert!(patch.contains("+id=5  status: 'shipped'  total_cents: 120"), "{patch}");
        assert!(patch.contains("-id=9  status: 'pending'"), "{patch}");
        assert!(patch.contains("+new row  customer_id: 3  status: 'pending'"), "{patch}");
    }

    #[test]
    fn typed_text_parses_like_the_old_value() {
        assert_eq!(parse_input("NULL", "text", &Value::text("x")), Value::Null);
        assert_eq!(parse_input(" 42 ", "int8", &Value::Int(1)), Value::Int(42));
        assert_eq!(parse_input("4x", "int8", &Value::Int(1)), Value::text("4x"));
        assert_eq!(parse_input("1.5", "float8", &Value::Null), Value::Float(1.5));
        assert_eq!(parse_input("t", "boolean", &Value::Null), Value::Bool(true));
        assert_eq!(parse_input("{}", "jsonb", &Value::Null), Value::Json("{}".into()));
        assert_eq!(parse_input("null", "text", &Value::Null), Value::text("null"));
        assert_eq!(edit_text(&Value::Null).as_deref(), Some("NULL"));
        assert_eq!(edit_text(&Value::Clipped { preview: "a".into(), len: 9 }), None);
    }
}
