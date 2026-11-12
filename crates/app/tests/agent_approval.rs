#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Write approval in the window (SECURITY.md layer C), driven headless:
//!
//! - a proposal is never executed without approve;
//! - a `ui.click` on Approve injected through the control port, and an injected
//!   Cmd/Ctrl+Alt+Enter on the focused card, do NOT approve (no `HumanTicket` while agent input
//!   plays);
//! - the person's own click (a pointer event the control port did not inject) approves: the write
//!   runs once on the person's connection and is audited with its undo statements;
//! - the person's own key press approves too; deny denies; a write whose row count leaves the
//!   estimate rolls back.
//!
//! Tests share the process-wide proposal book, so they run one at a time (a lock).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ashlar::app::Driver;
use ashlar::egui;
use ashlar::fs::AppDirs;
use quarry::services::services;
use quarry_core::*;
use serde_json::json;

fn serial() -> MutexGuard<'static, ()> {
    static M: Mutex<()> = Mutex::new(());
    M.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Env {
    dir: tempfile::TempDir,
    db: PathBuf,
    conn: &'static str,
}

fn env(conn: &'static str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{conn}.sqlite"));
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT NOT NULL, total_cents INTEGER NOT NULL);").unwrap();
    for i in 1..=20i64 {
        c.execute("INSERT INTO orders VALUES (?1, 'paid', ?2)", rusqlite::params![i, i * 100]).unwrap();
    }
    services().audit().set_path(Some(dir.path().join("audit.jsonl")));
    services().connections().register(
        ConnectionSpec {
            id: ConnectionId::new(conn),
            name: conn.into(),
            target: Target::Sqlite { path: db.clone() },
            color: ConnColor::Local,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec![],
            human_read_only: false,
        },
        None,
    );
    Env { dir, db, conn }
}

fn driver(e: &Env) -> Driver<Workspace> {
    let mut d = Driver::new(quarry::app().dirs(AppDirs::under(e.dir.path())).runtime());
    d.set_screen(1280.0, 800.0);
    d.settle(60);
    d.runtime.execute(quarry::ids::AGENT_TOGGLE_PANEL, json!({})).unwrap();
    d.settle(10);
    d
}

fn propose(d: &mut Driver<Workspace>, e: &Env, sql: &str) -> ProposalId {
    let r = d.runtime.execute(quarry::ids::DB_PROPOSE_WRITE, json!({"connection": e.conn, "sql": sql, "reason": "test"})).unwrap();
    d.settle(10);
    d.frame();
    ProposalId(r["proposal_id"].as_u64().unwrap())
}

fn status(id: ProposalId) -> ProposalStatus {
    services().proposals().get(id).map(|p| p.status).unwrap()
}

