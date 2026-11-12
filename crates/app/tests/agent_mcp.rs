#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The agent tools over MCP (`tools/list`, `tools/call`) against a SQLite fixture: reads work and
//! are redacted, every write attempt through `db.query` is refused, `db.propose_write` only
//! stores a pending proposal (the database is unchanged), and the UI and chart tools work.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ashlar::mcp::{Backend, McpServer, ServerOptions, ToolMode};
use quarry::services::services;
use quarry_core::*;
use serde_json::{Value as J, json};

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
}

fn fixture_db(path: &Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, country TEXT NOT NULL, password_hash TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, status TEXT NOT NULL, total_cents INTEGER NOT NULL);",
    )
    .unwrap();
    for i in 1..=50i64 {
        let country = ["US", "DE", "JP"][(i % 3) as usize];
        let status = ["paid", "shipped", "refunded"][(i % 3) as usize];
        c.execute("INSERT INTO customers VALUES (?1, ?2, ?3, 'secret-hash')", rusqlite::params![i, format!("c{i}"), country]).unwrap();
        c.execute("INSERT INTO orders VALUES (?1, ?2, ?3, ?4)", rusqlite::params![i, i, status, i * 100]).unwrap();
    }
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fixture.sqlite");
        fixture_db(&db);
        services().audit().set_path(Some(dir.path().join("audit.jsonl")));
        services().connections().register(
            ConnectionSpec {
                id: ConnectionId::new("fixture"),
                name: "fixture.sqlite".into(),
                target: Target::Sqlite { path: db.clone() },
                color: ConnColor::Local,
                agent_access: AgentAccess::ReadOnly,
                sensitive_patterns: vec![],
                human_read_only: false,
            },
            None,
        );
        Fixture { _dir: dir, db }
    })
}

fn server() -> McpServer {
    fixture();
    let backend = Backend::Headless(Box::new(quarry::app().headless()));
    let mut s = McpServer::new(backend, ServerOptions::new("quarry", "0").tools(ToolMode::PerCommand));
    s.handle(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}));
    s
}

/// `tools/call`: Ok(result JSON) or Err(error text).
fn call(s: &mut McpServer, tool: &str, args: J) -> Result<J, String> {
    let r = s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}})).unwrap();
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    if r["result"]["isError"] == json!(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(J::String(text))) }
}

fn orders_fingerprint(db: &Path) -> (i64, i64, String) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row("SELECT count(*), sum(total_cents), group_concat(status) FROM orders", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
}

