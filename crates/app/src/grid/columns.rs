//! Which columns the grid shows, in what order and width, and which slice of them fits.
//!
//! ashlar 0.2's `VirtualTable` has no column reorder, hide, frozen columns or horizontal
//! scrolling, so the grid rebuilds the table's column set each frame from this layout: the row
//! number, the frozen first column (when on), then the columns from the horizontal scroll
//! position that fit the width. Horizontal scrolling moves by whole columns.

use quarry_core::Column;

/// Width of a column the user never resized, from its type and name.
pub fn default_width(c: &Column) -> f32 {
    let t = c.type_name.to_ascii_lowercase();
    let by_type: f32 = if t.contains("bool") {
        70.0
    } else if t.contains("int") || t.contains("serial") || t.contains("real") || t.contains("double") || t.contains("float") || t.contains("numeric")
    {
        110.0
    } else if t.contains("json") {
        260.0
    } else if t.contains("time") || t.contains("date") {
        180.0
    } else if t.contains("uuid") {
        280.0
    } else {
        180.0
    };
    // the header shows the name and the type
    let header = (c.name.chars().count() + c.type_name.chars().count()) as f32 * 7.5 + 40.0;
    by_type.max(header).clamp(MIN_WIDTH, 360.0)
}

pub const MIN_WIDTH: f32 = 48.0;
pub const MAX_WIDTH: f32 = 1200.0;

/// The column layout of one grid.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnLayout {
    /// Source column indexes in display order (hidden ones included).
    order: Vec<usize>,
    /// By source index.
    hidden: Vec<bool>,
    /// By source index.
    widths: Vec<f32>,
    /// Keep the first shown column in place while scrolling sideways.
    pub frozen: bool,
    /// Horizontal scroll: how many scrollable shown columns are scrolled off to the left.
    first: usize,
}

impl ColumnLayout {
    pub fn new(columns: &[Column]) -> Self {
        Self {
            order: (0..columns.len()).collect(),
            hidden: vec![false; columns.len()],
            widths: columns.iter().map(default_width).collect(),
            frozen: false,
            first: 0,
        }
    }

    /// Source indexes of the shown columns, in order.
    pub fn shown(&self) -> Vec<usize> {
        self.order.iter().copied().filter(|&c| !self.hidden.get(c).copied().unwrap_or(true)).collect()
    }

    pub fn width(&self, col: usize) -> f32 {
        self.widths.get(col).copied().unwrap_or(120.0)
    }

    pub fn set_width(&mut self, col: usize, w: f32) {
        if let Some(x) = self.widths.get_mut(col)
            && w.is_finite()
        {
            *x = w.clamp(MIN_WIDTH, MAX_WIDTH);
        }
    }

    pub fn is_hidden(&self, col: usize) -> bool {
        self.hidden.get(col).copied().unwrap_or(true)
    }

    /// Hide or show a column; the last shown column stays.
    pub fn set_hidden(&mut self, col: usize, hide: bool) {
        if hide && self.shown().len() <= 1 {
            return;
        }
        if let Some(h) = self.hidden.get_mut(col) {
            *h = hide;
        }
        self.clamp_scroll();
    }

    pub fn show_all(&mut self) {
        self.hidden.iter_mut().for_each(|h| *h = false);
    }

    pub fn hidden_count(&self) -> usize {
        self.hidden.iter().filter(|h| **h).count()
    }

    /// Move source column `col` by `by` places among the shown columns (negative: left).
    pub fn shift(&mut self, col: usize, by: isize) {
        let shown = self.shown();
        let Some(at) = shown.iter().position(|&c| c == col) else { return };
        let to = at.saturating_add_signed(by).min(shown.len().saturating_sub(1));
        let Some(&target) = shown.get(to) else { return };
        let (Some(from), Some(dest)) = (self.order.iter().position(|&c| c == col), self.order.iter().position(|&c| c == target)) else { return };
        let c = self.order.remove(from);
        self.order.insert(dest.min(self.order.len()), c);
    }

    fn scrollable(&self) -> usize {
        self.shown().len().saturating_sub(usize::from(self.frozen))
    }

    fn clamp_scroll(&mut self) {
        self.first = self.first.min(self.scrollable().saturating_sub(1));
    }

    pub fn scroll_pos(&self) -> usize {
        self.first
    }

