//! `cargo xtask bench [--quick] [--runs N] [--no-launch] [--out PATH]`: Quarry's published numbers
//! (docs/benchmarks.md has the method and the latest table). Manual only: it opens real windows
//! for the launch timing, so CI never runs it. Run it through memgate:
//! `memgate -- cargo xtask bench --quick`.
//!
//! 1. Fixtures: `cargo xtask fixtures` files for 100k and 1M rows (made when missing), and 10M
//!    (made by a full run; `--quick` uses it only when it already exists, it takes ~5 minutes).
//! 2. Release builds of `quarry` and `quarry-bench`; binary size, stripped size.
//! 3. Launch (real window): spawn → `QUARRY_READY_FILE` (the first presented frame), median of N,
//!    each run in a fresh `ASHLAR_DIRS_ROOT`; then idle RSS, footprint, peak and CPU after a settle.
//! 4. `quarry-bench app` per fixture, one process per run: connect + schema, open `orders`, query
//!    to first row on screen for `SELECT * FROM orders`, memory.
//! 5. `quarry-bench engine` per fixture: first page and every row of `SELECT * FROM orders`.
//! 6. The grid's scroll bench (`crates/app/tests/grid_bench.rs`): CPU per frame p50/p95/p99.
//! 7. Baselines and competitors that are installed: the `sqlite3` CLI on the same query; GUI
//!    database tools launched on the same file (window visible, RSS after a settle). Missing ones
//!    are listed as not installed.
//! 8. PostgreSQL (`quarry-bench pg`) only when `QUARRY_PG_URL` is set.
//!
//! Writes `bench/results/<date>-<quick|full>.json` and prints a summary table.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const USAGE: &str = "usage: cargo xtask bench [--quick] [--runs N] [--no-launch] [--out PATH]";

struct Options {
    quick: bool,
    runs: Option<usize>,
    launch: bool,
    out: Option<PathBuf>,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut o = Options { quick: false, runs: None, launch: true, out: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--quick" => o.quick = true,
            "--no-launch" => o.launch = false,
            "--runs" => o.runs = Some(it.next().and_then(|v| v.parse().ok()).filter(|n| *n > 0).ok_or("--runs needs a number")?),
            "--out" => o.out = Some(PathBuf::from(it.next().ok_or("--out needs a path")?)),
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(o)
}

/// Kills the child when dropped, whatever happens in between.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A JavaScript-for-Automation watcher (macOS, no permission needed): it reads a pid on stdin and
/// prints `window` once that process has an on-screen window (CoreGraphics' window list, polled
/// every 2 ms). It starts before the app does, so its own start-up is not timed.
const WATCH_JS: &str = r#"ObjC.import('Foundation');
ObjC.import('CoreGraphics');
function visible(pid) {
  const l = ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0));
  for (let i = 0; i < l.count; i++) {
    const w = l.objectAtIndex(i);
    if (Number(w.objectForKey('kCGWindowOwnerPID').js) === pid && Number(w.objectForKey('kCGWindowLayer').js) === 0) return true;
  }
  return false;
}
function run() {
  visible(-1);
  const out = $.NSFileHandle.fileHandleWithStandardOutput;
  out.writeData($('ready\n').dataUsingEncoding($.NSUTF8StringEncoding));
  const data = $.NSFileHandle.fileHandleWithStandardInput.availableData;
  const pid = parseInt(ObjC.unwrap($.NSString.alloc.initWithDataEncoding(data, $.NSUTF8StringEncoding)));
  const t0 = $.NSDate.date;
  while (-t0.timeIntervalSinceNow < 60) {
    if (visible(pid)) return 'window';
    $.NSThread.sleepForTimeInterval(0.002);
  }
  return 'timeout';
}
"#;

/// Watches for the first window of a process launched after [`Watch::start`].
struct Watch {
    child: Running,
    seen: std::sync::mpsc::Receiver<Instant>,
}

