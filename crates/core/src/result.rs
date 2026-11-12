//! Results streamed from a cursor: columns, pages of rows, and what is known of the total.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Value;

/// One result column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// The engine's type name (`int8`, `text`, `jsonb`; SQLite: the declared type or `""`).
    #[serde(rename = "type")]
    pub type_name: String,
    /// Sensitive (human mark or name pattern): redacted in agent results.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self { name: name.into(), type_name: type_name.into(), sensitive: false }
    }
}

/// Rows `first_row .. first_row + rows.len()` of a result (0-based). Every row has one value per
/// column of its result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub first_row: u64,
    pub rows: Vec<Vec<Value>>,
}

impl Page {
    /// The row after the last one in this page.
    pub fn end_row(&self) -> u64 {
        self.first_row.saturating_add(self.rows.len() as u64)
    }
}

/// How many rows a result has, as far as is known: exactly (the cursor reached the end) or at
/// least (more may follow). Rendered as `1,000 of ≥ 10,000,000` in the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "rows")]
pub enum Total {
    Known(u64),
    AtLeast(u64),
}

impl Total {
    pub fn rows(self) -> u64 {
        match self {
            Total::Known(n) | Total::AtLeast(n) => n,
        }
    }
    /// Learn that at least `rows` exist (a fetched page reached that far).
    pub fn at_least(self, rows: u64) -> Total {
        match self {
            Total::Known(n) => Total::Known(n.max(rows)),
            Total::AtLeast(n) => Total::AtLeast(n.max(rows)),
        }
    }
}

/// A result being browsed: its columns, the pages fetched so far (ascending, not overlapping;
/// the grid keeps only visible pages and neighbours), and the total.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultSet {
    pub columns: Arc<[Column]>,
    pub pages: Vec<Page>,
    pub total: Total,
}

impl ResultSet {
    /// The page holding `row`, if fetched.
    pub fn page_of(&self, row: u64) -> Option<&Page> {
        let i = self.pages.partition_point(|p| p.end_row() <= row);
        self.pages.get(i).filter(|p| p.first_row <= row)
    }

    /// One cell, if its page is fetched.
    pub fn cell(&self, row: u64, col: usize) -> Option<&Value> {
        let p = self.page_of(row)?;
        let r = usize::try_from(row.checked_sub(p.first_row)?).ok()?;
        p.rows.get(r)?.get(col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_found_in_sparse_pages() {
        let page = |first: u64, n: i64| Page { first_row: first, rows: (0..n).map(|i| vec![Value::Int(first as i64 + i)]).collect() };
        let rs =
            ResultSet { columns: Arc::from(vec![Column::new("id", "int8")]), pages: vec![page(0, 3), page(1000, 2)], total: Total::AtLeast(1002) };
        assert_eq!(rs.cell(2, 0), Some(&Value::Int(2)));
        assert_eq!(rs.cell(3, 0), None);
        assert_eq!(rs.cell(1001, 0), Some(&Value::Int(1001)));
        assert_eq!(rs.cell(1001, 1), None);
        assert_eq!(rs.cell(5000, 0), None);
        assert_eq!(Total::AtLeast(5).at_least(9), Total::AtLeast(9));
        assert_eq!(Total::Known(5).at_least(3), Total::Known(5));
    }
}