#[test]
fn tools_list_has_the_agent_tools_with_schemas() {
    let mut s = server();
    let list = s.handle(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    for t in quarry::ids::AGENT_TOOLS {
        let name = ashlar::mcp::tools::tool_name(t.id);
        let tool = tools.iter().find(|x| x["name"] == json!(name)).unwrap_or_else(|| panic!("{name} missing"));
        assert!(tool["inputSchema"].is_object(), "{name}");
    }
}

#[test]
fn reads_work_and_redact_sensitive_columns() {
    let mut s = server();
    let conns = call(&mut s, "db_connections", json!({})).unwrap();
    let c = conns["connections"].as_array().unwrap().iter().find(|c| c["id"] == json!("fixture")).unwrap().clone();
    assert_eq!(c["engine"], json!("sqlite"));
    assert!(!conns.to_string().contains(fixture().db.to_string_lossy().as_ref()), "the path never reaches the agent");

    let schema = call(&mut s, "db_schema", json!({"connection": "fixture", "table": "orders"})).unwrap();
    assert!(schema.to_string().contains("total_cents"), "{schema}");

    let rows =
        call(&mut s, "db_query", json!({"connection": "fixture", "sql": "SELECT id, name, password_hash FROM customers ORDER BY id", "limit": 5}))
            .unwrap();
    assert_eq!(rows["row_count"], json!(5));
    assert_eq!(rows["truncated"], json!(true));
    assert_eq!(rows["rows"][0][2], json!({"$redacted": 11}), "{rows}");
    assert!(!rows.to_string().contains("secret-hash"));

    let sample = call(&mut s, "db_sample", json!({"connection": "fixture", "table": "customers", "n": 3})).unwrap();
    assert_eq!(sample["row_count"], json!(3));
    assert!(!sample.to_string().contains("secret-hash"), "{sample}");

    let plan = call(&mut s, "db_explain", json!({"connection": "fixture", "sql": "SELECT * FROM orders WHERE id = 3"})).unwrap();
    assert_eq!(plan["analyzed"], json!(false));
    let e = call(&mut s, "db_explain", json!({"connection": "fixture", "sql": "SELECT 1", "analyze": true})).unwrap_err();
    assert!(e.starts_with("not_read_only"), "{e}");
    // computing on or filtering by a sensitive column is refused
    let e = call(&mut s, "db_query", json!({"connection": "fixture", "sql": "SELECT lower(password_hash) FROM customers"})).unwrap_err();
    assert!(e.starts_with("sensitive"), "{e}");
    let e = call(&mut s, "db_query", json!({"connection": "nope", "sql": "SELECT 1"})).unwrap_err();
    assert!(e.starts_with("unknown_connection"), "{e}");
    assert!(services().audit().recent().iter().any(|a| a.kind == AuditKind::AgentRead && a.actor == Actor::Agent));
}

#[test]
fn every_write_through_db_query_is_refused_and_changes_nothing() {
    let mut s = server();
    let db = &fixture().db;
    let before = orders_fingerprint(db);
    let writes = [
        "DELETE FROM orders",
        "DELETE FROM orders WHERE id = 1",
        "UPDATE orders SET status = 'x'",
        "INSERT INTO orders VALUES (999, 1, 'paid', 1)",
        "REPLACE INTO orders VALUES (1, 1, 'x', 1)",
        "DROP TABLE orders",
        "CREATE TABLE t (x)",
        "ALTER TABLE orders ADD COLUMN y",
        "ATTACH 'other.db' AS o",
        "PRAGMA query_only = 0",
        "PRAGMA journal_mode = DELETE",
        "VACUUM",
        "BEGIN",
        "COMMIT",
        "SELECT 1; DELETE FROM orders",
        "SELECT 1 /* ; */; DROP TABLE orders",
        "WITH d AS (DELETE FROM orders RETURNING *) SELECT * FROM d",
        "EXPLAIN ANALYZE SELECT * FROM orders",
        "SELECT load_extension('x')",
        "SELECT writefile('/tmp/x', 'y')",
    ];
    for sql in writes {
        let r = call(&mut s, "db_query", json!({"connection": "fixture", "sql": sql}));
        let e = r.expect_err(sql);
        let code = e.split(':').next().unwrap_or_default();
        assert!(
            ["not_read_only", "multiple_statements", "parse_error", "denied_function"].contains(&code),
            "{sql}: refused with an unexpected code: {e}"
        );
        // charts take the same read path
        assert!(call(&mut s, "viz_chart", json!({"kind": "bar", "connection": "fixture", "sql": sql, "x": "status"})).is_err(), "{sql}");
    }
    assert_eq!(orders_fingerprint(db), before, "the database changed");
}

#[test]
fn propose_write_only_stores_a_pending_proposal() {
    let mut s = server();
    let db = &fixture().db;
    let before = orders_fingerprint(db);
    let r = call(
        &mut s,
        "db_propose_write",
        json!({"connection": "fixture", "sql": "UPDATE orders SET status = 'refunded' WHERE id = 3", "reason": "customer asked"}),
    )
    .unwrap();
    assert_eq!(r["status"], json!("pending"));
    let id = r["proposal_id"].as_u64().unwrap();
    let st = call(&mut s, "db_proposal_status", json!({"proposal_id": id})).unwrap();
    assert_eq!(st["status"], json!("pending"));
    assert_eq!(st["estimate"], json!({"rows": 1, "exact": true, "tolerance": 0}), "{st}");
    assert_eq!(st["sql"], json!("UPDATE orders SET status = 'refunded' WHERE id = 3"));
    // nothing an agent can call decides it
    for (tool, args) in [
        ("execute", json!({"command": "proposal.approve", "params": {"proposal_id": id}})),
        ("execute", json!({"command": "proposal.deny", "params": {"proposal_id": id}})),
        ("db_propose_write", json!({"connection": "fixture", "sql": "COMMIT", "reason": "x"})),
    ] {
        assert!(call(&mut s, tool, args).is_err(), "{tool}");
    }
    let st = call(&mut s, "db_proposal_status", json!({"proposal_id": id})).unwrap();
    assert_eq!(st["status"], json!("pending"));
    assert_eq!(orders_fingerprint(db), before, "a proposal ran without approval");
    assert!(services().audit().recent().iter().any(|a| a.kind == AuditKind::Proposed && a.proposal == Some(ProposalId(id))));
    // never-allowed statements are refused even as proposals; a reason is required
    let e = call(&mut s, "db_propose_write", json!({"connection": "fixture", "sql": "DELETE FROM orders; DELETE FROM customers", "reason": "x"}))
        .unwrap_err();
    assert!(e.starts_with("multiple_statements") || e.starts_with("parse_error"), "{e}");
    assert!(call(&mut s, "db_propose_write", json!({"connection": "fixture", "sql": "DELETE FROM orders", "reason": "  "})).is_err());
}

#[test]
fn view_and_viz_tools_drive_the_window_only() {
    let mut s = server();
    let tab = call(&mut s, "view_open_table", json!({"connection": "fixture", "table": "orders"})).unwrap()["tab"].as_u64().unwrap();
    assert!(call(&mut s, "view_open_table", json!({"connection": "fixture", "table": "nope"})).unwrap_err().starts_with("not_found"));
    let f = call(&mut s, "view_filter", json!({"tab": tab, "where": "status = 'paid' -- x"})).unwrap();
    assert_eq!(f["where"], json!("status = 'paid'"));
    assert!(call(&mut s, "view_filter", json!({"tab": tab, "where": "1 = 1; DROP TABLE orders"})).is_err());
    let sel = call(&mut s, "view_select", json!({"tab": tab, "rows": [0, {"from": 2, "to": 4}], "columns": ["status"]})).unwrap();
    assert_eq!((sel["rows"].as_u64(), sel["columns"].as_u64()), (Some(4), Some(1)));
    assert_eq!(call(&mut s, "view_focus", json!({"region": "agent"})).unwrap()["region"], json!("agent"));
    let ed = call(&mut s, "view_run_in_editor", json!({"sql": "DELETE FROM orders", "connection": "fixture"})).unwrap();
    assert!(ed["tab"].as_u64().is_some());

    let chart = call(
        &mut s,
        "viz_chart",
        json!({"kind": "bar", "connection": "fixture", "sql": "SELECT status, count(*) AS n FROM orders GROUP BY status", "x": "status", "y": "n"}),
    )
    .unwrap();
    assert_eq!((chart["kind"].clone(), chart["row_count"].clone()), (json!("bar"), json!(3)));
    let e = call(&mut s, "viz_chart", json!({"kind": "line", "connection": "fixture", "sql": "SELECT 1 AS a", "x": "a", "y": "b"})).unwrap_err();
    assert!(e.contains("column `b`"), "{e}");
    let h =
        call(&mut s, "viz_chart", json!({"kind": "histogram", "connection": "fixture", "sql": "SELECT total_cents FROM orders", "x": "total_cents"}))
            .unwrap();
    assert_eq!(h["row_count"], json!(50));
    let pivot = call(
        &mut s,
        "viz_pivot",
        json!({"connection": "fixture", "sql": "SELECT c.country, o.status, o.total_cents FROM orders o JOIN customers c ON c.id = o.customer_id", "rows": "country", "columns": "status", "value": "total_cents", "agg": "sum"}),
    )
    .unwrap();
    assert_eq!((pivot["row_count"].clone(), pivot["column_count"].clone()), (json!(3), json!(3)));
    let panel = chart["panel"].as_u64().unwrap();
    assert_eq!(call(&mut s, "viz_close", json!({"panel": panel})).unwrap()["closed"], json!(panel));
    assert!(call(&mut s, "viz_close", json!({"panel": panel})).unwrap_err().starts_with("not_found"));
}
