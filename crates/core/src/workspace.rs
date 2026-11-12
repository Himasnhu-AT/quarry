//! The ashlar document: what the window has open. Undo, autosave and crash recovery cover it.
//! Connections are in settings and secrets in the keychain, never here; proposals are runtime
//! state of the app (so undo can never touch a decision).

use serde::{Deserialize, Serialize};

use crate::{ConnectionId, PanelId, TabId, TableRef};

/// The document format id.
pub const FORMAT_ID: &str = "quarry.workspace";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active: Option<TabId>,
    /// The last id handed out to a tab or panel.
    #[serde(default)]
    pub last_id: u64,
    /// Chart and pivot panels.
    #[serde(default)]
    pub charts: Vec<ChartSpec>,
}

impl Workspace {
    /// A fresh id for a tab or panel.
    pub fn next_id(&mut self) -> u64 {
        self.last_id = self.last_id.saturating_add(1);
        self.last_id
    }
    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }
    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
}

/// One tab in the center area.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
    #[serde(default)]
    pub connection: Option<ConnectionId>,
    pub kind: TabKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TabKind {
    /// A SQL editor with its result grid below.
    Editor { sql: String },
    /// A table browsed directly, optionally filtered (the filter passed the guard).
    Table {
        table: TableRef,
        #[serde(default)]
        filter: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Scatter,
    Histogram,
    /// A pivot table panel (`viz.pivot`; additive, agent stream). Not a `viz.chart` kind.
    Pivot,
}

impl ChartKind {
    /// The `viz.chart` kinds (`Pivot` is made by `viz.pivot`).
    pub const NAMES: &'static [&'static str] = &["bar", "line", "area", "scatter", "histogram"];

    /// The kind named `name` (one of [`ChartKind::NAMES`], or `pivot`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "bar" => ChartKind::Bar,
            "line" => ChartKind::Line,
            "area" => ChartKind::Area,
            "scatter" => ChartKind::Scatter,
            "histogram" => ChartKind::Histogram,
            "pivot" => ChartKind::Pivot,
            _ => return None,
        })
    }
}

/// A chart panel (`viz.chart` or the human), or a pivot table (`viz.pivot`: `x` is the rows
/// column, `series` the columns column, `y` the value column, `agg` the aggregate).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartSpec {
    pub panel: PanelId,
    pub kind: ChartKind,
    pub connection: ConnectionId,
    pub sql: String,
    pub x: String,
    #[serde(default)]
    pub y: Option<String>,
    #[serde(default)]
    pub series: Option<String>,
    pub title: String,
    /// Made by the agent ("made by agent" chip).
    #[serde(default)]
    pub by_agent: bool,
    /// The tab it sits next to.
    #[serde(default)]
    pub tab: Option<TabId>,
    /// Pivot only: `count`, `sum`, `avg`, `min` or `max` (additive, agent stream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agg: Option<String>,
}

/// A focusable region of the window (`view.focus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    Sidebar,
    Editor,
    Grid,
    Agent,
}

impl Region {
    pub const NAMES: &'static [&'static str] = &["sidebar", "editor", "grid", "agent"];
}
