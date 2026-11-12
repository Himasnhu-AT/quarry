#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The result grid in the running app (in process, on a virtual clock): the `.ui` script that
//! scrolls, selects and copies; fetches through ashlar jobs; pending edits as undo steps; golden
//! snapshots of each grid scene; and what a screen reader sees of the visible rows.
//!
//! Golden files: `tests/snapshots/grid-*.png` (`ASHLAR_UPDATE_SNAPSHOTS=1` records them; a
//! baseline from another GPU adapter is skipped with a note) and `tests/golden/grid-*.a11y.txt`
//! (`QUARRY_BLESS=1` rewrites them).

use std::sync::{Arc, Mutex, MutexGuard};

use ashlar::app::{Driver, Renderer};
use ashlar::fs::AppDirs;
use quarry::grid::{self, EditTarget, GridState};
use quarry_core::{Column, TabId, TableRef, Total, Value, Workspace};

use serde_json::json;

/// Grids live in a process-wide registry keyed by tab: one driver at a time.
fn serial() -> MutexGuard<'static, ()> {
    static M: Mutex<()> = Mutex::new(());
    M.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn driver(dir: &std::path::Path, renderer: Renderer) -> Driver<Workspace> {
    let mut d = Driver::with_renderer(quarry::app().dirs(AppDirs::under(dir)).runtime(), renderer);
    d.set_screen(1280.0, 800.0);
    d.pixels_per_point(1.0);
    d
}

/// The first tab a scene opens (a fresh document hands out id 1 first).
const TAB: TabId = TabId(1);

/// The pending-changes bar's text ("" when there is none).
fn pending(d: &mut Driver<Workspace>) -> String {
    d.settle(5);
    d.call("ui.get", json!({"target": {"id": "quarry.grid.pending"}, "what": "text"}))
        .ok()
        .and_then(|v| v["value"].as_str().map(str::to_string))
        .unwrap_or_default()
}

#[test]
fn ui_script_scrolls_selects_and_copies() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut d = driver(dir.path(), Renderer::none());
    d.scene("results").unwrap();
    d.settle(60);
    let report = d.run_ui(include_str!("ui/grid.ui")).unwrap_or_else(|e| panic!("{e}"));
    assert!(report.passed >= 12, "{report:?}");
}

#[test]
fn deferred_sources_stream_through_jobs_and_cancel_stale_fetches() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut d = driver(dir.path(), Renderer::none());
    d.scene("editor").unwrap();
    d.settle(10);
    let tab = TAB;
    let src = grid::source::MemSource::generated(3_000_000, Arc::new(|i| vec![Value::Int(i as i64), Value::text(format!("row {i}"))])).deferred();
    let g = GridState::open(Arc::from(vec![Column::new("n", "int8"), Column::new("label", "text")]), Total::AtLeast(0), Box::new(src));
    grid::last_window::attach(tab, g);
    // the first pages arrive from jobs, a frame at a time
    for _ in 0..200 {
        d.advance(0.02);
        // jobs run on real threads
        std::thread::sleep(std::time::Duration::from_millis(5));
        if grid::last_window::with_grid(tab, |g, _| g.cell(5, 1).cloned()).flatten().is_some() {
            break;
        }
    }
    assert_eq!(grid::last_window::with_grid(tab, |g, _| g.cell(5, 1).cloned()).flatten(), Some(Value::text("row 5")));
    // jump far down: only pages around the new viewport are kept
    grid::last_window::with_grid(tab, |g, _| {
        g.total = Total::Known(3_000_000);
        g.set_window(2_600_000);
        // window-relative: the window starts at 2,500,000
        g.scroll_to = Some(100_010);
    });
    for _ in 0..200 {
        d.advance(0.02);
        // jobs run on real threads
        std::thread::sleep(std::time::Duration::from_millis(5));
        if grid::last_window::with_grid(tab, |g, _| g.cell(2_600_010, 1).cloned()).flatten().is_some() {
            break;
        }
    }
    let (pages, near, far) =
        grid::last_window::with_grid(tab, |g, _| (g.cache.footprint().0, g.cell(2_600_010, 1).cloned(), g.cell(5, 1).cloned())).unwrap();
    assert_eq!(near, Some(Value::text("row 2600010")));
    assert_eq!(far, None, "the first pages were evicted");
    assert!(pages <= 4, "{pages} pages kept");
}

