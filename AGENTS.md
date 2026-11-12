# Quarry: rules for agents and developers

Quarry is a desktop database browser built on ashlar (`ashlar-rs` 0.2, imported as `ashlar`). Read
docs/PLAN.md and docs/SECURITY.md before changing anything.

## 0. Never crash
No `unwrap`/`expect`/`panic!`/`unreachable!`/`todo!`/`unimplemented!` outside tests (denied in
`[workspace.lints.clippy]`), no `unsafe`. Everything from a database or a file is hostile: `get()` not
`[i]`, checked math, capped allocations (row caps, cell size caps), bounded recursion. Lock poisoning
is not fatal. Work off the UI thread runs as an ashlar job.

## 1. The agent never writes without the human
This is the product. Any change touching the agent connection, the SQL classifier or
`db.propose_write` needs a test in `crates/guard/tests/` and a line in docs/SECURITY.md. Never add a
path that runs agent SQL on the human's connection without an approved write record.

## 2. Every action is a command
Menus, shortcuts, the palette, the CLI and MCP go through one registry (`cmd!` / `CommandSpec`).
A feature without a command id is not done. Command ids and params are a shared contract
(docs/PLAN.md, "Shared contracts"): change them only additively, and tell the lead.

## 3. Performance is a feature
Idle does no work. Result grids are virtualized and stream from cursors; nothing loads a whole
table. No blocking I/O on the UI thread. Budgets are in docs/PLAN.md.

## 4. Gates
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`,
`cargo fmt --check`. Heavy commands run through memgate: `memgate -- cargo test --workspace`.

## 5. Working in parallel
One git worktree per agent under `.claude/worktrees/<name>`, branch `wt/<name>`, its own
`CARGO_TARGET_DIR=<worktree>/target`. Stay inside your workstream's files (docs/PLAN.md,
"Parallel work"); ask the owner of another area instead of editing it. Never commit on `main`: the
lead merges, in the order in docs/PLAN.md. Keep TEAM.md current (your row only).
