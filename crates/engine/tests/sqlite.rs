#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The SQLite engine against temp files: streaming cursors, values, schema, cancel and timeouts,
//! transactions, EXPLAIN, and layer A on the agent connection (every write fails in the engine,
//! with the classifier switched off).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quarry_core::limits::{BYTES_PREVIEW, CELL_CAP_BYTES};
use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, EngineKind, Estimate, TableKind, TableRef, Target, Total, Value};
use quarry_engine::{AgentConnection, EngineError, HumanConnection, QueryOptions, ReadStatement, SchemaFilter, engine};

fn spec(path: &Path) -> ConnectionSpec {
    ConnectionSpec {
        id: ConnectionId::new("t"),
        name: "T".into(),
        target: Target::Sqlite { path: path.to_path_buf() },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    }
}

const ROWS: i64 = 2_500;

/// A database with `items` (ROWS rows), `owners`, a view, an index, foreign keys and stats.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.sqlite");
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE owners (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            owner_id INTEGER REFERENCES owners(id),
            name TEXT NOT NULL DEFAULT 'x',
            price REAL,
            data BLOB,
            meta JSON,
            created DATETIME,
            flag BOOLEAN
         );
         CREATE INDEX items_owner ON items(owner_id);
         CREATE UNIQUE INDEX items_name_price ON items(name, price);
         CREATE VIEW cheap AS SELECT id, name FROM items WHERE price < 10;
         INSERT INTO owners VALUES (1, 'ada'), (2, 'grace');
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {ROWS})
         INSERT INTO items SELECT i, 1 + i % 2, 'item ' || i, i * 0.5, randomblob(100), '{{\"i\":' || i || '}}',
             '2024-05-01 12:00:00', i % 2 FROM n;
         ANALYZE;"
    ))
    .unwrap();
    (dir, path)
}

fn human(path: &Path) -> Box<dyn HumanConnection> {
    engine(EngineKind::Sqlite).connect_human(&spec(path), None).unwrap()
}

fn agent(path: &Path) -> Box<dyn AgentConnection> {
    engine(EngineKind::Sqlite).connect_agent(&spec(path), None).unwrap()
}

fn read(sql: &str) -> ReadStatement {
    ReadStatement::unchecked(sql, EngineKind::Sqlite)
}

fn ids(page: &quarry_core::Page) -> Vec<i64> {
    page.rows
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            ref v => panic!("{v:?}"),
        })
        .collect()
}

#[test]
fn streams_pages_in_any_order() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let c = h.query("SELECT id, name FROM items ORDER BY id", &QueryOptions::default()).unwrap();
    assert_eq!(c.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name"]);
    assert_eq!(c.total, Total::AtLeast(1000));
    let p = h.fetch(c.id, 0, 1000).unwrap();
    assert_eq!(ids(&p), (1..=1000).collect::<Vec<_>>());
    let p = h.fetch(c.id, 2000, 1000).unwrap();
    assert_eq!(p.first_row, 2000);
    assert_eq!(ids(&p), (2001..=ROWS).collect::<Vec<_>>());
    // backwards past the cache: the statement runs again
    let p = h.fetch(c.id, 10, 5).unwrap();
    assert_eq!(ids(&p), vec![11, 12, 13, 14, 15]);
    let p = h.fetch(c.id, 1500, 3).unwrap();
    assert_eq!(ids(&p), vec![1501, 1502, 1503]);
    assert!(h.fetch(c.id, 5000, 10).unwrap().rows.is_empty());
    h.close(c.id);
    assert_eq!(h.fetch(c.id, 0, 1).err(), Some(EngineError::Closed));
}

