//! The audit log (`audit.jsonl`, append-only, one [`AuditEntry`] per line). Written by the app;
//! every proposal and decision and every write on the human's connection is recorded.

use serde::{Deserialize, Serialize};

use crate::{Actor, ConnectionId, ProposalId, Timestamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// An agent read (`db.query`, `db.sample`, `db.explain`, `viz.*`), whether it ran or was refused.
    AgentRead,
    /// `db.propose_write` stored a proposal.
    Proposed,
    Approved,
    Denied,
    Expired,
    /// An approved proposal ran and committed.
    Executed,
    /// An approved proposal failed or was rolled back (row count over the estimate).
    Failed,
    /// The human committed pending edits or ran a write in the editor.
    HumanWrite,
}

/// One line of the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: Timestamp,
    pub actor: Actor,
    pub kind: AuditKind,
    pub connection: ConnectionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalId>,
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_affected: Option<u64>,
    /// For row edits: statements that undo this write, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_sql: Option<Vec<String>>,
    /// Refusal or failure text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl AuditEntry {
    /// The JSONL line (no trailing newline).
    pub fn to_line(&self) -> String {
        // An entry is plain data (strings, integers): serializing it cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}
