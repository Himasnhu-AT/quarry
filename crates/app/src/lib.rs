//! Quarry: a fast database browser an AI agent can use safely. Rank 2.
//!
//! [`app`] is the whole app: the `quarry` window, `quarry --headless`, and `quarry-cli` (CLI and
//! MCP). Contracts: [`ids`] (command ids and params), `quarry_core::tool` (agent results),
//! [`human`] (the window-only path for approve / deny and every write).

pub mod agent;
pub mod dialogs;
pub mod editor;
pub mod grid;
pub mod human;
pub mod ids;
pub mod scenes;
pub mod services;
pub mod sidebar;
pub mod theme;
pub mod view;

use ashlar::prelude::*;
use quarry_core::Workspace;
use quarry_core::workspace::FORMAT_ID;

/// What the MCP server tells agents about Quarry.
pub const INSTRUCTIONS: &str = "Quarry is a database browser. You have read-only access: list connections with \
db.connections, read the schema with db.schema, run one read-only statement with db.query (or db.sample / db.explain), \
and drive the window with view.* and viz.*. You cannot write: db.propose_write stores a proposal that a person approves \
or denies in the Quarry window; poll db.proposal_status. Refusals start with a code, e.g. `not_read_only: …`.";

/// The workspace file format.
pub fn format() -> Format<Workspace> {
    Format::new(FORMAT_ID, concat!("quarry ", env!("CARGO_PKG_VERSION")))
}

/// The whole app.
pub fn app() -> App<Workspace> {
    let mut app = App::new("quarry", env!("CARGO_PKG_VERSION"))
        .title("Quarry")
        .format(format())
        .file_extension("quarry")
        .window_size(1280.0, 800.0)
        .commands(ids::commands())
        .instructions(INSTRUCTIONS)
        .view(view::view());
    for name in scenes::SCENES {
        app = app.scene(*name, move |s: &mut Session<Workspace>| scenes::setup(name, s));
    }
    app
}
