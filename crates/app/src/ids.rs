//! Command ids and parameters (contract 1, see `docs/contracts.md`). Frozen in M0: change only
//! additively (new commands, new optional params) and tell every consumer and the lead.
//!
//! Three audiences:
//!
//! - **Agent tools** ([`AGENT_TOOLS`]): registered commands an agent calls over MCP, the CLI or
//!   the control port, and people through the palette. Read-only ones are `query!` (never
//!   journaled); the rest change only the workspace document or create a proposal.
//! - **Human commands** ([`HUMAN_COMMANDS`]): registered commands for menus, shortcuts and the
//!   palette. ashlar 0.2 cannot hide a registered command from MCP, the CLI or the control port,
//!   so every one of these is harmless when an agent calls it: none writes to a database,
//!   decides a proposal or changes agent access.
//! - **Human-only actions** ([`HUMAN_ONLY`]): approve / deny a proposal, run SQL on the human's
//!   connection, commit edits, change a connection's agent access, pause / resume the agent.
//!   They are **not registered** (so they are absent from every agent surface) and run only from
//!   the window through [`crate::human::perform`], which needs a [`crate::human::HumanTicket`]
//!   minted from a real click or key press. See `docs/contracts.md`, "Human-only actions".
//!
//! Handlers are M0 stubs failing with `not_yet: not yet` unless noted; streams replace them
//! without changing ids or params. The shell's (`conn.*`, `tab.*`, `table.open`, `query.cancel`,
//! `query.format`, `history.show`) live in `crate::sidebar::commands` and `crate::editor::commands`.

use ashlar::prelude::*;
use quarry_core::limits::{AGENT_ROW_CAP_MAX, AGENT_TIMEOUT_MS_MAX, SAMPLE_MAX};
use quarry_core::{ChartKind, Refusal, RefusalCode, Region, Tab, TabId, TabKind, Workspace};

type S = Session<Workspace>;
type R = Result<Value, CommandError>;

// ---------------------------------------------------------------------------------------------
// Agent tools (PLAN §3)
// ---------------------------------------------------------------------------------------------

pub const DB_CONNECTIONS: &str = "db.connections";
pub const DB_SCHEMA: &str = "db.schema";
pub const DB_QUERY: &str = "db.query";
pub const DB_EXPLAIN: &str = "db.explain";
pub const DB_SAMPLE: &str = "db.sample";
pub const DB_PROPOSE_WRITE: &str = "db.propose_write";
pub const DB_PROPOSAL_STATUS: &str = "db.proposal_status";
pub const VIEW_OPEN_TABLE: &str = "view.open_table";
pub const VIEW_RUN_IN_EDITOR: &str = "view.run_in_editor";
pub const VIEW_FILTER: &str = "view.filter";
pub const VIEW_SELECT: &str = "view.select";
pub const VIEW_FOCUS: &str = "view.focus";
pub const VIZ_CHART: &str = "viz.chart";
pub const VIZ_PIVOT: &str = "viz.pivot";
pub const VIZ_CLOSE: &str = "viz.close";

/// What an agent tool may do (PLAN §3 "Kind").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads only; runs without asking.
    ReadOnly,
    /// Only creates a proposal the human approves in the window.
    Approval,
    /// Changes what the window shows (tabs, selection, focus, panels), never the database.
    Ui,
}

/// One agent tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentTool {
    pub id: &'static str,
    pub kind: ToolKind,
}

pub const AGENT_TOOLS: &[AgentTool] = &[
    AgentTool { id: DB_CONNECTIONS, kind: ToolKind::ReadOnly },
    AgentTool { id: DB_SCHEMA, kind: ToolKind::ReadOnly },
    AgentTool { id: DB_QUERY, kind: ToolKind::ReadOnly },
    AgentTool { id: DB_EXPLAIN, kind: ToolKind::ReadOnly },
    AgentTool { id: DB_SAMPLE, kind: ToolKind::ReadOnly },
    AgentTool { id: DB_PROPOSE_WRITE, kind: ToolKind::Approval },
    AgentTool { id: DB_PROPOSAL_STATUS, kind: ToolKind::ReadOnly },
    AgentTool { id: VIEW_OPEN_TABLE, kind: ToolKind::Ui },
    AgentTool { id: VIEW_RUN_IN_EDITOR, kind: ToolKind::Ui },
    AgentTool { id: VIEW_FILTER, kind: ToolKind::Ui },
    AgentTool { id: VIEW_SELECT, kind: ToolKind::Ui },
    AgentTool { id: VIEW_FOCUS, kind: ToolKind::Ui },
    AgentTool { id: VIZ_CHART, kind: ToolKind::ReadOnly },
    AgentTool { id: VIZ_PIVOT, kind: ToolKind::ReadOnly },
    AgentTool { id: VIZ_CLOSE, kind: ToolKind::Ui },
];

