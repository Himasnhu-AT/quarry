//! Runtime state shared by commands, views and jobs that is not the document: the proposal book,
//! the agent pause flag, the registry of connections agent tools and approved writes use
//! ([`Connections`]) and the audit log ([`AuditLog`]). Streams add to it additively.
//!
//! It is process-wide because an ashlar 0.2 `Session` has no slot for app state (see
//! `docs/contracts.md`, "ashlar 0.2 gaps"), and must not be the document: undo must never touch a
//! decision. Locks recover from poisoning (AGENTS.md §0).

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use quarry_core::{AuditEntry, ConnectionId, ConnectionSpec, ConnectionSummary, ProposalBook, Refusal, RefusalCode, Secret, Timestamp};
use quarry_engine::{AgentConnection, EngineError, HumanConnection};

/// The app's runtime state.
#[derive(Default)]
pub struct Services {
    proposals: Mutex<ProposalBook>,
    agent_paused: AtomicBool,
    connections: Connections,
    audit: AuditLog,
}

impl Services {
    /// The proposal book (decisions go through `crate::human::perform` only).
    pub fn proposals(&self) -> MutexGuard<'_, ProposalBook> {
        self.proposals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the person paused the agent (tool calls are refused `agent_paused`).
    pub fn agent_paused(&self) -> bool {
        self.agent_paused.load(Ordering::Relaxed)
    }

    /// Only `crate::human` changes it (pause / resume are human-only).
    pub(crate) fn set_agent_paused(&self, paused: bool) {
        self.agent_paused.store(paused, Ordering::Relaxed);
    }

    /// The connections agent tools and approved writes use (agent stream, additive).
    pub fn connections(&self) -> &Connections {
        self.connections.load_env_once();
        &self.connections
    }

    /// The audit log (agent stream, additive).
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One registered connection: its spec, its secret, and the two engine connections, each opened
/// on first use and kept. Engine calls block: they run in jobs or, for agent tools (a command body
/// cannot spawn a job in ashlar 0.2), bounded by the statement timeout and the row cap.
pub struct ConnEntry {
    spec: ConnectionSpec,
    secret: Option<Secret>,
    agent: Mutex<Option<Box<dyn AgentConnection>>>,
    human: Mutex<Option<Box<dyn HumanConnection>>>,
}

impl ConnEntry {
    pub fn spec(&self) -> &ConnectionSpec {
        &self.spec
    }

    /// The guard policy of this connection (its sensitive-column patterns).
    pub fn policy(&self) -> quarry_guard::Policy {
        quarry_guard::Policy { safe_functions: Vec::new(), sensitive_patterns: self.spec.sensitive_patterns.clone() }
    }

    /// Run `f` on the agent's read-only connection (SECURITY.md layer A), opening it first.
    /// Refused `agent_access_off` when the person turned agent access off.
    pub fn with_agent<T>(&self, f: impl FnOnce(&mut dyn AgentConnection) -> Result<T, EngineError>) -> Result<T, Refusal> {
        if !self.spec.agent_access.allows_agent() {
            return Err(Refusal::new(RefusalCode::AgentAccessOff, format!("agent access is off for `{}`", self.spec.id)));
        }
        let mut slot = lock(&self.agent);
        if slot.is_none() {
            let c = quarry_engine::engine(self.spec.engine()).connect_agent(&self.spec, self.secret.as_ref()).map_err(Refusal::from)?;
            *slot = Some(c);
        }
        let Some(conn) = slot.as_mut() else { return Err(Refusal::new(RefusalCode::UnknownConnection, "not connected")) };
        let r = f(conn.as_mut());
        if matches!(r, Err(EngineError::Closed)) {
            *slot = None;
        }
        r.map_err(Refusal::from)
    }

