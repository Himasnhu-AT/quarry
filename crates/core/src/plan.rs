//! An EXPLAIN plan as a tree, engine-neutral (PostgreSQL `EXPLAIN (FORMAT JSON)`, SQLite
//! `EXPLAIN QUERY PLAN`).

use serde::{Deserialize, Serialize};

/// One plan node. Costs are the engine's units; `actual_*` only with ANALYZE.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanNode {
    /// `Seq Scan`, `Index Scan`, `Hash Join`, SQLite's detail text…
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_rows: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_ms: Option<f64>,
    /// Engine-specific extras (filter, index name…), short strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanNode>,
}
