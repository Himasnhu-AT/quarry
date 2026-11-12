//! Saved connections. A [`ConnectionSpec`] lives in settings; its password or key passphrase lives
//! in the OS keychain and only ever appears here as a [`Secret`] passed to `Engine::connect_*`.
//! Agents see a [`ConnectionSummary`], never a spec.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ConnectionId;

/// Which database engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Postgres,
    Sqlite,
}

/// TLS mode of a PostgreSQL connection (libpq names).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum TlsMode {
    Disable,
    #[default]
    Prefer,
    Require,
    /// Verify the server certificate and host name, optionally against a custom CA file.
    VerifyFull {
        #[serde(default)]
        ca_file: Option<PathBuf>,
    },
}

/// How the SSH tunnel authenticates (passwords later, PLAN §10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SshAuth {
    /// The running ssh-agent.
    Agent,
    /// A private key file; its passphrase, if any, is a keychain secret.
    Key { path: PathBuf },
}

/// An SSH tunnel to reach the database host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

/// Where the database is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "engine")]
pub enum Target {
    Postgres {
        host: String,
        port: u16,
        db: String,
        user: String,
        #[serde(default)]
        tls: TlsMode,
        #[serde(default)]
        ssh: Option<SshSpec>,
    },
    Sqlite {
        path: PathBuf,
    },
}

impl Target {
    pub fn engine(&self) -> EngineKind {
        match self {
            Target::Postgres { .. } => EngineKind::Postgres,
            Target::Sqlite { .. } => EngineKind::Sqlite,
        }
    }
}

/// A connection's color: tints the title bar and the status chip (DESIGN.md). `Prod` also turns
/// off "approve similar" (SECURITY.md, layer C).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnColor {
    #[default]
    None,
    Local,
    Staging,
    Prod,
}

/// What the agent may do on a connection (connection dialog, "Agent access").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum AgentAccess {
    /// No agent tools on this connection.
    Off,
    /// The agent's own read-only connection with the human's credentials (layer A session guards).
    #[default]
    ReadOnly,
    /// The agent's own connection as a dedicated SELECT-only role (PostgreSQL); its password is a
    /// separate keychain secret.
    ReadOnlyRole { role: String },
}

impl AgentAccess {
    pub fn allows_agent(&self) -> bool {
        !matches!(self, AgentAccess::Off)
    }
}

/// A saved connection (settings). Never holds a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSpec {
    pub id: ConnectionId,
    /// Shown in the window and to the agent.
    pub name: String,
    pub target: Target,
    #[serde(default)]
    pub color: ConnColor,
    #[serde(default)]
    pub agent_access: AgentAccess,
    /// Column name patterns (case-insensitive substrings) redacted in agent results, on top of the
    /// built-in ones (`password`, `token`, `secret`, `ssn`, …) and columns marked sensitive.
    #[serde(default)]
    pub sensitive_patterns: Vec<String>,
    /// The human's own connection runs read-only too (title bar badge).
    #[serde(default)]
    pub human_read_only: bool,
}

impl ConnectionSpec {
    pub fn engine(&self) -> EngineKind {
        self.target.engine()
    }

    /// What an agent may see of this connection.
    pub fn summary(&self) -> ConnectionSummary {
        ConnectionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            engine: self.engine(),
            color: self.color,
            agent_access: match self.agent_access {
                AgentAccess::Off => "off",
                AgentAccess::ReadOnly => "read_only",
                AgentAccess::ReadOnlyRole { .. } => "read_only_role",
            }
            .to_string(),
        }
    }
}

/// The agent-visible form of a connection (`db.connections`): no host, user, path or secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSummary {
    pub id: ConnectionId,
    pub name: String,
    pub engine: EngineKind,
    pub color: ConnColor,
    /// `off`, `read_only` or `read_only_role`.
    pub agent_access: String,
}

/// A password or passphrase read from the keychain. Not `Serialize`, not `Clone`, and its `Debug`
/// prints nothing, so it cannot leak into settings, logs, the document or an agent result.
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// The secret text, for the driver that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_has_no_location_or_user() {
        let spec = ConnectionSpec {
            id: ConnectionId::new("staging"),
            name: "Staging".into(),
            target: Target::Postgres {
                host: "db.internal".into(),
                port: 5432,
                db: "app".into(),
                user: "alice".into(),
                tls: TlsMode::Require,
                ssh: None,
            },
            color: ConnColor::Staging,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec![],
            human_read_only: false,
        };
        let json = serde_json::to_string(&spec.summary()).unwrap();
        for leak in ["db.internal", "alice", "5432", "app\""] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
        assert_eq!(format!("{:?}", Secret::new("hunter2")), "Secret(…)");
    }
}
