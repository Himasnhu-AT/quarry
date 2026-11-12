# Quarry contracts (M0, frozen)

The five shared contracts of docs/PLAN.md §9, as built on branch `wt/contracts` against
`ashlar-rs` 0.2.0. Every stream builds on them. Change them **only additively** (new commands,
new optional params and fields with `#[serde(default)]`, new enum variants at the end) and log it
in TEAM.md; anything else goes through the lead.

## Layout

| Crate | Package | Rank | What |
|---|---|---|---|
| `crates/core` | `quarry-core` | 0 | data model, proposal state machine, agent result types; no I/O |
| `crates/guard` | `quarry-guard` | 0 | SQL classifier (sqlparser 0.64), redaction; M0: parse + single statement + default deny |
| `crates/engine` | `quarry-engine` | 1 | `Engine` trait; `pg` (tokio-postgres) and `sqlite` (rusqlite, bundled) stubs |
| `crates/app` | `quarry` | 2 | `quarry::app()`, binaries `quarry` and `quarry-cli`, commands, views, scenes |
| `crates/viz` | `quarry-viz` | 2 | charts and pivots on the egui painter (stub) |
| `xtask` | `xtask` | – | `cargo xtask ci`, `cargo xtask fixtures --rows 10M` |

Toolchain 1.99.1 (`rust-toolchain.toml`), edition 2024, ashlar's lints (no unwrap / expect /
panic / todo / unreachable / unimplemented outside tests, no unsafe).

## 1. Command ids and params — `crates/app/src/ids.rs`

Three audiences, each a list in `ids.rs` and checked by `crates/app/tests/commands.rs`:

**Agent tools** (`AGENT_TOOLS`, 15, registered; MCP / CLI / control port / palette):

| Id | Params (`*` required) | Kind | `mutates` |
|---|---|---|---|
| `db.connections` | — | read only | no (`query!`) |
| `db.schema` | `connection*`, `schema`, `table` | read only | no |
| `db.query` | `connection*`, `sql*`, `limit` 1–5000, `timeout_ms` 1–30000 | read only | no |
| `db.explain` | `connection*`, `sql*`, `analyze` | read only | no |
| `db.sample` | `connection*`, `table*`, `n` 1–100 | read only | no |
| `db.propose_write` | `connection*`, `sql*`, `reason*` | approval | yes (so MCP clients never treat it as read-only) |
| `db.proposal_status` | `proposal_id*` | read only | no |
| `view.open_table` | `connection*`, `table*` | UI | yes (opens a tab) |
| `view.run_in_editor` | `sql*`, `connection` | UI | yes |
| `view.filter` | `tab*`, `where*` (guard-checked) | UI | yes |
| `view.select` | `tab*`, `rows*` (JSON), `columns` (JSON) | UI | no |
| `view.focus` | `region*` ∈ sidebar, editor, grid, agent | UI | no |
| `viz.chart` | `kind*` ∈ bar, line, area, scatter, histogram; `connection*`, `sql*`, `x*`, `y`, `series`, `title` | read only (DB) | yes (adds a panel) |
| `viz.pivot` | `connection*`, `sql*`, `rows*`, `columns*`, `value*`, `agg` ∈ count, sum, avg, min, max | read only (DB) | yes |
| `viz.close` | `panel*` | UI | yes |

MCP names (`ToolMode::PerCommand`) replace dots with underscores: `db_query`, `view_focus`.

**Human commands** (`HUMAN_COMMANDS`, registered, harmless from any surface — none writes to a
database, decides a proposal or changes agent access): `palette.open` (⌘K), `conn.new`,
`conn.edit`, `conn.open`, `conn.close`, `conn.test`, `tab.new_editor` (⌘T), `tab.close`,
`tab.select`, `table.open` (⌘P), `query.cancel` (⌘.), `query.explain` (⌘E, plain EXPLAIN),
`query.format`, `history.show`, `grid.filter` (⌘F), `grid.sort`, `grid.copy` (`format` ∈ tsv, csv,
json, insert), `edit.cell` (F2), `edit.add_row`, `edit.delete_rows`, `edit.discard`,
`edit.review` (⌘⌥S: shows the diff), `agent.toggle_panel` (⌘L), `proposal.focus` (⌘⇧A).