    /// Run `f` on the person's connection, opening it first. Only approved writes
    /// (`crate::agent::approval`) and human-only actions call it, never an agent tool.
    pub(crate) fn with_human<T>(&self, f: impl FnOnce(&mut dyn HumanConnection) -> Result<T, EngineError>) -> Result<T, EngineError> {
        let mut slot = lock(&self.human);
        if slot.is_none() {
            *slot = Some(quarry_engine::engine(self.spec.engine()).connect_human(&self.spec, self.secret.as_ref())?);
        }
        let Some(conn) = slot.as_mut() else { return Err(EngineError::Closed) };
        let r = f(conn.as_mut());
        if matches!(r, Err(EngineError::Closed)) {
            *slot = None;
        }
        r
    }
}

/// The connections agent tools and approved writes reach, by id. The window registers the
/// person's saved connections here when they connect; `quarry-cli` (no window, no settings in a
/// command body) reads `QUARRY_CONNECTIONS`, a JSON file holding a list of [`ConnectionSpec`]s.
#[derive(Default)]
pub struct Connections {
    entries: Mutex<Vec<Arc<ConnEntry>>>,
    env_loaded: OnceLock<()>,
}

/// Most registered connections.
const MAX_CONNECTIONS: usize = 256;

impl Connections {
    fn load_env_once(&self) {
        self.env_loaded.get_or_init(|| {
            let Some(path) = std::env::var_os("QUARRY_CONNECTIONS") else { return };
            let specs = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Vec<ConnectionSpec>>(&b).ok());
            match specs {
                Some(specs) => {
                    for spec in specs.into_iter().take(MAX_CONNECTIONS) {
                        self.register(spec, None);
                    }
                }
                None => log::warn!("QUARRY_CONNECTIONS: not a JSON list of connection specs"),
            }
        });
    }

    /// Add or replace a connection (a replaced one's open connections close). Saving a
    /// connection is a human-only action: call this only from that path, tests and setup.
    pub fn register(&self, spec: ConnectionSpec, secret: Option<Secret>) {
        let entry = Arc::new(ConnEntry { spec, secret, agent: Mutex::new(None), human: Mutex::new(None) });
        let mut all = lock(&self.entries);
        all.retain(|e| e.spec.id != entry.spec.id);
        if all.len() < MAX_CONNECTIONS {
            all.push(entry);
        }
    }

    pub fn remove(&self, id: &ConnectionId) {
        lock(&self.entries).retain(|e| &e.spec.id != id);
    }

    pub fn get(&self, id: &str) -> Option<Arc<ConnEntry>> {
        lock(&self.entries).iter().find(|e| e.spec.id.as_str() == id).cloned()
    }

    /// What an agent may see of every connection (no host, user, path or secret).
    pub fn summaries(&self) -> Vec<ConnectionSummary> {
        lock(&self.entries).iter().map(|e| e.spec.summary()).collect()
    }
}

/// Audit entries kept in memory (the panel and tests read them); the file keeps everything.
const AUDIT_KEPT: usize = 2_000;

/// The append-only audit log (`audit.jsonl`): every agent read, proposal, decision and approved
/// write. The file is `QUARRY_AUDIT`, else `audit.jsonl` in the app's data folder, else none.
#[derive(Default)]
pub struct AuditLog {
    state: Mutex<AuditState>,
}

#[derive(Default)]
struct AuditState {
    path: Option<Option<PathBuf>>,
    recent: VecDeque<AuditEntry>,
}

impl AuditLog {
    /// Write to `path` from now on (`None`: memory only). Tests point it at a temporary folder.
    pub fn set_path(&self, path: Option<PathBuf>) {
        lock(&self.state).path = Some(path);
    }

    /// Append one entry. A failed file write is logged, never fatal.
    pub fn record(&self, entry: AuditEntry) {
        let mut st = lock(&self.state);
        let path = st
            .path
            .get_or_insert_with(|| {
                std::env::var_os("QUARRY_AUDIT")
                    .map(PathBuf::from)
                    .or_else(|| ashlar::fs::AppDirs::new("org", "ashlar", "quarry").map(|d| d.data.join("audit.jsonl")))
            })
            .clone();
        if let Some(path) = path {
            let line = entry.to_line();
            let wrote = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
                writeln!(f, "{line}")
            });
            if let Err(e) = wrote {
                log::warn!("audit log {}: {e}", path.display());
            }
        }
        if st.recent.len() >= AUDIT_KEPT {
            st.recent.pop_front();
        }
        st.recent.push_back(entry);
    }

    /// The latest entries, oldest first.
    pub fn recent(&self) -> Vec<AuditEntry> {
        lock(&self.state).recent.iter().cloned().collect()
    }
}

/// The process's services.
pub fn services() -> &'static Services {
    static S: OnceLock<Services> = OnceLock::new();
    S.get_or_init(Services::default)
}

/// Now, as a [`Timestamp`] (0 if the clock is before 1970).
pub fn now() -> Timestamp {
    Timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_flag_round_trips_and_clock_is_after_2020() {
        let s = Services::default();
        assert!(!s.agent_paused());
        s.set_agent_paused(true);
        assert!(s.agent_paused());
        assert!(now() > Timestamp(1_577_836_800_000));
        assert_eq!(s.proposals().pending().count(), 0);
    }
}