impl Watch {
    fn start(scratch: &Path) -> Option<Watch> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let script = scratch.join("watch.js");
        std::fs::write(&script, WATCH_JS).ok()?;
        let child = Command::new("osascript")
            .arg("-l")
            .arg("JavaScript")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut child = Running(child);
        let mut lines = BufReader::new(child.0.stdout.take()?).lines();
        if lines.next()?.ok()? != "ready" {
            return None;
        }
        let (tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if lines.next().and_then(Result::ok).is_some_and(|l| l == "window") {
                let _ = tx.send(Instant::now());
            }
        });
        Some(Watch { child, seen })
    }

    fn watch(&mut self, pid: u32) {
        if let Some(stdin) = self.child.0.stdin.as_mut() {
            let _ = writeln!(stdin, "{pid}");
            let _ = stdin.flush();
        }
    }

    /// Milliseconds from `started` to the first window, waiting at most `timeout`.
    fn window_ms(&self, started: Instant, timeout: Duration) -> Option<f64> {
        self.seen.recv_timeout(timeout).ok().map(|at| at.duration_since(started).as_secs_f64() * 1000.0)
    }
}

/// The value at quantile `q` (nearest rank) of sorted samples.
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied().unwrap_or(f64::NAN)
}

/// `{median, min, max, n, samples}` of some samples (rounded to 0.01).
pub fn summary(samples: &[f64]) -> Value {
    let mut s: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    s.sort_by(f64::total_cmp);
    let r = |v: f64| (v * 100.0).round() / 100.0;
    json!({
        "median": r(quantile(&s, 0.5)),
        "min": r(s.first().copied().unwrap_or(f64::NAN)),
        "max": r(s.last().copied().unwrap_or(f64::NAN)),
        "n": s.len(),
        "samples": s.iter().map(|v| r(*v)).collect::<Vec<_>>(),
    })
}

fn run_text(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `ps -o time=` (`M:SS.ss` or `H:MM:SS`) in seconds.
pub fn parse_cpu_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    (!s.is_empty()).then_some(total)
}

/// `vmmap --summary` lines like `Physical footprint:         36.7M`, in MB.
pub fn vmmap_mb(summary: &str, label: &str) -> Option<f64> {
    let line = summary.lines().find(|l| l.trim_start().starts_with(label))?;
    let value = line.trim_start().strip_prefix(label)?.trim();
    let (num, unit) = value.split_at(value.find(|c: char| !(c.is_ascii_digit() || c == '.'))?);
    let n: f64 = num.parse().ok()?;
    Some(match unit.trim().chars().next()? {
        'K' => n / 1024.0,
        'M' => n,
        'G' => n * 1024.0,
        _ => n / (1024.0 * 1024.0),
    })
}

/// RSS (MB), CPU seconds and, on macOS, footprint and peak footprint (MB) of a process.
fn process_memory(pid: u32) -> Value {
    let pid = pid.to_string();
    let rss = run_text("ps", &["-o", "rss=", "-p", &pid]).and_then(|v| v.parse::<f64>().ok()).map(|kb| kb / 1024.0);
    let cpu = run_text("ps", &["-o", "time=", "-p", &pid]).and_then(|v| parse_cpu_time(&v));
    let vm = run_text("vmmap", &["--summary", &pid]).unwrap_or_default();
    json!({
        "rss_mb": rss,
        "cpu_s": cpu,
        "footprint_mb": vmmap_mb(&vm, "Physical footprint:"),
        "peak_footprint_mb": vmmap_mb(&vm, "Physical footprint (peak):"),
    })
}