**Human-only actions** (`HUMAN_ONLY`, **not registered**; `crates/app/src/human.rs`):
`proposal.approve`, `proposal.deny`, `query.run` (⌘↵), `query.run_all` (⇧⌘↵),
`query.explain_analyze`, `edit.commit`, `conn.save`, `conn.delete`, `agent.pause`,
`agent.resume`, `column.mark_sensitive`. See "Human-only actions" below.

Handlers are stubs failing with `CommandError::Failed { id, msg: "not_yet: not yet" }` (the
chokepoint fills in the id); `palette.open`, `tab.new_editor` and `agent.toggle_panel` work.
Params are validated by ashlar before a handler runs (a test checks `limit: 5001` is refused).

### Human-only actions (the approve/deny gap and its workaround)

ashlar 0.2 has no way to keep a registered command off MCP, the CLI or the control port: no
`human_only` flag on `CommandSpec`, no caller origin visible to a command body (`Session`'s
`Origin` is private and describes document changes), no hook into the MCP server `cli_main`
builds, and the control port can inject clicks, keys and menu invocations into the window
(`ui.click`, `ui.press`, `ui.menu.invoke`). **This is a blocking gap for SECURITY.md as written**
("the registry marks them `human_only`"). Workaround, in place now:

1. The actions above are not commands. They are absent from `list_commands`, MCP `tools/list`,
   `quarry-cli commands` and the native menu, and calling them by name fails `Unknown`
   (tests: `commands.rs`, `mcp_shapes.rs`, `headless.rs`).
2. They run only through `human::perform(session, HumanTicket, HumanAction)`.
3. A `HumanTicket` has a private field; it is minted only by `HumanTicket::from_click(ctx,
   &response)` or `HumanTicket::from_shortcut(ctx, &shortcut)`, and never while
   `human::agent_driving(ctx)` (= `ashlar::ui::devtools::has_pending`) holds, i.e. while the
   control port has injected input, waits or screenshots in flight.
