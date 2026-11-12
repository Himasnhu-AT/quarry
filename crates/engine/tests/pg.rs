#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The PostgreSQL engine against a throwaway server. Runs only when `QUARRY_PG_URL` is set (a
//! local container, e.g. `docker run --rm -e POSTGRES_PASSWORD=pw -p 127.0.0.1:55432:5432
//! postgres:17` and `QUARRY_PG_URL=postgres://postgres:pw@127.0.0.1:55432/postgres?sslmode=disable`);
//! never point it at a real database. Each test works in its own schema and drops it.

use std::time::{Duration, Instant};

use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, EngineKind, Estimate, Secret, TableKind, TableRef, Total, Value};
use quarry_engine::{AgentConnection, EngineError, HumanConnection, QueryOptions, ReadStatement, SchemaFilter, engine, pg};

struct Db {
    spec: ConnectionSpec,
    password: Option<String>,
    schema: String,
}

fn db(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("QUARRY_PG_URL") else {
        eprintln!("skipped: QUARRY_PG_URL is not set");
        return None;
    };
    let (target, secret) = pg::parse_url(&url).unwrap();
    let schema = format!("qt_{}_{name}", std::process::id());
    let spec = ConnectionSpec {
        id: ConnectionId::new("pg"),
        name: "pg".into(),
        target,
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    };
    let d = Db { spec, password: secret.map(|s| s.expose().to_string()), schema };
    d.human()
        .execute_in_tx(
            &[
                format!("DROP SCHEMA IF EXISTS {} CASCADE", d.schema),
                format!("CREATE SCHEMA {}", d.schema),
                format!("CREATE TYPE {}.mood AS ENUM ('ok', 'meh')", d.schema),
                format!("CREATE TABLE {}.owners (id int PRIMARY KEY, name text NOT NULL)", d.schema),
                format!(
                    "CREATE TABLE {s}.items (id bigint PRIMARY KEY, owner_id int REFERENCES {s}.owners(id), name text NOT NULL DEFAULT 'x',
                        price numeric(10,2), mood {s}.mood, created timestamptz)",
                    s = d.schema
                ),
                format!("CREATE INDEX items_owner ON {}.items(owner_id)", d.schema),
                format!("CREATE VIEW {s}.cheap AS SELECT id FROM {s}.items WHERE price < 10", s = d.schema),
                format!("INSERT INTO {}.owners VALUES (1, 'ada'), (2, 'grace')", d.schema),
                format!(
                    "INSERT INTO {}.items SELECT i, 1 + i % 2, 'item ' || i, i * 0.5, 'ok', '2024-05-01T12:00:00Z' FROM generate_series(1, 2500) i",
                    d.schema
                ),
                format!("ANALYZE {}.items", d.schema),
            ],
            None,
        )
        .unwrap();
    Some(d)
}

impl Db {
    fn secret(&self) -> Option<Secret> {
        self.password.as_deref().map(Secret::new)
    }
    fn human(&self) -> Box<dyn HumanConnection> {
        engine(EngineKind::Postgres).connect_human(&self.spec, self.secret().as_ref()).unwrap()
    }
    fn agent(&self) -> Box<dyn AgentConnection> {
        engine(EngineKind::Postgres).connect_agent(&self.spec, self.secret().as_ref()).unwrap()
    }
    fn t(&self, table: &str) -> String {
        format!("{}.{table}", self.schema)
    }
    fn count(&self, h: &mut Box<dyn HumanConnection>, sql: &str) -> i64 {
        let c = h.query(sql, &QueryOptions::default()).unwrap();
        match h.fetch(c.id, 0, 1).unwrap().rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("{v:?}"),
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = self.human().execute_in_tx(&[format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema)], None);
    }
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
    let Some(d) = db("stream") else { return };
    let mut h = d.human();
    let c = h.query(&format!("SELECT id, name FROM {} ORDER BY id", d.t("items")), &QueryOptions::default()).unwrap();
    assert_eq!(c.columns[0].type_name, "int8");
    assert_eq!(c.total, Total::AtLeast(1000));
    assert_eq!(ids(&h.fetch(c.id, 0, 1000).unwrap()), (1..=1000).collect::<Vec<_>>());
    assert_eq!(ids(&h.fetch(c.id, 2000, 1000).unwrap()), (2001..=2500).collect::<Vec<_>>());
    assert_eq!(ids(&h.fetch(c.id, 10, 3).unwrap()), vec![11, 12, 13]);
    // a second cursor, and a typo, leave the first one alone
    let c2 = h.query("SELECT 1", &QueryOptions::default()).unwrap();
    assert_eq!(c2.total, Total::Known(1));
    assert!(matches!(h.query("SELEC 1", &QueryOptions::default()), Err(EngineError::Database { position: Some(1), .. })));
    assert_eq!(ids(&h.fetch(c.id, 1500, 2).unwrap()), vec![1501, 1502]);
    let capped = h.query(&format!("SELECT id FROM {}", d.t("items")), &QueryOptions { row_cap: Some(1200), timeout_ms: None }).unwrap();
    assert_eq!(h.fetch(capped.id, 1000, 1000).unwrap().rows.len(), 200);
    h.close(c.id);
    assert_eq!(h.fetch(c.id, 0, 1).err(), Some(EngineError::Closed));
    // non-cursor statements with rows
    let c = h.query("SHOW server_version", &QueryOptions::default()).unwrap();
    assert_eq!(c.total, Total::Known(1));
}