fn statuses(db: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut s = c.prepare("SELECT status FROM orders ORDER BY id").unwrap();
    s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

/// The person's own click on element `id`: pointer events pushed into the frame loop, not
/// injected through the control port.
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

fn wait_until(d: &mut Driver<Workspace>, id: ProposalId, done: impl Fn(ProposalStatus) -> bool) -> ProposalStatus {
    let start = Instant::now();
    loop {
        d.advance(0.05);
        let s = status(id);
        if done(s) {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "proposal {} stuck at {s}", id.0);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn injected_clicks_and_keys_do_not_approve() {
    let _g = serial();
    let e = env("inject");
    let mut d = driver(&e);
    let id = propose(&mut d, &e, "UPDATE orders SET status = 'refunded' WHERE id = 3");
    let approve = format!("quarry.approval.{}.approve", id.0);
    let deny = format!("quarry.approval.{}.deny", id.0);
    assert!(ashlar::ui::automation::find(d.ctx(), &approve).is_some(), "the card is on screen");

    // the agent clicks Approve through the control port (also the plain alias)
    d.call("ui.click", json!({"id": approve})).unwrap();
    d.settle(30);
    d.call("ui.click", json!({"id": "quarry.approval.approve"})).unwrap();
    d.settle(30);
    // … and Deny
    d.call("ui.click", json!({"id": deny})).unwrap();
    d.settle(30);
    // … focuses the card and presses the approve shortcut
    let _ = d.call("ui.focus", json!({"id": deny}));
    d.call("ui.press", json!({"keys": "Mod+Alt+Enter"})).unwrap();
    d.settle(30);
    // … and runs a .ui script doing the same
    let _ = d.run_ui(&format!("click #{approve}\npress Mod+Alt+Enter\n"));
    d.settle(30);
    // … and pauses itself or asks the agent tools
    assert!(d.runtime.execute("proposal.approve", json!({"proposal_id": id.0})).is_err());
    std::thread::sleep(Duration::from_millis(200));
    d.advance(0.5);
    assert_eq!(status(id), ProposalStatus::Pending, "injected input decided a proposal");
    assert_eq!(statuses(&e.db).get(2).map(String::as_str), Some("paid"), "the write ran");
    assert!(!services().audit().recent().iter().any(|a| a.proposal == Some(id) && a.kind != AuditKind::Proposed));

    // clean up for the other tests: the person denies it
    real_click(&mut d, &deny);
    assert_eq!(status(id), ProposalStatus::Denied);
}

#[test]
fn the_persons_click_approves_runs_once_and_audits_with_undo() {
    let _g = serial();
    let e = env("click");
    let mut d = driver(&e);
    let id = propose(&mut d, &e, "UPDATE orders SET status = 'refunded' WHERE id = 3");
    assert_eq!(statuses(&e.db).get(2).map(String::as_str), Some("paid"), "not run before approval");
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    let s = wait_until(&mut d, id, |s| s.is_terminal());
    assert_eq!(s, ProposalStatus::Done);
    let st = statuses(&e.db);
    assert_eq!(st.get(2).map(String::as_str), Some("refunded"));
    assert_eq!(st.iter().filter(|s| *s == "refunded").count(), 1, "exactly the approved rows changed");
    let audit = services().audit().recent();
    let kinds: Vec<AuditKind> = audit.iter().filter(|a| a.proposal == Some(id)).map(|a| a.kind).collect();
    assert_eq!(kinds, vec![AuditKind::Proposed, AuditKind::Approved, AuditKind::Executed]);
    let exec = audit.iter().find(|a| a.proposal == Some(id) && a.kind == AuditKind::Executed).unwrap();
    assert_eq!(exec.rows_affected, Some(1));
    let undo = exec.undo_sql.clone().unwrap();
    assert_eq!(undo, vec!["UPDATE \"orders\" SET \"status\" = 'paid', \"total_cents\" = 300 WHERE \"id\" = 3".to_string()]);
    assert!(std::fs::read_to_string(e.dir.path().join("audit.jsonl")).unwrap().contains("\"executed\""));
    // the undo statements restore the row
    let c = rusqlite::Connection::open(&e.db).unwrap();
    for u in &undo {
        c.execute_batch(u).unwrap();
    }
    assert!(statuses(&e.db).iter().all(|s| s == "paid"));
}

#[test]
fn the_persons_key_press_approves_and_deny_denies() {
    let _g = serial();
    let e = env("keys");
    let mut d = driver(&e);
    let id = propose(&mut d, &e, "DELETE FROM orders WHERE id = 20");
    // ⌘⇧A focuses the newest card's Deny; the person presses Cmd/Ctrl+Alt+Enter
    d.runtime.execute(quarry::ids::PROPOSAL_FOCUS, json!({})).unwrap();
    d.settle(10);
    d.frame();
    let mods = egui::Modifiers::COMMAND | egui::Modifiers::ALT;
    let key = |pressed| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed, repeat: false, modifiers: mods };
    d.push_event(key(true));
    d.push_event(key(false));
    d.frame();
    assert_eq!(wait_until(&mut d, id, |s| s.is_terminal()), ProposalStatus::Done);
    assert_eq!(statuses(&e.db).len(), 19);
    let undo = services().audit().recent().into_iter().find(|a| a.proposal == Some(id) && a.kind == AuditKind::Executed).unwrap().undo_sql.unwrap();
    assert_eq!(undo, vec!["INSERT INTO \"orders\" (\"id\", \"status\", \"total_cents\") VALUES (20, 'paid', 2000)".to_string()]);

    let id = propose(&mut d, &e, "DELETE FROM orders WHERE id = 1");
    real_click(&mut d, &format!("quarry.approval.{}.deny", id.0));
    assert_eq!(status(id), ProposalStatus::Denied);
    assert_eq!(statuses(&e.db).len(), 19, "a denied write never runs");
    assert!(services().audit().recent().iter().any(|a| a.proposal == Some(id) && a.kind == AuditKind::Denied && a.actor == Actor::Human));
}

#[test]
fn a_write_outside_its_estimate_rolls_back() {
    let _g = serial();
    let e = env("estimate");
    let mut d = driver(&e);
    let id = propose(&mut d, &e, "UPDATE orders SET status = 'shipped' WHERE total_cents <= 100");
    assert_eq!(services().proposals().get(id).and_then(|p| p.estimate).map(|e| e.rows), Some(1));
    // the data changes between the estimate and the approval: 5 rows would now match
    rusqlite::Connection::open(&e.db).unwrap().execute_batch("UPDATE orders SET total_cents = 50 WHERE id <= 5").unwrap();
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    assert_eq!(wait_until(&mut d, id, |s| s.is_terminal()), ProposalStatus::Failed);
    assert!(statuses(&e.db).iter().all(|s| s == "paid"), "rolled back");
    assert!(services().audit().recent().iter().any(|a| a.proposal == Some(id) && a.kind == AuditKind::Failed));
}

#[test]
fn an_update_that_changes_the_primary_key_offers_no_undo() {
    let _g = serial();
    let e = env("pkmove");
    let mut d = driver(&e);
    // undo by the old key would find no row (or the wrong one): refused, the write still runs
    let id = propose(&mut d, &e, "UPDATE orders SET id = 100, status = 'moved' WHERE id = 4");
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    assert_eq!(wait_until(&mut d, id, |s| s.is_terminal()), ProposalStatus::Done);
    let exec = services().audit().recent().into_iter().find(|a| a.proposal == Some(id) && a.kind == AuditKind::Executed).unwrap();
    assert_eq!(exec.rows_affected, Some(1));
    assert_eq!(exec.undo_sql, None);
    assert_eq!(quarry::agent::approval::card(id).and_then(|c| c.undo), None);
    // a quoted, differently-cased key column is the same column
    let id = propose(&mut d, &e, "UPDATE orders SET \"ID\" = 101 WHERE id = 5");
    real_click(&mut d, &format!("quarry.approval.{}.approve", id.0));
    assert_eq!(wait_until(&mut d, id, |s| s.is_terminal()), ProposalStatus::Done);
    let exec = services().audit().recent().into_iter().find(|a| a.proposal == Some(id) && a.kind == AuditKind::Executed).unwrap();
    assert_eq!(exec.undo_sql, None);
}
