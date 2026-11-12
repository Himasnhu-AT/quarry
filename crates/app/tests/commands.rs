#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Contract 1: command ids, params and audiences.

use ashlar::command::shortcut::Platform;
use ashlar::command::{CommandError, Dispatch, Keymap};
use quarry::ids::{self, AGENT_TOOLS, HUMAN_COMMANDS, HUMAN_ONLY, ToolKind};
use serde_json::json;

#[test]
fn every_contract_id_is_registered_and_human_only_ids_never_are() {
    let h = quarry::app().headless();
    let infos = h.commands();
    let registered: Vec<&str> = infos.iter().map(|c| c.id.as_str()).collect();
    for id in AGENT_TOOLS.iter().map(|t| t.id).chain(HUMAN_COMMANDS.iter().copied()) {
        assert_eq!(registered.iter().filter(|r| **r == id).count(), 1, "{id} registered once");
    }
    for id in HUMAN_ONLY {
        assert!(!registered.contains(id), "{id} must not be a command: every agent surface would see it");
    }
    assert!(h.registry().duplicates().is_empty(), "{:?}", h.registry().duplicates());
    // every quarry command is in the contract lists (ashlar's built-ins aside)
    let builtin = |id: &str| id.starts_with("file.") || id.starts_with("app.") || id == "view.theme";
    for id in registered.iter().filter(|id| !builtin(id)) {
        assert!(AGENT_TOOLS.iter().any(|t| t.id == *id) || HUMAN_COMMANDS.contains(id), "{id} is in no contract list (ids.rs)");
    }
}

#[test]
fn read_only_db_tools_are_queries() {
    let h = quarry::app().headless();
    let infos = h.commands();
    for t in AGENT_TOOLS {
        let info = infos.iter().find(|c| c.id == t.id).unwrap();
        if t.kind == ToolKind::ReadOnly && t.id.starts_with("db.") {
            assert!(!info.mutates, "{} is read only: query!", t.id);
        }
        assert!(info.params_schema.is_object(), "{} has typed params", t.id);
    }
    let propose = infos.iter().find(|c| c.id == ids::DB_PROPOSE_WRITE).unwrap();
    assert!(propose.mutates, "MCP clients must not see db.propose_write as read-only");
}

#[test]
fn stubs_fail_with_their_id_and_not_yet() {
    let mut h = quarry::app().headless();
    let e = h.execute(ids::DB_QUERY, json!({"connection": "fixture", "sql": "SELECT 1"})).unwrap_err();
    assert_eq!(e, CommandError::failed(ids::DB_QUERY, "not_yet: not yet"));
    // params are validated against the contract before the stub runs
    let bad = h.execute(ids::DB_QUERY, json!({"connection": "fixture", "sql": "SELECT 1", "limit": 5001})).unwrap_err();
    assert!(matches!(bad, CommandError::BadParams { .. }), "{bad:?}");
    let missing = h.execute(ids::DB_PROPOSE_WRITE, json!({"connection": "fixture", "sql": "DELETE FROM t"})).unwrap_err();
    assert!(matches!(missing, CommandError::BadParams { .. }), "reason is required: {missing:?}");
    for id in HUMAN_ONLY {
        assert!(matches!(h.execute(id, json!({})), Err(CommandError::Unknown { .. })), "{id}");
    }
}

#[test]
fn real_human_commands_work() {
    let mut h = quarry::app().headless();
    assert_eq!(h.execute(ids::TAB_NEW_EDITOR, json!({})).unwrap(), json!({"tab": 1}));
    assert_eq!(h.session().doc().tabs.len(), 1);
    assert_eq!(h.execute(ids::AGENT_TOGGLE_PANEL, json!({})).unwrap(), json!({"open": true}));
}

#[test]
fn no_shortcut_conflicts_on_either_platform() {
    let h = quarry::app().headless();
    for p in [Platform::Mac, Platform::Other] {
        let k = Keymap::from_registry(h.registry(), p);
        assert!(k.conflicts().is_empty(), "{p:?}: {:?}", k.conflicts());
        assert!(k.invalid_defaults().is_empty(), "{:?}", k.invalid_defaults());
    }
}
