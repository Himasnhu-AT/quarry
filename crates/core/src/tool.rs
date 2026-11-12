//! What each agent tool returns (contract 3). Every agent command serializes one of these as its
//! result; `crates/app/tests/mcp/*.json` holds a golden example of each, checked by
//! `crates/app/tests/mcp_shapes.rs`. Change only additively.
//!
//! Failures are not results: a refused or failed call is `CommandError::Failed` with a
//! [`crate::Refusal`] message (`"not_read_only: …"`), which MCP reports as `isError: true`.

use serde::{Deserialize, Serialize};

use crate::{
    ChartKind, Column, ConnectionId, ConnectionSummary, Estimate, Namespace, Page, PanelId, PlanNode, Proposal, ProposalId, ProposalOutcome,
    ProposalStatus, Region, TabId, TableRef, Timestamp,
};

/// `db.connections`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectionsResult {
    pub connections: Vec<ConnectionSummary>,
}

/// `db.schema`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaResult {
    pub connection: ConnectionId,
    pub namespaces: Vec<Namespace>,
    /// More tables exist than were returned (cap or filter).
    pub truncated: bool,
}

/// `db.query` and `db.sample`: rows as agent JSON (see `Value::to_agent_json`), sensitive columns
/// redacted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowsResult {
    pub connection: ConnectionId,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: u64,
    /// The row cap or the result size cap cut the rows short.
    pub truncated: bool,
    pub elapsed_ms: u64,
}

impl RowsResult {
    /// Build from a page already redacted by the guard.
    pub fn from_page(connection: ConnectionId, columns: Vec<Column>, page: &Page, truncated: bool, elapsed_ms: u64) -> Self {
        let rows: Vec<Vec<serde_json::Value>> = page.rows.iter().map(|r| r.iter().map(crate::Value::to_agent_json).collect()).collect();
        Self { connection, columns, row_count: rows.len() as u64, rows, truncated, elapsed_ms }
    }
}

/// `db.explain`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExplainResult {
    pub connection: ConnectionId,
    /// ANALYZE ran (the statement executed, read-only).
    pub analyzed: bool,
    pub plan: PlanNode,
    pub elapsed_ms: u64,
}

/// `db.propose_write`: stored, not run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposeResult {
    pub proposal_id: ProposalId,
    /// Always `pending`.
    pub status: ProposalStatus,
    pub expires_at: Timestamp,
    /// What happens next, for the agent: a person decides in the window; poll
    /// `db.proposal_status`.
    pub note: String,
}

/// `db.proposal_status`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposalStatusResult {
    pub proposal_id: ProposalId,
    pub status: ProposalStatus,
    pub connection: ConnectionId,
    pub sql: String,
    pub reason: String,
    pub tables: Vec<TableRef>,
    pub estimate: Option<Estimate>,
    pub requested_at: Timestamp,
    pub expires_at: Timestamp,
    pub decided_at: Option<Timestamp>,
    pub result: Option<ProposalOutcome>,
}

impl From<&Proposal> for ProposalStatusResult {
    fn from(p: &Proposal) -> Self {
        Self {
            proposal_id: p.id,
            status: p.status,
            connection: p.connection.clone(),
            sql: p.sql.clone(),
            reason: p.reason.clone(),
            tables: p.tables.clone(),
            estimate: p.estimate,
            requested_at: p.requested_at,
            expires_at: p.expires_at,
            decided_at: p.decided_at,
            result: p.result.clone(),
        }
    }
}

/// `view.open_table`, `view.run_in_editor`: the tab opened and selected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabResult {
    pub tab: TabId,
}

/// `view.filter`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilterResult {
    pub tab: TabId,
    /// The filter now applied (it passed the guard).
    #[serde(rename = "where")]
    pub where_clause: String,
}

/// `view.select`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectResult {
    pub tab: TabId,
    pub rows: u64,
    pub columns: u64,
}

/// `view.focus`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusResult {
    pub region: Region,
}

/// `viz.chart`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartResult {
    pub panel: PanelId,
    pub kind: ChartKind,
    pub row_count: u64,
    pub truncated: bool,
}

/// `viz.pivot`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PivotResult {
    pub panel: PanelId,
    pub row_count: u64,
    pub column_count: u64,
    pub truncated: bool,
}

/// `viz.close`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloseResult {
    pub closed: PanelId,
}