#[test]
fn pending_edits_are_undo_steps_and_never_commit_from_a_command() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut d = driver(dir.path(), Renderer::none());
    d.scene("grid-edits").unwrap();
    d.settle(30);
    let tab = TAB;
    let _ = tab;
    let start = "Pending: 3 cells changed · 1 new row · 1 deleted";
    assert_eq!(pending(&mut d), start);
    // the review shows the statements; Commit is a click the control port cannot fake
    let tree = d.call("ui.snapshot", json!({})).unwrap();
    assert!(tree["tree"].as_str().unwrap().contains("quarry.grid.commit"));
    d.call("ui.click", json!({"target": {"id": "quarry.grid.commit"}})).unwrap();
    assert_eq!(pending(&mut d), start, "an injected click commits nothing");
    // add a row, then undo it
    d.call("engine.execute", json!({"command": "edit.add_row", "params": {}})).unwrap();
    assert_eq!(pending(&mut d), "Pending: 3 cells changed · 2 new rows · 1 deleted");
    d.call("engine.execute", json!({"command": "app.undo", "params": {}})).unwrap();
    assert_eq!(pending(&mut d), start);
    // discard is one undo step too
    d.call("engine.execute", json!({"command": "edit.discard", "params": {}})).unwrap();
    assert_eq!(pending(&mut d), "");
    d.call("engine.execute", json!({"command": "app.undo", "params": {}})).unwrap();
    assert_eq!(pending(&mut d), start);
}

#[test]
fn editing_a_cell_records_a_pending_change() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut d = driver(dir.path(), Renderer::none());
    d.scene("editor").unwrap();
    d.settle(10);
    let tab = TAB;
    let src = grid::source::MemSource::new((0..10).map(|i| vec![Value::Int(i + 1), Value::text(format!("n{i}"))]).collect());
    let g = GridState::open(Arc::from(vec![Column::new("id", "int8"), Column::new("name", "text")]), Total::Known(10), Box::new(src))
        .editable(EditTarget { table: TableRef::new(None, "people"), key: vec![0] });
    grid::last_window::attach(tab, g);
    d.settle(10);
    d.call("ui.click", json!({"target": {"id": "quarry.grid.table.row.3"}})).unwrap();
    d.settle(5);
    d.call("engine.execute", json!({"command": "edit.cell", "params": {}})).unwrap();
    d.settle(5);
    d.call("ui.fill", json!({"target": {"id": "quarry.grid.editor"}, "text": "Grace", "submit": true})).unwrap();
    d.settle(10);
    assert_eq!(pending(&mut d), "Pending: 1 cell changed");
}

/// What a screen reader is told about the visible part of the grid.
fn a11y(d: &mut Driver<Workspace>) -> String {
    // the first snapshot turns AccessKit on; the next frame has the nodes
    let _ = d.call("ui.snapshot", json!({}));
    d.settle(5);
    let t = d.call("ui.snapshot", json!({"scope": {"id": "quarry.grid.table"}, "depth": 3})).unwrap();
    let text = t["tree"].as_str().unwrap().to_string();
    // refs differ run to run
    text.lines()
        .map(|l| {
            let mut out = String::new();
            let mut rest = l;
            while let Some(i) = rest.find(" [ref=") {
                out.push_str(&rest[..i]);
                rest = rest[i + 1..].split_once(']').map_or("", |(_, r)| r);
            }
            out.push_str(rest);
            out
        })
        .take(80)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn screen_readers_see_the_visible_rows() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut d = driver(dir.path(), Renderer::none());
    d.scene("grid-values").unwrap();
    d.settle(30);
    let got = a11y(&mut d);
    assert!(got.contains("cell \"payload\"") && got.contains("value=\"NULL\""), "{got}");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/grid-values.a11y.txt");
    if std::env::var_os("QUARRY_BLESS").is_some() || !path.exists() {
        std::fs::write(&path, format!("{got}\n")).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap();
    assert_eq!(got.trim_end(), want.trim_end(), "QUARRY_BLESS=1 to accept");
}

#[test]
fn grid_scenes_match_golden_snapshots() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let snaps = ashlar_debug::Snapshots {
        dir: std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots"),
        ..ashlar_debug::Snapshots::from_env()
    };
    for scene in ["results", "query-running", "query-error", "grid-loading", "grid-values", "grid-edits", "grid-wide"] {
        // a fresh folder per scene: the window keeps layout in settings
        let dir = tempfile::tempdir_in(dir.path()).unwrap();
        let mut d = driver(dir.path(), Renderer::wgpu());
        d.scene(scene).unwrap();
        d.settle(60);
        // `grid-region-*`: the shell's test owns `<scene>.png` (the whole window)
        let out = d.check_snapshot(&format!("grid-region-{}", scene.trim_start_matches("grid-")), &snaps, Some(json!({"id": "quarry.grid"})));
        assert!(out.is_ok(), "{scene}: {out:?}");
    }
}

