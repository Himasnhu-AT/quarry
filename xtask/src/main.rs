//! `cargo xtask <task>`:
//!
//! - `ci`: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
//!   `cargo test --workspace --no-fail-fast`. Every step runs; the summary lists failures.
//! - `fixtures [--rows N] [--out PATH] [--seed S]`: a deterministic SQLite database with
//!   `customers`, `orders` and `events` for the grid and bench streams. `N` (default 100k) is the
//!   number of orders and of events (`10M`, `250k` and plain numbers work); customers are N/10.
//!   Default output: `fixtures/quarry-<N>.sqlite`.
//! - `bench [--quick] [--runs N] [--no-launch] [--out PATH]`: Quarry's published numbers
//!   (`bench.rs`, docs/benchmarks.md). Opens real windows; never run by CI.

mod bench;
mod fixtures;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask <ci | fixtures [--rows N] [--out PATH] [--seed S] | bench [--quick] [--runs N] [--no-launch] [--out PATH]>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("ci") => ci(),
        Some("fixtures") => fixtures_cmd(args.get(1..).unwrap_or_default()),
        Some("bench") => bench::bench(&root(), args.get(1..).unwrap_or_default()),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    // xtask/ is one level below the workspace root
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root());
    c
}

fn step(name: &str, mut cmd: Command) -> Result<(), String> {
    let t = std::time::Instant::now();
    eprintln!("ci: {name}…");
    let status = cmd.status().map_err(|e| format!("{name}: cannot run cargo: {e}"))?;
    if status.success() {
        eprintln!("ci: {name} ok in {} s", t.elapsed().as_secs());
        Ok(())
    } else {
        Err(format!("{name} failed ({status})"))
    }
}

fn ci() -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut fmt = cargo();
    fmt.args(["fmt", "--all", "--", "--check"]);
    let mut clippy = cargo();
    clippy.args(["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]);
    let mut test = cargo();
    test.args(["test", "--workspace", "--no-fail-fast", "--locked"]);
    let failures: Vec<String> = [("fmt", fmt), ("clippy", clippy), ("tests", test)].into_iter().filter_map(|(n, c)| step(n, c).err()).collect();
    eprintln!("ci: {} in {} s", if failures.is_empty() { "ok" } else { "FAILED" }, started.elapsed().as_secs());
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

fn fixtures_cmd(args: &[String]) -> Result<(), String> {
    let mut rows: u64 = 100_000;
    let mut out: Option<PathBuf> = None;
    let mut seed: u64 = fixtures::DEFAULT_SEED;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--rows" => rows = fixtures::parse_count(value()?)?,
            "--out" => out = Some(PathBuf::from(value()?)),
            "--seed" => seed = value()?.parse().map_err(|_| "--seed needs a number".to_string())?,
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let out = out.unwrap_or_else(|| root().join("fixtures").join(format!("quarry-{}.sqlite", fixtures::count_label(rows))));
    let t = std::time::Instant::now();
    let summary = fixtures::generate(&out, rows, seed, &mut |done, total| eprintln!("fixtures: {done} / {total} rows"))?;
    eprintln!("fixtures: wrote {} ({summary}) in {:.1} s", out.display(), t.elapsed().as_secs_f64());
    Ok(())
}
