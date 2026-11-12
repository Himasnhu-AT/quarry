//! One grid's state, apart from drawing: the result's columns and total, the page cache and the
//! fetches in flight, sorting, the cursor window, the cell selection and copy.
//!
//! Fetches run through a [`Spawner`] (ashlar jobs in the app, a thread in benches); finished
//! pages land in an inbox the UI thread drains each frame, so a frame never waits on a database.

use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use ashlar::ui::selection::Selection;
use quarry_core::{Column, EngineKind, Page, TableRef, Total, Value};
use quarry_engine::CancelToken;

use super::cache::{Flight, KEEP_ALL_ROWS, PageCache};
use super::columns::ColumnLayout;
use super::copy::{self, Format, MAX_COPY_ROWS};
use super::source::{Order, PageSource, compare};

/// Rows one scroll window spans. Row positions are `f32` points in egui: at 28 points a row,
/// 500,000 rows (14M points) keep them exact to a pixel; past that the grid pages the window.
pub const WINDOW_ROWS: u64 = 500_000;

/// A source shared with the jobs that fetch from it.
pub type SharedSource = Arc<Mutex<Box<dyn PageSource>>>;

/// Runs fetches off the UI thread.
pub trait Spawner {
    /// Run `work` in the background; returns a handle to cancel it with.
    fn spawn(&mut self, label: String, work: Box<dyn FnOnce() + Send>) -> Option<u64>;
    fn cancel(&mut self, job: u64);
}

/// Runs everything at once, on the calling thread (tests).
pub struct Inline;

impl Spawner for Inline {
    fn spawn(&mut self, _label: String, work: Box<dyn FnOnce() + Send>) -> Option<u64> {
        work();
        None
    }
    fn cancel(&mut self, _job: u64) {}
}

/// What a fetch job hands back.
enum Arrival {
    Page { generation: u64, index: u64, result: Result<Page, String> },
    Requeried { generation: u64, result: Result<Total, String> },
}

type Inbox = Arc<Mutex<Vec<Arrival>>>;

fn push(inbox: &Inbox, a: Arrival) {
    inbox.lock().unwrap_or_else(PoisonError::into_inner).push(a);
}

/// Where the grid is.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// The statement runs; no cursor yet (or a re-sort runs).
    Running,
    Ready,
    /// The statement or a fetch failed. `position`: 1-based character in the SQL, if known.
    Error {
        message: String,
        position: Option<u32>,
    },
}

/// A table tab's grid can be edited: its table and the primary-key columns (source indexes).
#[derive(Clone, Debug, PartialEq)]
pub struct EditTarget {
    pub table: TableRef,
    pub key: Vec<usize>,
}

/// The cell selection: the rows in the list's [`Selection`] × a range of shown columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ColumnSpan {
    /// Positions among the shown columns (anchor, cursor); `None`: whole rows.
    pub span: Option<(usize, usize)>,
}

impl ColumnSpan {
    pub fn range(&self) -> Option<Range<usize>> {
        self.span.map(|(a, b)| a.min(b)..a.max(b).saturating_add(1))
    }
}

/// One grid.
pub struct GridState {
    pub columns: Arc<[Column]>,
    pub layout: ColumnLayout,
    pub status: Status,
    pub total: Total,
    pub dialect: EngineKind,
    pub target: Option<EditTarget>,
    /// The sort shown in the header.
    pub sort: Option<Order>,
    /// Client-side sort of a small, fully loaded result: shown row → result row.
    client_order: Option<Vec<u64>>,
    /// First result row of the scroll window.
    pub window: u64,
    pub selection: Selection,
    pub cells: ColumnSpan,
    /// The cell whose detail popover is open (shown row, source column).
    pub detail: Option<(u64, usize)>,
    /// The cell being edited and its text (shown row, source column).
    pub editing: Option<(u64, usize, String)>,
    /// A line for the grid's footer (copied, refused…).
    pub notice: Option<String>,
    /// Why a table tab's grid is read-only (no primary key, a view…), shown in the footer.
    pub read_only_note: Option<String>,
    /// Text to put on the clipboard on the next frame.
    pub clipboard: Option<String>,
    /// Ask the list to scroll to this shown row on the next frame.
    pub scroll_to: Option<u64>,
    /// A sort asked for by `grid.sort` (a command cannot start jobs): applied on the next frame.
    pub requested_sort: Option<Option<Order>>,
    source: Option<SharedSource>,
    instant: bool,
    cancel_token: Option<CancelToken>,
    inbox: Inbox,
    pub cache: PageCache,
    requery: Option<Flight>,
    /// Bumped when anything shown changes (rows arrived, sorted, edited): caches key on it.
    pub revision: u64,
}

