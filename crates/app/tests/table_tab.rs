#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Table tabs in the window, against a fixture SQLite file: a table with a primary key opens
//! editable (its key from the schema), one without opens read-only with a note, `grid.filter`
//! re-reads with a guard-checked WHERE, and `edit.commit` shows the diff, asks to confirm, ignores
//! an injected click, and commits on the person's own click: one transaction, audited, pending
//! edits cleared, rows reloaded.

use std::path::Path;
use std::time::{Duration, Instant};

use ashlar::app::Driver;
use ashlar::egui;
use ashlar::fs::AppDirs;
use quarry::grid::last_window;
use quarry::services::services;
use quarry_core::*;
use serde_json::json;

fn fixture(dir: &Path) -> std::path::PathBuf {
    let db = dir.join("tt.sqlite");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT NOT NULL);
         CREATE TABLE log (line TEXT);
         INSERT INTO log VALUES ('a'), ('b');",
    )
    .unwrap();
    for i in 1..=10 {
        c.execute("INSERT INTO orders VALUES (?1, 'paid')", rusqlite::params![i]).unwrap();
    }
    let spec = ConnectionSpec {
        id: ConnectionId::new("tt"),
        name: "tt".into(),
        target: Target::Sqlite { path: db.clone() },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    };
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let saved = json!({"schema_version": 1, "values": {"quarry.connections": [{"spec": spec, "approve_similar": false}]}});
    std::fs::write(config.join("settings.json"), serde_json::to_string(&saved).unwrap()).unwrap();
    db
}

fn wait<T>(d: &mut Driver<Workspace>, what: &str, f: impl Fn(&mut Driver<Workspace>) -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        d.advance(0.05);
        if let Some(v) = f(d) {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// A grid fact once the table tab's rows have loaded.
fn loaded<T>(tab: TabId, f: impl Fn(&quarry::grid::GridState) -> T) -> Option<T> {
    last_window::with_grid(tab, |g, _| (g.status == quarry::grid::Status::Ready && !g.columns.is_empty()).then(|| f(g))).flatten()
}

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

fn statuses(db: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut s = c.prepare("SELECT status FROM orders ORDER BY id").unwrap();
    s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

#[test]
fn table_tabs_load_filter_and_commit_only_on_the_persons_confirmed_click() {
    let tmp = tempfile::tempdir().unwrap();
    let db = fixture(tmp.path());
    services().audit().set_path(Some(tmp.path().join("audit.jsonl")));
    let mut d = Driver::new(quarry::app().dirs(AppDirs::under(tmp.path())).runtime());
    d.set_screen(1280.0, 800.0);
    d.settle(60);
    d.runtime.execute(quarry::ids::CONN_OPEN, json!({"connection": "tt"})).unwrap();
    wait(&mut d, "the connection", |_| services().connections().get("tt").map(|_| ()));
    d.settle(30);

    // no primary key: read-only, with a note
    let log = TabId(d.runtime.execute(quarry::ids::TABLE_OPEN, json!({"connection": "tt", "table": "main.log"})).unwrap()["tab"].as_u64().unwrap());
    let (target, note) = wait(&mut d, "log rows", |_| loaded(log, |g| (g.target.clone(), g.read_only_note.clone())));
    assert_eq!(target, None);
    assert!(note.unwrap().contains("no primary key"));

    // a primary key: editable by `id`
    let tab =
        TabId(d.runtime.execute(quarry::ids::TABLE_OPEN, json!({"connection": "tt", "table": "main.orders"})).unwrap()["tab"].as_u64().unwrap());
    let target = wait(&mut d, "orders rows", |_| loaded(tab, |g| g.target.clone()));
    assert_eq!(target.map(|t| t.key), Some(vec![0]));

    // filter: guard-checked, canonical, re-read
    let e = d.runtime.execute(quarry::ids::GRID_FILTER, json!({"tab": tab.0, "where": "id <= 3; DROP TABLE orders"})).unwrap_err();
    assert!(e.to_string().contains("parse_error"), "{e}");
    let r = d.runtime.execute(quarry::ids::GRID_FILTER, json!({"tab": tab.0, "where": "id <= 3 -- first three"})).unwrap();
    assert_eq!(r["where"], "id <= 3");
    wait(&mut d, "the filtered rows", |_| loaded(tab, |g| g.total).filter(|t| matches!(t, Total::Known(3) | Total::AtLeast(3))));
    assert!(d.runtime.execute(quarry::ids::GRID_FILTER, json!({"tab": log.0, "where": "line = 'a'"})).is_ok());

    // a pending edit; Commit… shows the diff and asks, it commits nothing yet
    d.runtime.session_mut().edit("Edit Cell", |w| {
        let mut e = PendingEdits::new(tab, TableRef::new(Some("main"), "orders"), vec!["id".into()]);
        e.set(vec![Value::Int(2)], "status", Value::text("paid"), Value::text("shipped"));
        w.edits.push(e);
    });
    d.settle(10);
    real_click(&mut d, "quarry.grid.commit");
    d.settle(10);
    assert!(ashlar::ui::automation::find(d.ctx(), "quarry.grid.confirm.commit").is_some(), "the confirm shows");
    assert!(statuses(&db).iter().all(|s| s == "paid"));
    // an injected click on the confirm does nothing
    let _ = d.call("ui.click", json!({"target": {"id": "quarry.grid.confirm.commit"}}));
    d.settle(10);
    assert!(statuses(&db).iter().all(|s| s == "paid"));
    assert!(d.runtime.session().doc().edits(tab).is_some());
    // the person's click commits
    real_click(&mut d, "quarry.grid.confirm.commit");
    wait(&mut d, "the commit", |_| (statuses(&db).get(1).map(String::as_str) == Some("shipped")).then_some(()));
    wait(&mut d, "the edits to clear", |d| d.runtime.session().doc().edits(tab).is_none().then_some(()));
    let audit = services().audit().recent();
    let w = audit.iter().find(|a| a.kind == AuditKind::HumanWrite && a.connection.as_str() == "tt").unwrap();
    assert_eq!(w.sql, "UPDATE \"main\".\"orders\" SET \"status\" = 'shipped' WHERE \"id\" = 2;");
    assert_eq!(w.rows_affected, Some(1));
    // the grid reloads (still filtered)
    wait(&mut d, "the reload", |_| loaded(tab, |g| g.total).filter(|t| matches!(t, Total::Known(3) | Total::AtLeast(3))));
}
