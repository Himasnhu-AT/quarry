# Team

Who works on what. Each agent edits **its own row only** and appends to the log; the lead merges
in the order in docs/PLAN.md §9 (contracts → engine → guard → shell → grid → agent → bench).
Contracts are frozen (docs/contracts.md): change them only additively and tell every consumer and
the lead.

## Roster

| Agent | Stream | Branch | Owns | Now | Done |
|---|---|---|---|---|---|
| claude | engine | wt/engine | `crates/engine/**` | M1 done; Postgres suite written, not yet run against a server (no `QUARRY_PG_URL`, Docker off) | SQLite + PostgreSQL backends, layer A guards, cursors, cancel, EXPLAIN, `execute_in_tx`, 1M-row bench |
| claude | guard | wt/guard | `crates/guard/**`, `docs/SECURITY.md` | review / merge | classifier, proposal drafts, row cap, filters, sensitive-column rule; threat, corpus, policy, fuzz tests |
| claude | shell | wt/shell | `crates/app/src/{main.rs,view.rs,theme.rs,sidebar,editor,dialogs}/**` | review / merge | theme + fonts, sidebar schema tree, SQL editor (human-only run), history, connection dialog (keychain, agent access), table picker, every scene with pixel + accessibility goldens, `.ui` script test against the SQLite engine |
| claude | agent | wt/agent | `crates/app/src/agent/**`, `crates/viz/**` | review / merge | 15 agent tools over MCP (guarded reads, redaction, row/size caps, rate limit, pause), `db.propose_write` → pending + counted estimate + preview, approval card (human-only approve/deny, ⌘⌥↵), approved write in a job via `execute_in_tx` with rollback outside the estimate, audit log + undo SQL, expiry, activity list with jump-to, egui charts (bar/line/area/scatter/histogram) + pivots; MCP, injected-input, Driver approve/deny/rollback and expiry tests |
| claude | grid | wt/grid | `crates/app/src/grid/**` | review / merge | streaming grid on VirtualTable (page cache, cancel, 500k-row windows), sort (re-query / client), columns hide/reorder/freeze, cell popover, multi-cell copy TSV/CSV/JSON/INSERT, pending edits + DiffView/SQL, wired to the shell's runs; scenes, goldens, `.ui` script, scroll bench |
| | bench | wt/bench | `bench/**`, `xtask/**` | | |

Shared files (additive edits only, say so in the log): `crates/core/**`, `crates/app/src/ids.rs`,
`crates/app/src/human.rs`, `crates/app/src/services.rs`, `crates/app/src/scenes.rs`,
`crates/app/tests/mcp/**`, `docs/contracts.md`.

## Log

Newest last. One line per event: date, stream, what changed, who must know.

- 2026-11-12 · contracts · M0 on `wt/contracts`: workspace, five contracts, minimal app,
  `cargo xtask ci`, `cargo xtask fixtures`, CI. Every stream: read docs/contracts.md first
  (human-only actions are not commands; `ui.*` agent tools are `view.*`).
- 2026-11-12 · guard · classifier on `wt/guard` (QRY-GRD.1). Additive contract changes:
  `RefusalCode::Sensitive` ("sensitive", core, at the end); `WriteStatement` gains `kind:
  WriteKind`, `unbounded`, `count: Option<ReadStatement>` (estimate hook) and
  `approve_similar_allowed()`; new `propose_write`, `cap_rows`, `allowed_functions`; guard enables
  sqlparser's `visitor` feature (Cargo.lock adds `sqlparser_derive`). `ReadStatement::sql()` is now
  the canonical rendering, not the agent's text. **agent**: `db.propose_write` → `propose_write`,
  `db.query` / `viz.*` → `classify` then `cap_rows`, `view.filter` applies `check_filter`'s returned
  clause. **engine**: run `WriteStatement.sql` as exactly one statement (extended protocol /
  single prepare); `count` is the UPDATE/DELETE estimate query.
