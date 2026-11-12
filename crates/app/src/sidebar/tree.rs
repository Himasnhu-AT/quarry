//! The schema tree as flat rows (connections › schemas › tables › columns), rebuilt only when the
//! connections, the expansion or the filter change. Pure: the sidebar paints these rows in a
//! virtual list, so a schema of 50,000 tables costs the same per frame as one of five.

use std::collections::BTreeSet;

use quarry_core::{ConnColor, ConnectionId, EngineKind, TableKind, TableRef};

use super::connections::{Connections, Status};

/// At most this many rows are laid out (the rest is cut with a note).
pub const MAX_ROWS: usize = 200_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Conn {
        id: ConnectionId,
        engine: EngineKind,
        color: ConnColor,
        status: Status,
    },
    Schema {
        conn: ConnectionId,
        name: String,
    },
    Table {
        conn: ConnectionId,
        table: TableRef,
        kind: TableKind,
    },
    Column {
        conn: ConnectionId,
        table: TableRef,
        primary_key: bool,
    },
    /// Loading placeholder under a connecting connection.
    Loading,
    /// A message row (connect error, empty schema, cut tree).
    Note {
        error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub depth: u8,
    pub kind: RowKind,
    pub label: String,
    /// Right-aligned detail: row estimate or column type.
    pub detail: String,
    /// Expansion key and the automation id suffix.
    pub key: String,
    pub expandable: bool,
    pub expanded: bool,
}

/// Expansion keys.
pub fn conn_key(id: &ConnectionId) -> String {
    format!("conn.{id}")
}
pub fn schema_key(id: &ConnectionId, ns: &str) -> String {
    format!("schema.{id}.{ns}")
}
pub fn table_key(id: &ConnectionId, ns: &str, table: &str) -> String {
    format!("table.{id}.{ns}.{table}")
}

/// Rows for `conns` with `expanded` keys open, keeping only tables whose name contains `filter`
/// (ignoring case; their parents open by themselves while filtering).
pub fn rows(conns: &Connections, expanded: &BTreeSet<String>, filter: &str) -> Vec<Row> {
    let filter = filter.trim().to_lowercase();
    let filtering = !filter.is_empty();
    let mut out = Vec::new();
    for saved in &conns.saved {
        let spec = &saved.spec;
        let id = &spec.id;
        let status = conns.status(id);
        let key = conn_key(id);
        let live = conns.live.get(id);
        let open = expanded.contains(&key) || filtering;
        out.push(Row {
            depth: 0,
            kind: RowKind::Conn { id: id.clone(), engine: spec.engine(), color: spec.color, status: status.clone() },
            label: spec.name.clone(),
            detail: match spec.engine() {
                EngineKind::Postgres => "pg".into(),
                EngineKind::Sqlite => "sqlite".into(),
            },
            key,
            expandable: true,
            expanded: open,
        });
        if !open {
            continue;
        }
        match &status {
            Status::Connecting => {
                for n in 0..3 {
                    out.push(note(1, RowKind::Loading, "Loading schema…", format!("loading.{id}.{n}")));
                }
                continue;
            }
            Status::Failed(e) => {
                out.push(note(1, RowKind::Note { error: true }, e, format!("error.{id}")));
                continue;
            }
            Status::Closed => {
                out.push(note(1, RowKind::Note { error: false }, "Not connected", format!("closed.{id}")));
                continue;
            }
            Status::Connected => {}
        }
        let Some(tree) = live.and_then(|l| l.schema.as_ref()) else {
            let msg = live.and_then(|l| l.schema_error.clone()).unwrap_or_else(|| "No schema".into());
            out.push(note(1, RowKind::Note { error: live.is_some_and(|l| l.schema_error.is_some()) }, &msg, format!("noschema.{id}")));
            continue;
        };
        // SQLite's single `main` schema adds nothing: tables sit right under the connection
        let flat = tree.namespaces.len() == 1 && spec.engine() == EngineKind::Sqlite;
        let mut shown_any = false;
        for ns in &tree.namespaces {
            let skey = schema_key(id, &ns.name);
            let tables: Vec<_> = ns.tables.iter().filter(|t| !filtering || t.name.to_lowercase().contains(&filter)).collect();
            if filtering && tables.is_empty() {
                continue;
            }
            shown_any = true;
            let depth = if flat {
                1
            } else {
                let open = expanded.contains(&skey) || filtering;
                out.push(Row {
                    depth: 1,
                    kind: RowKind::Schema { conn: id.clone(), name: ns.name.clone() },
                    label: ns.name.clone(),
                    detail: ns.tables.len().to_string(),
                    key: skey,
                    expandable: true,
                    expanded: open,
                });
                if !open {
                    continue;
                }
                2
            };
            for t in tables {
                let tkey = table_key(id, &ns.name, &t.name);
                let open = expanded.contains(&tkey);
                let table = TableRef::new((!flat).then_some(ns.name.as_str()), &t.name);
                out.push(Row {
                    depth,
                    kind: RowKind::Table { conn: id.clone(), table: table.clone(), kind: t.kind },
                    label: t.name.clone(),
                    detail: t.row_estimate.map(compact_count).unwrap_or_default(),
                    key: tkey.clone(),
                    expandable: !t.columns.is_empty(),
                    expanded: open,
                });
                if open {
                    for c in &t.columns {
                        out.push(Row {
                            depth: depth + 1,
                            kind: RowKind::Column { conn: id.clone(), table: table.clone(), primary_key: c.primary_key },
                            label: c.name.clone(),
                            detail: c.type_name.to_lowercase(),
                            key: format!("{tkey}.{}", c.name),
                            expandable: false,
                            expanded: false,
                        });
                    }
                }
                if out.len() >= MAX_ROWS {
                    out.push(note(depth, RowKind::Note { error: false }, "Too many objects: use the filter", format!("cut.{id}")));
                    return out;
                }
            }
        }
        if !shown_any {
            let msg = if filtering { "No match" } else { "No tables" };
            out.push(note(1, RowKind::Note { error: false }, msg, format!("empty.{id}")));
        }
    }
    out
}

