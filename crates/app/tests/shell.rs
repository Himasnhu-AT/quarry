#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The shell: every scene's accessibility outline matches its golden (`tests/a11y/<scene>.txt`),
//! every scene's pixels match theirs where a GPU is available (`tests/snapshots/<scene>.png`), and
//! a `.ui` script opens a fixture SQLite file, expands a table and types a SELECT that only the
//! person's own key press runs.
//!
//! After an intended change: `QUARRY_BLESS=1 cargo test -p quarry --test shell` rewrites the
//! outlines; `ASHLAR_UPDATE_SNAPSHOTS=1` records the pixels again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ashlar::app::{Driver, Renderer};
use ashlar::egui;
use ashlar::fs::AppDirs;
use quarry::scenes::SCENES;
use quarry_core::{Workspace, *};
use serde_json::json;

const W: f32 = 1024.0;
const H: f32 = 700.0;

fn driver(dir: &Path, renderer: Option<Renderer>) -> Driver<Workspace> {
    let rt = quarry::app().dirs(AppDirs::under(dir)).runtime();
    let mut d = match renderer {
        Some(r) => Driver::with_renderer(rt, r),
        None => Driver::new(rt),
    };
    d.set_screen(W, H);
    d.pixels_per_point(1.0);
    d.settle(60);
    d
}

fn outline(d: &mut Driver<Workspace>) -> String {
    let snap = d.call("ui.snapshot", json!({"compact": true})).unwrap();
    snap["tree"].as_str().unwrap_or("").to_string()
}

fn golden_dir(kind: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(kind)
}

#[test]
fn every_scene_matches_its_accessibility_golden() {
    let bless = std::env::var_os("QUARRY_BLESS").is_some();
    let dir = golden_dir("a11y");
    std::fs::create_dir_all(&dir).unwrap();
    let mut failed = Vec::new();
    for scene in SCENES {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = driver(tmp.path(), None);
        d.scene(scene).unwrap();
        d.advance(0.5);
        d.settle(60);
        let tree = outline(&mut d);
        assert!(tree.contains(&format!("quarry.screen.{scene}")), "{scene}: {tree}");
        let path = dir.join(format!("{scene}.txt"));
        if bless {
            std::fs::write(&path, &tree).unwrap();
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(want) if want == tree => {}
            Ok(want) => failed.push(format!("{scene}: outline changed\n--- golden\n{want}\n--- now\n{tree}")),
            Err(_) => failed.push(format!("{scene}: no golden {} (QUARRY_BLESS=1)", path.display())),
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

#[test]
fn every_scene_matches_its_golden_snapshot() {
    for scene in SCENES {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = driver(tmp.path(), Some(Renderer::wgpu()));
        d.scene(scene).unwrap();
        d.advance(0.5);
        d.settle(60);
        match d.assert_snapshot(scene) {
            Ok(o) => eprintln!("{scene}: {}", o.text()),
            Err(e) => panic!("{scene}: {e}"),
        }
    }
}

#[test]
fn shell_scenes_show_their_states() {
    let tmp = tempfile::tempdir().unwrap();
    let mut d = driver(tmp.path(), None);
    let expect: &[(&str, &[&str])] = &[
        ("empty", &["Connect to a database", "quarry.empty.new_connection", "quarry.sidebar.new_connection"]),
        ("connecting", &["Connecting to quarry-10M.sqlite", "quarry.empty.spinner"]),
        ("disconnected", &["quarry.empty.reconnect", "unable to open database file"]),
        (
            "editor",
            &["quarry.editor", "quarry.tree.table.fixture.main.orders", "quarry.tree.table.fixture.main.orders.total_cents", "built with ashlar"],
        ),
        ("results", &["rows 1,000 of ≥ 10,000,000 · 42 ms"]),
        ("query-error", &["quarry.editor.error", "line 1, column 15"]),
        ("connection-dialog", &["quarry.dialog.connection.agent", "quarry.dialog.connection.sensitive", "Agent access"]),
        ("history", &["quarry.history.search", "SELECT * FROM orderz;"]),
        ("open-table", &["quarry.open_table.search", "customers"]),
        ("sidebar-menu", &["quarry.tree.menu.open_table", "quarry.tree.menu.copy_name", "quarry.tree.menu.new_query"]),
    ];
    for (scene, wants) in expect {
        d.scene(scene).unwrap();
        d.settle(60);
        let tree = outline(&mut d);
        for w in *wants {
            assert!(tree.contains(w), "{scene}: {w} missing in\n{tree}");
        }
    }
}

fn fixture_db(path: &Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, password_hash TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id), status TEXT NOT NULL, total_cents INTEGER NOT NULL);",
    )
    .unwrap();
    for i in 1..=50 {
        c.execute("INSERT INTO customers VALUES (?1, ?2, 'x')", rusqlite::params![i, format!("c{i}")]).unwrap();
        c.execute("INSERT INTO orders VALUES (?1, ?2, 'paid', ?3)", rusqlite::params![i, i, i * 100]).unwrap();
    }
}

fn spec(path: &Path) -> ConnectionSpec {
    ConnectionSpec {
        id: ConnectionId::new("fixture"),
        name: "fixture".into(),
        target: Target::Sqlite { path: path.to_path_buf() },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec!["password".into()],
        human_read_only: false,
    }
}

/// Run frames on real time until `pred` holds for the outline (jobs run on real threads).
fn wait_for(d: &mut Driver<Workspace>, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let start = Instant::now();
    loop {
        d.advance(0.05);
        let tree = outline(d);
        if pred(&tree) {
            return tree;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}:\n{tree}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn a_ui_script_opens_a_fixture_expands_a_table_and_only_the_person_runs_the_select() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fixture.sqlite");
    fixture_db(&db);
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let saved = json!({"schema_version": 1, "values": {"quarry.connections": [{"spec": spec(&db), "approve_similar": false}]}});
    std::fs::write(config.join("settings.json"), serde_json::to_string(&saved).unwrap()).unwrap();
    let engine_ready = !matches!(quarry_engine::engine(EngineKind::Sqlite).connect_human(&spec(&db), None), Err(quarry_engine::EngineError::NotYet));

    let mut d = driver(tmp.path(), None);
    let script = include_str!("ui/open_fixture.ui");
    let (open, rest) = script.split_once("# --- connected\n").unwrap();
    let (expand_and_type, refused) = rest.split_once("# --- typed\n").unwrap();
    d.run_ui(open).unwrap();
    if !engine_ready {
        // the engine stream has not landed yet: the tree says why instead of hanging
        let tree = wait_for(&mut d, "the connect error", |t| t.contains("not built yet"));
        assert!(tree.contains("quarry.tree.error.fixture"), "{tree}");
        eprintln!("note: engine stubs; the run half of this test is skipped until quarry-engine connects");
        return;
    }
    wait_for(&mut d, "the schema", |t| t.contains("quarry.tree.table.fixture") && t.contains("orders"));
    d.run_ui(expand_and_type).unwrap();
    // the script's own Mod+Enter is injected input: no ticket, nothing runs
    d.run_ui(refused).unwrap();
    d.advance(0.3);
    let tree = outline(&mut d);
    assert!(tree.contains("rows —"), "an injected key press must not run SQL:\n{tree}");
    // the person's own key press runs it
    let key = |pressed| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::COMMAND };
    d.push_event(key(true));
    d.push_event(key(false));
    d.frame();
    let tree = wait_for(&mut d, "the result", |t| t.contains("rows 5"));
    assert!(tree.contains("sqlite@fixture"), "{tree}");
}
