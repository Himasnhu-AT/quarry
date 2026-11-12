#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Contract 3: one golden JSON per agent tool in `tests/mcp/<id>.json`, holding the tool's MCP
//! name, kind, input schema (generated from the command params) and an example result (built from
//! `quarry_core::tool`). The test fails on any drift; `QUARRY_BLESS=1 cargo test -p quarry --test
//! mcp_shapes` rewrites the files after an intended, additive change.
//!
//! Also: over MCP, the tool list has every agent tool and no human-only action, and calling one
//! by name fails.

use std::path::PathBuf;
use std::sync::Arc;

use ashlar::command::Dispatch;
use ashlar::mcp::{Backend, McpServer, ServerOptions, ToolMode};
use quarry::ids::{self, AGENT_TOOLS, HUMAN_ONLY, ToolKind};
use quarry_core::tool::*;
use quarry_core::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value as J, json};

fn conn() -> ConnectionId {
    ConnectionId::new("staging")
}

fn columns() -> Vec<Column> {
    vec![
        Column::new("id", "int8"),
        Column::new("email", "text"),
        Column { sensitive: true, ..Column::new("password_hash", "text") },
        Column::new("avatar", "bytea"),
    ]
}

fn page() -> Page {
    Page {
        first_row: 0,
        rows: vec![
            vec![
                Value::Int(1),
                Value::text("ada@example.com"),
                Value::Redacted { len: 60 },
                Value::Bytes { len: 2048, preview: Arc::from(&[0x89u8, 0x50][..]) },
            ],
            vec![Value::Int(2), Value::Null, Value::Redacted { len: 60 }, Value::Null],
        ],
    }
}

fn proposal() -> Proposal {
    let mut p = Proposal::new(
        ProposalId(7),
        conn(),
        "UPDATE orders SET status = 'refunded' WHERE id = 42",
        "customer asked for a refund",
        vec![TableRef::new(Some("public"), "orders")],
        Timestamp(1_731_400_000_000),
    );
    p.estimate = Some(Estimate { rows: 1, exact: true, tolerance: 0 });
    p
}