#[test]
fn values_map() {
    let Some(d) = db("values") else { return };
    let mut h = d.human();
    let c = h
        .query(
            &format!(
                "SELECT 1::int2, 2::int4, 3::int8, 1.5::float8, 12.30::numeric(5,2), 'é'::text, '\\x00ff'::bytea, true,
                    '2024-05-01'::date, '2024-05-01T12:00:00Z'::timestamptz, '1 day 02:00'::interval, '{{\"a\":1}}'::jsonb,
                    '00000000-0000-0000-0000-000000000001'::uuid, ARRAY[1,2,NULL]::int[], ARRAY['a','b'], 'ok'::{s}.mood,
                    '10.0.0.0/8'::cidr, NULL::text, ROW(1, 'x'), '[1,5)'::int4range, repeat('x', 70000)",
                s = d.schema
            ),
            &QueryOptions::default(),
        )
        .unwrap();
    let r = &h.fetch(c.id, 0, 1).unwrap().rows[0];
    let want = [
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Float(1.5),
        Value::text("12.30"),
        Value::text("é"),
        Value::Bytes { len: 2, preview: std::sync::Arc::from(&[0u8, 255][..]) },
        Value::Bool(true),
        Value::Time("2024-05-01".into()),
        Value::Time("2024-05-01T12:00:00Z".into()),
        Value::Time("P1DT2H".into()),
        Value::Json("{\"a\": 1}".into()),
        Value::text("00000000-0000-0000-0000-000000000001"),
        Value::Json("[1,2,null]".into()),
        Value::Json("[\"a\",\"b\"]".into()),
        Value::text("ok"),
        Value::text("10.0.0.0/8"),
        Value::Null,
        Value::Json("[1,\"x\"]".into()),
        Value::text("[1,5)"),
    ];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(&r[i], w, "column {i}");
    }
    assert!(matches!(&r[20], Value::Clipped { len: 70_000, .. }));
}

#[test]
fn schema_from_the_catalog() {
    let Some(d) = db("schema") else { return };
    for tree in [d.human().schema(&SchemaFilter::default()).unwrap(), d.agent().schema(&SchemaFilter::default()).unwrap()] {
        let items = tree.table(&TableRef::new(Some(&d.schema), "items")).unwrap();
        assert_eq!(items.kind, TableKind::Table);
        assert_eq!(items.row_estimate, Some(2500));
        assert!(items.columns[0].primary_key && !items.columns[0].nullable);
        assert_eq!(items.columns[3].type_name, "numeric(10,2)");
        assert_eq!(items.columns[2].default.as_deref(), Some("'x'::text"));
        assert!(items.indexes.iter().any(|i| i.name == "items_owner" && i.columns == ["owner_id"]));
        assert!(items.indexes.iter().any(|i| i.primary));
        assert_eq!(items.foreign_keys[0].references, TableRef::new(Some(&d.schema), "owners"));
        assert_eq!(items.foreign_keys[0].ref_columns, ["id"]);
        assert_eq!(tree.table(&TableRef::new(Some(&d.schema), "cheap")).unwrap().kind, TableKind::View);
    }
    let one = d.human().schema(&SchemaFilter { schema: Some(d.schema.clone()), table: Some("owners".into()), max_tables: 0 }).unwrap();
    assert_eq!(one.table_count(), 1);
}