// ---------------------------------------------------------------------------------------------
// Human commands (registered; harmless from any surface)
// ---------------------------------------------------------------------------------------------

pub const PALETTE_OPEN: &str = "palette.open";
pub const CONN_NEW: &str = "conn.new";
pub const CONN_EDIT: &str = "conn.edit";
pub const CONN_OPEN: &str = "conn.open";
pub const CONN_CLOSE: &str = "conn.close";
pub const CONN_TEST: &str = "conn.test";
pub const TAB_NEW_EDITOR: &str = "tab.new_editor";
pub const TAB_CLOSE: &str = "tab.close";
pub const TAB_SELECT: &str = "tab.select";
pub const TABLE_OPEN: &str = "table.open";
pub const QUERY_CANCEL: &str = "query.cancel";
pub const QUERY_EXPLAIN: &str = "query.explain";
pub const QUERY_FORMAT: &str = "query.format";
pub const HISTORY_SHOW: &str = "history.show";
pub const GRID_FILTER: &str = "grid.filter";
pub const GRID_SORT: &str = "grid.sort";
pub const GRID_COPY: &str = "grid.copy";
pub const EDIT_CELL: &str = "edit.cell";
pub const EDIT_ADD_ROW: &str = "edit.add_row";
pub const EDIT_DELETE_ROWS: &str = "edit.delete_rows";
pub const EDIT_DISCARD: &str = "edit.discard";
pub const EDIT_REVIEW: &str = "edit.review";
pub const AGENT_TOGGLE_PANEL: &str = "agent.toggle_panel";
pub const PROPOSAL_FOCUS: &str = "proposal.focus";

pub const HUMAN_COMMANDS: &[&str] = &[
    PALETTE_OPEN,
    CONN_NEW,
    CONN_EDIT,
    CONN_OPEN,
    CONN_CLOSE,
    CONN_TEST,
    TAB_NEW_EDITOR,
    TAB_CLOSE,
    TAB_SELECT,
    TABLE_OPEN,
    QUERY_CANCEL,
    QUERY_EXPLAIN,
    QUERY_FORMAT,
    HISTORY_SHOW,
    GRID_FILTER,
    GRID_SORT,
    GRID_COPY,
    EDIT_CELL,
    EDIT_ADD_ROW,
    EDIT_DELETE_ROWS,
    EDIT_DISCARD,
    EDIT_REVIEW,
    AGENT_TOGGLE_PANEL,
    PROPOSAL_FOCUS,
];

// ---------------------------------------------------------------------------------------------
// Human-only actions (NOT registered: window only, see crate::human)
// ---------------------------------------------------------------------------------------------

pub const PROPOSAL_APPROVE: &str = "proposal.approve";
pub const PROPOSAL_DENY: &str = "proposal.deny";
pub const QUERY_RUN: &str = "query.run";
pub const QUERY_RUN_ALL: &str = "query.run_all";
pub const QUERY_EXPLAIN_ANALYZE: &str = "query.explain_analyze";
pub const EDIT_COMMIT: &str = "edit.commit";
pub const CONN_SAVE: &str = "conn.save";
pub const CONN_DELETE: &str = "conn.delete";
pub const AGENT_PAUSE: &str = "agent.pause";
pub const AGENT_RESUME: &str = "agent.resume";
pub const COLUMN_MARK_SENSITIVE: &str = "column.mark_sensitive";

/// Never in the registry, so never in `list_commands`, MCP `tools/list`, `quarry-cli commands`
/// or the native menu. A test keeps it that way.
pub const HUMAN_ONLY: &[&str] = &[
    PROPOSAL_APPROVE,
    PROPOSAL_DENY,
    QUERY_RUN,
    QUERY_RUN_ALL,
    QUERY_EXPLAIN_ANALYZE,
    EDIT_COMMIT,
    CONN_SAVE,
    CONN_DELETE,
    AGENT_PAUSE,
    AGENT_RESUME,
    COLUMN_MARK_SENSITIVE,
];

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

