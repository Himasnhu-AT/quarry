//! Saved connections (app settings, never a secret) and the window's live connections.
//!
//! Passwords live only in the OS keychain (`ashlar_native::secrets::Keychain`, service
//! [`KEYCHAIN_SERVICE`], one entry per connection named `conn.<id>`). Settings hold the
//! [`Saved`] list under [`SETTINGS_KEY`]. Saving, deleting and changing agent access are human-only
//! actions (`conn.save`, `conn.delete`): only [`crate::dialogs`] calls [`Connections::save`] /
//! [`Connections::delete`], and only with a [`crate::human::HumanTicket`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ashlar::fs::Settings;
use quarry_core::{ConnectionId, ConnectionSpec, SchemaTree, Secret};
use quarry_engine::{CancelToken, HumanConnection};
use serde::{Deserialize, Serialize};

use crate::human::HumanTicket;

/// Settings key of the saved connections.
pub const SETTINGS_KEY: &str = "quarry.connections";
/// Keychain service for connection passwords.
pub const KEYCHAIN_SERVICE: &str = "dev.quarry.connections";
/// At most this many saved connections are read from settings.
pub const MAX_SAVED: usize = 256;

/// One saved connection: the spec plus Quarry-only agent policy that is not part of the core
/// contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub spec: ConnectionSpec,
    /// "Approve similar for this session" may be offered on this connection's approval cards
    /// (never for DDL, unbounded DELETE or prod-colored connections; the agent stream enforces).
    #[serde(default)]
    pub approve_similar: bool,
}

/// The person's own connection, shared with jobs (calls block; they run in jobs only).
pub type Human = Arc<Mutex<Box<dyn HumanConnection>>>;

/// Lock a connection; a poisoned lock is not fatal (AGENTS.md §0).
pub fn lock(h: &Human) -> MutexGuard<'_, Box<dyn HumanConnection>> {
    h.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a connection is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Closed,
    Connecting,
    Connected,
    /// Connecting failed or the connection dropped (message without secrets).
    Failed(String),
}

/// A connection's live state in this window.
#[derive(Default)]
pub struct Live {
    pub status: Status,
    pub human: Option<Human>,
    pub cancel: Option<CancelToken>,
    pub schema: Option<Arc<SchemaTree>>,
    /// The schema failed to load (connected anyway).
    pub schema_error: Option<String>,
    /// Bumps whenever `schema` changes (the tree caches by it).
    pub schema_rev: u64,
}

/// Saved and live connections of one window.
#[derive(Default)]
pub struct Connections {
    pub saved: Vec<Saved>,
    pub live: BTreeMap<ConnectionId, Live>,
}

impl Connections {
    /// Read the saved list from settings (bad entries are skipped, the list is capped).
    pub fn load(settings: &Settings) -> Self {
        let raw: Vec<serde_json::Value> = settings.get(SETTINGS_KEY).unwrap_or_default();
        let saved = raw.into_iter().take(MAX_SAVED).filter_map(|v| serde_json::from_value::<Saved>(v).ok()).collect();
        Self { saved, live: BTreeMap::new() }
    }

    pub fn get(&self, id: &ConnectionId) -> Option<&Saved> {
        self.saved.iter().find(|s| &s.spec.id == id)
    }

    pub fn spec(&self, id: &ConnectionId) -> Option<&ConnectionSpec> {
        self.get(id).map(|s| &s.spec)
    }

    pub fn status(&self, id: &ConnectionId) -> Status {
        self.live.get(id).map(|l| l.status.clone()).unwrap_or_default()
    }

    pub fn live_mut(&mut self, id: &ConnectionId) -> &mut Live {
        self.live.entry(id.clone()).or_default()
    }

    /// The open human connection for `id`.
    pub fn human(&self, id: &ConnectionId) -> Option<Human> {
        self.live.get(id).filter(|l| l.status == Status::Connected).and_then(|l| l.human.clone())
    }

    /// A fresh id for a connection called `name`: its slug, made unique.
    pub fn new_id(&self, name: &str) -> ConnectionId {
        let base = slug(name);
        let taken = |s: &str| self.saved.iter().any(|c| c.spec.id.as_str() == s);
        if !taken(&base) {
            return ConnectionId::new(base);
        }
        let n = (2..10_000u32).find(|n| !taken(&format!("{base}-{n}"))).unwrap_or(0);
        ConnectionId::new(format!("{base}-{n}"))
    }

