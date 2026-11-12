//! Quarry's benchmark probes. `cargo xtask bench` runs them (one process per run, so every run
//! starts cold in-process and the memory peak is that run's own) and keeps the medians; see
//! docs/benchmarks.md for the method.
//!
//! - `quarry-bench app FIXTURE`: the whole app on a headless ashlar `Driver` (no GPU renderer:
//!   the egui pass only, no tessellation or painting), 1280×800 pt at 2×. One run: connect and
//!   load the schema, open the `orders` table, then type `SELECT * FROM orders` and run it with
//!   the person's own Mod+Enter. Wall-clock milliseconds from the command (or key press) to the
//!   frame whose accessibility outline first shows the result, plus `app.stats` memory.
//! - `quarry-bench engine FIXTURE [--runs N]`: the engine alone on the same file: connect, schema,
//!   `SELECT * FROM orders` to the first page, and every page after it (rows/s).
//! - `quarry-bench pg [--rows N] [--runs N]`: the same against `QUARRY_PG_URL`, read-only: the
//!   rows come from `generate_series`, nothing is created on the server.
//!
//! Every probe prints one JSON object on stdout.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ashlar::app::Driver;
use ashlar::egui;
use ashlar::fs::AppDirs;
use quarry_core::limits::PAGE_ROWS;
use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, EngineKind, Target, Workspace};
use quarry_engine::{HumanConnection, QueryOptions, SchemaFilter, engine};
use serde_json::{Value, json};

const USAGE: &str = "usage: quarry-bench <app FIXTURE | engine FIXTURE [--runs N] | pg [--rows N] [--runs N]>";
/// How long one step may take before the run fails.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The frame step while waiting: 120 Hz on the virtual clock.
const DT: f64 = 1.0 / 120.0;
pub const SELECT: &str = "SELECT * FROM orders";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("app") => args.get(1).ok_or_else(|| USAGE.to_string()).and_then(|f| app(Path::new(f))),
        Some("engine") => args.get(1).ok_or_else(|| USAGE.to_string()).and_then(|f| engine_probe(Path::new(f), flag(&args, "--runs", 5))),
        Some("pg") => pg_probe(flag(&args, "--rows", 1_000_000), flag(&args, "--runs", 5)),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("quarry-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], name: &str, default: u64) -> u64 {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn spec(id: &str, target: Target) -> ConnectionSpec {
    ConnectionSpec {
        id: ConnectionId::new(id),
        name: id.into(),
        target,
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    }
}

/// A scratch folder for one run's settings, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // only the folder made by `scratch`
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Result<Scratch, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = std::env::temp_dir().join(format!("quarry-bench-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("config")).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(Scratch(dir))
}

fn outline(d: &mut Driver<Workspace>) -> Result<String, String> {
    let snap = d.call("ui.snapshot", json!({"compact": true}))?;
    Ok(snap["tree"].as_str().unwrap_or_default().to_string())
}

/// Run frames until `pred` holds for the outline; milliseconds since `since`.
fn wait(d: &mut Driver<Workspace>, since: Instant, what: &str, pred: impl Fn(&str) -> bool) -> Result<f64, String> {
    wait_up_to(d, since, TIMEOUT, what, pred)
}

fn wait_up_to(d: &mut Driver<Workspace>, since: Instant, timeout: Duration, what: &str, pred: impl Fn(&str) -> bool) -> Result<f64, String> {
    loop {
        d.advance(DT);
        let tree = outline(d)?;
        if pred(&tree) {
            return Ok(ms(since.elapsed()));
        }
        if since.elapsed() > timeout {
            return Err(format!("timed out after {} s waiting for {what}", timeout.as_secs()));
        }
    }
}

/// The status bar's `rows N` once a result has its first page (`rows —` before).
fn has_rows(tree: &str) -> bool {
    tree.match_indices("rows ").any(|(i, m)| tree.get(i + m.len()..).and_then(|t| t.chars().next()).is_some_and(|c| c.is_ascii_digit()))
}

fn ui(d: &mut Driver<Workspace>, script: &str) -> Result<(), String> {
    d.run_ui(script).map(|_| ()).map_err(|e| format!("{script}: {e}"))
}

fn mb(v: &Value) -> Option<f64> {
    v.as_u64().map(|b| b as f64 / 1_048_576.0)
}

fn app(fixture: &Path) -> Result<Value, String> {
    let fixture = fixture.canonicalize().map_err(|e| format!("{}: {e}", fixture.display()))?;
    let dir = scratch()?;
    let saved = json!({"schema_version": 1, "values": {"quarry.connections": [{"spec": spec("bench", Target::Sqlite { path: fixture }), "approve_similar": false}]}});
    std::fs::write(dir.0.join("config").join("settings.json"), saved.to_string()).map_err(|e| e.to_string())?;

    let rt = quarry::app().dirs(AppDirs::under(&dir.0)).runtime();
    let mut d: Driver<Workspace> = Driver::new(rt);
    d.set_screen(1280.0, 800.0);
    d.pixels_per_point(2.0);
    d.settle(60);
    let idle = d.call("app.stats", json!({}))?;

    // what one poll costs: the resolution of every timing below
    let t = Instant::now();
    d.advance(DT);
    outline(&mut d)?;
    let poll_ms = ms(t.elapsed());

    let t = Instant::now();
    ui(&mut d, "exec conn.open '{\"connection\": \"bench\"}'")?;
    let connect_schema_ms = wait(&mut d, t, "the schema", |t| t.contains("quarry.tree.table.bench") && t.contains("orders"))?;

    let t = Instant::now();
    ui(&mut d, "exec table.open '{\"connection\": \"bench\", \"table\": \"orders\"}'")?;
    // a table tab that never shows rows is reported, not fatal (the shell did not load them yet)
    let open_table = wait_up_to(&mut d, t, Duration::from_secs(5), "the table's first rows", has_rows);
    let open_tree = outline(&mut d)?;
    let after_open = d.call("app.stats", json!({}))?;

    ui(&mut d, "exec tab.new_editor '{\"connection\": \"bench\"}'")?;
    d.settle(30);
    ui(&mut d, &format!("fill id=quarry.editor \"{SELECT}\""))?;
    d.settle(30);
    // the person's own key press: injected script keys run nothing (docs/SECURITY.md)
    let key = |pressed| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::COMMAND };
    let t = Instant::now();
    d.push_event(key(true));
    d.push_event(key(false));
    let query_first_row_ms = wait(&mut d, t, "the query's first rows", has_rows)?;
    let query_tree = outline(&mut d)?;
    // let the grid's background streaming run for a second, as a person would look at it
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(1) {
        d.advance(DT);
        std::thread::sleep(Duration::from_millis(8));
    }
    let end = d.call("app.stats", json!({}))?;
    let status = |tree: &str| tree.lines().find(|l| has_rows(l)).unwrap_or_default().trim().to_string();
    Ok(json!({
        "probe": "app",
        "poll_ms": poll_ms,
        "connect_schema_ms": connect_schema_ms,
        "open_table_ms": open_table.as_ref().ok(),
        "open_table_error": open_table.err(),
        "open_table_status": status(&open_tree),
        "query_first_row_ms": query_first_row_ms,
        "query_status": status(&query_tree),
        "idle_footprint_mb": mb(&idle["process"]["footprint_bytes"]),
        "table_open_footprint_mb": mb(&after_open["process"]["footprint_bytes"]),
        "end_footprint_mb": mb(&end["process"]["footprint_bytes"]),
        "peak_footprint_mb": mb(&end["process"]["peak_bytes"]),
        "threads": end["process"]["threads"],
        "cpu_s": end["process"]["cpu_s"],
    }))
}