fn machine(root: &Path) -> Value {
    let rustc = run_text("rustc", &["-V"]);
    let commit = run_text("git", &["-C", &root.to_string_lossy(), "rev-parse", "--short", "HEAD"]);
    let dirty = run_text("git", &["-C", &root.to_string_lossy(), "status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    json!({
        "cpu": run_text("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "cores": run_text("sysctl", &["-n", "hw.ncpu"]),
        "memory_gb": run_text("sysctl", &["-n", "hw.memsize"]).and_then(|m| m.parse::<f64>().ok()).map(|b| b / 1_073_741_824.0),
        "os": run_text("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| run_text("uname", &["-sr"])),
        "rustc": rustc,
        "commit": commit,
        "dirty": dirty,
        "load_average": run_text("sysctl", &["-n", "vm.loadavg"]),
        "memgate": run_text("memgate", &["status"]),
    })
}

fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).map_or_else(|| root.join("target"), |p| if p.is_absolute() { p } else { root.join(p) })
}

fn cargo(root: &Path) -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root);
    c
}

fn fixture(root: &Path, rows: u64, label: &str, make: bool) -> Result<Option<PathBuf>, String> {
    let path = root.join("fixtures").join(format!("quarry-{label}.sqlite"));
    if path.exists() {
        return Ok(Some(path));
    }
    if !make {
        eprintln!("bench: no {} (a full run makes it); skipped", path.display());
        return Ok(None);
    }
    eprintln!("bench: making {} …", path.display());
    crate::fixtures::generate(&path, rows, crate::fixtures::DEFAULT_SEED, &mut |_, _| {})?;
    Ok(Some(path))
}

fn build(root: &Path) -> Result<(), String> {
    eprintln!("bench: building quarry and quarry-bench (release) …");
    let status = cargo(root)
        .args(["build", "--release", "--locked", "-p", "quarry", "-p", "quarry-bench"])
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("cargo build failed ({status})")) }
}

fn sizes(exe: &Path, scratch: &Path) -> Value {
    let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len() as f64 / 1_048_576.0);
    let stripped = scratch.join("quarry-stripped");
    let _ = std::fs::remove_file(&stripped);
    let ok = Command::new("strip").arg("-o").arg(&stripped).arg(exe).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    json!({
        "release_mb": size(exe),
        "release_stripped_mb": if ok { size(&stripped) } else { None },
        "note": "`[profile.release]` with line-table debug info; no release-small profile is defined; no .app or .dmg is built yet",
    })
}

