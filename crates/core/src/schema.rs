//! The schema tree: namespaces (PostgreSQL schemas; SQLite `main` and attached databases), tables
//! and views, columns, indexes, foreign keys. Row counts are estimates, never `COUNT(*)`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A table named with its namespace (`None`: the engine's default search path).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TableRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: Option<&str>, name: impl Into<String>) -> Self {
        Self { schema: schema.map(str::to_string), name: name.into() }
    }

    /// `schema.name` or `name`, as typed by a person or an agent (no quoting rules: a dot splits
    /// once). Empty parts are refused.
    pub fn parse(s: &str) -> Option<Self> {
        match s.split_once('.') {
            Some((a, b)) if !a.is_empty() && !b.is_empty() => Some(Self::new(Some(a), b)),
            Some(_) => None,
            None if !s.is_empty() => Some(Self::new(None, s)),
            None => None,
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(s) => write!(f, "{s}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    Foreign,
}

/// One column of a table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primary_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub references: TableRef,
    pub ref_columns: Vec<String>,
}

/// A table or view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
    /// Estimated rows (`pg_class.reltuples`, SQLite `sqlite_stat1` when present); `None` unknown.
    #[serde(default)]
    pub row_estimate: Option<u64>,
    pub columns: Vec<ColumnInfo>,
    #[serde(default)]
    pub indexes: Vec<Index>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
}

/// A namespace and its tables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Namespace {
    pub name: String,
    pub tables: Vec<Table>,
}

/// The schema of one connection (or the part asked for).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaTree {
    pub namespaces: Vec<Namespace>,
}

impl SchemaTree {
    pub fn table(&self, r: &TableRef) -> Option<&Table> {
        self.namespaces.iter().filter(|n| r.schema.as_deref().is_none_or(|s| s == n.name)).flat_map(|n| &n.tables).find(|t| t.name == r.name)
    }
    pub fn table_count(&self) -> usize {
        self.namespaces.iter().map(|n| n.tables.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_refs_parse_and_print() {
        assert_eq!(TableRef::parse("public.orders"), Some(TableRef::new(Some("public"), "orders")));
        assert_eq!(TableRef::parse("orders").map(|t| t.to_string()), Some("orders".into()));
        for bad in ["", ".x", "x.", "."] {
            assert_eq!(TableRef::parse(bad), None, "{bad}");
        }
    }
}
