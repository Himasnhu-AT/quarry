//! Query history per connection, in app settings (`quarry.history.<connection>`), newest first,
//! capped. Only the person's own runs are recorded; the agent's queries are the agent panel's.

use ashlar::fs::Settings;
use quarry_core::ConnectionId;
use serde::{Deserialize, Serialize};

/// Settings key prefix.
pub const KEY_PREFIX: &str = "quarry.history.";
/// Entries kept per connection.
pub const MAX_ENTRIES: usize = 200;
/// Longest SQL kept in an entry (longer text is cut at a char boundary).
pub const MAX_SQL_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub sql: String,
    /// Unix ms.
    pub at: u64,
    pub ms: u64,
    #[serde(default)]
    pub rows: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn key(conn: &ConnectionId) -> String {
    format!("{KEY_PREFIX}{conn}")
}

pub fn load(settings: &Settings, conn: &ConnectionId) -> Vec<Entry> {
    let raw: Vec<serde_json::Value> = settings.get(&key(conn)).unwrap_or_default();
    raw.into_iter().take(MAX_ENTRIES).filter_map(|v| serde_json::from_value(v).ok()).collect()
}

/// Add `entry` on top (an identical SQL moves up instead of repeating).
pub fn record(settings: &mut Settings, conn: &ConnectionId, mut entry: Entry) {
    if entry.sql.len() > MAX_SQL_BYTES {
        let mut cut = MAX_SQL_BYTES;
        while !entry.sql.is_char_boundary(cut) {
            cut -= 1;
        }
        entry.sql.truncate(cut);
    }
    let mut all = load(settings, conn);
    all.retain(|e| e.sql != entry.sql);
    all.insert(0, entry);
    all.truncate(MAX_ENTRIES);
    if let Err(e) = settings.set(&key(conn), &all) {
        log::warn!("history: {e}");
    }
}

/// Entries whose SQL contains every word of `query` (ignoring case).
pub fn search<'a>(entries: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let words: Vec<String> = query.split_whitespace().take(16).map(str::to_lowercase).collect();
    entries.iter().filter(|e| words.is_empty() || words.iter().all(|w| e.sql.to_lowercase().contains(w))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn settings() -> Settings {
        Settings::load(Arc::new(ashlar::fs::MemoryConfig::new()), "settings.json", &[])
    }

    fn entry(sql: &str) -> Entry {
        Entry { sql: sql.into(), at: 1, ms: 2, rows: Some(3), error: None }
    }

    #[test]
    fn newest_first_deduplicated_capped_and_per_connection() {
        let mut s = settings();
        let (a, b) = (ConnectionId::new("a"), ConnectionId::new("b"));
        record(&mut s, &a, entry("SELECT 1"));
        record(&mut s, &a, entry("SELECT 2"));
        record(&mut s, &a, entry("SELECT 1"));
        assert_eq!(load(&s, &a).iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["SELECT 1", "SELECT 2"]);
        assert!(load(&s, &b).is_empty());
        for i in 0..(MAX_ENTRIES + 10) {
            record(&mut s, &b, entry(&format!("SELECT {i}")));
        }
        assert_eq!(load(&s, &b).len(), MAX_ENTRIES);
        record(&mut s, &b, entry(&"é".repeat(MAX_SQL_BYTES)));
        assert!(load(&s, &b)[0].sql.len() <= MAX_SQL_BYTES);
    }

    #[test]
    fn search_matches_all_words() {
        let all = vec![entry("SELECT * FROM orders WHERE id = 1"), entry("select count(*) from customers")];
        assert_eq!(search(&all, "").len(), 2);
        assert_eq!(search(&all, "FROM orders").len(), 1);
        assert_eq!(search(&all, "customers count").len(), 1);
        assert_eq!(search(&all, "nope").len(), 0);
    }
}
