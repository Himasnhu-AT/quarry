# Benchmarks

How fast Quarry is, how that was measured, and how to reproduce it. Every number here comes from a
JSON file in `bench/results/`, written by `cargo xtask bench`. Numbers that are not in that folder
must not appear in marketing (docs/PLAN.md §7).

## Machine and conditions

- Apple M5 Pro (15 cores), 24 GB, macOS 26.6.2, rustc 1.99.1, Quarry at `cff7466`.
- Release build (`[profile.release]`, line-table debug info), run through `memgate`.
- **This machine was not quiet.** Other agents were building on it (load average 7–15 on 15 cores,
  and a second job in memgate's queue during the run). Timings on a loaded machine are inflated
  and vary from run to run; sizes, frame counts and memory are not much affected. An earlier quick
  run the same afternoon, at load average 15–17 and with `cargo xtask ci` running beside it, gave a
  launch median of 926 ms (samples 247–1,437 ms) and scroll p95 / p99 of 1.04 / 1.25 ms (1M) and
  1.08 / 1.39 ms (10M). Compare only runs taken under the same conditions, and prefer a quiet rerun
  (`cargo xtask bench`, the full run) before publishing anything.

## Results (quick run, 2026-11-12)

Raw data: `bench/results/2026-11-12-quick.json` (every sample is in it). Medians: of 5 launches,
of 3 app runs (one process each), of 3 engine runs. Fixtures from `cargo xtask fixtures` (seed
fixed, byte-identical on every OS): `orders` has 100k, 1M or 10M rows of 7 columns.

| Measure | Quarry | Budget (PLAN §6) | Within? |
|---|---|---|---|
| Launch → first presented frame, warm (app's own figure) | 160 ms (133–231) | ≤ 200 ms | yes, at the median; one run of 5 over |
| Spawn → ready file / spawn → window on screen | 173 / 184 ms (one run 983 / 980 ms) | | |
| Idle CPU, 3 s right after launch | 0.05 s (≈ 1.7 %) | 0 % | **no**: not zero yet (may be the start-up tail; the full run idles 10 s) |
| Idle RSS / footprint / peak footprint, real window | 118 / 83 / 225 MB | | |
| Connect + schema tree visible (SQLite, 3 tables) | 2.4–3.1 ms | ≤ 300 ms for 2,000 tables (not measured) | n/a |
| `SELECT * FROM orders` → first row on screen, 100k / 1M / 10M | 4.2 / 14.4 / 16.2 ms | ≤ 50 ms over the server | yes (SQLite in-process, no server) |
| Open the `orders` table → first rows | not measurable | | the table tab shows no rows yet |
| Engine: first page (1,000 rows), 100k / 1M / 10M | 0.3 / 0.4 / 0.6 ms | | |
| Engine: every row, 100k / 1M / 10M | 28 ms / 483 ms / 4.79 s (≈ 2.1M rows/s) | | |
| Scroll 1M rows from SQLite, CPU per frame p50 / p95 / p99 | 0.12 / 0.43 / 0.49 ms | ≤ 1 ms | yes this run; **no** on the loaded run (p95 1.04, p99 1.25) |
| Scroll 10M generated rows, p50 / p95 / p99 | 0.15 / 0.45 / 0.78 ms (max 1.70) | ≤ 1 ms | yes this run; **no** on the loaded run (p95 1.08, p99 1.39) |
| Footprint with the 10M result open, headless (no GPU) | 11.4 MB, peak 11.5 MB; grid cache 2 pages / 572 KB | ≤ 150 MB | yes, but see "What is not covered" |
| Binary size, release / stripped | 38.2 / 27.4 MB | | no `release-small` profile, no .app / .dmg yet |

### Baseline: the `sqlite3` CLI on the same file

`/usr/bin/sqlite3` 3.51.0 (Apple's), `sqlite3 -readonly FIXTURE 'SELECT * FROM orders'` with its
output read from a pipe, process start included. Quarry's engine uses bundled SQLite 3.54.1.

| Rows | sqlite3 first byte | sqlite3 all rows | Quarry engine first page | Quarry engine all rows |
|---|---|---|---|---|
| 100k | 3.1 ms | 49 ms | 0.3 ms | 28 ms |
| 1M | 5.5 ms | 781 ms | 0.4 ms | 483 ms |
| 10M | 8.6 ms | 4,752 ms | 0.6 ms | 4,791 ms |

This is not like for like: `sqlite3` formats every row as text and writes it to a pipe, and pays for
its own process start; Quarry's engine converts rows into typed values in memory. At 10M rows the
two are level (the CLI is 1 % faster); on the earlier loaded run the CLI won clearly (8.8 s against
10.8 s). The grid never reads a whole 10M-row result: it fetches visible pages only.

### Competitors

None of the GUI tools in PLAN §7 is installed on this machine, and nothing was installed for this
run. `cargo xtask bench` looks for them in `/Applications` and `~/Applications` and measures any it
finds with the method below, with no change to the harness:

| Tool | Status here |
|---|---|
| DBeaver | not installed here |
| Beekeeper Studio | not installed here |
| DB Browser for SQLite | not installed here |
| TablePlus | not installed here |
| pgAdmin 4 | not installed here |
| `psql` | not installed here (PostgreSQL numbers need `QUARRY_PG_URL` as well) |

What can fairly be compared with a GUI tool from outside it: **spawn → first window on screen**
and **RSS / footprint after opening the same SQLite file and idling**. Quarry's own launch row
reports the same spawn → window figure so the two line up. Query-to-first-row, scroll CPU and
memory with a 10M-row result open need each tool's own instrumentation or a scripted UI, and are
not claimed for other tools.

## Method

`cargo xtask bench` (`xtask/src/bench.rs`) runs, in order:

1. **Fixtures.** 100k and 1M are made when missing; 10M (3 GB, about 5 minutes) is made by a full
   run and used by `--quick` only if it is already there.
2. **Builds** `quarry` and `quarry-bench` in release (`--locked`); records the binary and a
   `strip`ped copy.
3. **Launch** (real window; it opens and closes Quarry N times). Each run gets a fresh
   `ASHLAR_DIRS_ROOT` (no saved connections, nothing restored) and `DO_NOT_TRACK=1`. Three figures:
   the app's `QUARRY_READY_FILE` `ready_ms` (start of `run()` to the first presented frame),
   spawn → that file, and spawn → the first on-screen window in CoreGraphics' window list (a JXA
   watcher started *before* the app, polling every 2 ms, so its own start-up is not timed; no
   permission prompt). Then idle for the settle time and read `ps` RSS and CPU time and `vmmap`
   footprint and peak.
4. **App probe** (`quarry-bench app`, `bench/src/main.rs`), once per fixture and run, each in its
   own process: the whole Quarry app on a headless ashlar `Driver`, 1280×800 pt at 2×, with no GPU
   renderer (the egui pass runs; tessellation and painting do not). Wall-clock time from the
   command (`conn.open`, `table.open`) or the person's own Mod+Enter to the first frame whose
   accessibility outline shows the tables / `rows N`. One poll (a frame plus a `ui.snapshot`)
   costs 0.23–0.34 ms: that is the resolution. Memory is `app.stats` (`phys_footprint`).
5. **Engine probe** (`quarry-bench engine`): `connect_human`, `schema`, `query` (first page of
   1,000) and every following page, in-process.
6. **Scroll**: `crates/app/tests/grid_bench.rs` (the grid stream's bench): the whole window at
   2×, wheel 2,000 pt a frame for 600 frames at 120 Hz then jumps far and back; UI-thread CPU per
   frame of the egui pass, from ashlar's frame log. `fixture_1m` reads the 1M fixture; `generated_10m`
   is 10M generated rows.
7. **Baselines and competitors** as above.
8. **PostgreSQL** (`quarry-bench pg`) only with `QUARRY_PG_URL`: the `orders` shape from
   `generate_series` (read-only; nothing is created on the server), connect, schema, first page,
   every row. Not run here (no server, Docker off).

Medians are nearest-rank. Full runs use 9 launches and 9 app / engine runs, and idle 10 s; `--quick`
uses 5 and 3, and idles 3 s.

## What is not covered yet

- **Open a table**: `table.open` adds a table tab, but the tab loads no rows yet (an integration
  gap being fixed separately). The probe waits 5 s, records the failure and moves on; it will
  measure as soon as the tab loads rows.
- **Memory with a 10M-row result in a real window**: running SQL is a person-only action (an
  injected key press runs nothing), so the windowed app cannot be driven to a result from outside.
  The 11 MB figure is headless, without GPU buffers; a real window adds roughly the 80–200 MB
  footprint of the launch rows above.
- **Schema of 2,000 tables** and **PostgreSQL** query-to-first-row: need a fixture and a server.
- **Cold launch** (first run after a reboot or a cache flush): only warm launches are measured.

## Reproduce

```sh
cargo xtask fixtures --rows 10M                 # optional for --quick; a full run makes it
memgate -- cargo xtask bench --quick            # ≈ 2–4 minutes with fixtures present
memgate -- cargo xtask bench                    # full: 9 runs, 10 s idle
QUARRY_PG_URL=postgres://… memgate -- cargo xtask bench   # adds PostgreSQL
cargo xtask bench --no-launch                   # no windows (e.g. over SSH)

# the pieces on their own
target/release/quarry-bench app fixtures/quarry-1M.sqlite
target/release/quarry-bench engine fixtures/quarry-1M.sqlite --runs 5
cargo test -p quarry --release --test grid_bench -- --ignored --nocapture --test-threads=1
```

Results go to `bench/results/<date>-<quick|full>.json` (`--out PATH` to change), with the machine,
the commit (and whether the tree was dirty), the load average before and after, and `memgate status`.
