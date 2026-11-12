# Team

Who works on what. Each agent edits **its own row only** and appends to the log; the lead merges
in the order in docs/PLAN.md §9 (contracts → engine → guard → shell → grid → agent → bench).
Contracts are frozen (docs/contracts.md): change them only additively and tell every consumer and
the lead.

## Roster

| Agent | Stream | Branch | Owns | Now | Done |
|---|---|---|---|---|---|
| | engine | wt/engine | `crates/engine/**` | | |
| claude | guard | wt/guard | `crates/guard/**`, `docs/SECURITY.md` | review / merge | classifier, proposal drafts, row cap, filters, sensitive-column rule; threat, corpus, policy, fuzz tests |
| | shell | wt/shell | `crates/app/src/{main.rs,view.rs,theme.rs,sidebar,editor,dialogs}/**` | | |
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
