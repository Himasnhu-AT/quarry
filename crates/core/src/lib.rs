//! Quarry's shared data model (frozen contract 2, see `docs/contracts.md`).
//!
//! Rank 0: no I/O, no clocks, no threads. Every other crate speaks these types. Change them only
//! additively (new optional fields with `#[serde(default)]`, new enum variants at the end) and tell
//! every consumer and the lead.
//!
//! - [`connection`]: what a saved connection is ([`ConnectionSpec`]) and what an agent may see of it
//!   ([`ConnectionSummary`]).
//! - [`value`]: one cell ([`Value`]) and its agent-facing JSON form.
//! - [`result`]: columns, pages and result sets streamed from cursors.
//! - [`schema`]: the schema tree.
//! - [`plan`]: an EXPLAIN plan tree.
//! - [`proposal`]: the write-approval contract (contract 4): [`Proposal`], its state machine and
//!   [`ProposalBook`].
//! - [`audit`]: the append-only audit log entry.
//! - [`tool`]: what each agent tool returns (contract 3).
//! - [`workspace`]: the ashlar document (open tabs, editor texts, pinned charts, pending edits).
//! - [`edits`]: pending table edits (the grid's change set before a human commit).
//! - [`refusal`]: why the guard or a limit turned a call down.
//! - [`limits`]: caps and timeouts shared by guard, engine and app.

pub mod audit;
pub mod connection;
pub mod edits;
pub mod ids;
pub mod limits;
pub mod plan;
pub mod proposal;
pub mod refusal;
pub mod result;
pub mod schema;
pub mod time;
pub mod tool;
pub mod value;
pub mod workspace;

pub use audit::{AuditEntry, AuditKind};
pub use connection::{AgentAccess, ConnColor, ConnectionSpec, ConnectionSummary, EngineKind, Secret, SshAuth, SshSpec, Target, TlsMode};
pub use edits::{PendingEdits, RowChange};
pub use ids::{ConnectionId, CursorId, PanelId, ProposalId, TabId};
pub use plan::PlanNode;
pub use proposal::{Actor, Estimate, Proposal, ProposalBook, ProposalEvent, ProposalOutcome, ProposalStatus, TransitionError, transition};
pub use refusal::{Refusal, RefusalCode};
pub use result::{Column, Page, ResultSet, Total};
pub use schema::{ColumnInfo, ForeignKey, Index, Namespace, SchemaTree, Table, TableKind, TableRef};
pub use time::Timestamp;
pub use value::Value;
pub use workspace::{ChartKind, ChartSpec, Region, Tab, TabKind, Workspace};
