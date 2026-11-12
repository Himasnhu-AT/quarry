# Quarry plan

Status: planning. Work starts after ashlar 0.2 is on crates.io.

## 1. Users and jobs

| User | Job | What hurts today |
|---|---|---|
| Backend developer | Look at prod/staging data, check a migration, find a bad row | Heavy GUIs (JVM, Electron) that take seconds to open; psql is fast but blind |
| Data-curious PM / support | Answer "how many users did X" | Writing SQL; fear of breaking something |
| Developer with an AI agent | Let Claude/Cursor look at the real schema and data while coding | Giving an agent a connection string means giving it write rights |
| Indie / SQLite user | Inspect an app's `.sqlite` file | DB Browser for SQLite feels dated; no AI |

The wedge: **the safest way to let an AI look at your database**, and a native browser fast enough to
keep open all day.

## 2. Scope

### MVP (v0.1)
- **Engines:** PostgreSQL 12+ and SQLite 3.
- **Connections:** host/port/db/user, TLS modes (disable/prefer/require/verify-full, custom CA),
  SSH tunnel (key or agent), SQLite file picker; passwords in the OS keychain
  (`ashlar_native::secrets`), never in settings files. Saved connections with colors (prod = red).
- **Schema tree:** schemas, tables, views, columns with types, indexes, foreign keys; counts are
  estimates (`pg_class.reltuples`), never `COUNT(*)` on open.
- **Result grid:** virtualized rows and columns, streaming from a server-side cursor
  (`DECLARE … CURSOR` / SQLite step), fetches pages of 1,000 as the viewport needs them; column
  resize, sort (re-query), copy as TSV/CSV/JSON/INSERT; cell viewer for JSON/text/bytea.
- **SQL editor:** tabs, highlighting (`ashlar-syntax` `Lang::Sql`), completion from the schema,
  run statement under cursor / selection / all, cancel (`pg_cancel_backend` via a cancel token).
- **Table editing:** edit cells, add/delete rows; changes are pending (diff view) until Commit,
  which runs them in one transaction. Undo before commit through ashlar's document history.
- **Query history** per connection, searchable.
- **EXPLAIN view:** `EXPLAIN (FORMAT JSON)` as a tree with cost/time bars; ANALYZE only on request
  and only for read-only statements.
- **Agent:** MCP server (stdio and the running window), the tools in section 3, the security model in
  docs/SECURITY.md.

### Later
Export CSV/JSON/Parquet as jobs with progress; import CSV; ER diagram; MySQL/MariaDB, DuckDB,
ClickHouse; saved charts and dashboards; query parameters; team-shared connections (no secrets).

### Not doing
A DBA console (users, replication, backups), a migration tool, a cloud service.

## 3. AI / MCP surface

Every tool is an ashlar command, so it is also in the palette and the CLI. "Read only" = can run
without asking; "Approval" = only creates a proposal the human approves in the window.

| Tool | Params | Kind | Notes |
|---|---|---|---|
| `db.connections` | — | read only | names, engine, color; never secrets |
| `db.schema` | `connection`, `schema?`, `table?` | read only | tree with columns, keys, indexes, row estimates |
| `db.query` | `connection`, `sql`, `limit≤5000`, `timeout_ms≤30000` | read only | runs on the agent's read-only connection after the classifier; sensitive columns redacted |
| `db.explain` | `connection`, `sql`, `analyze=false` | read only | ANALYZE only for classified-read statements |
| `db.sample` | `connection`, `table`, `n≤100` | read only | `TABLESAMPLE` / `ORDER BY random()` with caps |
| `db.propose_write` | `connection`, `sql`, `reason` | approval | returns `proposal_id`, status `pending`; never runs SQL itself |
| `db.proposal_status` | `proposal_id` | read only | pending / approved / denied / failed + result |
| `ui.open_table` | `connection`, `table` | UI | opens a tab, selects it |
| `ui.run_in_editor` | `sql` | UI | puts SQL in a new editor tab; does not run writes |
| `ui.filter` | `tab`, `where` | UI | the where clause goes through the classifier |
| `ui.select` | `tab`, `rows`, `columns?` | UI | highlights cells the human should look at |
| `ui.focus` | `region` | UI | sidebar, editor, grid, agent panel |
| `viz.chart` | `kind` (bar, line, area, scatter, histogram), `sql`, `x`, `y`, `series?`, `title` | read only | runs the query read-only, draws a chart panel next to the grid |
| `viz.pivot` | `sql`, `rows`, `columns`, `value`, `agg` | read only | pivot table panel |
| `viz.close` | `panel` | UI | |

