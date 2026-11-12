# Quarry

A fast desktop database browser for **PostgreSQL** and **SQLite** that an AI agent can use safely. It
can read your schema, run read-only queries, drive the UI and draw charts, but it **can never write**
unless you approve the exact statement first.

Built in Rust on [ashlar](https://github.com/Himasnhu-AT/ashlar) (egui). One window, millions of rows,
zero CPU when idle.

> **Status:** planning. Development starts after ashlar 0.2 is released.

## Why

- Database GUIs are either heavy (Electron, JVM) or dated. Quarry aims at native speed: open a
  10M-row table and scroll it like a short list.
- AI agents are useful with data, but letting one hold a connection with write rights is reckless.
  Quarry gives the agent its own read-only connection, checks every statement, and routes any change
  through an approval card you see in the window.
- Every action is a command, so the same features work from the menu, the palette, the CLI and MCP.

## What it will do (MVP)

- Connections to PostgreSQL (TLS, SSH tunnel) and SQLite files; passwords in the OS keychain
- Schema tree, SQL editor with highlighting and completion, query history
- Virtualized result grid over streaming cursors
- Table editing with a pending-changes diff before commit
- EXPLAIN plan view
- MCP server: `db.schema`, `db.query` (read only), `db.explain`, `db.propose_write`, `ui.*`, `viz.chart`

## Documents

- [docs/PLAN.md](docs/PLAN.md): scope, architecture, MCP surface, benchmarks, milestones, parallel work
- [docs/SECURITY.md](docs/SECURITY.md): how the agent is kept read-only
- [docs/DESIGN.md](docs/DESIGN.md): screens, interactions, accessibility

## License

To be decided before the first release (the ashlar crates are BSD-3-Clause).