impl GridState {
    /// A grid whose statement is still running.
    pub fn running() -> Self {
        Self::with_columns(Arc::from(Vec::new()), Total::AtLeast(0), Status::Running)
    }

    /// A grid showing an error.
    pub fn error(message: impl Into<String>, position: Option<u32>) -> Self {
        Self::with_columns(Arc::from(Vec::new()), Total::Known(0), Status::Error { message: message.into(), position })
    }

    fn with_columns(columns: Arc<[Column]>, total: Total, status: Status) -> Self {
        Self {
            layout: ColumnLayout::new(&columns),
            columns,
            status,
            total,
            dialect: EngineKind::Postgres,
            target: None,
            sort: None,
            client_order: None,
            window: 0,
            selection: Selection::new(),
            cells: ColumnSpan::default(),
            detail: None,
            editing: None,
            notice: None,
            read_only_note: None,
            clipboard: None,
            scroll_to: None,
            requested_sort: None,
            source: None,
            instant: false,
            cancel_token: None,
            inbox: Arc::default(),
            cache: PageCache::new(quarry_core::limits::PAGE_ROWS),
            requery: None,
            revision: 0,
        }
    }

    /// A grid reading `source`, whose cursor has `columns` and `total` so far.
    pub fn open(columns: Arc<[Column]>, total: Total, source: Box<dyn PageSource>) -> Self {
        let mut g = Self::with_columns(columns, total, Status::Ready);
        g.instant = source.instant();
        g.cancel_token = source.cancel_token();
        g.source = Some(Arc::new(Mutex::new(source)));
        g
    }

    pub fn page_rows(mut self, rows: u32) -> Self {
        self.cache = PageCache::new(rows);
        self
    }

    pub fn dialect(mut self, dialect: EngineKind) -> Self {
        self.dialect = dialect;
        self
    }

    pub fn editable(mut self, target: EditTarget) -> Self {
        self.target = Some(target);
        self
    }

    /// Cancel the statement or fetch in flight (`query.cancel`).
    pub fn cancel(&self) {
        if let Some(t) = &self.cancel_token {
            t.cancel();
        }
    }

    /// Rows in the scroll window.
    pub fn window_rows(&self) -> u64 {
        self.total.rows().saturating_sub(self.window).min(WINDOW_ROWS)
    }

    /// Whether the result goes on past the window / starts before it.
    pub fn window_more(&self) -> (bool, bool) {
        (self.window > 0, self.window.saturating_add(WINDOW_ROWS) < self.total.rows())
    }

