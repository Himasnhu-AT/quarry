//! SQLite schema introspection with pragmas (all on the agent connection's read-pragma list).
//! Row estimates come from `sqlite_stat1` when `ANALYZE` has run; never `COUNT(*)`.

use std::collections::{BTreeMap, HashMap};

use quarry_core::{ColumnInfo, ForeignKey, Index, Namespace, SchemaTree, Table, TableKind, TableRef};
use rusqlite::Connection;

use crate::SchemaFilter;

/// `"name"` with inner quotes doubled.
pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn databases(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut s = conn.prepare("PRAGMA database_list")?;
    let names = s.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

fn estimates(conn: &Connection, db: &str) -> rusqlite::Result<HashMap<String, u64>> {
    let q = quote(db);
    let has: bool = conn.prepare(&format!("SELECT 1 FROM {q}.sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'"))?.exists([])?;
    let mut out = HashMap::new();
    if !has {
        return Ok(out);
    }
    let mut s = conn.prepare(&format!("SELECT tbl, stat FROM {q}.sqlite_stat1"))?;
    let mut rows = s.query([])?;
    while let Some(r) = rows.next()? {
        let (Ok(tbl), Ok(stat)) = (r.get::<_, String>(0), r.get::<_, String>(1)) else { continue };
        if let Some(n) = stat.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
            let e = out.entry(tbl).or_insert(0);
            *e = (*e).max(n);
        }
    }
    Ok(out)
}

fn columns(conn: &Connection, db: &str, table: &str) -> rusqlite::Result<Vec<ColumnInfo>> {
    let mut s = conn.prepare(&format!("PRAGMA {}.table_info({})", quote(db), quote(table)))?;
    let cols = s
        .query_map([], |r| {
            Ok(ColumnInfo {
                name: r.get(1)?,
                type_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                nullable: r.get::<_, i64>(3)? == 0,
                default: r.get::<_, Option<String>>(4)?,
                primary_key: r.get::<_, i64>(5)? > 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

fn indexes(conn: &Connection, db: &str, table: &str) -> rusqlite::Result<Vec<Index>> {
    let q = quote(db);
    let mut s = conn.prepare(&format!("PRAGMA {q}.index_list({})", quote(table)))?;
    let list = s
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, String>(3)? == "pk")))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(list.len());
    for (name, unique, primary) in list {
        let mut s = conn.prepare(&format!("PRAGMA {q}.index_info({})", quote(&name)))?;
        let mut cols: Vec<(i64, String)> = s
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "<expr>".into()))))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        cols.sort_by_key(|c| c.0);
        out.push(Index { name, columns: cols.into_iter().map(|c| c.1).collect(), unique, primary });
    }
    Ok(out)
}

fn foreign_keys(conn: &Connection, db: &str, table: &str) -> rusqlite::Result<Vec<ForeignKey>> {
    let mut s = conn.prepare(&format!("PRAGMA {}.foreign_key_list({})", quote(db), quote(table)))?;
    let rows = s
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_id: BTreeMap<i64, Vec<(i64, String, String, Option<String>)>> = BTreeMap::new();
    for (id, seq, target, from, to) in rows {
        by_id.entry(id).or_default().push((seq, target, from, to));
    }
    let mut out = Vec::with_capacity(by_id.len());
    for (_, mut parts) in by_id {
        parts.sort_by_key(|p| p.0);
        let target = parts.first().map(|p| p.1.clone()).unwrap_or_default();
        let ref_columns = if parts.iter().all(|p| p.3.is_some()) {
            parts.iter().filter_map(|p| p.3.clone()).collect()
        } else {
            // `REFERENCES t` without columns: the target's primary key
            columns(conn, db, &target).map(|c| c.into_iter().filter(|c| c.primary_key).map(|c| c.name).collect()).unwrap_or_default()
        };
        out.push(ForeignKey {
            name: None,
            columns: parts.iter().map(|p| p.2.clone()).collect(),
            references: TableRef::new(Some(db), target),
            ref_columns,
        });
    }
    Ok(out)
}

pub(crate) fn load(conn: &Connection, filter: &SchemaFilter) -> rusqlite::Result<SchemaTree> {
    let mut tree = SchemaTree::default();
    let mut count = 0usize;
    for db in databases(conn)? {
        if filter.schema.as_deref().is_some_and(|s| s != db) {
            continue;
        }
        let est = estimates(conn, &db)?;
        let mut s = conn.prepare(&format!(
            "SELECT type, name FROM {}.sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
            quote(&db)
        ))?;
        let list = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tables = Vec::new();
        for (kind, name) in list {
            if filter.table.as_deref().is_some_and(|t| t != name) {
                continue;
            }
            if filter.max_tables != 0 && count >= filter.max_tables {
                break;
            }
            count += 1;
            let view = kind == "view";
            tables.push(Table {
                kind: if view { TableKind::View } else { TableKind::Table },
                row_estimate: est.get(&name).copied(),
                columns: columns(conn, &db, &name)?,
                indexes: if view { Vec::new() } else { indexes(conn, &db, &name)? },
                foreign_keys: if view { Vec::new() } else { foreign_keys(conn, &db, &name)? },
                name,
            });
        }
        if db == "temp" && tables.is_empty() {
            continue;
        }
        tree.namespaces.push(Namespace { name: db, tables });
    }
    Ok(tree)
}