#[test]
fn small_results_are_known_at_once_and_row_caps_hold() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let c = h.query("SELECT id FROM items WHERE id <= 10", &QueryOptions::default()).unwrap();
    assert_eq!(c.total, Total::Known(10));
    let c = h.query("SELECT id FROM items", &QueryOptions { row_cap: Some(1500), timeout_ms: None }).unwrap();
    assert_eq!(c.total, Total::AtLeast(1000));
    let p = h.fetch(c.id, 1000, 1000).unwrap();
    assert_eq!(p.rows.len(), 500);
    let c = h.query("SELECT id FROM items", &QueryOptions { row_cap: Some(7), timeout_ms: None }).unwrap();
    assert_eq!(c.total, Total::Known(7));
    // a statement without rows
    let c = h.query("UPDATE owners SET name = 'Ada' WHERE id = 1", &QueryOptions::default()).unwrap();
    assert!(c.columns.is_empty());
    assert_eq!(c.total, Total::Known(0));
    let c = h.query("SELECT name FROM owners WHERE id = 1", &QueryOptions::default()).unwrap();
    assert_eq!(h.fetch(c.id, 0, 1).unwrap().rows, vec![vec![Value::text("Ada")]]);
    // errors are errors
    assert!(matches!(h.query("SELEC 1", &QueryOptions::default()), Err(EngineError::Database { .. })));
    assert!(matches!(h.query("SELECT 1; SELECT 2", &QueryOptions::default()), Err(EngineError::Database { .. })));
}

#[test]
fn values_map_and_cells_are_capped() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let c = h.query("SELECT id, price, data, meta, created, flag, NULL, x'00ff' FROM items WHERE id = 3", &QueryOptions::default()).unwrap();
    let row = &h.fetch(c.id, 0, 1).unwrap().rows[0];
    assert_eq!(row[0], Value::Int(3));
    assert_eq!(row[1], Value::Float(1.5));
    assert!(matches!(&row[2], Value::Bytes { len: 100, preview } if preview.len() == BYTES_PREVIEW));
    assert_eq!(row[3], Value::Json("{\"i\":3}".into()));
    assert_eq!(row[4], Value::Time("2024-05-01 12:00:00".into()));
    assert_eq!(row[5], Value::Bool(true));
    assert_eq!(row[6], Value::Null);
    assert!(matches!(&row[7], Value::Bytes { len: 2, preview } if **preview == [0, 255]));
    let c = h.query("SELECT replace(hex(zeroblob(100000)), '00', 'é'), length(hex(zeroblob(10)))", &QueryOptions::default()).unwrap();
    let row = &h.fetch(c.id, 0, 1).unwrap().rows[0];
    match &row[0] {
        Value::Clipped { preview, len } => {
            assert_eq!(*len, 200_000);
            assert!(preview.len() <= CELL_CAP_BYTES && preview.chars().all(|c| c == 'é'));
        }
        v => panic!("{v:?}"),
    }
    assert_eq!(row[1], Value::Int(20));
}

#[test]
fn schema_has_tables_views_columns_indexes_keys_and_estimates() {
    let (_d, path) = fixture();
    for tree in [human(&path).schema(&SchemaFilter::default()).unwrap(), agent(&path).schema(&SchemaFilter::default()).unwrap()] {
        assert_eq!(tree.namespaces.len(), 1, "{tree:?}");
        assert_eq!(tree.namespaces[0].name, "main");
        let items = tree.table(&TableRef::new(None, "items")).unwrap();
        assert_eq!(items.kind, TableKind::Table);
        assert_eq!(items.row_estimate, Some(ROWS as u64));
        let cols: Vec<_> = items.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str(), c.primary_key)).collect();
        assert_eq!(cols[0], ("id", "INTEGER", true));
        assert_eq!(cols[5], ("meta", "JSON", false));
        assert_eq!(items.columns[2].default.as_deref(), Some("'x'"));
        assert!(!items.columns[2].nullable);
        let idx: Vec<_> = items.indexes.iter().map(|i| (i.name.as_str(), i.columns.clone(), i.unique)).collect();
        assert!(idx.contains(&("items_owner", vec!["owner_id".to_string()], false)), "{idx:?}");
        assert!(idx.contains(&("items_name_price", vec!["name".to_string(), "price".to_string()], true)), "{idx:?}");
        assert_eq!(items.foreign_keys.len(), 1);
        assert_eq!(items.foreign_keys[0].columns, ["owner_id"]);
        assert_eq!(items.foreign_keys[0].references.name, "owners");
        assert_eq!(items.foreign_keys[0].ref_columns, ["id"]);
        let view = tree.table(&TableRef::new(Some("main"), "cheap")).unwrap();
        assert_eq!(view.kind, TableKind::View);
        assert_eq!(view.columns.len(), 2);
        assert!(tree.table(&TableRef::new(None, "sqlite_stat1")).is_none());
    }
    let mut h = human(&path);
    let one = h.schema(&SchemaFilter { table: Some("owners".into()), ..SchemaFilter::default() }).unwrap();
    assert_eq!(one.table_count(), 1);
    let capped = h.schema(&SchemaFilter { max_tables: 2, ..SchemaFilter::default() }).unwrap();
    assert_eq!(capped.table_count(), 2);
    assert_eq!(h.schema(&SchemaFilter { schema: Some("nope".into()), ..SchemaFilter::default() }).unwrap().table_count(), 0);
}