    /// Move the window to the one holding result row `row`.
    pub fn set_window(&mut self, row: u64) {
        let w = row / WINDOW_ROWS * WINDOW_ROWS;
        if w != self.window {
            self.window = w;
            self.selection.clear();
            self.detail = None;
            self.editing = None;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// The result row shown at `shown` (a row of the result, after a client sort).
    pub fn result_row(&self, shown: u64) -> u64 {
        match &self.client_order {
            Some(o) => usize::try_from(shown).ok().and_then(|i| o.get(i)).copied().unwrap_or(shown),
            None => shown,
        }
    }

    /// The values of shown row `shown`, if loaded.
    pub fn row(&self, shown: u64) -> Option<&Vec<Value>> {
        self.cache.row(self.result_row(shown))
    }

    pub fn cell(&self, shown: u64, col: usize) -> Option<&Value> {
        self.row(shown)?.get(col)
    }

    /// The primary key of shown row `shown`, when the grid is editable and the key is loaded.
    pub fn key_of(&self, shown: u64) -> Option<Vec<Value>> {
        let t = self.target.as_ref()?;
        let row = self.row(shown)?;
        t.key.iter().map(|&c| row.get(c).cloned().filter(super::cells::complete)).collect()
    }

    pub fn is_loading(&self) -> bool {
        self.status == Status::Running || self.cache.is_loading() || self.requery.is_some()
    }

    /// Apply finished fetches, then fetch what rows `visible` (shown rows) need and cancel what
    /// they no longer need.
    pub fn pump(&mut self, visible: Range<u64>, spawner: &mut dyn Spawner) {
        self.drain();
        if self.requery.is_some() || self.status == Status::Running {
            return;
        }
        let Some(source) = self.source.clone() else { return };
        // with a client sort every row is loaded: the visible range maps anywhere
        let range = if self.client_order.is_some() { 0..self.total.rows().max(1) } else { visible };
        // an instant source answers at once: keep planning until nothing is missing (bounded)
        for _ in 0..if self.instant { 16 } else { 1 } {
            if !self.plan_and_fetch(range.clone(), &source, spawner) {
                break;
            }
        }
        self.drain();
    }

    /// One round of [`GridState::pump`]: returns whether it fetched anything.
    fn plan_and_fetch(&mut self, range: Range<u64>, source: &SharedSource, spawner: &mut dyn Spawner) -> bool {
        self.drain();
        let plan = self.cache.plan(range, self.total);
        let fetched = !plan.fetch.is_empty();
        for f in plan.cancel {
            if let Some(j) = f.job {
                spawner.cancel(j);
            }
        }
        let rows = self.cache.page_rows();
        for p in plan.fetch {
            let flight = self.cache.begin(p);
            let generation = self.cache.generation();
            let first = p.saturating_mul(u64::from(rows));
            if self.instant {
                let result = source.lock().unwrap_or_else(PoisonError::into_inner).fetch(first, rows);
                push(&self.inbox, Arrival::Page { generation, index: p, result });
                continue;
            }
            let (src, inbox, cancel) = (source.clone(), self.inbox.clone(), flight.cancel.clone());
            let job = spawner.spawn(
                format!("Fetching rows {}–{}", first.saturating_add(1), first.saturating_add(u64::from(rows))),
                Box::new(move || {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = src.lock().unwrap_or_else(PoisonError::into_inner).fetch(first, rows);
                    if !cancel.load(Ordering::Relaxed) {
                        push(&inbox, Arrival::Page { generation, index: p, result });
                    }
                }),
            );
            self.cache.set_job(p, job);
        }
        fetched
    }

    /// Apply what fetches handed back.
    fn drain(&mut self) {
        let arrived = std::mem::take(&mut *self.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        for a in arrived {
            match a {
                Arrival::Page { generation, index, result: Ok(page) } => {
                    if let Some(t) = self.cache.accept(generation, index, page, self.total) {
                        self.total = t;
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
                Arrival::Page { generation, index, result: Err(e) } => {
                    if generation == self.cache.generation() {
                        self.cache.failed(generation, index);
                        self.status = Status::Error { message: e, position: None };
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
                Arrival::Requeried { generation, result } => {
                    if generation != self.cache.generation() {
                        continue;
                    }
                    self.requery = None;
                    match result {
                        Ok(total) => {
                            self.total = total;
                            self.status = Status::Ready;
                        }
                        Err(e) => self.status = Status::Error { message: e, position: None },
                    }
                    self.revision = self.revision.wrapping_add(1);
                }
            }
        }
    }

    /// Whether a sort can run here, on the loaded rows (a small, fully loaded result).
    pub fn sorts_on_client(&self) -> bool {
        matches!(self.total, Total::Known(n) if n <= KEEP_ALL_ROWS && self.cache.has_all(n))
    }

    /// Sort by `order` (`None`: the result's own order). A small, fully loaded result is sorted
    /// here; anything else re-runs the statement with an `ORDER BY`.
    pub fn sort_by(&mut self, order: Option<Order>, spawner: &mut dyn Spawner) {
        self.sort = order;
        self.selection.clear();
        self.detail = None;
        self.editing = None;
        self.revision = self.revision.wrapping_add(1);
        if self.sorts_on_client() {
            self.client_order = order.map(|o| {
                let n = self.total.rows();
                let mut idx: Vec<u64> = (0..n).collect();
                idx.sort_by(|a, b| {
                    let c = compare(self.cache.row(*a).and_then(|r| r.get(o.column)), self.cache.row(*b).and_then(|r| r.get(o.column)));
                    if o.descending { c.reverse() } else { c }
                });
                idx
            });
            return;
        }
        let Some(source) = self.source.clone() else { return };
        self.client_order = None;
        self.window = 0;
        for f in self.cache.reset() {
            if let Some(j) = f.job {
                spawner.cancel(j);
            }
        }
        if let Some(f) = self.requery.take() {
            f.cancel.store(true, Ordering::Relaxed);
            if let Some(j) = f.job {
                spawner.cancel(j);
            }
        }
        let generation = self.cache.generation();
        let mut flight = Flight::new();
        let (inbox, cancel) = (self.inbox.clone(), flight.cancel.clone());
        let work = move || {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let result = source.lock().unwrap_or_else(PoisonError::into_inner).requery(order);
            push(&inbox, Arrival::Requeried { generation, result });
        };
        if self.instant {
            work();
        } else {
            flight.job = spawner.spawn("Sorting".into(), Box::new(work));
        }
        self.requery = Some(flight);
        self.drain();
    }

    /// Next sort for a click on source column `col`'s header: ascending, descending, none.
    pub fn next_sort(&self, col: usize) -> Option<Order> {
        match self.sort {
            Some(o) if o.column == col && !o.descending => Some(Order { column: col, descending: true }),
            Some(o) if o.column == col => None,
            _ => Some(Order { column: col, descending: false }),
        }
    }

    /// The selected cells: shown rows (window-relative, ascending) × source columns. Whole rows
    /// when no column span is set; the cursor's row when nothing is selected.
    pub fn selected(&self) -> (Vec<u64>, Vec<usize>) {
        let shown = self.layout.shown();
        let cols = match self.cells.range() {
            Some(r) => shown.get(r.start.min(shown.len())..r.end.min(shown.len())).map(<[usize]>::to_vec).unwrap_or_default(),
            None => shown,
        };
        let rows = self.selection.iter().map(|r| r as u64).collect();
        (rows, cols)
    }

    /// Copy the selection in `format`: the text goes to the clipboard on the next frame. Returns
    /// (rows, columns) copied. Rows that are not loaded are left out, and the notice says so.
    pub fn copy(&mut self, format: Format) -> (usize, usize) {
        let (rows, cols) = self.selected();
        let names: Vec<&str> = cols.iter().filter_map(|&c| self.columns.get(c).map(|c| c.name.as_str())).collect();
        let mut picked: Vec<Vec<&Value>> = Vec::new();
        let mut missing = 0usize;
        for &r in rows.iter().take(MAX_COPY_ROWS) {
            match self.row(self.window.saturating_add(r)) {
                Some(row) => picked.push(cols.iter().map(|&c| row.get(c).unwrap_or(&Value::Null)).collect()),
                None => missing += 1,
            }
        }
        let table = self.target.as_ref().map(|t| &t.table);
        let text = copy::copy(format, &names, &picked, table, self.dialect);
        let n = picked.len();
        let left_out = missing + rows.len().saturating_sub(MAX_COPY_ROWS);
        self.notice = Some(if left_out > 0 {
            format!("Copied {n} rows as {} · {left_out} not loaded, left out", format.name().to_uppercase())
        } else {
            format!("Copied {n} {} as {}", if n == 1 { "row" } else { "rows" }, format.name().to_uppercase())
        });
        if n > 0 {
            self.clipboard = Some(text);
        }
        (n, names.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::source::MemSource;

    fn grid(n: u64, deferred: bool) -> GridState {
        let src = MemSource::generated(n, Arc::new(|i| vec![Value::Int((i * 7 % 13) as i64), Value::text(format!("r{i}"))]));
        let src = if deferred { src.deferred() } else { src };
        let total = src.total();
        GridState::open(Arc::from(vec![Column::new("k", "int8"), Column::new("name", "text")]), total, Box::new(src)).page_rows(100)
    }

    /// Spawns real threads and records cancels.
    #[derive(Default)]
    struct Threads {
        handles: Vec<std::thread::JoinHandle<()>>,
        cancelled: Vec<u64>,
    }

    impl Spawner for Threads {
        fn spawn(&mut self, _label: String, work: Box<dyn FnOnce() + Send>) -> Option<u64> {
            self.handles.push(std::thread::spawn(work));
            Some(self.handles.len() as u64)
        }
        fn cancel(&mut self, job: u64) {
            self.cancelled.push(job);
        }
    }

    impl Threads {
        fn join(&mut self) {
            for h in self.handles.drain(..) {
                h.join().unwrap();
            }
        }
    }

    #[test]
    fn instant_sources_fill_at_once() {
        let mut g = grid(1_000, false);
        g.pump(0..30, &mut Inline);
        assert_eq!(g.cell(5, 1), Some(&Value::text("r5")));
        assert_eq!(g.cell(150, 1), Some(&Value::text("r150")), "the next page is fetched ahead");
        // 1,000 rows is a small result: fetched whole
        assert_eq!(g.cell(999, 1), Some(&Value::text("r999")));
        let mut big = grid(1_000_000, false);
        big.pump(0..30, &mut Inline);
        assert!(big.cell(250, 1).is_none(), "a big one only around the viewport");
    }

    #[test]
    fn deferred_sources_fetch_in_jobs_and_stale_pages_are_dropped() {
        let mut g = grid(100_000, true);
        let mut t = Threads::default();
        g.pump(0..30, &mut t);
        assert!(g.is_loading());
        t.join();
        g.pump(0..30, &mut t);
        assert_eq!(g.cell(10, 1), Some(&Value::text("r10")));
        // jump: the old pages are evicted
        g.pump(50_000..50_030, &mut t);
        t.join();
        g.pump(50_000..50_030, &mut t);
        assert!(g.cell(10, 1).is_none());
        assert_eq!(g.cell(50_001, 1), Some(&Value::text("r50001")));
        let (pages, bytes) = g.cache.footprint();
        assert!(pages <= 3 && bytes < 100_000, "{pages} pages, {bytes} bytes");
    }

    #[test]
    fn small_results_sort_on_the_client_large_ones_requery() {
        let mut g = grid(300, false);
        g.pump(0..30, &mut Inline);
        g.pump(250..280, &mut Inline);
        assert!(g.sorts_on_client());
        g.sort_by(Some(Order { column: 0, descending: false }), &mut Inline);
        assert_eq!(g.cell(0, 0), Some(&Value::Int(0)));
        assert_eq!(g.cell(299, 0), Some(&Value::Int(12)));
        g.sort_by(None, &mut Inline);
        assert_eq!(g.cell(1, 1), Some(&Value::text("r1")));

        let rows: Vec<Vec<Value>> = (0..50_000).map(|i| vec![Value::Int(50_000 - i)]).collect();
        let src = MemSource::new(rows);
        let mut g = GridState::open(Arc::from(vec![Column::new("n", "int8")]), src.total(), Box::new(src)).page_rows(100);
        g.pump(0..30, &mut Inline);
        assert!(!g.sorts_on_client());
        let generation = g.cache.generation();
        g.sort_by(Some(Order { column: 0, descending: false }), &mut Inline);
        assert_eq!(g.cache.generation(), generation + 1, "a re-query starts a new generation");
        g.pump(0..30, &mut Inline);
        assert_eq!(g.cell(0, 0), Some(&Value::Int(1)));
        assert_eq!(g.next_sort(0), Some(Order { column: 0, descending: true }));
    }

    #[test]
    fn copy_takes_the_selected_cells() {
        let mut g = grid(1_000_000, false);
        g.pump(0..30, &mut Inline);
        g.selection.select_only(2);
        g.selection.extend_to(3);
        g.cells.span = Some((1, 1));
        assert_eq!(g.copy(Format::Tsv), (2, 1));
        assert_eq!(g.clipboard.as_deref(), Some("name\nr2\nr3\n"));
        g.selection.select_only(900_000);
        g.cells.span = None;
        assert_eq!(g.copy(Format::Csv), (0, 2));
        assert!(g.notice.as_deref().unwrap_or("").contains("not loaded"));
    }

    #[test]
    fn errors_surface_and_windows_page_past_half_a_million_rows() {
        let src = MemSource::new(vec![]).failing("disk I/O error");
        let mut g = GridState::open(Arc::from(vec![Column::new("n", "int8")]), Total::AtLeast(0), Box::new(src));
        g.pump(0..30, &mut Inline);
        assert_eq!(g.status, Status::Error { message: "disk I/O error".into(), position: None });

        let mut g = grid(10_000_000, false);
        assert_eq!(g.window_rows(), WINDOW_ROWS);
        g.set_window(9_999_999);
        assert_eq!(g.window, 9_500_000);
        assert_eq!(g.window_rows(), 500_000);
        assert_eq!(g.window_more(), (true, false));
        g.pump(9_999_990..10_000_000, &mut Inline);
        assert_eq!(g.cell(9_999_999, 1), Some(&Value::text("r9999999")));
    }
}