    /// Scroll sideways by `by` columns.
    pub fn scroll_by(&mut self, by: isize) {
        self.first = self.first.saturating_add_signed(by);
        self.clamp_scroll();
    }

    /// Scroll back while the columns from an earlier position still fit `width`: never leave
    /// blank space on the right with columns hidden on the left.
    pub fn fit(&mut self, width: f32) {
        while self.first > 0 {
            let shown = self.shown();
            let fixed: f32 = if self.frozen { shown.first().map_or(0.0, |&c| self.width(c)) } else { 0.0 };
            let rest: f32 = shown.iter().skip(usize::from(self.frozen) + self.first - 1).map(|&c| self.width(c)).sum();
            if fixed + rest > width {
                break;
            }
            self.first -= 1;
        }
    }

    /// Scroll so shown column `col` is in view in a body `width` points wide.
    pub fn reveal(&mut self, col: usize, width: f32) {
        let shown = self.shown();
        let Some(at) = shown.iter().position(|&c| c == col) else { return };
        if self.frozen && at == 0 {
            return;
        }
        let at = at - usize::from(self.frozen);
        if at < self.first {
            self.first = at;
        } else {
            while self.first < at && !self.window(width).contains(&col) {
                self.first += 1;
            }
        }
    }

    /// The source columns that fit a body `width` points wide (after the row numbers): the frozen
    /// one, then from the scroll position on; the last may be cut off.
    pub fn window(&self, width: f32) -> Vec<usize> {
        let shown = self.shown();
        let mut out = Vec::new();
        let mut x = 0.0;
        let (fixed, rest) = if self.frozen { shown.split_at(shown.len().min(1)) } else { shown.split_at(0) };
        for &c in fixed.iter().chain(rest.iter().skip(self.first)) {
            if x >= width && !out.is_empty() {
                break;
            }
            out.push(c);
            x += self.width(c);
        }
        out
    }

    /// Whether columns are cut off to the left / right of [`ColumnLayout::window`].
    pub fn overflow(&self, width: f32) -> (bool, bool) {
        let w = self.window(width);
        let total: f32 = w.iter().map(|&c| self.width(c)).sum();
        let last = self.shown().last().copied();
        (self.first > 0, w.last().copied() != last || total > width + 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: usize) -> Vec<Column> {
        (0..n).map(|i| Column::new(format!("c{i}"), "text")).collect()
    }

    #[test]
    fn window_scrolls_by_whole_columns_and_keeps_the_frozen_one() {
        let mut l = ColumnLayout::new(&cols(6));
        for c in 0..6 {
            l.set_width(c, 100.0);
        }
        assert_eq!(l.window(250.0), vec![0, 1, 2]);
        l.scroll_by(2);
        assert_eq!(l.window(250.0), vec![2, 3, 4]);
        l.frozen = true;
        assert_eq!(l.window(250.0), vec![0, 3, 4]);
        l.scroll_by(100);
        assert_eq!(l.window(250.0), vec![0, 5]);
        assert_eq!(l.overflow(250.0), (true, false));
        l.reveal(1, 250.0);
        assert_eq!(l.window(250.0), vec![0, 1, 2]);
        l.reveal(5, 250.0);
        assert!(l.window(250.0).contains(&5));
    }

    #[test]
    fn hide_and_reorder_rebuild_the_set() {
        let mut l = ColumnLayout::new(&cols(4));
        l.set_hidden(1, true);
        assert_eq!(l.shown(), vec![0, 2, 3]);
        l.shift(3, -2);
        assert_eq!(l.shown(), vec![3, 0, 2]);
        l.shift(3, 10);
        assert_eq!(l.shown(), vec![0, 2, 3]);
        for c in [0, 2] {
            l.set_hidden(c, true);
        }
        l.set_hidden(3, true);
        assert_eq!(l.shown(), vec![3], "the last column stays");
        l.show_all();
        assert_eq!(l.shown().len(), 4);
        l.set_width(0, f32::NAN);
        l.set_width(0, 5.0);
        assert_eq!(l.width(0), MIN_WIDTH);
    }

    #[test]
    fn widths_follow_types() {
        assert!(default_width(&Column::new("id", "int8")) < default_width(&Column::new("payload", "jsonb")));
    }
}
