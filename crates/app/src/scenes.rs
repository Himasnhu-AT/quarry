//! Named scenes: one per screen and state in DESIGN.md, for `--scene NAME`, `.ui` scripts and
//! golden screenshots. Streams add scenes additively and make existing ones richer.

use ashlar::prelude::*;
use quarry_core::{ConnectionId, Tab, TabId, TabKind, Workspace};

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
    "history",
    "open-table",
    "sidebar-menu",
    // grid stream (crate::grid::scenes::SCENES)
    "grid-loading",
    "grid-values",
    "grid-edits",
    "grid-wide",
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
///
/// Shell view keys (read once when the scene starts): `scene.connections` (`connected`,
/// `connecting`, `failed`: the demo connections of `crate::sidebar::scene_connections`),
/// `scene.expand` (tree keys to open), `scene.select`, `scene.menu` (a tree key whose context
/// menu is open), `scene.dialog` (`history`, `open_table`).
pub fn setup(name: &str, s: &mut S) {
    shell(name, s);
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
            crate::agent::scenes::setup(name, s);
            if name == "agent-paused" {
                s.set_view("agent_state", &"paused");
            }
        }
        _ if name.starts_with("approval-") => {
            crate::agent::scenes::setup(name, s);
        }
        "chart" => {
            editor_tab(s, "SELECT status, count(*) AS n FROM orders GROUP BY status");
            crate::agent::scenes::pin_demo_chart(s);
        }
        // "empty", "connecting", "disconnected": the screen marker only, for now
        _ => {}
    }
    crate::grid::scenes::setup(name, s);
}

/// The shell's part of a scene: demo connections, the open tree nodes and dialogs.
fn shell(name: &str, s: &mut S) {
    // a scene applied to a running window starts from a clean view state
    for key in ["scene.connections", "scene.select", "scene.menu", "scene.dialog", "proposal", crate::agent::SCENE_KEY] {
        s.set_view_value(key, Value::Null);
    }
    s.set_view("scene.expand", &Vec::<String>::new());
    s.set_view("query_state", &"");
    s.set_view("dialog", &"");
    s.set_view("agent_state", &"");
    s.set_view("agent_panel", &false);
    let status = match name {
        "empty" => return,
        "connecting" => "connecting",
        "disconnected" => "failed",
        _ => "connected",
    };
    s.set_view("scene.connections", &status);
    let mut expand = vec!["conn.fixture"];
    if matches!(name, "editor" | "query-running" | "results" | "query-error" | "sidebar-menu") {
        expand.push("table.fixture.main.orders");
    }
    s.set_view("scene.expand", &expand);
    match name {
        "editor" | "query-running" | "results" | "query-error" => s.set_view("scene.select", &"table.fixture.main.orders"),
        "sidebar-menu" => s.set_view("scene.menu", &"table.fixture.main.orders"),
        "history" => {
            editor_tab(s, "SELECT status, count(*) FROM orders GROUP BY status;");
            s.set_view("scene.dialog", &"history");
        }
        "open-table" => s.set_view("scene.dialog", &"open_table"),
        _ => {}
    }
}
