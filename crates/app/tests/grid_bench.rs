#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The grid's scroll benchmark (PLAN §6: scrolling a 10M-row result costs ≤ 1 ms CPU a frame, no
//! hitch on a page fetch, memory bounded by the page cache).
//!
//! The whole Quarry window at 1280×800 points, 2 pixels per point; the grid scrolls by wheel
//! (2,000 points a frame, about 70 rows) for 600 frames at 120 Hz, then jumps far and back. CPU
//! per frame is the UI thread's, as ashlar's frame log reports it (`Runtime::log_frames`: the
//! egui pass, without tessellation or painting).
//!
//! - `fixture_1m`: `orders` of `cargo xtask fixtures --rows 1M` read from SQLite by fetch jobs
//!   (skipped when the file is missing; `QUARRY_FIXTURE=path` picks another).
//! - `generated_10m`: 10M generated rows (windowing past 500,000 rows).
//!
//! `cargo test -p quarry --release --test grid_bench -- --ignored --nocapture --test-threads=1`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Pages fetched (both benches), to show the scroll really crossed the result.
static FETCHES: AtomicU64 = AtomicU64::new(0);

use ashlar::app::Driver;
use ashlar::egui::{self, Event, MouseWheelUnit, TouchPhase, pos2, vec2};
use ashlar::fs::AppDirs;
use quarry::grid::source::{MemSource, Order, PageSource};
use quarry::grid::{self, GridState};
use quarry_core::{Column, Page, TabId, Total, Value, Workspace};
use serde_json::json;

const FRAMES: usize = 600;
const STEP: f32 = 2_000.0;
const DT: f64 = 1.0 / 120.0;

/// `orders` from the SQLite fixture, by primary key (row i = id i + 1).
struct Sqlite {
    conn: rusqlite::Connection,
}

impl PageSource for Sqlite {
    fn fetch(&mut self, first_row: u64, rows: u32) -> Result<Page, String> {
        FETCHES.fetch_add(1, Ordering::Relaxed);
        let mut st = self.conn.prepare_cached("SELECT * FROM orders WHERE id > ?1 ORDER BY id LIMIT ?2").map_err(|e| e.to_string())?;
        let out = st
            .query_map(rusqlite::params![first_row as i64, rows], |r| {
                (0..7)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => Value::Int(n),
                            rusqlite::types::ValueRef::Real(f) => Value::Float(f),
                            rusqlite::types::ValueRef::Text(t) => Value::text(String::from_utf8_lossy(t)),
                            rusqlite::types::ValueRef::Blob(b) => Value::Bytes { len: b.len() as u64, preview: Arc::from(&b[..b.len().min(64)]) },
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(Page { first_row, rows: out })
    }
    fn requery(&mut self, _: Option<Order>) -> Result<Total, String> {
        Ok(Total::Known(1_000_000))
    }
}

fn fixture() -> Option<PathBuf> {
    let p = std::env::var_os("QUARRY_FIXTURE").map(PathBuf::from).unwrap_or_else(|| {
        // the worktree's or the main checkout's fixtures folder
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/quarry-1M.sqlite")
    });
    p.exists().then_some(p)
}

struct Stats {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    frames: usize,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted.get(((sorted.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(f64::NAN)
}

/// Run the scroll workload over `g`; returns CPU per frame and the pages and bytes cached at the end.
fn run(name: &str, g: GridState) -> (Stats, (usize, usize)) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("frames.jsonl");
    let mut rt = quarry::app().dirs(AppDirs::under(dir.path())).runtime();
    rt.log_frames(&log).unwrap();
    let mut d: Driver<Workspace> = Driver::new(rt);
    d.set_screen(1280.0, 800.0);
    d.pixels_per_point(2.0);
    d.scene("editor").unwrap();
    let tab = TabId(1);
    grid::last_window::attach(tab, g);
    d.settle(120);
    // warm up, then mark where measuring starts
    let pointer = pos2(700.0, 560.0);
    let start_frame = d.frames();
    let wheel = |d: &mut Driver<Workspace>, dy: f32| {
        d.push_event(Event::PointerMoved(pointer));
        d.push_event(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: vec2(0.0, dy),
            phase: TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        d.advance(DT);
    };
    for i in 0..FRAMES {
        wheel(&mut d, if i < FRAMES * 3 / 4 { -STEP } else { STEP });
    }
    // jump far (random access) and back
    for target in [WINDOW_JUMP, 0] {
        grid::last_window::with_grid(tab, |g, _| {
            g.set_window(target);
            g.scroll_to = Some(target % quarry::grid::model::WINDOW_ROWS + 10);
        });
        for _ in 0..30 {
            d.advance(DT);
        }
    }
    let footprint = grid::last_window::with_grid(tab, |g, _| g.cache.footprint()).unwrap();
    let stats = d.call("app.stats", json!({})).unwrap();
    let rss = stats["process"]["footprint_bytes"].as_u64();
    drop(d);
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut cpu: Vec<f64> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["frame"].as_u64().is_some_and(|f| f > start_frame))
        .filter_map(|v| v["cpu_ms"].as_f64())
        .collect();
    cpu.sort_by(f64::total_cmp);
    let s =
        Stats { p50: pct(&cpu, 0.5), p95: pct(&cpu, 0.95), p99: pct(&cpu, 0.99), max: cpu.last().copied().unwrap_or(f64::NAN), frames: cpu.len() };
    println!(
        "bench: grid_scroll_{name} pages_fetched={} frames={} cpu_ms p50={:.3} p95={:.3} p99={:.3} max={:.3} cache_pages={} cache_kb={} rss_mb={}",
        FETCHES.swap(0, Ordering::Relaxed),
        s.frames,
        s.p50,
        s.p95,
        s.p99,
        s.max,
        footprint.0,
        footprint.1 / 1024,
        rss.map_or("n/a".into(), |b| format!("{:.0}", b as f64 / 1_048_576.0))
    );
    (s, footprint)
}

const WINDOW_JUMP: u64 = 900_000;

fn columns() -> Arc<[Column]> {
    quarry::grid::scenes::orders_columns()
}

#[test]
#[ignore = "benchmark: run in release with --ignored"]
fn fixture_1m() {
    let Some(path) = fixture() else {
        eprintln!("skipped: no fixtures/quarry-1M.sqlite (cargo xtask fixtures --rows 1M)");
        return;
    };
    let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let g = GridState::open(columns(), Total::Known(1_000_000), Box::new(Sqlite { conn }));
    let (s, (pages, bytes)) = run("fixture_1m", g);
    assert!(pages <= 6 && bytes < 4 * 1024 * 1024, "{pages} pages, {bytes} bytes");
    assert!(s.frames >= FRAMES, "{} frames", s.frames);
}

#[test]
#[ignore = "benchmark: run in release with --ignored"]
fn generated_10m() {
    let src = MemSource::generated(
        10_000_000,
        Arc::new(|i| {
            if i % 1000 == 0 {
                FETCHES.fetch_add(1, Ordering::Relaxed);
            }
            quarry::grid::scenes::order(i)
        }),
    )
    .deferred();
    let g = GridState::open(columns(), Total::Known(10_000_000), Box::new(src));
    let (s, (pages, _)) = run("generated_10m", g);
    assert!(pages <= 6, "{pages} pages");
    assert!(s.frames >= FRAMES, "{} frames", s.frames);
}