/// (first page, every row, rows)
fn stream(h: &mut dyn HumanConnection, sql: &str) -> Result<(f64, f64, u64), String> {
    let started = Instant::now();
    let c = h.query(sql, &QueryOptions::default()).map_err(|e| e.to_string())?;
    let first = ms(started.elapsed());
    let (mut n, mut at) = (0u64, 0u64);
    loop {
        let p = h.fetch(c.id, at, PAGE_ROWS).map_err(|e| e.to_string())?;
        if p.rows.is_empty() {
            break;
        }
        n += p.rows.len() as u64;
        at = p.end_row();
    }
    let all = ms(started.elapsed());
    h.close(c.id);
    Ok((first, all, n))
}

fn engine_runs(kind: EngineKind, spec: &ConnectionSpec, secret: Option<&quarry_core::Secret>, sql: &str, runs: u64) -> Result<Value, String> {
    let (mut connect, mut schema, mut first, mut all, mut rows, mut tables) = (vec![], vec![], vec![], vec![], 0, 0);
    for _ in 0..runs.max(1) {
        let t = Instant::now();
        let mut h = engine(kind).connect_human(spec, secret).map_err(|e| e.to_string())?;
        connect.push(ms(t.elapsed()));
        let t = Instant::now();
        let tree = h.schema(&SchemaFilter::default()).map_err(|e| e.to_string())?;
        schema.push(ms(t.elapsed()));
        tables = tree.table_count();
        let (f, a, n) = stream(h.as_mut(), sql)?;
        first.push(f);
        all.push(a);
        rows = n;
    }
    Ok(
        json!({"connect_ms": connect, "schema_ms": schema, "tables": tables, "first_page_ms": first, "all_rows_ms": all, "rows": rows, "page_rows": PAGE_ROWS}),
    )
}

fn engine_probe(fixture: &Path, runs: u64) -> Result<Value, String> {
    let s = spec("bench", Target::Sqlite { path: fixture.to_path_buf() });
    let mut v = engine_runs(EngineKind::Sqlite, &s, None, SELECT, runs)?;
    v["probe"] = json!("engine");
    v["sqlite"] = json!(quarry_engine::sqlite::library_version());
    Ok(v)
}

fn pg_probe(rows: u64, runs: u64) -> Result<Value, String> {
    let url = std::env::var("QUARRY_PG_URL").map_err(|_| "QUARRY_PG_URL is not set".to_string())?;
    let (target, secret) = quarry_engine::pg::parse_url(&url).map_err(|e| e.to_string())?;
    let s = spec("bench-pg", target);
    // the orders shape, generated on the server: nothing is written
    let sql = format!(
        "SELECT i::bigint AS id, i % 100000 AS customer_id, (ARRAY['paid','shipped','pending'])[i % 3 + 1] AS status, \
         (i * 7919) % 100000 AS total_cents, 'USD' AS currency, to_timestamp(1672531200 + i * 37) AS created_at, \
         CASE WHEN i % 10 = 0 THEN 'note ' || i END AS note FROM generate_series(1, {rows}) i"
    );
    let mut v = engine_runs(EngineKind::Postgres, &s, secret.as_ref(), &sql, runs)?;
    v["probe"] = json!("pg");
    Ok(v)
}