#[test]
fn cancel_and_timeouts() {
    let Some(d) = db("cancel") else { return };
    let mut h = d.human();
    let keep = h.query(&format!("SELECT id FROM {} ORDER BY id", d.t("items")), &QueryOptions::default()).unwrap();
    let token = h.cancel_token();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
    });
    let started = Instant::now();
    assert_eq!(h.query("SELECT pg_sleep(30)", &QueryOptions::default()).err(), Some(EngineError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(10));
    t.join().unwrap();
    // the first page of the cancelled query ran in a savepoint: the other cursor lives
    assert_eq!(ids(&h.fetch(keep.id, 1000, 1).unwrap()), vec![1001]);
    assert_eq!(h.query("SELECT pg_sleep(30)", &QueryOptions { timeout_ms: Some(200), row_cap: None }).err(), Some(EngineError::Timeout));
    let mut a = d.agent();
    assert_eq!(
        a.query(&ReadStatement::unchecked("SELECT pg_sleep(30)", EngineKind::Postgres), &QueryOptions { timeout_ms: Some(200), row_cap: None }).err(),
        Some(EngineError::Timeout)
    );
    let c = a.query(&ReadStatement::unchecked("SELECT 1", EngineKind::Postgres), &QueryOptions::default()).unwrap();
    assert_eq!(c.total, Total::Known(1));
}

#[test]
fn transactions_and_explain() {
    let Some(d) = db("tx") else { return };
    let mut h = d.human();
    let out = h
        .execute_in_tx(
            &[format!("UPDATE {} SET price = 0 WHERE id <= 10", d.t("items")), format!("DELETE FROM {} WHERE id = 11", d.t("items"))],
            None,
        )
        .unwrap();
    assert_eq!(out.per_statement, vec![10, 1]);
    assert!(h.execute_in_tx(&[format!("DELETE FROM {} WHERE id = 12", d.t("items")), "INSERT INTO nope VALUES (1)".into()], None).is_err());
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM {} WHERE id = 12", d.t("items"))), 1);
    let r = h.execute_in_tx(&[format!("DELETE FROM {} WHERE id > 100", d.t("items"))], Some(Estimate { rows: 10, exact: true, tolerance: 0 }));
    assert_eq!(r.err(), Some(EngineError::OverEstimate { rows_affected: 2400, expected: 10 }));
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM {}", d.t("items"))), 2499);
    // a write typed by the person commits
    h.query(&format!("UPDATE {} SET name = 'Ada' WHERE id = 1", d.t("owners")), &QueryOptions::default()).unwrap();
    let mut other = d.human();
    assert_eq!(d.count(&mut other, &format!("SELECT count(*) FROM {} WHERE name = 'Ada'", d.t("owners"))), 1);
    let plan = h.explain(&format!("SELECT * FROM {} WHERE id = 5", d.t("items")), false).unwrap();
    assert!(plan.op.contains("Scan"), "{plan:?}");
    assert!(plan.total_cost.is_some());
    let plan = h.explain(&format!("DELETE FROM {}", d.t("items")), true).unwrap();
    assert!(plan.actual_rows.is_some());
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM {}", d.t("items"))), 2499, "EXPLAIN ANALYZE committed");
}

