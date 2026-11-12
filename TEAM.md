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
| | grid | wt/grid | `crates/app/src/grid/**` | | |
| | agent | wt/agent | `crates/app/src/agent/**`, `crates/viz/**` | | |
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
