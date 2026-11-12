//! Streaming 1M rows through an engine cursor: first-page latency and rows/s.
//!
//! `cargo bench -p quarry-engine --bench stream` builds a 1M-row SQLite table in a temp file
//! (an `orders`-like shape: ints, text, a timestamp, a nullable note), then times `query` (which
//! reads the first page) and fetching every page of 1,000. With `QUARRY_PG_URL` set it also loads
//! the same rows into a table on that (throwaway) server and streams them. Rows: `ROWS=…`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use quarry_core::limits::PAGE_ROWS;
use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, EngineKind, Target};
use quarry_engine::{HumanConnection, QueryOptions, engine};

const RUNS: usize = 5;

fn spec(target: Target) -> ConnectionSpec {
    ConnectionSpec {
        id: ConnectionId::new("bench"),
        name: "bench".into(),
        target,
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    }
}

/// (first page, everything)
fn stream(h: &mut dyn HumanConnection, sql: &str, rows: u64) -> (Duration, Duration) {
    let started = Instant::now();
    let c = h.query(sql, &QueryOptions::default()).unwrap();
    let first = started.elapsed();
    let mut n = 0u64;
    let mut at = 0u64;
    loop {
        let p = h.fetch(c.id, at, PAGE_ROWS).unwrap();
        if p.rows.is_empty() {
            break;
        }
        n += p.rows.len() as u64;
        at = p.end_row();
    }
    let all = started.elapsed();
    h.close(c.id);
    assert_eq!(n, rows);
    (first, all)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn report(label: &str, rows: u64, runs: &[(Duration, Duration)]) {
    let first = median(runs.iter().map(|r| r.0).collect());
    let all = median(runs.iter().map(|r| r.1).collect());
    println!(
        "{label}: {rows} rows, first page {:.2} ms, all {:.0} ms, {:.0} rows/s (medians of {RUNS})",
        first.as_secs_f64() * 1e3,
        all.as_secs_f64() * 1e3,
        rows as f64 / all.as_secs_f64()
    );
}

const SELECT: &str = "SELECT id, customer_id, status, total_cents, created_at, note FROM orders";

fn main() {
    let rows: u64 = std::env::var("ROWS").ok().and_then(|r| r.parse().ok()).unwrap_or(1_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.sqlite");
    let started = Instant::now();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, status TEXT NOT NULL,
                total_cents INTEGER NOT NULL, created_at TEXT NOT NULL, note TEXT);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows})
             INSERT INTO orders SELECT i, i % 100000, CASE i % 3 WHEN 0 THEN 'paid' WHEN 1 THEN 'shipped' ELSE 'pending' END,
                 (i * 7919) % 100000, datetime(1672531200 + i * 37, 'unixepoch'), CASE WHEN i % 10 = 0 THEN 'note ' || i END FROM n;"
        ))
        .unwrap();
    println!("sqlite {} (bundled), fixture built in {:.1} s", quarry_engine::sqlite::library_version(), started.elapsed().as_secs_f64());
    let mut h = engine(EngineKind::Sqlite).connect_human(&spec(Target::Sqlite { path }), None).unwrap();
    let runs: Vec<_> = (0..RUNS).map(|_| stream(h.as_mut(), SELECT, rows)).collect();
    report("sqlite", rows, &runs);

    if let Ok(url) = std::env::var("QUARRY_PG_URL") {
        let (target, secret) = quarry_engine::pg::parse_url(&url).unwrap();
        let mut h = engine(EngineKind::Postgres).connect_human(&spec(target), secret.as_ref()).unwrap();
        h.execute_in_tx(
            &[
                "DROP TABLE IF EXISTS quarry_bench_orders".into(),
                format!(
                    "CREATE TABLE quarry_bench_orders AS SELECT i::bigint AS id, i % 100000 AS customer_id,
                        (ARRAY['paid','shipped','pending'])[i % 3 + 1] AS status, (i * 7919) % 100000 AS total_cents,
                        to_timestamp(1672531200 + i * 37) AS created_at, CASE WHEN i % 10 = 0 THEN 'note ' || i END AS note
                     FROM generate_series(1, {rows}) i"
                ),
            ],
            None,
        )
        .unwrap();
        let sql = SELECT.replace("orders", "quarry_bench_orders");
        let runs: Vec<_> = (0..RUNS).map(|_| stream(h.as_mut(), &sql, rows)).collect();
        report("postgres", rows, &runs);
        h.execute_in_tx(&["DROP TABLE quarry_bench_orders".into()], None).unwrap();
    } else {
        println!("postgres: skipped (QUARRY_PG_URL is not set)");
    }
}
