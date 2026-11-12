//! PostgreSQL schema introspection from `pg_catalog`, four queries whatever the table count.
//! Row estimates are `pg_class.reltuples` (`None` before the first `ANALYZE`), never `COUNT(*)`.

use std::collections::HashMap;

use quarry_core::{ColumnInfo, ForeignKey, Index, Namespace, SchemaTree, Table, TableKind, TableRef};
use tokio_postgres::Client;

use crate::SchemaFilter;

const USER_SCHEMAS: &str =
    "nspname NOT IN ('pg_catalog', 'information_schema') AND nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp\\_%'";

pub(crate) async fn load(client: &Client, filter: &SchemaFilter) -> Result<SchemaTree, tokio_postgres::Error> {
    let limit: Option<i64> = (filter.max_tables != 0).then(|| i64::try_from(filter.max_tables).unwrap_or(i64::MAX));
    let namespaces = client
        .query(
            &format!("SELECT nspname::text FROM pg_namespace WHERE {USER_SCHEMAS} AND ($1::text IS NULL OR nspname = $1) ORDER BY 1"),
            &[&filter.schema],
        )
        .await?;
    let tables = client
        .query(
            &format!(
                "SELECT c.oid, n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::float8
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND {USER_SCHEMAS}
                   AND ($1::text IS NULL OR n.nspname = $1) AND ($2::text IS NULL OR c.relname = $2)
                 ORDER BY 2, 3 LIMIT $3"
            ),
            &[&filter.schema, &filter.table, &limit],
        )
        .await?;
    let oids: Vec<u32> = tables.iter().filter_map(|r| r.try_get::<_, u32>(0).ok()).collect();

    let mut columns: HashMap<u32, Vec<ColumnInfo>> = HashMap::new();
    for r in client
        .query(
            "SELECT a.attrelid, a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                    pg_get_expr(d.adbin, d.adrelid)
             FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE a.attrelid = ANY($1) AND a.attnum > 0 AND NOT a.attisdropped
             ORDER BY a.attrelid, a.attnum",
            &[&oids],
        )
        .await?
    {
        let (Ok(oid), Ok(name)) = (r.try_get::<_, u32>(0), r.try_get::<_, String>(1)) else { continue };
        columns.entry(oid).or_default().push(ColumnInfo {
            name,
            type_name: r.try_get::<_, Option<String>>(2).ok().flatten().unwrap_or_default(),
            nullable: r.try_get(3).unwrap_or(true),
            default: r.try_get(4).ok().flatten(),
            primary_key: false,
        });
    }

    let mut indexes: HashMap<u32, Vec<Index>> = HashMap::new();
    for r in client
        .query(
            "SELECT i.indrelid, ic.relname::text, i.indisunique, i.indisprimary,
                    ARRAY(SELECT COALESCE(a.attname::text, '<expr>')
                          FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
                          LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                          ORDER BY k.ord)
             FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid
             WHERE i.indrelid = ANY($1) ORDER BY 1, 2",
            &[&oids],
        )
        .await?
    {
        let (Ok(oid), Ok(name)) = (r.try_get::<_, u32>(0), r.try_get::<_, String>(1)) else { continue };
        indexes.entry(oid).or_default().push(Index {
            name,
            unique: r.try_get(2).unwrap_or(false),
            primary: r.try_get(3).unwrap_or(false),
            columns: r.try_get(4).unwrap_or_default(),
        });
    }

    let mut fks: HashMap<u32, Vec<ForeignKey>> = HashMap::new();
    for r in client
        .query(
            "SELECT c.conrelid, c.conname::text, fn.nspname::text, fc.relname::text,
                    ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(n, o)
                          JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n ORDER BY k.o),
                    ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(n, o)
                          JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n ORDER BY k.o)
             FROM pg_constraint c JOIN pg_class fc ON fc.oid = c.confrelid JOIN pg_namespace fn ON fn.oid = fc.relnamespace
             WHERE c.contype = 'f' AND c.conrelid = ANY($1) ORDER BY 1, 2",
            &[&oids],
        )
        .await?
    {
        let (Ok(oid), Ok(schema), Ok(table)) = (r.try_get::<_, u32>(0), r.try_get::<_, String>(2), r.try_get::<_, String>(3)) else {
            continue;
        };
        fks.entry(oid).or_default().push(ForeignKey {
            name: r.try_get(1).ok(),
            columns: r.try_get(4).unwrap_or_default(),
            references: TableRef::new(Some(&schema), table),
            ref_columns: r.try_get(5).unwrap_or_default(),
        });
    }

    let mut tree = SchemaTree {
        namespaces: namespaces.iter().filter_map(|r| r.try_get::<_, String>(0).ok()).map(|name| Namespace { name, tables: Vec::new() }).collect(),
    };
    for r in &tables {
        let (Ok(oid), Ok(schema), Ok(name), Ok(kind)) =
            (r.try_get::<_, u32>(0), r.try_get::<_, String>(1), r.try_get::<_, String>(2), r.try_get::<_, String>(3))
        else {
            continue;
        };
        let kind = match kind.as_str() {
            "v" => TableKind::View,
            "m" => TableKind::MaterializedView,
            "f" => TableKind::Foreign,
            _ => TableKind::Table,
        };
        let reltuples: f64 = r.try_get(4).unwrap_or(-1.0);
        let idx = indexes.remove(&oid).unwrap_or_default();
        let mut cols = columns.remove(&oid).unwrap_or_default();
        if let Some(pk) = idx.iter().find(|i| i.primary) {
            for c in &mut cols {
                c.primary_key = pk.columns.contains(&c.name);
            }
        }
        let table = Table {
            name,
            kind,
            // reltuples is -1 before the first ANALYZE (PostgreSQL 14+); views have none
            row_estimate: (kind != TableKind::View && reltuples >= 0.0).then(|| reltuples.round() as u64),
            columns: cols,
            indexes: idx,
            foreign_keys: fks.remove(&oid).unwrap_or_default(),
        };
        match tree.namespaces.iter_mut().find(|n| n.name == schema) {
            Some(n) => n.tables.push(table),
            None => tree.namespaces.push(Namespace { name: schema, tables: vec![table] }),
        }
    }
    Ok(tree)
}