/// Layer A alone: writes sent down the agent path with the classifier switched off fail, and the
/// data is unchanged. Statements that start like a read (`SELECT`, `WITH`, …) reach the server,
/// whose read-only transaction refuses the write inside them; every other statement is refused
/// by the agent session's keyword gate before the server sees it, including the ones PostgreSQL
/// itself accepts in a read-only transaction (`LOCK`, `REINDEX`, `CLUSTER`, `NOTIFY`, `LISTEN`,
/// `DO`, `PREPARE`, `SET`).
#[test]
fn every_write_through_the_agent_connection_fails_at_the_server() {
    let Some(d) = db("agent") else { return };
    let mut a = d.agent();
    let q = |a: &mut Box<dyn AgentConnection>, sql: &str| a.query(&ReadStatement::unchecked(sql, EngineKind::Postgres), &QueryOptions::default());
    let c = q(&mut a, &format!("SELECT count(*) FROM {}", d.t("items"))).unwrap();
    assert_eq!(a.fetch(c.id, 0, 1).unwrap().rows[0][0], Value::Int(2500));
    a.close(c.id);
    let s = &d.schema;
    let writes = [
        format!("INSERT INTO {s}.owners VALUES (3, 'linus')"),
        format!("INSERT INTO {s}.owners VALUES (4, 'x') RETURNING id"),
        format!("UPDATE {s}.items SET price = 0"),
        format!("DELETE FROM {s}.items"),
        format!("WITH d AS (DELETE FROM {s}.items RETURNING *) SELECT count(*) FROM d"),
        format!("MERGE INTO {s}.owners o USING (SELECT 1 AS id) n ON o.id = n.id WHEN MATCHED THEN DELETE"),
        format!("TRUNCATE {s}.items"),
        format!("CREATE TABLE {s}.t (a int)"),
        format!("DROP TABLE {s}.items CASCADE"),
        format!("ALTER TABLE {s}.items ADD COLUMN z int"),
        format!("CREATE INDEX ON {s}.items(price)"),
        format!("COPY {s}.owners FROM STDIN"),
        format!("SELECT * FROM {s}.items FOR UPDATE"),
        format!("LOCK TABLE {s}.items"),
        format!("CREATE SEQUENCE {s}.q"),
        format!("GRANT SELECT ON {s}.items TO PUBLIC"),
        format!("COMMENT ON TABLE {s}.items IS 'x'"),
        format!("VACUUM {s}.items"),
        format!("REINDEX TABLE {s}.items"),
        format!("CLUSTER {s}.items"),
        format!("LOCK TABLE {s}.items IN ACCESS SHARE MODE"),
        "LISTEN quarry".into(),
        "PREPARE p AS SELECT 1".into(),
        "CHECKPOINT".into(),
        "SELECT nextval('pg_catalog.pg_class_oid_index')".into(),
        "DO $$ BEGIN PERFORM 1; END $$".into(),
        "NOTIFY quarry".into(),
        "SET TRANSACTION READ WRITE".into(),
        "SELECT 1; DELETE FROM pg_class".into(),
    ];
    for sql in &writes {
        let r = q(&mut a, sql);
        assert!(r.is_err(), "{sql} ran on the agent connection: {r:?}");
    }
    // session tampering is undone: the read-only transaction rolls back
    let _ = q(&mut a, "SET default_transaction_read_only = off");
    let _ = q(&mut a, "SELECT set_config('default_transaction_read_only', 'off', false)");
    assert!(q(&mut a, &format!("INSERT INTO {s}.owners VALUES (5, 'x')")).is_err());
    // ending the transaction is caught, and the next statement is read-only again
    let r = q(&mut a, "COMMIT");
    assert!(r.is_err(), "{r:?}");
    let _ = q(&mut a, "SET default_transaction_read_only = off");
    assert!(q(&mut a, &format!("INSERT INTO {s}.owners VALUES (6, 'x')")).is_err());
    let c = q(&mut a, "SHOW default_transaction_read_only").unwrap();
    assert_eq!(a.fetch(c.id, 0, 1).unwrap().rows[0][0], Value::text("on"));
    // a session-level advisory lock is allowed in a read-only transaction and would outlive it:
    // the agent session drops them when its transaction ends
    let c = q(&mut a, "SELECT pg_advisory_lock(424242)").unwrap();
    a.close(c.id);
    let mut h = d.human();
    assert_eq!(
        d.count(&mut h, "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 424242"),
        0,
        "advisory lock held after the agent statement"
    );
    drop(h);
    // EXPLAIN ANALYZE of a write fails too
    assert!(a.explain(&ReadStatement::unchecked(&format!("DELETE FROM {s}.items"), EngineKind::Postgres), true).is_err());
    drop(a);
    let mut h = d.human();
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM {s}.items")), 2500);
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM {s}.owners")), 2);
    assert_eq!(d.count(&mut h, &format!("SELECT count(*) FROM pg_tables WHERE schemaname = '{s}'")), 2);
}
