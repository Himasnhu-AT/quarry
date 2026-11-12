//! Why a call was turned down. Agent tools fail with `CommandError::Failed { msg }` where `msg` is
//! [`Refusal`]'s `Display`: `"<code>: <message>"`, so agents (and tests) can match on the code.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable refusal codes (contract 3). Add new ones at the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The SQL does not parse in the connection's dialect.
    ParseError,
    /// More than one statement.
    MultipleStatements,
    /// The statement can change something (or might): use `db.propose_write`.
    NotReadOnly,
    /// A function outside the read-only allow-list.
    DeniedFunction,
    /// The connection does not exist or is not open.
    UnknownConnection,
    /// Agent access is off for this connection.
    AgentAccessOff,
    /// The agent panel is paused by the human.
    AgentPaused,
    /// Too many calls or pending proposals.
    RateLimited,
    /// Over a size limit (SQL text, result).
    TooLarge,
    /// The statement ran past its timeout.
    Timeout,
    /// No such proposal, tab or panel.
    NotFound,
    /// The database refused or failed (its message follows).
    Database,
    /// Not built yet (M0 stubs).
    NotYet,
}

impl RefusalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalCode::ParseError => "parse_error",
            RefusalCode::MultipleStatements => "multiple_statements",
            RefusalCode::NotReadOnly => "not_read_only",
            RefusalCode::DeniedFunction => "denied_function",
            RefusalCode::UnknownConnection => "unknown_connection",
            RefusalCode::AgentAccessOff => "agent_access_off",
            RefusalCode::AgentPaused => "agent_paused",
            RefusalCode::RateLimited => "rate_limited",
            RefusalCode::TooLarge => "too_large",
            RefusalCode::Timeout => "timeout",
            RefusalCode::NotFound => "not_found",
            RefusalCode::Database => "database",
            RefusalCode::NotYet => "not_yet",
        }
    }
}

/// A refusal with a human-readable message (never containing a secret).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_code_colon_message_and_codes_match_serde() {
        assert_eq!(Refusal::new(RefusalCode::NotReadOnly, "DELETE changes data").to_string(), "not_read_only: DELETE changes data");
        for c in [RefusalCode::ParseError, RefusalCode::AgentAccessOff, RefusalCode::NotYet, RefusalCode::Database] {
            assert_eq!(serde_json::to_value(c).unwrap(), serde_json::json!(c.as_str()));
        }
    }
}
