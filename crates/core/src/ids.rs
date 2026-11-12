//! Identifiers. All are plain values (no handles), so they serialize into the document, the audit
//! log and agent results unchanged.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A saved connection's stable id (a short slug chosen when the connection is created, e.g.
/// `"staging"`). Agents name connections by this id; it never contains a secret.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(pub Arc<str>);

impl ConnectionId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! numeric_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

numeric_id!(
    /// A write proposal (`db.propose_write`). Unique for the life of the process.
    ProposalId
);
numeric_id!(
    /// An open tab (editor or table) in the [`crate::Workspace`].
    TabId
);
numeric_id!(
    /// A chart or pivot panel (`viz.*`).
    PanelId
);
numeric_id!(
    /// An open server-side cursor on one engine connection.
    CursorId
);