/// The example result of each tool, as its contract type.
fn example(id: &str) -> J {
    fn v<T: Serialize>(t: T) -> J {
        serde_json::to_value(t).unwrap()
    }
    match id {
        ids::DB_CONNECTIONS => v(ConnectionsResult {
            connections: vec![
                ConnectionSummary {
                    id: conn(),
                    name: "Staging".into(),
                    engine: EngineKind::Postgres,
                    color: ConnColor::Staging,
                    agent_access: "read_only".into(),
                },
                ConnectionSummary {
                    id: ConnectionId::new("app-db"),
                    name: "app.sqlite".into(),
                    engine: EngineKind::Sqlite,
                    color: ConnColor::Local,
                    agent_access: "read_only".into(),
                },
            ],
        }),
        ids::DB_SCHEMA => v(SchemaResult {
            connection: conn(),
            namespaces: vec![Namespace {
                name: "public".into(),
                tables: vec![Table {
                    name: "orders".into(),
                    kind: TableKind::Table,
                    row_estimate: Some(10_000_000),
                    columns: vec![
                        ColumnInfo { name: "id".into(), type_name: "int8".into(), nullable: false, default: None, primary_key: true },
                        ColumnInfo { name: "customer_id".into(), type_name: "int8".into(), nullable: false, default: None, primary_key: false },
                        ColumnInfo {
                            name: "status".into(),
                            type_name: "text".into(),
                            nullable: false,
                            default: Some("'pending'::text".into()),
                            primary_key: false,
                        },
                    ],
                    indexes: vec![
                        Index { name: "orders_pkey".into(), columns: vec!["id".into()], unique: true, primary: true },
                        Index { name: "orders_customer".into(), columns: vec!["customer_id".into()], unique: false, primary: false },
                    ],
                    foreign_keys: vec![ForeignKey {
                        name: Some("orders_customer_id_fkey".into()),
                        columns: vec!["customer_id".into()],
                        references: TableRef::new(Some("public"), "customers"),
                        ref_columns: vec!["id".into()],
                    }],
                }],
            }],
            truncated: false,
        }),
        ids::DB_QUERY | ids::DB_SAMPLE => v(RowsResult::from_page(conn(), columns(), &page(), id == ids::DB_QUERY, 42)),
        ids::DB_EXPLAIN => v(ExplainResult {
            connection: conn(),
            analyzed: false,
            plan: PlanNode {
                op: "Hash Join".into(),
                total_cost: Some(1234.5),
                est_rows: Some(1000.0),
                children: vec![PlanNode {
                    op: "Seq Scan".into(),
                    relation: Some("orders".into()),
                    total_cost: Some(800.0),
                    est_rows: Some(10_000_000.0),
                    ..PlanNode::default()
                }],
                ..PlanNode::default()
            },
            elapsed_ms: 3,
        }),
        ids::DB_PROPOSE_WRITE => {
            let p = proposal();
            v(ProposeResult {
                proposal_id: p.id,
                status: p.status,
                expires_at: p.expires_at,
                note: "A person must approve this in the Quarry window. Poll db.proposal_status.".into(),
            })
        }
        ids::DB_PROPOSAL_STATUS => {
            let mut p = proposal();
            p.apply(ProposalEvent::Approve, Actor::Human, Timestamp(1_731_400_030_000)).unwrap();
            p.apply(ProposalEvent::Start, Actor::System, Timestamp(1_731_400_030_001)).unwrap();
            p.apply(ProposalEvent::Finish { rows_affected: 1 }, Actor::System, Timestamp(1_731_400_030_050)).unwrap();
            v(ProposalStatusResult::from(&p))
        }
        ids::VIEW_OPEN_TABLE | ids::VIEW_RUN_IN_EDITOR => v(TabResult { tab: TabId(3) }),
        ids::VIEW_FILTER => v(FilterResult { tab: TabId(3), where_clause: "status = 'refunded'".into() }),
        ids::VIEW_SELECT => v(SelectResult { tab: TabId(3), rows: 5, columns: 2 }),
        ids::VIEW_FOCUS => v(FocusResult { region: Region::Grid }),
        ids::VIZ_CHART => v(ChartResult { panel: PanelId(4), kind: ChartKind::Bar, row_count: 6, truncated: false }),
        ids::VIZ_PIVOT => v(PivotResult { panel: PanelId(5), row_count: 12, column_count: 4, truncated: false }),
        ids::VIZ_CLOSE => v(CloseResult { closed: PanelId(4) }),
        other => panic!("no example for {other}: add one (and a golden) for every agent tool"),
    }
}

/// The result parses back into its contract type unchanged (no field the type does not know).
fn round_trips(id: &str, result: &J) {
    fn check<T: Serialize + DeserializeOwned>(id: &str, result: &J) {
        let t: T = serde_json::from_value(result.clone()).unwrap_or_else(|e| panic!("{id}: golden result does not parse: {e}"));
        assert_eq!(&serde_json::to_value(t).unwrap(), result, "{id}: the golden has fields the type drops");
    }
    match id {
        ids::DB_CONNECTIONS => check::<ConnectionsResult>(id, result),
        ids::DB_SCHEMA => check::<SchemaResult>(id, result),
        ids::DB_QUERY | ids::DB_SAMPLE => check::<RowsResult>(id, result),
        ids::DB_EXPLAIN => check::<ExplainResult>(id, result),
        ids::DB_PROPOSE_WRITE => check::<ProposeResult>(id, result),
        ids::DB_PROPOSAL_STATUS => check::<ProposalStatusResult>(id, result),
        ids::VIEW_OPEN_TABLE | ids::VIEW_RUN_IN_EDITOR => check::<TabResult>(id, result),
        ids::VIEW_FILTER => check::<FilterResult>(id, result),
        ids::VIEW_SELECT => check::<SelectResult>(id, result),
        ids::VIEW_FOCUS => check::<FocusResult>(id, result),
        ids::VIZ_CHART => check::<ChartResult>(id, result),
        ids::VIZ_PIVOT => check::<PivotResult>(id, result),
        ids::VIZ_CLOSE => check::<CloseResult>(id, result),
        other => panic!("no type for {other}"),
    }
}