const CONNECTION: Param = Param::string("connection").required().about("connection id from db.connections");
const SQL: Param = Param::string("sql").required().about("exactly one SQL statement");
const TAB: Param = Param::int_in("tab", 1, i64::MAX).required().about("tab id");
const PANEL: Param = Param::int_in("panel", 1, i64::MAX).required().about("chart or pivot panel id");

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The M0 stub: `not_yet: not yet` (the chokepoint fills in the command id).
pub fn not_yet(_: &mut S, _: &Value) -> R {
    Err(CommandError::failed("", Refusal::new(RefusalCode::NotYet, "not yet").to_string()))
}

fn palette_open(s: &mut S, _: &Value) -> R {
    s.request(Effect::OpenPalette);
    Ok(Value::Null)
}

fn agent_toggle_panel(s: &mut S, _: &Value) -> R {
    let open = !s.view_or("agent_panel", false);
    s.set_view("agent_panel", &open);
    Ok(json!({"open": open}))
}

fn tab_new_editor(s: &mut S, p: &Value) -> R {
    let connection = p.get("connection").and_then(Value::as_str).map(quarry_core::ConnectionId::new);
    let id = s.edit("New Editor Tab", |w| {
        let id = TabId(w.next_id());
        let title = format!("Query {}", w.tabs.iter().filter(|t| matches!(t.kind, TabKind::Editor { .. })).count() + 1);
        w.tabs.push(Tab { id, title, connection, kind: TabKind::Editor { sql: String::new() } });
        w.active = Some(id);
        id
    });
    Ok(json!({"tab": id}))
}

