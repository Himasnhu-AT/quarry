#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The app-level agent path against a throwaway PostgreSQL server: `db.query` (guarded,
//! capped, redacted, on the read-only connection), `db.propose_write` (counted estimate, nothing
//! runs), the person's click on Approve (the write runs once, on the window's connection, with
//! undo), and refusals. Runs only when `QUARRY_PG_URL` is set (CI's `postgres` job); never point
//! it at a real database. Works in its own schema and drops it.

use std::time::{Duration, Instant};

use ashlar::app::Driver;
use ashlar::egui;
use ashlar::fs::AppDirs;
use quarry::services::services;
use quarry_core::*;
use quarry_engine::{HumanConnection, QueryOptions};
use serde_json::json;

struct Pg {
    spec: ConnectionSpec,
    password: Option<String>,
    schema: String,
}

impl Pg {
    fn human(&self) -> Box<dyn HumanConnection> {
        quarry_engine::engine(quarry_core::EngineKind::Postgres)
            .connect_human(&self.spec, self.password.as_deref().map(Secret::new).as_ref())
            .unwrap()
    }
    fn statuses(&self) -> Vec<(i64, String)> {
        let mut h = self.human();
        let c = h.query(&format!("SELECT id, status FROM {}.orders ORDER BY id", self.schema), &QueryOptions::default()).unwrap();
        h.fetch(c.id, 0, 100)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| match (&r[0], &r[1]) {
                (Value::Int(i), Value::Text(s)) => (*i, s.to_string()),
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

impl Drop for Pg {
    fn drop(&mut self) {
        let _ = self.human().execute_in_tx(&[format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema)], None);
    }
}

fn pg(conn: &str) -> Option<Pg> {
    let Ok(url) = std::env::var("QUARRY_PG_URL") else {
        eprintln!("skipped: QUARRY_PG_URL is not set");
        return None;
    };
    let (target, secret) = quarry_engine::pg::parse_url(&url).unwrap();
    let spec = ConnectionSpec {
        id: ConnectionId::new(conn),
        name: conn.into(),
        target,
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec!["password".into()],
        human_read_only: false,
    };
    let p = Pg { spec, password: secret.map(|s| s.expose().to_string()), schema: format!("qa_{}_{conn}", std::process::id()) };
    p.human()
        .execute_in_tx(
            &[
                format!("DROP SCHEMA IF EXISTS {} CASCADE", p.schema),
                format!("CREATE SCHEMA {}", p.schema),
                format!("CREATE TABLE {}.orders (id int PRIMARY KEY, status text NOT NULL, password text)", p.schema),
                format!("INSERT INTO {}.orders SELECT i, 'paid', 'secret' || i FROM generate_series(1, 20) i", p.schema),
            ],
            None,
        )
        .unwrap();
    services().connections().register(p.spec.clone(), p.password.as_deref().map(Secret::new));
    Some(p)
}

fn driver(dir: &std::path::Path) -> Driver<Workspace> {
    let mut d = Driver::new(quarry::app().dirs(AppDirs::under(dir)).runtime());
    d.set_screen(1280.0, 800.0);
    d.settle(60);
    d.runtime.execute(quarry::ids::AGENT_TOGGLE_PANEL, json!({})).unwrap();
    d.settle(10);
    d
}

/// The person's own click (pointer events in the frame loop, not injected by the control port).
fn real_click(d: &mut Driver<Workspace>, id: &str) {
    d.frame();
    let el = ashlar::ui::automation::find(d.ctx(), id).unwrap_or_else(|| panic!("no element {id}"));
    let pos = egui::pos2(el.rect[0] + el.rect[2] / 2.0, el.rect[1] + el.rect[3] / 2.0);
    let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    d.push_event(egui::Event::PointerMoved(pos));
    d.frame();
    d.push_event(button(true));
    d.frame();
    d.push_event(button(false));
    d.frame();
}

fn wait_done(d: &mut Driver<Workspace>, id: ProposalId) -> ProposalStatus {
    let start = Instant::now();
    loop {
        d.advance(0.05);
        let s = services().proposals().get(id).map(|p| p.status).unwrap();
        if s.is_terminal() {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "proposal {} stuck at {s}", id.0);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_agent_reads_proposes_and_only_the_persons_click_writes_on_postgres() {
    let Some(p) = pg("pgagent") else { return };
    let tmp = tempfile::tempdir().unwrap();
    services().audit().set_path(Some(tmp.path().join("audit.jsonl")));
    let mut d = driver(tmp.path());
    let s = &p.schema;

    // reads: guarded, capped, redacted
    let r = d
        .runtime
        .execute(
            quarry::ids::DB_QUERY,
            json!({"connection": "pgagent", "sql": format!("SELECT id, password FROM {s}.orders ORDER BY id"), "limit": 5}),
        )
        .unwrap();
    assert_eq!(r["rows"].as_array().unwrap().len(), 5, "{r}");
    assert!(!r.to_string().contains("secret"), "a sensitive column leaked: {r}");
    for sql in [format!("DELETE FROM {s}.orders"), format!("LOCK TABLE {s}.orders"), format!("SELECT 1; DELETE FROM {s}.orders")] {
        let e = d.runtime.execute(quarry::ids::DB_QUERY, json!({"connection": "pgagent", "sql": sql})).unwrap_err();
        assert!(!e.to_string().is_empty());
    }

    // a proposal runs nothing and carries the counted estimate
    let r = d
        .runtime
        .execute(
            quarry::ids::DB_PROPOSE_WRITE,
            json!({"connection": "pgagent", "sql": format!("UPDATE {s}.orders SET status = 'refunded' WHERE id <= 2"), "reason": "test"}),
        )
        .unwrap();
    let id = ProposalId(r["proposal_id"].as_u64().unwrap());
    assert_eq!(services().proposals().get(id).and_then(|p| p.estimate).map(|e| e.rows), Some(2));
    d.settle(10);
    assert!(p.statuses().iter().all(|(_, st)| st == "paid"), "nothing runs before approval");

    // an injected click does not approve; the person's click does
    let _ = d.call("ui.click", json!({"target": {"id": format!("quarry.approval.{}.approve", id.0)}}));
    d.settle(10);
    assert_eq!(services().proposals().get(id).map(|p| p.status), Some(ProposalStatus::Pending));
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    assert_eq!(wait_done(&mut d, id), ProposalStatus::Done);
    let st = p.statuses();
    assert_eq!(st.iter().filter(|(_, s)| s == "refunded").count(), 2);
    let exec = services().audit().recent().into_iter().find(|a| a.proposal == Some(id) && a.kind == AuditKind::Executed).unwrap();
    assert_eq!(exec.rows_affected, Some(2));
    let undo = exec.undo_sql.unwrap();
    assert_eq!(undo.len(), 2);
    let mut h = p.human();
    h.execute_in_tx(&undo, None).unwrap();
    assert!(p.statuses().iter().all(|(_, st)| st == "paid"), "the undo statements restore the rows");

    // a write whose row count leaves the estimate rolls back
    let r = d
        .runtime
        .execute(
            quarry::ids::DB_PROPOSE_WRITE,
            json!({"connection": "pgagent", "sql": format!("DELETE FROM {s}.orders WHERE id > 18"), "reason": "test"}),
        )
        .unwrap();
    let id = ProposalId(r["proposal_id"].as_u64().unwrap());
    h.execute_in_tx(&[format!("UPDATE {s}.orders SET id = id + 100 WHERE id <= 5")], None).unwrap();
    d.settle(10);
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    assert_eq!(wait_done(&mut d, id), ProposalStatus::Failed);
    assert_eq!(p.statuses().len(), 20, "rolled back");
}