4. Window shortcuts for them are consumed by the view, not the keymap. Approve is
   `Mod+Alt+Enter` while the card has focus (DESIGN.md's ⌘⇧↵ collided with Run all); Deny is the
   default button.

Cost: these actions are not in the palette or the CLI. The `agent` stream must add a test that
a `ui.click` on `quarry.approval.approve` through the control port does not approve (the
injected frame must see `has_pending` true). Ask ashlar for: a per-command surface filter
(`CommandSpec::surfaces(Window)`), caller origin in `Host::before`, and an injected-input marker
or a control-port method allow-list. When it lands, these become registered human-only commands
with the same ids (additive).

## 2. Data types — `crates/core`

`ConnectionId` (string slug), `ProposalId`, `TabId`, `PanelId`, `CursorId` (u64), `Timestamp`
(Unix ms; core never reads the clock).

- `ConnectionSpec { id, name, target: Target::{Postgres{host,port,db,user,tls,ssh}, Sqlite{path}},
  color, agent_access: Off | ReadOnly | ReadOnlyRole{role}, sensitive_patterns, human_read_only }`.
  No secret ever; `Secret` (keychain value) is not `Serialize`/`Clone` and `Debug` prints
  `Secret(…)`. Agents see `ConnectionSummary { id, name, engine, color, agent_access }`.
- `Value = Null | Bool | Int | Float | Text | Bytes{len, preview} | Json | Time | Clipped{preview,
  len} | Redacted{len} | Other`. Cell caps in `limits` (64 KB, 64-byte byte previews).
  `Value::to_agent_json` is the agent form: scalars where exact, strings for text-like types,
  `{"$bytes", "$hex"}`, `{"$clipped", "$preview"}`, `{"$redacted": len}`.
- `Column { name, type, sensitive }`, `Page { first_row, rows }`, `Total = Known(n) | AtLeast(n)`,
  `ResultSet { columns, pages, total }` with `cell(row, col)`.
- `SchemaTree { namespaces: [Namespace { name, tables: [Table { name, kind, row_estimate, columns,
  indexes, foreign_keys }] }] }`, `TableRef { schema?, name }`.
- `PlanNode` (EXPLAIN tree), `AuditEntry { at, actor, kind, connection, proposal?, sql,
  rows_affected?, undo_sql?, note? }` (one JSONL line), `Refusal { code, message }` displayed as
  `"<code>: <message>"`.
- `Workspace` (the ashlar document): tabs (`Editor { sql }` | `Table { table, filter }`), active
  tab, chart panels. Proposals are **not** in the document (undo must never touch a decision);
  they live in `crates/app/src/services.rs` (process-wide, see gaps).
- `limits`: page 1,000 rows; agent row cap 5,000; timeout 15 s default / 30 s max; sample ≤ 100;
  proposal TTL 10 min; ≤ 20 pending proposals.

## 3. MCP tool results — `crates/core/src/tool.rs`, `crates/app/tests/mcp/*.json`

One result type per agent tool (`ConnectionsResult`, `SchemaResult`, `RowsResult` for `db.query`
and `db.sample`, `ExplainResult`, `ProposeResult`, `ProposalStatusResult`, `TabResult`,
`FilterResult`, `SelectResult`, `FocusResult`, `ChartResult`, `PivotResult`, `CloseResult`).
`tests/mcp/<tool>.json` holds `{tool, mcp_name, kind, mutates, input_schema, result}`; the input
schema is generated from the command params. `tests/mcp_shapes.rs` fails on any drift and checks
each golden parses back into its type unchanged. After an intended additive change:
`QUARRY_BLESS=1 cargo test -p quarry --test mcp_shapes`. Failures are not results: a refused call
is `CommandError::Failed` with a `Refusal` message (`not_read_only: …`), `isError: true` on MCP.

## 4. Write approval — `crates/core/src/proposal.rs`

```
pending ─approve (Human)→ approved ─start (System)→ running ─finish (System)→ done
   │                                                   └────fail (System)──→ failed
   ├─deny (Human)→ denied
   └─expire (System, 10 min)→ expired
```

`transition(from, &event, actor)` is the whole rule, pure; the 126 (state × event × actor)
combinations are tested against the six allowed ones. The agent causes **no** transition: it
only creates a pending proposal (`ProposalBook::propose`, capped at 20 pending). A decision after
the deadline expires the proposal instead. `Human` means `human::perform` with a ticket (above);
`System` is the expiry timer and the executor job that runs the approved SQL with
`HumanConnection::execute_in_tx(statements, Some(estimate))`, which rolls back when the affected
rows leave the estimate's tolerance (→ `fail`).

## 5. Engine — `crates/engine/src/lib.rs`

```rust
trait Engine { kind; connect_human(spec, secret) -> Box<dyn HumanConnection>;
               connect_agent(spec, secret) -> Box<dyn AgentConnection> }
trait Browse { schema(&SchemaFilter); fetch(cursor, first_row, rows) -> Page; close(cursor);
               cancel_token() -> CancelToken }
trait HumanConnection: Browse { query(&str, &QueryOptions) -> Cursor; explain(&str, analyze);
               execute_in_tx(&[String], Option<Estimate>) -> TxOutcome }
trait AgentConnection: Browse { query(&ReadStatement, ..) -> Cursor; explain(&ReadStatement, analyze) }
```

Blocking API, called from ashlar jobs only; pg runs tokio-postgres on its own current-thread
runtime. `ReadStatement` is built only by `quarry_guard::classify` (and, for engine tests proving
layer A alone holds, `ReadStatement::unchecked` behind the guard's `unchecked` feature, enabled
only as a dev-dependency). Two `compile_fail` doc tests prove the agent connection has no write
method and takes no raw SQL. `connect_agent` must apply every layer-A guard before returning.
Postgres tests run only when `QUARRY_PG_URL` is set; never against a real database otherwise.

## ashlar 0.2: plan names checked

| Plan said | 0.2 has | Fix |
|---|---|---|
| `VirtualTable` | `ashlar::ui::virtual_list::VirtualTable` | ok |
| `CodeView`-based SQL editor | `ashlar::ui::code_view::CodeView` is **read-only** | editor = `egui::TextEdit` with a layouter from `code_view::highlight_job(Some("sql"), …)`; `CodeView` for read-only SQL (approval card, history) |
| `ashlar-syntax` `Lang::Sql` | exists; not re-exported by `ashlar-rs` | `CodeView::lang(Some("sql"))`; depend on `ashlar-syntax` only if `Lang` is needed |
| `DiffView` | `ashlar::ui::diff_view::DiffView` (text diff) | ok; pending row edits shown as generated SQL text or grid marks |
| chat kit `Transcript`, `ToolCard`, `Approval` | `ashlar::ui::chat::{Transcript, ToolCard, Approval}`; `Approval` is the answer enum | ok, but `ToolCardResponse::approval` cannot tell a person from injected input: gate it with `HumanTicket` |
| `ashlar_native::secrets` | `ashlar_native::secrets::Keychain::{new, get, set, delete}` behind feature `secrets`, which `ashlar-rs` does not enable | add `ashlar-native = { version = "0.2", default-features = false, features = ["secrets"] }` to `crates/app` |
| `spawn_streaming` | `JobBuilder::spawn_streaming`: `cx.jobs.job(kind, label).spawn_streaming(input, \|i, progress, updates\| …)`; cancel via `progress.is_cancelled()` | jobs are reachable from the view (`Cx::jobs`) only, not from a command body |
| `ashlar-fs` settings | `ashlar::fs::Settings`, `Cx::settings` | ok |
| `cmd!` / `CommandSpec` / `query!` | ok; no `human_only` | see "Human-only actions" |
| "MCP server refuses them" | no hook from `cli_main` (`McpServer::on_execute` exists, but `ashlar_cli::Cli` builds its own server) | not registered instead |
| `ui.open_table`, `ui.filter`, `ui.select`, `ui.focus` | MCP names `ui_*` are ashlar's devtools passthrough tools; `ui_focus` exists and a colliding command tool is dropped (log warning only) | renamed **`view.*`** |
| `viz.chart`, `viz.pivot` params | no `connection` | `connection*` added |
| ⌘O open table, ⌘S commit, ⌘K palette | built-ins own ⌘O (`file.open`), ⌘S (`file.save`), ⇧⌘S; palette is ⇧⌘P | `table.open` ⌘P, `edit.review` ⌘⌥S, `palette.open` ⌘K (a Quarry command); a test checks no conflicts on either platform |
| egui `SidePanel` | egui 0.36 `egui::Panel::{left, right, top, bottom}` | ok |

## ashlar 0.2 gaps (not patched; workarounds in place)

1. **Blocking:** no per-command surface control (hide from MCP / CLI / control port), no caller
   origin in command bodies, and injected control-port input is indistinguishable from a person's
   (only `devtools::has_pending` hints at it). Workaround: human-only actions are not commands
   (above). Built-ins remain agent-callable: `file.save_as` / `file.open` (write or read any path),
   `app.quit`, `app.undo`.
2. `Session` has no slot for app state, and command bodies cannot spawn jobs. Runtime state lives
   in `quarry::services::services()` (process-wide; tests share it). Agent read tools that run
   SQL from a command body run synchronously (bounded by timeout and row cap); over the window's
   control port that blocks the UI thread for the query's duration. Ask: `Session` extensions or
   an `Effect::Spawn`, and async command replies (`ToolOutcome::pending` exists only for a custom
   `McpServer`).
3. `quarry-cli mcp` cannot add hooks or custom tools to the server `Cli` builds.
4. Built-in shortcuts cannot be rebound by the app (only by user keymap overrides).

## Every stream must know

- Read this file and the type docs in `crates/core` before coding. Ids, params, result shapes and
  the state machine are frozen; additive changes only, logged in TEAM.md.
- Never put a database write, a proposal decision or an agent-access change behind a registered
  command. Use `HumanAction` + `HumanTicket`.
- Agent SQL reaches an engine only as a `ReadStatement` from `quarry_guard::classify`.
- Agent results are built from `quarry_core::tool` types, cells via `Value::to_agent_json`, after
  `quarry_guard::redact`. Refusals are `Refusal` strings with a stable code.
- Each DESIGN.md screen has a scene (`quarry --list-scenes`; `crates/app/src/scenes.rs`); make
  yours real and keep its `quarry.screen.<name>` marker until a better automation id exists.
- Fixtures: `cargo xtask fixtures --rows 10M` writes `fixtures/quarry-10M.sqlite` (gitignored):
  `customers` (N/10, with a sensitive `password_hash`), `orders` (N, nullable `note`, a few long
  notes), `events` (N, JSON `payload`), indexes and `ANALYZE` stats. Same inputs, same bytes.
- Gate: `memgate -- cargo xtask ci`; Windows: `cargo xwin clippy --workspace --all-targets
  --target x86_64-pc-windows-msvc -- -D warnings`.