/// Every registered Quarry command (after ashlar's built-in File / Edit / View ones).
pub fn commands() -> Vec<CommandSpec<S>> {
    vec![
        // ---- agent tools: read only
        query!("db.connections", "List Connections", [], crate::agent::tools::db_connections),
        query!(
            "db.schema",
            "Show Schema",
            [
                CONNECTION,
                Param::string("schema").about("only this schema"),
                Param::string("table").about("only this table, with columns, keys and indexes")
            ],
            crate::agent::tools::db_schema
        ),
        query!(
            "db.query",
            "Run Read-Only Query",
            [
                CONNECTION,
                SQL,
                Param::int_in("limit", 1, AGENT_ROW_CAP_MAX as i64).about("max rows (default 5000)"),
                Param::int_in("timeout_ms", 1, AGENT_TIMEOUT_MS_MAX as i64).about("statement timeout (default 15000)"),
            ],
            crate::agent::tools::db_query
        ),
        query!(
            "db.explain",
            "Explain Query",
            [CONNECTION, SQL, Param::bool("analyze").about("run it to measure (read-only statements only); default false")],
            crate::agent::tools::db_explain
        ),
        query!(
            "db.sample",
            "Sample Rows",
            [
                CONNECTION,
                Param::string("table").required().about("table, optionally schema.table"),
                Param::int_in("n", 1, SAMPLE_MAX as i64).about("rows (default 20)")
            ],
            crate::agent::tools::db_sample
        ),
        query!(
            "db.proposal_status",
            "Proposal Status",
            [Param::int_in("proposal_id", 1, i64::MAX).required()],
            crate::agent::tools::db_proposal_status
        ),
        // ---- agent tools: approval (creates a proposal; never runs SQL)
        cmd!(
            "db.propose_write",
            "Propose a Write",
            [],
            None,
            [
                CONNECTION,
                Param::string("sql").required().about("the exact statement(s) to run after approval"),
                Param::string("reason").required().about("why, shown to the person")
            ],
            always,
            crate::agent::tools::db_propose_write
        ),
        // ---- agent tools: UI
        cmd!(
            "view.open_table",
            "Open Table in a Tab",
            [],
            None,
            [CONNECTION, Param::string("table").required().about("table, optionally schema.table")],
            always,
            crate::agent::tools::view_open_table
        ),
        cmd!(
            "view.run_in_editor",
            "Put SQL in a New Editor Tab",
            [],
            None,
            [Param::string("sql").required().about("shown, not run"), Param::string("connection").about("connection id")],
            always,
            crate::agent::tools::view_run_in_editor
        ),
        cmd!(
            "view.filter",
            "Filter a Table Tab",
            [],
            None,
            [TAB, Param::string("where").required().about("a WHERE clause; checked by the guard")],
            always,
            crate::agent::tools::view_filter
        ),
        query!(
            "view.select",
            "Highlight Cells",
            [
                TAB,
                Param::json("rows").required().about("row indexes [0, 5] or ranges [{\"from\": 0, \"to\": 9}]"),
                Param::json("columns").about("column names; default all")
            ],
            crate::agent::tools::view_select
        ),
        query!("view.focus", "Focus a Region", [Param::one_of("region", Region::NAMES).required()], crate::agent::tools::view_focus),
        cmd!(
            "viz.chart",
            "Chart a Query",
            [],
            None,
            [
                Param::one_of("kind", ChartKind::NAMES).required(),
                CONNECTION,
                SQL,
                Param::string("x").required().about("column for the x axis"),
                Param::string("y").about("column for the y axis (not for histogram)"),
                Param::string("series").about("column that splits series"),
                Param::string("title"),
            ],
            always,
            crate::agent::tools::viz_chart
        ),
        cmd!(
            "viz.pivot",
            "Pivot a Query",
            [],
            None,
            [
                CONNECTION,
                SQL,
                Param::string("rows").required().about("column for pivot rows"),
                Param::string("columns").required().about("column for pivot columns"),
                Param::string("value").required().about("column aggregated in cells"),
                Param::one_of("agg", &["count", "sum", "avg", "min", "max"]).about("default count"),
            ],
            always,
            crate::agent::tools::viz_pivot
        ),
        cmd!("viz.close", "Close a Chart", [], None, [PANEL], always, crate::agent::tools::viz_close),
        // ---- human commands
        CommandSpec::new("palette.open", "Command Palette", palette_open).shortcut("Mod+K").query(),
        cmd!("conn.new", "New Connection…", ["Connection"], None, [], always, crate::sidebar::commands::conn_new),
        cmd!("conn.edit", "Edit Connection…", ["Connection"], None, [CONNECTION], always, crate::sidebar::commands::conn_edit),
        cmd!("conn.open", "Connect", ["Connection"], None, [CONNECTION], always, crate::sidebar::commands::conn_open),
        cmd!("conn.close", "Disconnect", ["Connection"], None, [CONNECTION], always, crate::sidebar::commands::conn_close),
        cmd!("conn.test", "Test Connection", [], None, [CONNECTION], always, crate::sidebar::commands::conn_test),
        cmd!(
            "tab.new_editor",
            "New Editor Tab",
            ["Query"],
            Some("Mod+T"),
            [Param::string("connection").about("connection id")],
            always,
            tab_new_editor
        ),
        cmd!("tab.close", "Close Tab", [], None, [TAB], always, crate::editor::commands::tab_close),
        query!("tab.select", "Select Tab", [TAB], crate::editor::commands::tab_select),
        cmd!(
            "table.open",
            "Open Table…",
            ["Query"],
            Some("Mod+P"),
            [Param::string("connection"), Param::string("table")],
            always,
            crate::sidebar::commands::table_open
        ),
        query!("query.cancel", "Cancel Query", [Param::int_in("tab", 1, i64::MAX)], crate::editor::commands::query_cancel).shortcut("Mod+."),
        query!("query.explain", "Explain", [Param::int_in("tab", 1, i64::MAX)], not_yet).shortcut("Mod+E"),
        cmd!("query.format", "Format SQL", ["Query"], None, [Param::int_in("tab", 1, i64::MAX)], always, crate::editor::commands::query_format),
        query!("history.show", "Query History", [Param::string("search")], crate::editor::commands::history_show),
        query!("grid.filter", "Filter Grid", [], not_yet).shortcut("Mod+F"),
        query!("grid.sort", "Sort Grid", [TAB, Param::string("column").required(), Param::bool("descending")], not_yet),
        query!("grid.copy", "Copy Rows", [Param::one_of("format", &["tsv", "csv", "json", "insert"]).about("default tsv")], not_yet),
        cmd!("edit.cell", "Edit Cell", [], Some("F2"), [], always, not_yet),
        cmd!("edit.add_row", "Add Row", ["Edit"], None, [], always, not_yet),
        cmd!("edit.delete_rows", "Delete Rows", ["Edit"], None, [], always, not_yet),
        cmd!("edit.discard", "Discard Pending Changes", ["Edit"], None, [], always, not_yet),
        query!("edit.review", "Review Pending Changes…", [], not_yet).shortcut("Mod+Alt+S"),
        CommandSpec::new("agent.toggle_panel", "Agent Panel", agent_toggle_panel).menu(&["View"]).shortcut("Mod+L").query(),
        query!("proposal.focus", "Review Agent Proposal", [], crate::agent::proposal_focus).shortcut("Mod+Shift+A"),
    ]
}
