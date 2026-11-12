//! Table tabs: the rows of one table on the person's connection, filtered by the tab's WHERE
//! clause (`grid.filter`, `view.filter`) and editable when the table has a primary key.
//!
//! The statement is built here, never typed: `SELECT * FROM <quoted table> [WHERE <filter>]`. The
//! filter is checked by `quarry_guard::check_filter` when it is set **and** again here (a
//! workspace file can carry any text), and the whole statement must classify as a read before it
//! runs, so a table tab never writes even though it reads on the person's connection.

use quarry_core::limits::PAGE_ROWS;
use quarry_core::{Column, ConnectionSpec, SchemaTree, TableKind, TableRef};
use quarry_engine::QueryOptions;
use quarry_guard::Verdict;

use crate::editor::Rows;
use crate::grid::EditTarget;
use crate::grid::copy::table_ident;
use crate::sidebar::connections::{Human, lock};

/// The statement a table tab runs (canonical, classified as a read).
pub fn select_sql(spec: &ConnectionSpec, table: &TableRef, filter: Option<&str>) -> Result<String, String> {
    let engine = spec.engine();
    let policy = quarry_guard::Policy { safe_functions: Vec::new(), sensitive_patterns: spec.sensitive_patterns.clone() };
    let sql = match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => {
            let f = quarry_guard::check_filter(f, engine, &policy).map_err(|r| format!("filter refused: {r}"))?;
            format!("SELECT * FROM {} WHERE {f}", table_ident(table))
        }
        None => format!("SELECT * FROM {}", table_ident(table)),
    };
    match quarry_guard::classify(&sql, engine, &policy) {
        Verdict::Read(r) => Ok(r.sql().to_string()),
        Verdict::Write(_) => Err("filter refused: not a read".into()),
        Verdict::Refused(r) => Err(format!("filter refused: {r}")),
    }
}

/// Whether a table tab showing `columns` of `table` can be edited: its primary key's positions
/// in `columns`, or the note the grid shows instead (read-only).
pub fn edit_target(spec: &ConnectionSpec, schema: Option<&SchemaTree>, table: &TableRef, columns: &[Column]) -> Result<EditTarget, String> {
    if spec.human_read_only {
        return Err("Read-only: this connection is set read-only".into());
    }
    let Some(t) = schema.and_then(|s| s.table(table)) else {
        return Err(format!("Read-only: `{table}` is not in the loaded schema, so its primary key is unknown"));
    };
    if t.kind != TableKind::Table {
        return Err(format!("Read-only: `{table}` is a view, not a table"));
    }
    let mut names: Vec<&str> = t.columns.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect();
    if names.is_empty()
        && let Some(pk) = t.indexes.iter().find(|i| i.primary)
    {
        names = pk.columns.iter().map(String::as_str).collect();
    }
    if names.is_empty() {
        return Err(format!("Read-only: `{table}` has no primary key, so its rows cannot be edited"));
    }
    let key = names.iter().map(|n| columns.iter().position(|c| c.name == *n)).collect::<Option<Vec<usize>>>();
    match key {
        Some(key) => Ok(EditTarget { table: table.clone(), key }),
        None => Err(format!("Read-only: a primary-key column of `{table}` is not in the result")),
    }
}

/// Run `sql` on the person's connection and fetch its first page (blocking: call from a job).
pub fn load(human: &Human, sql: &str) -> Result<Rows, String> {
    let mut h = lock(human);
    let cur = h.query(sql, &QueryOptions::default()).map_err(|e| e.to_string())?;
    let first = h.fetch(cur.id, 0, PAGE_ROWS).map_err(|e| e.to_string())?;
    Ok(Rows { columns: cur.columns, total: cur.total, first, cursor: cur.id })
}

#[cfg(test)]
mod tests {
    use quarry_core::{AgentAccess, ColumnInfo, ConnColor, ConnectionId, Index, Namespace, Table, Target};

    use super::*;

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            id: ConnectionId::new("t"),
            name: "t".into(),
            target: Target::Sqlite { path: "/tmp/t.sqlite".into() },
            color: ConnColor::Local,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec!["password".into()],
            human_read_only: false,
        }
    }

    fn col(name: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), type_name: "INTEGER".into(), nullable: false, default: None, primary_key: pk }
    }

    fn tree() -> SchemaTree {
        let table =
            |name: &str, kind, columns, indexes| Table { name: name.into(), kind, row_estimate: None, columns, indexes, foreign_keys: vec![] };
        SchemaTree {
            namespaces: vec![Namespace {
                name: "main".into(),
                tables: vec![
                    table("orders", TableKind::Table, vec![col("note", false), col("id", true)], vec![]),
                    table(
                        "pairs",
                        TableKind::Table,
                        vec![col("a", false), col("b", false)],
                        vec![Index { name: "pk".into(), columns: vec!["b".into(), "a".into()], unique: true, primary: true }],
                    ),
                    table("log", TableKind::Table, vec![col("line", false)], vec![]),
                    table("v", TableKind::View, vec![col("id", true)], vec![]),
                ],
            }],
        }
    }

    fn cols(names: &[&str]) -> Vec<Column> {
        names.iter().map(|n| Column { name: (*n).into(), type_name: String::new(), sensitive: false }).collect()
    }

    #[test]
    fn statements_quote_the_table_and_check_the_filter() {
        let t = TableRef::new(Some("main"), "or\"ders");
        assert_eq!(select_sql(&spec(), &t, None).unwrap(), "SELECT * FROM \"main\".\"or\"\"ders\"");
        assert_eq!(select_sql(&spec(), &t, Some("  ")).unwrap(), "SELECT * FROM \"main\".\"or\"\"ders\"");
        assert_eq!(select_sql(&spec(), &t, Some("id = 1 -- x")).unwrap(), "SELECT * FROM \"main\".\"or\"\"ders\" WHERE id = 1");
        // a filter from a crafted workspace file is checked again
        for bad in ["1=1; DROP TABLE t", "EXISTS (WITH d AS (DELETE FROM t RETURNING 1) SELECT 1)", "nextval('s') > 0", "password = 'x'"] {
            assert!(select_sql(&spec(), &t, Some(bad)).unwrap_err().starts_with("filter refused"), "{bad}");
        }
    }

    #[test]
    fn primary_keys_come_from_the_schema_or_the_grid_is_read_only() {
        let s = tree();
        let orders = TableRef::new(Some("main"), "orders");
        assert_eq!(edit_target(&spec(), Some(&s), &orders, &cols(&["id", "note"])).unwrap().key, vec![0]);
        let pairs = TableRef::new(None, "pairs");
        assert_eq!(edit_target(&spec(), Some(&s), &pairs, &cols(&["a", "b"])).unwrap().key, vec![1, 0]);
        let no_pk = edit_target(&spec(), Some(&s), &TableRef::new(None, "log"), &cols(&["line"])).unwrap_err();
        assert!(no_pk.contains("no primary key"), "{no_pk}");
        assert!(edit_target(&spec(), Some(&s), &TableRef::new(None, "v"), &cols(&["id"])).unwrap_err().contains("view"));
        assert!(edit_target(&spec(), None, &orders, &cols(&["id"])).is_err());
        assert!(edit_target(&spec(), Some(&s), &orders, &cols(&["note"])).is_err(), "key column missing from the result");
        let ro = ConnectionSpec { human_read_only: true, ..spec() };
        assert!(edit_target(&ro, Some(&s), &orders, &cols(&["id"])).unwrap_err().contains("read-only"));
    }
}