const SLOW: &str = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";

#[test]
fn cancel_stops_the_running_statement_only() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let other = h.query("SELECT id FROM items ORDER BY id", &QueryOptions::default()).unwrap();
    let token = h.cancel_token();
    token.cancel(); // nothing runs: does nothing
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        token.cancel();
    });
    let started = Instant::now();
    assert_eq!(h.query(SLOW, &QueryOptions::default()).err(), Some(EngineError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
    t.join().unwrap();
    // the other cursor goes on
    assert_eq!(ids(&h.fetch(other.id, 1000, 2).unwrap()), vec![1001, 1002]);
}

#[test]
fn timeouts_hold_and_the_agent_has_one_by_default() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let started = Instant::now();
    assert_eq!(h.query(SLOW, &QueryOptions { timeout_ms: Some(150), row_cap: None }).err(), Some(EngineError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(3));
    let mut a = agent(&path);
    assert_eq!(a.query(&read(SLOW), &QueryOptions { timeout_ms: Some(100), row_cap: None }).err(), Some(EngineError::Timeout));
    // after a timeout the connection still works
    let c = a.query(&read("SELECT count(*) FROM items"), &QueryOptions::default()).unwrap();
    assert_eq!(a.fetch(c.id, 0, 1).unwrap().rows, vec![vec![Value::Int(ROWS)]]);
}

#[test]
fn execute_in_tx_commits_rolls_back_and_checks_estimates() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let out = h.execute_in_tx(&["UPDATE items SET price = 0 WHERE id <= 10".into(), "DELETE FROM items WHERE id = 11".into()], None).unwrap();
    assert_eq!(out.per_statement, vec![10, 1]);
    assert_eq!(out.rows_affected, 11);
    let count = |h: &mut Box<dyn HumanConnection>, sql: &str| {
        let c = h.query(sql, &QueryOptions::default()).unwrap();
        match h.fetch(c.id, 0, 1).unwrap().rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("{v:?}"),
        }
    };
    assert_eq!(count(&mut h, "SELECT count(*) FROM items WHERE price = 0"), 10);
    // an error rolls everything back
    let r = h.execute_in_tx(&["DELETE FROM items WHERE id = 12".into(), "INSERT INTO nope VALUES (1)".into()], None);
    assert!(matches!(r, Err(EngineError::Database { .. })), "{r:?}");
    assert_eq!(count(&mut h, "SELECT count(*) FROM items WHERE id = 12"), 1);
    // over the estimate: rolled back
    let r = h.execute_in_tx(&["DELETE FROM items WHERE id > 100".into()], Some(Estimate { rows: 10, exact: true, tolerance: 2 }));
    assert_eq!(r.err(), Some(EngineError::OverEstimate { rows_affected: (ROWS - 100) as u64, expected: 10 }));
    assert_eq!(count(&mut h, "SELECT count(*) FROM items"), ROWS - 1);
    // within it: committed
    let out = h.execute_in_tx(&["DELETE FROM items WHERE id > 2490".into()], Some(Estimate { rows: 9, exact: false, tolerance: 1 })).unwrap();
    assert_eq!(out.rows_affected, 10);
    // the person's read-only connection cannot write
    let mut ro = spec(&path);
    ro.human_read_only = true;
    let mut h = engine(EngineKind::Sqlite).connect_human(&ro, None).unwrap();
    assert!(h.execute_in_tx(&["DELETE FROM items".into()], None).is_err());
    assert!(h.query("DELETE FROM items", &QueryOptions::default()).is_err());
}