/// A real run on a SQLite file: the shell opens the cursor and hands it to the grid, which
/// fetches the pages after the first one in jobs as the person scrolls.
#[test]
fn a_real_run_streams_past_the_first_page() {
    use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, Target};
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fixture.sqlite");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT NOT NULL, note TEXT);").unwrap();
    c.execute_batch("BEGIN").unwrap();
    for i in 1..=3_000 {
        c.execute("INSERT INTO orders VALUES (?1, ?2, ?3)", rusqlite::params![i, format!("s{i}"), if i % 2 == 0 { None } else { Some("n") }])
            .unwrap();
    }
    c.execute_batch("COMMIT").unwrap();
    drop(c);
    let spec = ConnectionSpec {
        id: ConnectionId::new("fixture"),
        name: "fixture".into(),
        target: Target::Sqlite { path: db.clone() },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    };
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let saved = json!({"schema_version": 1, "values": {"quarry.connections": [{"spec": spec, "approve_similar": false}]}});
    std::fs::write(config.join("settings.json"), serde_json::to_string(&saved).unwrap()).unwrap();
    let mut d = driver(tmp.path(), Renderer::none());
    d.settle(30);
    d.run_ui("exec conn.open '{\"connection\": \"fixture\"}'\nexec tab.new_editor '{\"connection\": \"fixture\"}'\n").unwrap();
    let wait = |d: &mut Driver<Workspace>, what: &str, pred: &dyn Fn(&str) -> bool| {
        let start = std::time::Instant::now();
        loop {
            d.advance(0.05);
            let t = d.call("ui.snapshot", json!({"compact": true})).unwrap()["tree"].as_str().unwrap_or("").to_string();
            if pred(&t) {
                return t;
            }
            assert!(start.elapsed().as_secs() < 20, "timed out waiting for {what}:\n{t}");
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    };
    wait(&mut d, "the connection", &|t| t.contains("quarry.tree.table.fixture"));
    d.run_ui("fill id=quarry.editor \"SELECT id, status, note FROM orders ORDER BY id\"\n").unwrap();
    // the person's own key press runs it (injected input cannot)
    let key = |pressed| ashlar::egui::Event::Key {
        key: ashlar::egui::Key::Enter,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: ashlar::egui::Modifiers::COMMAND,
    };
    d.push_event(key(true));
    d.push_event(key(false));
    d.frame();
    wait(&mut d, "the first rows", &|t| t.contains("quarry.grid.table.row.0"));
    // past the first page (1,000 rows): fetched by a job from the same cursor
    d.run_ui("scroll id=quarry.grid.table down 60000\nwait 300\n").unwrap();
    // a short last page ended the cursor: the count is exact
    wait(&mut d, "rows past the first page", &|t| t.contains("\"3,000 rows\"") && t.contains("quarry.grid.table.row.2"));
    let rows = d.call("ui.snapshot", json!({"scope": {"id": "quarry.grid.table"}})).unwrap()["tree"].as_str().unwrap_or("").to_string();
    let first = rows
        .lines()
        .find_map(|l| l.split("quarry.grid.table.row.").nth(1))
        .and_then(|r| r.split(']').next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap();
    assert!(first > 1_000, "scrolled past the first page: {first}");
    assert!(rows.contains(&format!("value=\"s{}\"", first + 1)), "the row's own values, fetched from the cursor:\n{rows}");
}