/// Launch Quarry once in a fresh data folder: (the app's own ready_ms, spawn → ready file, idle).
fn launch_once(exe: &Path, scratch: &Path, i: usize, settle: Duration) -> Result<(f64, f64, Value), String> {
    let home = scratch.join(format!("launch-{i}"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let ready = home.join("ready.json");
    let mut watch = Watch::start(scratch);
    let started = Instant::now();
    let child = Command::new(exe)
        .env("QUARRY_READY_FILE", &ready)
        .env("ASHLAR_DIRS_ROOT", &home)
        .env("DO_NOT_TRACK", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("launch: {}: {e}", exe.display()))?;
    let mut child = Running(child);
    if let Some(w) = watch.as_mut() {
        w.watch(child.0.id());
    }
    let deadline = started + Duration::from_secs(30);
    let (ready_ms, spawn_ms) = loop {
        if let Ok(text) = std::fs::read_to_string(&ready)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            let spawn_ms = started.elapsed().as_secs_f64() * 1000.0;
            break (v["ready_ms"].as_f64().ok_or("launch: the ready file has no ready_ms")?, spawn_ms);
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            return Err(format!("launch: quarry exited before its first frame ({status})"));
        }
        if Instant::now() > deadline {
            return Err("launch: no ready file after 30 s".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let pid = child.0.id();
    let window_ms = watch.as_ref().and_then(|w| w.window_ms(started, Duration::from_secs(10)));
    let at_ready = process_memory(pid);
    std::thread::sleep(settle);
    let mut idle = process_memory(pid);
    if let (Some(a), Some(b)) = (at_ready["cpu_s"].as_f64(), idle["cpu_s"].as_f64()) {
        idle["idle_cpu_s"] = json!(((b - a) * 100.0).round() / 100.0);
    }
    idle["window_ms"] = json!(window_ms);
    drop(child);
    Ok((ready_ms, spawn_ms, idle))
}

fn launch(exe: &Path, scratch: &Path, runs: usize, settle: Duration) -> Result<Value, String> {
    let (mut ready, mut spawn, mut idle) = (vec![], vec![], vec![]);
    for i in 0..runs {
        eprintln!("bench: launch {}/{runs} (idle {} s)", i + 1, settle.as_secs());
        let (r, s, m) = launch_once(exe, scratch, i, settle)?;
        ready.push(r);
        spawn.push(s);
        idle.push(m);
        std::thread::sleep(Duration::from_millis(500));
    }
    let col = |k: &str| idle.iter().filter_map(|m| m[k].as_f64()).collect::<Vec<_>>();
    Ok(json!({
        "method": "real window; spawn → QUARRY_READY_FILE written after the first presented frame (ready_ms is the app's own figure from the start of run(), spawn_to_ready_ms adds dyld and main), and spawn → first on-screen window in CoreGraphics' window list (the method used for other tools); fresh ASHLAR_DIRS_ROOT per run (no saved connections, nothing restored); memory read with ps and vmmap after the settle",
        "settle_s": settle.as_secs(),
        "ready_ms": summary(&ready),
        "spawn_to_ready_ms": summary(&spawn),
        "spawn_to_window_ms": summary(&col("window_ms")),
        "idle_rss_mb": summary(&col("rss_mb")),
        "idle_footprint_mb": summary(&col("footprint_mb")),
        "peak_footprint_mb": summary(&col("peak_footprint_mb")),
        "idle_cpu_s": summary(&col("idle_cpu_s")),
    }))
}

fn probe(exe: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new(exe).args(args).env("DO_NOT_TRACK", "1").output().map_err(|e| format!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!("quarry-bench {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| l.starts_with('{')).ok_or("quarry-bench printed no JSON")?;
    serde_json::from_str(line).map_err(|e| format!("quarry-bench output: {e}"))
}

fn app_probe(exe: &Path, fixture: &Path, runs: usize) -> Result<Value, String> {
    let f = fixture.to_string_lossy();
    let all: Vec<Value> = (0..runs)
        .map(|i| {
            eprintln!("bench: app {} run {}/{runs}", fixture.display(), i + 1);
            probe(exe, &["app", &f])
        })
        .collect::<Result<_, _>>()?;
    let col = |k: &str| all.iter().filter_map(|v| v[k].as_f64()).collect::<Vec<_>>();
    let first = all.first().cloned().unwrap_or(Value::Null);
    Ok(json!({
        "method": "headless ashlar Driver, 1280×800 pt at 2×, no GPU renderer; wall clock from the command / the person's Mod+Enter to the first frame whose accessibility outline shows `rows N`; one process per run",
        "poll_ms": summary(&col("poll_ms")),
        "connect_schema_ms": summary(&col("connect_schema_ms")),
        "open_table_ms": summary(&col("open_table_ms")),
        "open_table_error": first["open_table_error"],
        "query_first_row_ms": summary(&col("query_first_row_ms")),
        "query_status": first["query_status"],
        "idle_footprint_mb": summary(&col("idle_footprint_mb")),
        "end_footprint_mb": summary(&col("end_footprint_mb")),
        "peak_footprint_mb": summary(&col("peak_footprint_mb")),
        "runs": all,
    }))
}

fn engine_probe(exe: &Path, fixture: &Path, runs: usize) -> Result<Value, String> {
    eprintln!("bench: engine {} ({runs} runs)", fixture.display());
    let v = probe(exe, &["engine", &fixture.to_string_lossy(), "--runs", &runs.to_string()])?;
    engine_summary(v)
}

fn engine_summary(v: Value) -> Result<Value, String> {
    let col = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
    let rows = v["rows"].as_f64().unwrap_or(0.0);
    let rate: Vec<f64> = col("all_rows_ms").iter().map(|ms| rows / (ms / 1000.0)).collect();
    Ok(json!({
        "method": "in-process engine: connect_human, schema, query (first page of 1,000 rows), then fetch every page",
        "rows": v["rows"],
        "tables": v["tables"],
        "sqlite": v["sqlite"],
        "connect_ms": summary(&col("connect_ms")),
        "schema_ms": summary(&col("schema_ms")),
        "first_page_ms": summary(&col("first_page_ms")),
        "all_rows_ms": summary(&col("all_rows_ms")),
        "rows_per_s": summary(&rate),
    }))
}

/// `bench: grid_scroll_<name> k=v …` lines of the grid's scroll bench.
pub fn parse_grid_lines(text: &str) -> Value {
    let mut out = serde_json::Map::new();
    for line in text.lines() {
        let Some(rest) = line.split("bench: grid_scroll_").nth(1) else { continue };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next() else { continue };
        let mut m = serde_json::Map::new();
        for w in words {
            if let Some((k, v)) = w.split_once('=') {
                m.insert(k.to_string(), v.parse::<f64>().map_or_else(|_| json!(v), |n| json!(n)));
            }
        }
        out.insert(name.to_string(), Value::Object(m));
    }
    Value::Object(out)
}

fn grid(root: &Path, fixture_1m: Option<&Path>) -> Result<Value, String> {
    eprintln!("bench: grid scroll bench (crates/app/tests/grid_bench.rs) …");
    let mut c = cargo(root);
    c.args(["test", "--release", "--locked", "-p", "quarry", "--test", "grid_bench", "--", "--ignored", "--nocapture", "--test-threads=1"]);
    if let Some(f) = fixture_1m {
        c.env("QUARRY_FIXTURE", f);
    }
    let out = c.stderr(Stdio::inherit()).output().map_err(|e| format!("cargo test: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!("grid bench failed ({}):\n{text}", out.status));
    }
    let mut v = json!({
        "method": "whole window 1280×800 pt at 2×, wheel 2,000 pt a frame for 600 frames at 120 Hz (reversing at 3/4), then jumps far and back; UI-thread CPU of the egui pass per frame from ashlar's frame log (no tessellation or painting); budget ≤ 1 ms",
    });
    v["results"] = parse_grid_lines(&text);
    Ok(v)
}

/// The `sqlite3` CLI on the same query: spawn → first byte of output, and every row (to a pipe).
fn sqlite3(fixture: &Path, runs: usize) -> Value {
    let exe = ["/usr/bin/sqlite3"].iter().map(PathBuf::from).find(|p| p.exists()).or_else(|| which("sqlite3"));
    let Some(exe) = exe else {
        return json!({"installed": false, "command": "sqlite3 -readonly FIXTURE 'SELECT * FROM orders'"});
    };
    let version = run_text(&exe.to_string_lossy(), &["--version"]);
    let (mut first, mut all, mut bytes) = (vec![], vec![], 0u64);
    for _ in 0..runs {
        let started = Instant::now();
        let Ok(child) =
            Command::new(&exe).arg("-readonly").arg(fixture).arg("SELECT * FROM orders").stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        else {
            break;
        };
        let mut child = Running(child);
        let Some(mut out) = child.0.stdout.take() else { break };
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0u64;
        let mut first_at = None;
        while let Ok(k) = out.read(&mut buf) {
            if k == 0 {
                break;
            }
            first_at.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
            n += k as u64;
        }
        let _ = child.0.wait();
        all.push(started.elapsed().as_secs_f64() * 1000.0);
        first.extend(first_at);
        bytes = n;
    }
    json!({
        "installed": true,
        "path": exe,
        "version": version,
        "method": "spawn `sqlite3 -readonly FIXTURE 'SELECT * FROM orders'` with stdout to a pipe; first byte and end of output (process start included)",
        "first_byte_ms": summary(&first),
        "all_rows_ms": summary(&all),
        "output_mb": bytes as f64 / 1_048_576.0,
    })
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|p| p.is_file()))
}

/// GUI tools compared in docs/PLAN.md §7, and where they install.
const COMPETITORS: &[(&str, &[&str])] = &[
    ("DBeaver", &["DBeaver.app", "DBeaverEE.app", "DBeaver Community.app"]),
    ("Beekeeper Studio", &["Beekeeper Studio.app"]),
    ("DB Browser for SQLite", &["DB Browser for SQLite.app"]),
    ("TablePlus", &["TablePlus.app"]),
    ("pgAdmin 4", &["pgAdmin 4.app"]),
];

fn find_app(names: &[&str]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dirs: Vec<PathBuf> = [Some(PathBuf::from("/Applications")), home.map(|h| h.join("Applications"))].into_iter().flatten().collect();
    dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.exists())
}

/// Launch an installed GUI tool on the fixture: spawn → first on-screen window ([`Watch`]), then
/// RSS after the settle. A tool that shows a splash or a project picker counts that window: the
/// result says so.
fn measure_app(app: &Path, fixture: &Path, scratch: &Path, runs: usize, settle: Duration) -> Value {
    let name = app.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let exe_dir = app.join("Contents").join("MacOS");
    let exe = std::fs::read_dir(&exe_dir).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()).filter(|p| p.is_file())));
    let Some(exe) = exe else { return json!({"installed": true, "error": format!("no executable in {}", exe_dir.display())}) };
    let version = run_text("defaults", &["read", &app.join("Contents").join("Info").to_string_lossy(), "CFBundleShortVersionString"]);
    let (mut window, mut rss, mut footprint) = (vec![], vec![], vec![]);
    for i in 0..runs {
        eprintln!("bench: {name} launch {}/{runs}", i + 1);
        let mut watch = Watch::start(scratch);
        let started = Instant::now();
        let Ok(child) = Command::new(&exe).arg(fixture).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { break };
        let child = Running(child);
        let pid = child.0.id();
        if let Some(w) = watch.as_mut() {
            w.watch(pid);
            window.extend(w.window_ms(started, Duration::from_secs(60)));
        }
        std::thread::sleep(settle);
        let m = process_memory(pid);
        rss.extend(m["rss_mb"].as_f64());
        footprint.extend(m["footprint_mb"].as_f64());
        drop(child);
        std::thread::sleep(Duration::from_secs(1));
    }
    json!({
        "installed": true,
        "path": app,
        "version": version,
        "method": "spawn the bundle's executable with the fixture path; first on-screen window in CoreGraphics' window list (polled every 2 ms; a splash screen or project picker counts); main process RSS / footprint after the settle (helper processes not counted)",
        "window_ms": summary(&window),
        "rss_after_open_mb": summary(&rss),
        "footprint_after_open_mb": summary(&footprint),
    })
}

fn competitors(fixture: Option<&Path>, scratch: &Path, runs: usize, settle: Duration) -> Value {
    let mut out = serde_json::Map::new();
    for (name, bundles) in COMPETITORS {
        let v = match (find_app(bundles), fixture) {
            (Some(app), Some(f)) => measure_app(&app, f, scratch, runs, settle),
            (Some(app), None) => json!({"installed": true, "path": app, "error": "no fixture"}),
            (None, _) => json!({"installed": false, "looked_in": ["/Applications", "~/Applications"], "bundles": bundles}),
        };
        out.insert((*name).to_string(), v);
    }
    out.insert(
        "psql".into(),
        match which("psql") {
            Some(p) => json!({"installed": true, "path": p, "measured": false, "why": "needs QUARRY_PG_URL; not wired yet"}),
            None => json!({"installed": false}),
        },
    );
    Value::Object(out)
}

fn ms(v: &Value) -> String {
    v["median"].as_f64().filter(|f| f.is_finite()).map_or_else(|| "—".into(), |m| if m >= 100.0 { format!("{m:.0}") } else { format!("{m:.1}") })
}

/// The summary table printed at the end (and pasted into docs/benchmarks.md by hand).
fn table(r: &Value) -> String {
    let mut s = String::from("| Measure | Result | Budget |\n|---|---|---|\n");
    let mut row = |m: String, v: String, b: &str| s.push_str(&format!("| {m} | {v} | {b} |\n"));
    let l = &r["launch"];
    if l.is_object() {
        row(
            "Launch → first presented frame (warm, median)".into(),
            format!(
                "{} ms (spawn → ready {} ms, spawn → window {} ms)",
                ms(&l["ready_ms"]),
                ms(&l["spawn_to_ready_ms"]),
                ms(&l["spawn_to_window_ms"])
            ),
            "≤ 200 ms",
        );
        row(
            "Idle RSS / footprint / peak footprint".into(),
            format!("{} / {} / {} MB", ms(&l["idle_rss_mb"]), ms(&l["idle_footprint_mb"]), ms(&l["peak_footprint_mb"])),
            "",
        );
        row(format!("Idle CPU over {} s", l["settle_s"]), format!("{} s", ms(&l["idle_cpu_s"])), "0 %");
    }
    if let Some(fx) = r["fixtures"].as_object() {
        for (label, f) in ["100k", "1M", "10M"].iter().filter_map(|l| fx.get(*l).map(|f| (l, f))) {
            let a = &f["app"];
            let e = &f["engine"];
            row(format!("{label}: connect + schema in the window"), format!("{} ms", ms(&a["connect_schema_ms"])), "");
            row(
                format!("{label}: `SELECT * FROM orders` → first row on screen"),
                format!("{} ms", ms(&a["query_first_row_ms"])),
                "≤ 50 ms over server",
            );
            row(
                format!("{label}: open `orders` table → first rows"),
                if a["open_table_ms"]["n"].as_u64().unwrap_or(0) > 0 {
                    format!("{} ms", ms(&a["open_table_ms"]))
                } else {
                    "not measurable: the table tab shows no rows yet".into()
                },
                "",
            );
            row(
                format!("{label}: footprint end / peak (headless)"),
                format!("{} / {} MB", ms(&a["end_footprint_mb"]), ms(&a["peak_footprint_mb"])),
                "≤ 150 MB",
            );
            row(
                format!("{label}: engine first page / all rows"),
                format!("{} ms / {} ms ({} rows/s)", ms(&e["first_page_ms"]), ms(&e["all_rows_ms"]), ms(&e["rows_per_s"])),
                "",
            );
            let q = &f["sqlite3"];
            if q["installed"] == true {
                row(
                    format!("{label}: sqlite3 CLI first byte / all rows"),
                    format!("{} ms / {} ms", ms(&q["first_byte_ms"]), ms(&q["all_rows_ms"])),
                    "baseline",
                );
            }
        }
    }
    if let Some(g) = r["grid"]["results"].as_object() {
        for (name, v) in g {
            let f = |k: &str| v[k].as_f64().map_or("—".into(), |x| format!("{x:.2}"));
            row(format!("Scroll {name}: CPU per frame p50 / p95 / p99"), format!("{} / {} / {} ms", f("p50"), f("p95"), f("p99")), "≤ 1 ms");
        }
    }
    let b = &r["binary"];
    row(
        "Binary (release / stripped)".into(),
        format!("{:.1} / {:.1} MB", b["release_mb"].as_f64().unwrap_or(f64::NAN), b["release_stripped_mb"].as_f64().unwrap_or(f64::NAN)),
        "",
    );
    s
}

pub fn bench(root: &Path, args: &[String]) -> Result<(), String> {
    let o = options(args)?;
    let started = Instant::now();
    let runs = o.runs.unwrap_or(if o.quick { 3 } else { 9 });
    let launch_runs = o.runs.unwrap_or(if o.quick { 5 } else { 9 });
    let settle = Duration::from_secs(if o.quick { 3 } else { 10 });
    let mut fixtures = vec![];
    for (rows, label, make) in [(100_000, "100k", true), (1_000_000, "1M", true), (10_000_000, "10M", !o.quick)] {
        if let Some(p) = fixture(root, rows, label, make)? {
            fixtures.push((label, p));
        }
    }
    build(root)?;
    let target = target_dir(root).join("release");
    let quarry = target.join("quarry");
    let probe_exe = target.join("quarry-bench");
    let scratch = target_dir(root).join("bench");
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;

    let mut r = json!({
        "kind": if o.quick { "quick" } else { "full" },
        "date": run_text("date", &["+%Y-%m-%dT%H:%M:%S%z"]),
        "machine": machine(root),
        "binary": sizes(&quarry, &scratch),
    });
    if o.launch {
        r["launch"] = launch(&quarry, &scratch, launch_runs, settle)?;
    }
    let mut fx = serde_json::Map::new();
    for (label, path) in &fixtures {
        let mut f = json!({"path": path.strip_prefix(root).unwrap_or(path), "bytes": std::fs::metadata(path).map(|m| m.len()).ok()});
        f["app"] = app_probe(&probe_exe, path, runs)?;
        f["engine"] = engine_probe(&probe_exe, path, runs)?;
        f["sqlite3"] = sqlite3(path, runs);
        fx.insert((*label).to_string(), f);
    }
    r["fixtures"] = Value::Object(fx);
    let one_m = fixtures.iter().find(|(l, _)| *l == "1M").map(|(_, p)| p.as_path());
    r["grid"] = grid(root, one_m)?;
    r["competitors"] = competitors(one_m, &scratch, if o.quick { 3 } else { 5 }, settle);
    r["postgres"] = if std::env::var_os("QUARRY_PG_URL").is_some() {
        let rows = if o.quick { "100000" } else { "1000000" };
        engine_summary(probe(&probe_exe, &["pg", "--rows", rows, "--runs", &runs.to_string()])?)?
    } else {
        json!({"measured": false, "why": "QUARRY_PG_URL is not set"})
    };
    r["machine"]["load_average_after"] = json!(run_text("sysctl", &["-n", "vm.loadavg"]));
    r["took_s"] = json!(started.elapsed().as_secs());

    let out = o.out.unwrap_or_else(|| {
        let day = run_text("date", &["+%Y-%m-%d"]).unwrap_or_else(|| "undated".into());
        root.join("bench").join("results").join(format!("{day}-{}.json", r["kind"].as_str().unwrap_or("run")))
    });
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?;
    std::fs::write(&out, text + "\n").map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{}", table(&r));
    eprintln!("bench: wrote {} in {} s", out.display(), started.elapsed().as_secs());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_use_nearest_rank() {
        let s = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        assert_eq!(quantile(&s, 0.5), 5.0);
        assert_eq!(quantile(&s, 0.95), 9.0);
        assert!(quantile(&[], 0.5).is_nan());
    }

    #[test]
    fn summaries_drop_missing_samples() {
        let v = summary(&[3.0, f64::NAN, 1.0, 2.0]);
        assert_eq!(v["median"], 2.0);
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn cpu_times_and_vmmap_sizes_parse() {
        assert_eq!(parse_cpu_time("0:01.50"), Some(1.5));
        assert_eq!(parse_cpu_time("1:00:00"), Some(3600.0));
        assert_eq!(parse_cpu_time(""), None);
        assert_eq!(vmmap_mb("Physical footprint:         36.7M", "Physical footprint:"), Some(36.7));
        assert_eq!(vmmap_mb("Physical footprint (peak):  1.5G", "Physical footprint (peak):"), Some(1536.0));
    }

    #[test]
    fn grid_bench_lines_parse() {
        let v = parse_grid_lines("test x ... bench: grid_scroll_fixture_1m pages_fetched=49 frames=1302 cpu_ms p50=0.505 p95=1.599 rss_mb=n/a\nok");
        assert_eq!(v["fixture_1m"]["p95"], 1.599);
        assert_eq!(v["fixture_1m"]["frames"], 1302.0);
        assert_eq!(v["fixture_1m"]["rss_mb"], "n/a");
    }
}