#[test]
fn explain_gives_a_plan_tree() {
    let (_d, path) = fixture();
    let mut h = human(&path);
    let plan = h.explain("SELECT * FROM items i JOIN owners o ON o.id = i.owner_id WHERE i.owner_id = 1", false).unwrap();
    assert_eq!(plan.op, "QUERY PLAN");
    let ops: Vec<_> = plan.children.iter().map(|c| c.op.clone()).collect();
    assert!(ops.iter().any(|o| o.starts_with("SEARCH")), "{ops:?}");
    assert!(plan.children.iter().any(|c| c.relation.as_deref() == Some("i") || c.relation.as_deref() == Some("items")), "{plan:?}");
    let plan = h.explain("SELECT * FROM items WHERE price > 100", true).unwrap();
    assert_eq!(plan.actual_rows, Some((ROWS - 200) as f64));
    assert!(plan.actual_ms.is_some());
    // EXPLAIN ANALYZE of a write never commits it
    h.explain("DELETE FROM items", true).unwrap();
    let c = h.query("SELECT count(*) FROM items", &QueryOptions::default()).unwrap();
    assert_eq!(h.fetch(c.id, 0, 1).unwrap().rows[0][0], Value::Int(ROWS));
    let mut a = agent(&path);
    let plan = a.explain(&read("SELECT * FROM items WHERE id = 5"), true).unwrap();
    assert_eq!(plan.actual_rows, Some(1.0));
    assert!(a.explain(&read("DELETE FROM items"), false).is_err());
}

#[test]
fn the_agent_reads() {
    let (_d, path) = fixture();
    let mut a = agent(&path);
    let c = a.query(&read("SELECT id, name FROM items WHERE id < 4 ORDER BY id"), &QueryOptions::default()).unwrap();
    assert_eq!(c.total, Total::Known(3));
    assert_eq!(ids(&a.fetch(c.id, 0, 10).unwrap()), vec![1, 2, 3]);
    for sql in [
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT sum(i) FROM n",
        "SELECT json_extract(meta, '$.i'), strftime('%Y', created), row_number() OVER () FROM items LIMIT 2",
        "SELECT * FROM cheap",
        "PRAGMA table_info(items)",
        "PRAGMA index_list(items)",
        "PRAGMA foreign_key_list(items)",
        "PRAGMA user_version",
        "SELECT * FROM pragma_table_info('items')",
        "EXPLAIN QUERY PLAN SELECT * FROM items",
    ] {
        assert!(a.query(&read(sql), &QueryOptions::default()).is_ok(), "{sql}: {:?}", a.query(&read(sql), &QueryOptions::default()));
    }
    // a statement classified for another engine is refused
    assert!(a.query(&ReadStatement::unchecked("SELECT 1", EngineKind::Postgres), &QueryOptions::default()).is_err());
}

