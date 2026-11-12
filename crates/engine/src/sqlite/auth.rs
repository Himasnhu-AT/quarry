//! The agent connection's authorizer (SECURITY.md layer A): a second write guard inside SQLite,
//! independent of the classifier. It runs when a statement is prepared, so a denied statement
//! never executes.
//!
//! Allowed: `SELECT`, reading tables and columns, recursive CTEs, built-in functions except the
//! denied ones below (no application function is ever registered on the agent connection, so
//! every function it can call is one of SQLite's), and the read-only pragmas below. Everything
//! else is denied, including codes this SQLite version adds later.

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// Functions that load code, touch files or plant pointers.
const DENIED_FUNCTIONS: &[&str] = &["load_extension", "fts3_tokenizer", "readfile", "writefile", "edit", "lsdir"];

/// Pragmas that only read, with or without an argument (a table or index name, a row limit).
const READ_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "database_list",
    "collation_list",
    "function_list",
    "module_list",
    "pragma_list",
    "compile_options",
    "data_version",
    "freelist_count",
    "page_count",
    "integrity_check",
    "quick_check",
];

/// Pragmas that read without an argument and write with one (`PRAGMA user_version = 5`).
const READ_PRAGMAS_NO_VALUE: &[&str] = &["page_size", "encoding", "user_version", "application_id", "journal_mode", "schema_version", "auto_vacuum"];

fn listed(list: &[&str], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// The decision for one action.
pub(crate) fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Read { .. } | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Function { function_name } if !listed(DENIED_FUNCTIONS, function_name) => Authorization::Allow,
        AuthAction::Pragma { pragma_name, pragma_value } => {
            if listed(READ_PRAGMAS, pragma_name) || (pragma_value.is_none() && listed(READ_PRAGMAS_NO_VALUE, pragma_name)) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}
