//! The agent scenes (`--scene NAME`, DESIGN.md "Agent panel"). A scene only sets view state: the
//! panel then draws a fixed demo [`model`] (no database, a fixed clock, so goldens are stable)
//! instead of the live proposals. Applied over the control port (`ui.scene`), a scene therefore
//! cannot create, decide, pause or resume anything real.

use std::collections::HashMap;

use ashlar::prelude::*;
use quarry_core::{
    Actor, ChartKind, ChartSpec, ConnColor, ConnectionId, Estimate, PanelId, Proposal, ProposalEvent, ProposalId, TableRef, Timestamp, Workspace,
};
use quarry_guard::WriteKind;

use super::activity::{Activity, Jump};
use super::approval::{self, Preview};
use super::panel::{Model, SCENE_KEY};

/// The scenes' clock: 2024-11-12 08:26:40 UTC.
const T0: u64 = 1_731_400_000_000;

const SQL: &str = "UPDATE orders\nSET status = 'refunded', note = 'refund #1042'\nWHERE id = 42";

/// Set up agent scene `name` (view state only); false when it is not one.
pub fn setup(name: &str, s: &mut Session<Workspace>) -> bool {
    if !(name.starts_with("agent-") || name.starts_with("approval-")) {
        return false;
    }
    s.set_view("agent_panel", &true);
    s.set_view(SCENE_KEY, &name);
    true
}

fn act(seq: u64, secs: u64, tool: &'static str, summary: &str, rows: Option<u64>, ms: u64, refused: Option<&str>, jump: Option<Jump>) -> Activity {
    Activity {
        seq,
        at: Timestamp(T0 + secs * 1_000),
        tool,
        summary: summary.into(),
        rows,
        elapsed_ms: ms,
        refused: refused.map(str::to_string),
        jump,
    }
}

/// The demo the panel draws for scene `name`.
pub(crate) fn model(name: &str) -> Model {
    let mut activity = vec![
        act(1, 0, crate::ids::DB_SCHEMA, "staging orders", Some(1), 12, None, None),
        act(2, 4, crate::ids::DB_QUERY, "SELECT id, customer_id, status, total_cents FROM orders WHERE customer_id = 1042", Some(3), 18, None, None),
        act(3, 9, crate::ids::DB_QUERY, "DELETE FROM orders WHERE id = 42", None, 1, Some("not_read_only: DELETE can change data"), None),
        act(4, 15, crate::ids::VIZ_CHART, "SELECT status, count(*) AS n FROM orders GROUP BY status", Some(6), 41, None, None),
    ];
    let mut proposals = Vec::new();
    let mut cards = HashMap::new();
    if name.starts_with("approval-") {
        let id = ProposalId(1);
        let mut p = Proposal::new(
            id,
            ConnectionId::new("staging"),
            SQL,
            "Customer 1042 asked for a refund of order 42 (ticket #1042).",
            vec![TableRef::new(Some("public"), "orders")],
            Timestamp(T0 + 20_000),
        );
        p.estimate = Some(Estimate { rows: 1, exact: true, tolerance: 0 });
        let at = Timestamp(T0 + 60_000);
        let _ = match name {
            "approval-approved" => p
                .apply(ProposalEvent::Approve, Actor::Human, at)
                .and_then(|()| p.apply(ProposalEvent::Start, Actor::System, at))
                .and_then(|()| p.apply(ProposalEvent::Finish { rows_affected: 1 }, Actor::System, at)),
            "approval-denied" => p.apply(ProposalEvent::Deny, Actor::Human, at),
            "approval-expired" => p.apply(ProposalEvent::Expire, Actor::System, at),
            _ => Ok(()),
        };
        let preview = Preview {
            columns: ["id", "customer_id", "status", "total_cents", "note"].map(String::from).to_vec(),
            rows: vec![["42", "1042", "paid", "4999", "NULL"].map(String::from).to_vec()],
        };
        let undo = (name == "approval-approved").then(|| {
            vec!["UPDATE \"public\".\"orders\" SET \"customer_id\" = 1042, \"status\" = 'paid', \"total_cents\" = 4999, \"note\" = NULL WHERE \"id\" = 42".to_string()]
        });
        cards.insert(id, approval::scene_card(WriteKind::Update, "Staging", ConnColor::Staging, Some(preview), undo));
        activity.push(act(5, 20, crate::ids::DB_PROPOSE_WRITE, SQL, Some(1), 9, None, Some(Jump::Proposal(id))));
        proposals.push(p);
    }
    activity.reverse();
    Model { proposals, cards, activity, now: Timestamp(T0 + 30_000), demo: true }
}

/// Demo rows of the "chart" scene.
pub fn demo_chart_data() -> Option<quarry_viz::ChartData> {
    let cols = [quarry_core::Column::new("status", "text"), quarry_core::Column::new("n", "int8")];
    let rows: Vec<Vec<quarry_core::Value>> =
        [("delivered", 27_310), ("shipped", 18_204), ("paid", 27_118), ("pending", 9_071), ("refunded", 9_133), ("cancelled", 9_164)]
            .iter()
            .map(|(k, n)| vec![quarry_core::Value::text(*k), quarry_core::Value::Int(*n)])
            .collect();
    quarry_viz::ChartData::build(ChartKind::Bar, &cols, &rows, "status", Some("n"), None).ok()
}

/// Pin the demo chart next to the active tab (the "chart" scene).
pub fn pin_demo_chart(s: &mut Session<Workspace>) {
    let Some(data) = demo_chart_data() else { return };
    let tab = s.doc().active;
    let panel = s.edit("Scene", |w| {
        let panel = PanelId(w.next_id());
        w.charts.push(ChartSpec {
            panel,
            kind: ChartKind::Bar,
            connection: ConnectionId::new("fixture"),
            sql: "SELECT status, count(*) AS n FROM orders GROUP BY status".into(),
            x: "status".into(),
            y: Some("n".into()),
            series: None,
            title: "Orders by status".into(),
            by_agent: true,
            tab,
            agg: None,
        });
        panel
    });
    quarry_viz::store::put(panel, quarry_viz::PanelData::Chart(data));
}
