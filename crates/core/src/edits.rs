//! Pending table edits: what the person changed in a table tab's grid and has not committed yet.
//!
//! They live in the [`crate::Workspace`] document, so every edit is an undo step (⌘Z undoes the
//! last pending edit) and survives a crash. Committing is a human-only action (`edit.commit`):
//! the app turns the set into statements and runs them in one transaction on the person's
//! connection. Nothing here runs SQL.

use serde::{Deserialize, Serialize};

use crate::{TabId, TableRef, Value};

/// The pending edits of one table tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingEdits {
    pub tab: TabId,
    pub table: TableRef,
    /// The primary-key columns that identify a row (in key order).
    pub key: Vec<String>,
    /// In the order they were made; at most one `Update` per (row, column) and one `Delete` per row.
    #[serde(default)]
    pub changes: Vec<RowChange>,
}

/// One pending change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum RowChange {
    /// Set one cell of the row with primary key `key` (values in [`PendingEdits::key`] order).
    Update { key: Vec<Value>, column: String, old: Value, new: Value },
    /// A new row: the columns given (others take their defaults).
    Insert { values: Vec<(String, Value)> },
    /// Delete the row with primary key `key`; `before` keeps its shown values for the diff.
    Delete {
        key: Vec<Value>,
        #[serde(default)]
        before: Vec<(String, Value)>,
    },
}

impl PendingEdits {
    pub fn new(tab: TabId, table: TableRef, key: Vec<String>) -> Self {
        Self { tab, table, key, changes: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Set a cell. A second edit of the same cell keeps the first `old`; setting it back to `old`
    /// drops the change. Editing a row pending deletion does nothing.
    pub fn set(&mut self, key: Vec<Value>, column: &str, old: Value, new: Value) {
        if self.deleted(&key) {
            return;
        }
        let at = self.changes.iter().position(|c| matches!(c, RowChange::Update { key: k, column: col, .. } if *k == key && col == column));
        match at {
            Some(i) => {
                let back = matches!(self.changes.get(i), Some(RowChange::Update { old: o, .. }) if *o == new);
                if back {
                    self.changes.remove(i);
                } else if let Some(RowChange::Update { new: n, .. }) = self.changes.get_mut(i) {
                    *n = new;
                }
            }
            None if old != new => self.changes.push(RowChange::Update { key, column: column.to_string(), old, new }),
            None => {}
        }
    }

    /// The pending new value of a cell, if it was edited.
    pub fn updated(&self, key: &[Value], column: &str) -> Option<&Value> {
        self.changes.iter().find_map(|c| match c {
            RowChange::Update { key: k, column: col, new, .. } if k.as_slice() == key && col == column => Some(new),
            _ => None,
        })
    }

    /// Whether the row is pending deletion.
    pub fn deleted(&self, key: &[Value]) -> bool {
        self.changes.iter().any(|c| matches!(c, RowChange::Delete { key: k, .. } if k.as_slice() == key))
    }

    /// Delete a row (its pending updates are dropped).
    pub fn delete(&mut self, key: Vec<Value>, before: Vec<(String, Value)>) {
        if self.deleted(&key) {
            return;
        }
        self.changes.retain(|c| !matches!(c, RowChange::Update { key: k, .. } if *k == key));
        self.changes.push(RowChange::Delete { key, before });
    }

    pub fn insert(&mut self, values: Vec<(String, Value)>) {
        self.changes.push(RowChange::Insert { values });
    }

    /// The pending new rows, in order.
    pub fn inserts(&self) -> impl Iterator<Item = &Vec<(String, Value)>> {
        self.changes.iter().filter_map(|c| match c {
            RowChange::Insert { values } => Some(values),
            _ => None,
        })
    }

    /// The `n`th pending new row, to edit it.
    pub fn insert_mut(&mut self, n: usize) -> Option<&mut Vec<(String, Value)>> {
        self.changes
            .iter_mut()
            .filter_map(|c| match c {
                RowChange::Insert { values } => Some(values),
                _ => None,
            })
            .nth(n)
    }

    /// Counts of (updated cells, inserted rows, deleted rows).
    pub fn counts(&self) -> (usize, usize, usize) {
        self.changes.iter().fold((0, 0, 0), |(u, i, d), c| match c {
            RowChange::Update { .. } => (u + 1, i, d),
            RowChange::Insert { .. } => (u, i + 1, d),
            RowChange::Delete { .. } => (u, i, d + 1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits() -> PendingEdits {
        PendingEdits::new(TabId(1), TableRef::new(None, "orders"), vec!["id".into()])
    }

    #[test]
    fn edits_merge_and_cancel() {
        let mut e = edits();
        let k = vec![Value::Int(5)];
        e.set(k.clone(), "note", Value::Null, Value::text("a"));
        e.set(k.clone(), "note", Value::Null, Value::text("b"));
        assert_eq!(e.changes.len(), 1);
        assert_eq!(e.updated(&k, "note"), Some(&Value::text("b")));
        e.set(k.clone(), "note", Value::Null, Value::Null);
        assert!(e.is_empty());
        e.set(k.clone(), "note", Value::Null, Value::Null);
        assert!(e.is_empty(), "a no-op edit is not a change");
    }

    #[test]
    fn delete_drops_updates_and_blocks_edits() {
        let mut e = edits();
        let k = vec![Value::Int(5)];
        e.set(k.clone(), "note", Value::Null, Value::text("a"));
        e.delete(k.clone(), vec![]);
        e.delete(k.clone(), vec![]);
        e.set(k.clone(), "status", Value::text("x"), Value::text("y"));
        assert_eq!(e.counts(), (0, 0, 1));
        e.insert(vec![("id".into(), Value::Int(9))]);
        assert_eq!(e.counts(), (0, 1, 1));
        assert!(e.insert_mut(0).is_some() && e.insert_mut(1).is_none());
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<PendingEdits>(&json).unwrap(), e);
    }
}