/// Layer A alone: every write sent down the agent path with the classifier switched off fails in
/// the engine, and the file is unchanged afterwards.
#[test]
fn every_write_through_the_agent_connection_fails_in_the_engine() {
    let (dir, path) = fixture();
    let other = dir.path().join("other.sqlite");
    rusqlite::Connection::open(&other).unwrap().execute_batch("CREATE TABLE x (a)").unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut a = agent(&path);
    let writes = [
        "INSERT INTO owners VALUES (3, 'linus')",
        "INSERT INTO owners VALUES (4, 'x') RETURNING id",
        "REPLACE INTO owners VALUES (1, 'evil')",
        "INSERT OR REPLACE INTO owners VALUES (1, 'evil')",
        "UPDATE items SET price = 0",
        "UPDATE items SET price = 0 RETURNING id",
        "DELETE FROM items",
        "WITH d AS (SELECT 1) DELETE FROM items",
        "CREATE TABLE t (a)",
        "CREATE TABLE t AS SELECT * FROM items",
        "CREATE TEMP TABLE t (a)",
        "CREATE TEMP VIEW v AS SELECT 1",
        "CREATE VIEW v AS SELECT 1",
        "CREATE INDEX i ON items(price)",
        "CREATE TRIGGER tr AFTER INSERT ON owners BEGIN DELETE FROM items; END",
        "CREATE VIRTUAL TABLE f USING fts5(a)",
        "DROP TABLE items",
        "DROP VIEW cheap",
        "DROP INDEX items_owner",
        "ALTER TABLE items ADD COLUMN z",
        "ALTER TABLE items RENAME TO things",
        &format!("ATTACH '{}' AS o", other.display()),
        "ATTACH ':memory:' AS m",
        "DETACH main",
        "PRAGMA query_only = 0",
        "PRAGMA query_only",
        "PRAGMA writable_schema = 1",
        "PRAGMA user_version = 5",
        "PRAGMA journal_mode = DELETE",
        "PRAGMA foreign_keys = OFF",
        "PRAGMA trusted_schema = 1",
        "BEGIN",
        "BEGIN IMMEDIATE",
        "COMMIT",
        "SAVEPOINT s",
        "VACUUM",
        "VACUUM INTO 'copy.sqlite'",
        "ANALYZE",
        "REINDEX",
        "SELECT load_extension('libevil')",
        "SELECT fts3_tokenizer('simple')",
        "SELECT 1; DELETE FROM items",
    ];
    for sql in writes {
        let r = a.query(&read(sql), &QueryOptions::default());
        assert!(r.is_err(), "{sql} ran on the agent connection: {r:?}");
    }
    drop(a);
    let mut h = human(&path);
    let c = h.query("SELECT count(*) FROM items", &QueryOptions::default()).unwrap();
    assert_eq!(h.fetch(c.id, 0, 1).unwrap().rows[0][0], Value::Int(ROWS));
    let c = h.query("SELECT name FROM owners WHERE id = 1", &QueryOptions::default()).unwrap();
    assert_eq!(h.fetch(c.id, 0, 1).unwrap().rows[0][0], Value::text("ada"));
    drop(h);
    assert!(!dir.path().join("copy.sqlite").exists());
    // WAL frames may differ by checkpoints; the main file has no new schema or rows
    let after = std::fs::read(&path).unwrap();
    assert_eq!(before.len(), after.len());
}

#[test]
fn agent_access_off_refuses_and_missing_files_are_not_created() {
    let (_d, path) = fixture();
    let mut s = spec(&path);
    s.agent_access = AgentAccess::Off;
    assert!(matches!(engine(EngineKind::Sqlite).connect_agent(&s, None).err(), Some(EngineError::Connect(_))));
    let missing = path.with_file_name("missing.sqlite");
    assert!(engine(EngineKind::Sqlite).connect_human(&spec(&missing), None).is_err());
    assert!(engine(EngineKind::Sqlite).connect_agent(&spec(&missing), None).is_err());
    assert!(!missing.exists());
}

/// The xtask fixture, when generated (`cargo xtask fixtures --rows 100k --out fixtures/quarry-100k.sqlite`).
#[test]
fn xtask_fixture_when_present() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/quarry-100k.sqlite");
    if !path.exists() {
        eprintln!("skipped: {} not generated", path.display());
        return;
    }
    let mut a = agent(&path);
    let tree = a.schema(&SchemaFilter::default()).unwrap();
    let orders = tree.table(&TableRef::new(None, "orders")).unwrap();
    assert_eq!(orders.row_estimate, Some(100_000));
    let c = a.query(&read("SELECT * FROM orders"), &QueryOptions { row_cap: Some(5000), timeout_ms: None }).unwrap();
    assert_eq!(a.fetch(c.id, 4000, 1000).unwrap().rows.len(), 1000);
}