The window shows every agent action in the agent panel (tool, params, result size, time), so the human
can see what the agent saw.

## 4. Architecture on ashlar

```
quarry/                       workspace
  crates/core      rank 0  data model: Connection, Schema, Value, Page, Proposal, AuditEntry (no I/O)
  crates/guard     rank 0  SQL classifier on sqlparser-rs + policy (see SECURITY.md)
  crates/engine    rank 1  Engine trait; pg (tokio-postgres) and sqlite (rusqlite) backends, cursors,
                           cancel, the two connection pools (human, agent)
  crates/app       rank 2  ashlar App<Workspace>: commands, views, MCP tools
  crates/viz       rank 2  charts and pivot on egui painter (no web view)
  xtask                    gates, benchmark harness
```

- **Document:** `Workspace` (open tabs, editor texts, pending edits, pinned charts) is the ashlar
  document, so undo/redo, autosave and crash recovery come free. Connections live in settings
  (`ashlar-fs` settings), secrets in the keychain.
- **Commands:** `cmd!`/`CommandSpec` for every action (`conn.open`, `query.run`, `query.cancel`,
  `table.open`, `edit.commit`, `proposal.approve`, …); MCP tools are the same commands through
  `ashlar-mcp` and the CLI (`quarry-cli exec`).
- **Jobs:** queries, page fetches, schema loads and exports run with `ashlar-jobs`
  (`spawn_streaming` for cursors, cancel through the job's cancel flag); results land on the UI
  thread as revisions.
- **UI:** `ashlar-ui` sidebar (schema tree), tabs, `VirtualTable` for the grid, `CodeView`-based SQL
  editor with `ashlar-syntax`, `DiffView` for pending changes, chat kit (`Transcript`, `ToolCard`,
  `Approval`) for the agent panel, toasts, forms for the connection dialog.
- **Testing:** `--headless --snap/--tree`, `.ui` scripts on a virtual clock, golden screenshots of
  each screen with fixture databases (SQLite file + a Postgres container in CI).

### Drivers

| Option | For | Against | Choice |
|---|---|---|---|
| tokio-postgres | async, cancel tokens, binary protocol, portals (cursors), small | own type mapping | **Postgres** |
| sqlx | one API, compile-time checks | compile-time checks need a DB at build; heavier; weaker cursor control | no |
| rusqlite (bundled) | mature, `OPEN_READ_ONLY`, `query_only`, authorizer callback | sync (fine: runs in a job) | **SQLite** |
| postgres (sync) | simple | no cancel while blocked | no |

The SQLite authorizer callback (`sqlite3_set_authorizer`) gives a second, engine-level write guard
for the agent connection, independent of the classifier.

## 5. Data model (crates/core)

- `ConnectionId`, `ConnectionSpec { engine, host, port, db, user, tls, ssh, color, agent_access }`
- `Value` = Null | Bool | Int(i64) | Float(f64) | Text(Arc<str>) | Bytes(len, preview) | Json | Time | Other(text); cells capped at 64 KB in the grid (full value on demand)
- `Page { first_row, rows: Vec<Vec<Value>> }`, `ResultSet { columns, pages, total: Known(n) | AtLeast(n) }`
- `Proposal { id, connection, sql, reason, tables, estimate, status, requested_at, decided_by, result }`
- `AuditEntry { at, actor: Human|Agent, kind, sql, rows_affected, undo_sql? }` (append-only JSONL)

## 6. Performance budgets

| Measure | Budget |
|---|---|
| Launch to first frame (warm) | ≤ 200 ms |
| Idle CPU, window open | 0 % |
| Query to first row on screen (local PG, simple SELECT) | ≤ 50 ms over the server time |
| Scroll a 10M-row result | ≤ 1 ms CPU per frame, no hitch on page fetch |
| Memory with a 10M-row result open | ≤ 150 MB (only visible pages + 2 neighbours cached) |
| Schema of 2,000 tables | tree ready ≤ 300 ms |

## 7. Benchmarks (published)

Compared with DBeaver, Beekeeper Studio, pgAdmin 4, TablePlus and DB Browser for SQLite (SQLite only).
Same Mac, same fixtures, median of 9 runs, scripts and fixtures in `bench/` so anyone can rerun them.

| Measure | How |
|---|---|
| Launch to usable window (cold, warm) | launcher timing spawn → first frame / window visible |
| Connect to local PG | click → schema tree visible |
| Open a 10M-row table | open → first rows drawn |
| Query to first row | fixed SELECT on a 10M-row table |
| Scroll CPU per frame | scripted fling over 100k rows; CPU time / frames |
| Memory (RSS) | idle after 60 s; with the 10M-row table open |
| Idle CPU | 60 s, window open, nothing happening |
| Download / install size | dmg and installed app |

Rules: report where others win, give versions, no numbers in marketing that aren't in the repo.

## 8. Milestones (after ashlar 0.2)

| M | Weeks | Done when |
|---|---|---|
| M0 Contracts | 0.5 | shared contracts below frozen in `crates/core` and `docs/contracts.md` |
| M1 Engine | 1–2 | pg + sqlite connect, schema, streaming query, cancel; tests on fixtures |
| M2 Guard | 1–2 | classifier + agent connection + proposals + audit; SECURITY.md tests pass |
| M3 Shell | 1–3 | window, sidebar, editor, grid, connection dialog, history |
| M4 Agent | 3–4 | MCP tools, agent panel, approval cards, viz.chart |
| M5 Edit + explain | 4–5 | pending edits with diff, commit, EXPLAIN tree |
| M6 Bench + launch | 5–6 | benchmark scripts, numbers, video, release |

## 9. Parallel work

The owner builds Quarry with parallel agents. Contracts first, then six streams.

### Shared contracts (frozen in M0; change only additively, tell every consumer and the lead)
1. **Command ids and params:** the list in section 3 plus UI commands, in `crates/app/src/ids.rs`.
2. **Data types:** `crates/core` (section 5).
3. **MCP tool schemas:** JSON schemas generated from the command params, snapshot-tested.
4. **Write-approval contract:** `Proposal` + states `pending → approved → running → done | failed`,
   `pending → denied | expired`; who may change state (only `proposal.approve`/`deny` commands from
   the human surface; never MCP).
5. **Engine trait:** `connect`, `schema`, `query(sql, Cursor)`, `fetch(page)`, `cancel`, `explain`,
   `execute_in_tx(statements)` (human connection only).

### Workstreams

| Stream | Owns (files) | Depends on | Done when |
|---|---|---|---|
| `engine` | `crates/engine/**` | contracts 2, 5 | pg + sqlite pass the engine test suite incl. cursors and cancel |
| `guard` | `crates/guard/**`, `docs/SECURITY.md` | contract 2, 4 | every threat in SECURITY.md has a passing test |
| `grid` | `crates/app/src/grid/**` | contracts 1, 2 | 10M-row fixture scrolls within budget; copy formats |
| `shell` | `crates/app/src/{main.rs,sidebar,editor,dialogs}/**` | contract 1 | window, schema tree, editor, connection dialog, history |
| `agent` | `crates/app/src/agent/**`, `crates/viz/**` | contracts 1, 3, 4 | MCP tools, agent panel, approval cards, charts; `.ui` scripts green |
| `bench` | `bench/**`, `xtask/**` | M3 | scripts produce the section 7 table reproducibly |

### Worktrees
`.claude/worktrees/<stream>` on branch `wt/<stream>`, `CARGO_TARGET_DIR=<worktree>/target`, heavy
commands through `memgate`. One agent per worktree; nobody commits on `main`.

### Merge order (the lead merges, full gate before each)
contracts → engine → guard → shell → grid → agent → bench.

### Roster (TEAM.md template)

| Agent | Stream | Branch | Now | Done |
|---|---|---|---|---|
| | engine | wt/engine | | |
| | guard | wt/guard | | |
| | shell | wt/shell | | |
| | grid | wt/grid | | |
| | agent | wt/agent | | |
| | bench | wt/bench | | |

## 10. Risks

| Risk | Mitigation |
|---|---|
| Classifier misses a write path | Engine-level guards too (read-only session, read-only role, SQLite authorizer); fuzz tests |
| Grid jank on page fetch | prefetch neighbours; fetch in jobs; never block a frame |
| Postgres type zoo (arrays, ranges, custom) | `Value::Other(text)` fallback via text format |
| SSH tunnel complexity | start with `russh` + key/agent auth only; password auth later |
| egui text editing limits for a SQL editor | start on ashlar's editor + CodeView; measure on 5k-line scripts |

## 11. Open questions
- License (BSD-3 like ashlar, or AGPL for the app)?
- Bundle a local agent (ashlar-assistant) or MCP only at first?
- Should the agent be allowed `EXPLAIN ANALYZE` on reads by default (it executes the query)?