- 2026-11-12 · engine · M1 on `wt/engine`. Additive only: `quarry_engine::pg::parse_url(url) ->
  (Target, Option<Secret>)`; engine deps rustls (ring), tokio-postgres-rustls, webpki-roots,
  futures-util; tokio feature `sync`; rusqlite features `hooks`, `column_decltype`, `limits`
  (Cargo.lock updated). Behaviour all consumers should know: `query` reads the first page (the
  cursor's `total` is real); a person's write on PostgreSQL commits the cursor transaction first, so
  open PostgreSQL cursors of that connection end (`fetch` → `Closed`); results that cannot be a
  server cursor (`SHOW`, `… RETURNING`) are read whole up to 100k rows. guard: SECURITY.md layer A
  says "allow-listed functions" for SQLite; the authorizer denies `load_extension`,
  `fts3_tokenizer`, `readfile`, `writefile`, `edit`, `lsdir` and allows other built-ins (no app
  function is registered on the agent connection), and the PostgreSQL agent session checks after
  each statement that its read-only transaction is still the one it began; please word SECURITY.md
  to match.
- 2026-11-12 · shell · QRY-SH.1 on `wt/shell` (rebased on QRY-ENG.2). Additive shared edits:
  `human::HumanTicket::from_dialog` (a dialog reports an outcome, not a response); `ids.rs` wires
  `conn.*`, `tab.close/select`, `table.open`, `query.cancel/format`, `history.show` to
  `sidebar::commands` / `editor::commands` (ids and params unchanged); `lib.rs` installs
  `theme::install` in `App::setup`; `scenes.rs` adds `history`, `open-table`, `sidebar-menu` and
  resets `query_state`, `dialog`, `agent_panel`, `agent_state`, `proposal` before each scene
  (`Driver::scene` keeps view state); app deps `ashlar-native` (`secrets`), dev-dep `rusqlite`.
  Golden files: `crates/app/tests/a11y/<scene>.txt` (`QUARRY_BLESS=1 cargo test -p quarry --test
  shell`) and `tests/snapshots/<scene>.png` (`ASHLAR_UPDATE_SNAPSHOTS=1`): **every stream re-blesses
  the scenes it changes**. Settings: `quarry.connections` = `[{spec, approve_similar}]`,
  `quarry.history.<connection>`; passwords in the keychain (`dev.quarry.connections`, `conn.<id>`).
  Human-only actions that need settings or jobs (`query.run`, `query.run_all`, `conn.save`,
  `conn.delete`) take the `HumanTicket` in `editor::start_run` / `Connections::save|delete`, since
  `human::perform` only gets a `Session`. **grid**: `view.rs` still calls `grid::show(ui, &state)`;
  a run's first page, columns, total and cursor are in `editor::Outcome` (per tab in
  `view::Shell::tabs`) and the connection is `sidebar::connections::Human`: say which signature you
  want and the shell passes it. **agent**: `view_or("agent_active")` lights the status bar's
  READ-ONLY badge; `Saved::approve_similar` is false for prod and agent-access-off connections.
- 2026-11-12 · agent · QRY-AGT.1 on `wt/agent` (rebased on shell). Additive: core `ChartKind::Pivot`
  (end; not in `ChartKind::NAMES`), `ChartKind::from_name`, `ChartSpec.agg` (`serde(default)`,
  skipped when none; **every** `ChartSpec { … }` literal needs `agg: None`); `services::Connections`
  (agent tools and approved writes reach connections registered there; `quarry-cli` reads
  `QUARRY_CONNECTIONS`, a JSON list of specs) and `services::AuditLog` (`audit.jsonl`, `QUARRY_AUDIT`
  or the data folder); `human::perform` handles approve / deny; ids.rs agent-tool handlers and
  `proposal.focus` filled; scenes.rs: agent / approval / chart scenes go through
  `agent::scenes` (view key `agent.scene`, reset with the others). **shell**: `view.rs` registers a
  connection in `services().connections()` when the person connects (one block in `connect`), and
  ⌘↵ no longer swallows ⌘⌥↵ (Approve; `matches_logically` ignored Alt). **lead**: approved writes
  open their own human connection (not `sidebar::connections::Human`); worth sharing one.
- 2026-11-12 · grid · QRY-GRID.1 on `wt/grid` (rebased on shell + agent). Additive contract
  changes: core `edits` module (`PendingEdits`, `RowChange`) and `Workspace.edits`
  (`#[serde(default)]`, `Workspace::edits(tab)`); handlers for `grid.sort`, `grid.copy`,
  `edit.cell/add_row/delete_rows/discard/review` (ids.rs, ids/params unchanged; `grid.copy`
  returns counts only, never cell text); scenes `grid-loading`, `grid-values`, `grid-edits`,
  `grid-wide`; deps `ashlar-diff` (not re-exported by ashlar-rs) and dev `ashlar-debug`.
  **shell**: view.rs now calls `grid::show_tab(ui, cx, tab, &state)` and, when a run's outcome
  lands, `grid::open_rows(session, tab, human, rows, sql, dialect)` (the grid streams the rest of
  the cursor in jobs and closes it) or `grid::detach`; re-blessed `results`, `query-running`,
  `query-error` goldens (real rows / skeleton). **edit.commit** (human-only, not done here):
  run `grid::edits::statements(doc.edits(tab), engine)` via `execute_in_tx`, then drop the set.
  Table tabs become editable with `GridState::editable(EditTarget { table, key })` (primary key
  from the schema) when the shell opens them.
