//! Caps and timeouts shared by the guard, the engines and the app (SECURITY.md, layer D; PLAN §2).

/// Rows per page fetched from a cursor for the grid.
pub const PAGE_ROWS: u32 = 1_000;
/// A cell's text or bytes kept in a page; longer values become [`crate::Value::Clipped`] /
/// [`crate::Value::Bytes`] previews (the full value is fetched on demand).
pub const CELL_CAP_BYTES: usize = 64 * 1024;
/// Bytes of a binary cell kept as a preview.
pub const BYTES_PREVIEW: usize = 64;
/// Default and maximum rows an agent query returns (`db.query` `limit`).
pub const AGENT_ROW_CAP_DEFAULT: u32 = 5_000;
pub const AGENT_ROW_CAP_MAX: u32 = 5_000;
/// Default and maximum agent statement timeout (`db.query` `timeout_ms`).
pub const AGENT_TIMEOUT_MS_DEFAULT: u32 = 15_000;
pub const AGENT_TIMEOUT_MS_MAX: u32 = 30_000;
/// Maximum rows of `db.sample`.
pub const SAMPLE_MAX: u32 = 100;
/// A proposal not decided within this time expires (SECURITY.md, layer C).
pub const PROPOSAL_TTL_MS: u64 = 10 * 60 * 1_000;
/// At most this many proposals wait for a decision at once; more are refused (approval fatigue).
pub const MAX_PENDING_PROPOSALS: usize = 20;
/// Proposals kept in a [`crate::ProposalBook`] (the oldest decided ones go first).
pub const MAX_KEPT_PROPOSALS: usize = 1_000;
/// Rows shown before/after on an approval card.
pub const APPROVAL_PREVIEW_ROWS: u32 = 20;
/// Bytes of one agent tool result (JSON); bigger results are truncated and say so.
pub const AGENT_RESULT_BYTES_MAX: usize = 4 * 1024 * 1024;
/// SQL text accepted from an agent.
pub const AGENT_SQL_BYTES_MAX: usize = 256 * 1024;