fn note(depth: u8, kind: RowKind, label: &str, key: String) -> Row {
    Row { depth, kind, label: label.to_string(), detail: String::new(), key, expandable: false, expanded: false }
}

/// `999`, `1.2K`, `84K`, `1.2M`, `10M`, `3.4B`: a row estimate in a few characters.
pub fn compact_count(n: u64) -> String {
    let f = n as f64;
    let (v, unit) = match n {
        0..1_000 => return n.to_string(),
        1_000..1_000_000 => (f / 1e3, "K"),
        1_000_000..1_000_000_000 => (f / 1e6, "M"),
        _ => (f / 1e9, "B"),
    };
    if v < 10.0 { format!("{:.1}{unit}", (v * 10.0).floor() / 10.0).replace(".0", "") } else { format!("{}{unit}", v.floor()) }
}

/// `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::connections::{Live, Saved};
    use quarry_core::{AgentAccess, ColumnInfo, ConnectionSpec, Namespace, SchemaTree, Table, Target};
    use std::sync::Arc;

    pub(crate) fn fixture() -> Connections {
        let spec = ConnectionSpec {
            id: ConnectionId::new("fixture"),
            name: "fixture.sqlite".into(),
            target: Target::Sqlite { path: "fixture.sqlite".into() },
            color: ConnColor::Local,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec![],
            human_read_only: false,
        };
        let col = |n: &str, t: &str, pk: bool| ColumnInfo { name: n.into(), type_name: t.into(), nullable: !pk, default: None, primary_key: pk };
        let table = |n: &str, rows: u64| Table {
            name: n.into(),
            kind: TableKind::Table,
            row_estimate: Some(rows),
            columns: vec![col("id", "INTEGER", true), col("note", "TEXT", false)],
            indexes: vec![],
            foreign_keys: vec![],
        };
        let mut c = Connections::default();
        c.saved.push(Saved { spec, approve_similar: false });
        let live = c.live_mut(&ConnectionId::new("fixture"));
        live.status = Status::Connected;
        live.schema = Some(Arc::new(SchemaTree {
            namespaces: vec![Namespace { name: "main".into(), tables: vec![table("customers", 1_000_000), table("orders", 10_000_000)] }],
        }));
        let _ = Live::default();
        c
    }

    #[test]
    fn rows_follow_expansion_and_filter() {
        let c = fixture();
        let mut open = BTreeSet::new();
        assert_eq!(rows(&c, &open, "").len(), 1);
        open.insert(conn_key(&ConnectionId::new("fixture")));
        let r = rows(&c, &open, "");
        assert_eq!(r.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["fixture.sqlite", "customers", "orders"]);
        assert_eq!(r[2].detail, "10M");
        assert_eq!(r[2].depth, 1, "sqlite tables sit under the connection");
        open.insert(table_key(&ConnectionId::new("fixture"), "main", "orders"));
        let r = rows(&c, &open, "");
        assert_eq!(r.len(), 5);
        assert!(matches!(r[3].kind, RowKind::Column { primary_key: true, .. }));
        assert_eq!(r[3].detail, "integer");
        let f = rows(&c, &BTreeSet::new(), "CUST");
        assert_eq!(f.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["fixture.sqlite", "customers"]);
        let none = rows(&c, &BTreeSet::new(), "zzz");
        assert_eq!(none.last().map(|r| r.label.as_str()), Some("No match"));
    }

    #[test]
    fn connecting_and_failed_show_a_placeholder() {
        let mut c = fixture();
        let id = ConnectionId::new("fixture");
        let open: BTreeSet<String> = [conn_key(&id)].into();
        c.live_mut(&id).status = Status::Connecting;
        assert_eq!(rows(&c, &open, "")[1].kind, RowKind::Loading);
        c.live_mut(&id).status = Status::Failed("cannot connect: refused".into());
        let r = rows(&c, &open, "");
        assert_eq!((r[1].kind.clone(), r[1].label.as_str()), (RowKind::Note { error: true }, "cannot connect: refused"));
    }

    #[test]
    fn counts() {
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1K");
        assert_eq!(compact_count(1_250), "1.2K");
        assert_eq!(compact_count(84_000), "84K");
        assert_eq!(compact_count(10_000_000), "10M");
        assert_eq!(compact_count(3_400_000_000), "3.4B");
        assert_eq!(compact_count(u64::MAX), "18446744073B");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(10_000_000), "10,000,000");
    }
}