fn kind(k: ToolKind) -> &'static str {
    match k {
        ToolKind::ReadOnly => "read_only",
        ToolKind::Approval => "approval",
        ToolKind::Ui => "ui",
    }
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("mcp")
}

#[test]
fn every_agent_tool_matches_its_golden() {
    let bless = std::env::var_os("QUARRY_BLESS").is_some();
    let h = quarry::app().headless();
    let infos = h.commands();
    let mut drift = Vec::new();
    for t in AGENT_TOOLS {
        let info = infos.iter().find(|c| c.id == t.id).unwrap();
        let want = json!({
            "tool": t.id,
            "mcp_name": ashlar::mcp::tools::tool_name(t.id),
            "kind": kind(t.kind),
            "mutates": info.mutates,
            "input_schema": info.params_schema,
            "result": example(t.id),
        });
        let path = golden_dir().join(format!("{}.json", t.id));
        let text = format!("{}\n", serde_json::to_string_pretty(&want).unwrap());
        if bless {
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let got = std::fs::read_to_string(&path).unwrap_or_default();
        if got != text {
            drift.push(format!("{} differs from the contract:\n--- golden\n{got}\n--- now\n{text}", path.display()));
        }
        let golden: J = serde_json::from_str(&got).unwrap_or(J::Null);
        round_trips(t.id, &golden["result"]);
    }
    assert!(drift.is_empty(), "{}\n(QUARRY_BLESS=1 to accept an intended, additive change)", drift.join("\n\n"));
    // no stray goldens
    for entry in std::fs::read_dir(golden_dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        let id = name.trim_end_matches(".json");
        assert!(AGENT_TOOLS.iter().any(|t| t.id == id), "golden {name} for a tool that is not an agent tool");
    }
}

fn rpc(server: &mut McpServer, id: u64, method: &str, params: J) -> J {
    server.handle(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap_or(J::Null)
}

#[test]
fn over_mcp_agent_tools_are_listed_and_human_only_actions_are_not() {
    let backend = Backend::Headless(Box::new(quarry::app().headless()));
    let mut server = McpServer::new(backend, ServerOptions::new("quarry", "0").tools(ToolMode::PerCommand));
    rpc(&mut server, 1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}));
    let list = rpc(&mut server, 2, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for t in AGENT_TOOLS {
        let name = ashlar::mcp::tools::tool_name(t.id);
        assert!(names.contains(&name.as_str()), "{name} missing from {names:?}");
    }
    for id in HUMAN_ONLY {
        assert!(!names.contains(&ashlar::mcp::tools::tool_name(id).as_str()), "{id} is an MCP tool");
    }
    let listed = rpc(&mut server, 3, "tools/call", json!({"name": "list_commands", "arguments": {}}));
    let text = listed.to_string();
    for id in HUMAN_ONLY {
        assert!(!text.contains(&format!("\\\"{id}\\\"")), "{id} in list_commands");
    }
    for (n, id) in [ids::PROPOSAL_APPROVE, ids::PROPOSAL_DENY, ids::EDIT_COMMIT, ids::QUERY_RUN].into_iter().enumerate() {
        let r = rpc(&mut server, 10 + n as u64, "tools/call", json!({"name": "execute", "arguments": {"command": id, "params": {"proposal_id": 1}}}));
        assert_eq!(r["result"]["isError"], json!(true), "{id}: {r}");
    }
}