    /// Save (insert or replace) a connection and, when given, its password: `conn.save`, a
    /// human-only action. The ticket is consumed.
    pub fn save(&mut self, _ticket: HumanTicket, settings: &mut Settings, saved: Saved, password: Option<&str>) -> Result<(), String> {
        log::info!("human action {}", crate::ids::CONN_SAVE);
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            store_secret(&saved.spec.id, pw)?;
        }
        match self.saved.iter_mut().find(|s| s.spec.id == saved.spec.id) {
            Some(s) => *s = saved,
            None => self.saved.push(saved),
        }
        self.persist(settings)
    }

    /// Delete a connection and its password: `conn.delete`, a human-only action.
    pub fn delete(&mut self, _ticket: HumanTicket, settings: &mut Settings, id: &ConnectionId) -> Result<(), String> {
        log::info!("human action {}", crate::ids::CONN_DELETE);
        self.saved.retain(|s| &s.spec.id != id);
        self.live.remove(id);
        if let Ok(k) = ashlar::native::secrets::Keychain::new(KEYCHAIN_SERVICE) {
            let _ = k.delete(&secret_name(id));
        }
        self.persist(settings)
    }

    fn persist(&self, settings: &mut Settings) -> Result<(), String> {
        settings.set(SETTINGS_KEY, &self.saved).map_err(|e| e.to_string())
    }
}

/// `conn.<id>` with characters the keychain refuses replaced.
pub fn secret_name(id: &ConnectionId) -> String {
    let clean: String = id.as_str().chars().map(|c| if c.is_ascii_alphanumeric() || "_-.".contains(c) { c } else { '_' }).take(59).collect();
    format!("conn.{clean}")
}

/// The stored password of `id` (blocking: call from a job). `None` when there is none or the
/// keychain is unavailable.
pub fn read_secret(id: &ConnectionId) -> Option<Secret> {
    let k = ashlar::native::secrets::Keychain::new(KEYCHAIN_SERVICE).ok()?;
    k.get(&secret_name(id)).ok().flatten().map(Secret::new)
}

fn store_secret(id: &ConnectionId, password: &str) -> Result<(), String> {
    let k = ashlar::native::secrets::Keychain::new(KEYCHAIN_SERVICE).map_err(|e| e.to_string())?;
    k.set(&secret_name(id), password).map_err(|e| e.to_string())
}

/// A connection id slug: lower-case ASCII letters, digits and `-`, at most 40 characters.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "connection".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarry_core::{AgentAccess, ConnColor, Target};

    fn spec(id: &str) -> ConnectionSpec {
        ConnectionSpec {
            id: ConnectionId::new(id),
            name: id.into(),
            target: Target::Sqlite { path: "/tmp/x.sqlite".into() },
            color: ConnColor::Local,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec!["password".into()],
            human_read_only: false,
        }
    }

    #[test]
    fn slugs_and_unique_ids() {
        assert_eq!(slug("Prod DB (EU)"), "prod-db-eu");
        assert_eq!(slug("  ##  "), "connection");
        assert_eq!(slug(&"é".repeat(300)), "connection");
        let mut c = Connections::default();
        c.saved.push(Saved { spec: spec("staging"), approve_similar: false });
        assert_eq!(c.new_id("Staging").as_str(), "staging-2");
        assert_eq!(c.new_id("Local").as_str(), "local");
        assert_eq!(secret_name(&ConnectionId::new("a b/c")), "conn.a_b_c");
        assert!(ashlar::native::secrets::valid_name(&secret_name(&ConnectionId::new("x".repeat(200)))));
    }

    #[test]
    fn saved_round_trips_without_secrets_and_bad_entries_are_skipped() {
        let s = Saved { spec: spec("local"), approve_similar: true };
        let json = serde_json::to_value(vec![&s]).unwrap();
        assert!(!json.to_string().contains("password\":"), "{json}");
        let mut settings = Settings::load(Arc::new(ashlar::fs::MemoryConfig::new()), "settings.json", &[]);
        settings.set_value(SETTINGS_KEY, serde_json::json!([json[0].clone(), {"spec": "garbage"}]));
        let c = Connections::load(&settings);
        assert_eq!(c.saved, vec![s]);
        assert_eq!(c.status(&ConnectionId::new("local")), Status::Closed);
        assert!(c.human(&ConnectionId::new("local")).is_none());
    }
}
