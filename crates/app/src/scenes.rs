//! Named scenes: one per screen and state in DESIGN.md, for `--scene NAME`, `.ui` scripts and
//! golden screenshots. Streams add scenes additively and make existing ones richer.

use ashlar::prelude::*;
use quarry_core::{ChartKind, ChartSpec, ConnectionId, PanelId, Tab, TabId, TabKind, Workspace};

type S = Session<Workspace>;

/// Every scene name, in the order `--list-scenes` prints them.
pub const SCENES: &[&str] = &[
    "empty",
    "connecting",
    "editor",
    "query-running",
    "results",
    "query-error",
    "disconnected",
    "connection-dialog",
    "agent-panel",
    "agent-paused",
    "approval-pending",
    "approval-approved",
    "approval-denied",
    "approval-expired",
    "chart",
];

fn editor_tab(s: &mut S, sql: &str) -> TabId {
    let sql = sql.to_string();
    s.edit("Scene", |w| {
        let id = TabId(w.next_id());
        w.tabs.push(Tab { id, title: "Query 1".into(), connection: Some(ConnectionId::new("fixture")), kind: TabKind::Editor { sql } });
        w.active = Some(id);
        id
    })
}

/// Set up scene `name` on a fresh session.
pub fn setup(name: &str, s: &mut S) {
    s.set_view("screen", &name);
    let q = "SELECT id, customer_id, total_cents, status FROM orders ORDER BY id";
    match name {
        "editor" => {
            editor_tab(s, q);
        }
        "query-running" | "results" | "query-error" => {
            editor_tab(s, if name == "query-error" { "SELECT * FROM orderz" } else { q });
            let state = match name {
                "query-running" => "running",
                "query-error" => "error",
                _ => "results",
            };
            s.set_view("query_state", &state);
        }
        "connection-dialog" => s.set_view("dialog", &"connection"),
        "agent-panel" | "agent-paused" => {
            s.set_view("agent_panel", &true);
            if name == "agent-paused" {
                s.set_view("agent_state", &"paused");
            }
        }
        _ if name.starts_with("approval-") => {
            s.set_view("agent_panel", &true);
            s.set_view("proposal", &name.trim_start_matches("approval-"));
        }
        "chart" => {
            let tab = editor_tab(s, "SELECT status, count(*) FROM orders GROUP BY status");
            s.edit("Scene", |w| {
                let panel = PanelId(w.next_id());
                w.charts.push(ChartSpec {
                    panel,
                    kind: ChartKind::Bar,
                    connection: ConnectionId::new("fixture"),
                    sql: "SELECT status, count(*) FROM orders GROUP BY status".into(),
                    x: "status".into(),
                    y: Some("count".into()),
                    series: None,
                    title: "Orders by status".into(),
                    by_agent: true,
                    tab: Some(tab),
                });
            });
        }
        // "empty", "connecting", "disconnected": the screen marker only, for now
        _ => {}
    }
}
