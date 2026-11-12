//! The page cache: the pages around the viewport, fetched ahead and evicted behind, so memory
//! stays bounded at any result size (PLAN §6: visible pages and their neighbours).

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use quarry_core::{Page, Total, Value};

/// Pages fetched beyond each side of the visible ones, and kept: the rest is evicted.
pub const NEIGHBOURS: u64 = 1;
/// Fetches in flight at once (stale ones are cancelled first).
pub const MAX_IN_FLIGHT: usize = 4;
/// A result this small, once fully known, is kept whole (and sorted on the client).
pub const KEEP_ALL_ROWS: u64 = 10_000;

/// One fetch in flight: its cancel flag (checked by the job before it touches the source) and
/// its job id.
#[derive(Clone, Debug)]
pub struct Flight {
    pub cancel: Arc<AtomicBool>,
    pub job: Option<u64>,
}

impl Flight {
    pub fn new() -> Self {
        Self { cancel: Arc::new(AtomicBool::new(false)), job: None }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Default for Flight {
    fn default() -> Self {
        Self::new()
    }
}

/// What to do after the viewport moved.
#[derive(Debug, Default)]
pub struct Plan {
    /// Page indexes to fetch, most urgent first (visible, then the nearest neighbours).
    pub fetch: Vec<u64>,
    /// Fetches no longer wanted: cancel them.
    pub cancel: Vec<Flight>,
}

struct Cached {
    page: Page,
    bytes: usize,
}

/// Pages of one result generation (a re-sort starts a new one).
pub struct PageCache {
    page_rows: u32,
    pages: BTreeMap<u64, Cached>,
    in_flight: HashMap<u64, Flight>,
    generation: u64,
    keep_all: bool,
}

/// Approximate heap bytes of a value.
fn value_bytes(v: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match v {
            Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => s.len(),
            Value::Clipped { preview, .. } => preview.len(),
            Value::Bytes { preview, .. } => preview.len(),
            _ => 0,
        }
}

impl PageCache {
    pub fn new(page_rows: u32) -> Self {
        Self { page_rows: page_rows.max(1), pages: BTreeMap::new(), in_flight: HashMap::new(), generation: 0, keep_all: false }
    }

    pub fn page_rows(&self) -> u32 {
        self.page_rows
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drop everything for a new generation; returns the flights to cancel.
    pub fn reset(&mut self) -> Vec<Flight> {
        self.generation = self.generation.wrapping_add(1);
        self.pages.clear();
        self.keep_all = false;
        self.in_flight.drain().map(|(_, f)| f).collect()
    }

    fn page_of(&self, row: u64) -> u64 {
        row / u64::from(self.page_rows)
    }

    /// The row, if its page is here.
    pub fn row(&self, row: u64) -> Option<&Vec<Value>> {
        let c = self.pages.get(&self.page_of(row))?;
        let at = usize::try_from(row.checked_sub(c.page.first_row)?).ok()?;
        c.page.rows.get(at)
    }

    /// Whether rows `0..n` are all here.
    pub fn has_all(&self, n: u64) -> bool {
        let pages = n.div_ceil(u64::from(self.page_rows));
        (0..pages).all(|p| self.pages.contains_key(&p))
    }

    pub fn is_loading(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Pages held and their approximate size in bytes.
    pub fn footprint(&self) -> (usize, usize) {
        (self.pages.len(), self.pages.values().map(|c| c.bytes).sum())
    }

    /// Start tracking a fetch of `page` (the caller spawns it).
    pub fn begin(&mut self, page: u64) -> Flight {
        let f = Flight::new();
        self.in_flight.insert(page, f.clone());
        f
    }

    /// Note the job id of a fetch that was begun.
    pub fn set_job(&mut self, page: u64, job: Option<u64>) {
        if let Some(f) = self.in_flight.get_mut(&page) {
            f.job = job;
        }
    }

    /// The viewport shows rows `visible` of a result with `total` rows: what to fetch, what to
    /// cancel; pages far from it are evicted now.
    pub fn plan(&mut self, visible: Range<u64>, total: Total) -> Plan {
        let pr = u64::from(self.page_rows);
        // a known small result is kept whole
        if let Total::Known(n) = total
            && n <= KEEP_ALL_ROWS
        {
            self.keep_all = true;
        }
        let first = self.page_of(visible.start);
        let last = self.page_of(visible.end.saturating_sub(1).max(visible.start));
        let lo = first.saturating_sub(NEIGHBOURS);
        let hi = last.saturating_add(NEIGHBOURS);
        // rows that may exist: all known ones, or one page past what a streaming cursor has shown
        let limit = match total {
            Total::Known(n) => n,
            Total::AtLeast(n) => n.saturating_add(pr),
        };
        let mut plan = Plan::default();
        if !self.keep_all {
            self.pages.retain(|p, _| (lo..=hi).contains(p));
        }
        let stale: Vec<u64> = self.in_flight.keys().copied().filter(|p| !(lo..=hi).contains(p)).collect();
        for p in stale {
            if let Some(f) = self.in_flight.remove(&p) {
                f.cancel.store(true, Ordering::Relaxed);
                plan.cancel.push(f);
            }
        }
        // visible pages first, then neighbours nearest first
        let mut want: Vec<u64> = (first..=last).collect();
        for d in 1..=NEIGHBOURS {
            want.push(last.saturating_add(d));
            if let Some(p) = first.checked_sub(d) {
                want.push(p);
            }
        }
        // a small known result is fetched whole (then it sorts on the client)
        if self.keep_all {
            want.extend(0..limit.div_ceil(pr));
        }
        for p in want {
            if self.in_flight.len() + plan.fetch.len() >= MAX_IN_FLIGHT {
                break;
            }
            let start = p.saturating_mul(pr);
            if start >= limit || self.pages.contains_key(&p) || self.in_flight.contains_key(&p) || plan.fetch.contains(&p) {
                continue;
            }
            plan.fetch.push(p);
        }
        plan
    }

    /// A fetch of `page` for `generation` finished. Returns the total it implies, or `None` when
    /// the page is stale (another generation, or cancelled).
    pub fn accept(&mut self, generation: u64, page_index: u64, page: Page, total: Total) -> Option<Total> {
        if generation != self.generation {
            return None;
        }
        let flight = self.in_flight.remove(&page_index)?;
        if flight.cancelled() {
            return None;
        }
        let pr = u64::from(self.page_rows);
        let start = page_index.saturating_mul(pr);
        let end = start.saturating_add(page.rows.len() as u64);
        let total = if page.rows.is_empty() {
            // nothing at `start`: the result ends at or before it
            Total::Known(total.rows().min(start))
        } else if (page.rows.len() as u64) < pr {
            // a short page ends the result
            Total::Known(end)
        } else {
            total.at_least(end)
        };
        if !page.rows.is_empty() {
            let bytes = page.rows.iter().flatten().map(value_bytes).sum::<usize>() + page.rows.len() * std::mem::size_of::<Vec<Value>>();
            self.pages.insert(page_index, Cached { page, bytes });
        }
        if let Total::Known(n) = total
            && n <= KEEP_ALL_ROWS
        {
            self.keep_all = true;
        }
        Some(total)
    }

    /// A fetch failed: forget it (the next plan asks again).
    pub fn failed(&mut self, generation: u64, page_index: u64) {
        if generation == self.generation {
            self.in_flight.remove(&page_index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(first: u64, n: u64) -> Page {
        Page { first_row: first, rows: (first..first + n).map(|i| vec![Value::Int(i as i64)]).collect() }
    }

    #[test]
    fn plans_visible_pages_first_then_neighbours_and_evicts_far_ones() {
        let mut c = PageCache::new(100);
        let total = Total::Known(1_000_000);
        let plan = c.plan(500..530, total);
        assert_eq!(plan.fetch, vec![5, 6, 4]);
        for p in plan.fetch {
            c.begin(p);
            assert!(c.accept(0, p, page(p * 100, 100), total).is_some());
        }
        assert!(c.row(450).is_some() && c.row(650).is_some());
        // jump far: the old pages go, and nothing stale is in flight
        let plan = c.plan(90_000..90_030, total);
        assert_eq!(plan.fetch, vec![900, 901, 899]);
        assert_eq!(c.footprint().0, 0);
        for p in &plan.fetch {
            c.begin(*p);
        }
        // scroll on before they land: the far fetches are cancelled
        let plan = c.plan(10..40, total);
        assert_eq!(plan.cancel.len(), 3);
        assert!(plan.cancel.iter().all(Flight::cancelled));
        assert_eq!(plan.fetch, vec![0, 1]);
        // a cancelled fetch landing late is ignored
        assert!(c.accept(0, 900, page(90_000, 100), total).is_none());
    }

    #[test]
    fn streaming_totals_grow_and_short_pages_end_the_result() {
        let mut c = PageCache::new(100);
        // nothing known yet: only the first page may exist
        let plan = c.plan(0..30, Total::AtLeast(0));
        assert_eq!(plan.fetch, vec![0]);
        c.begin(0);
        let t = c.accept(0, 0, page(0, 100), Total::AtLeast(0)).unwrap();
        assert_eq!(t, Total::AtLeast(100));
        // a full page: the next one is fetched ahead
        assert_eq!(c.plan(0..30, t).fetch, vec![1]);
        c.begin(1);
        let t = c.accept(0, 1, page(100, 40), t).unwrap();
        assert_eq!(t, Total::Known(140));
        // a stale generation is dropped
        let flights = c.reset();
        assert!(flights.is_empty());
        c.begin(0);
        assert!(c.accept(0, 0, page(0, 100), t).is_none());
    }

    #[test]
    fn small_results_are_kept_whole() {
        let mut c = PageCache::new(100);
        let total = Total::Known(300);
        for p in 0..3 {
            c.begin(p);
            c.accept(0, p, page(p * 100, 100), total);
        }
        c.plan(250..280, total);
        assert!(c.has_all(300));
        let mut big = PageCache::new(100);
        let total = Total::Known(1_000_000);
        for p in 0..3 {
            big.begin(p);
            big.accept(0, p, page(p * 100, 100), total);
        }
        big.plan(250..280, total);
        assert!(!big.has_all(300) && big.row(250).is_some());
    }

    #[test]
    fn in_flight_is_capped() {
        let mut c = PageCache::new(10);
        let plan = c.plan(0..200, Total::Known(1_000));
        assert_eq!(plan.fetch.len(), MAX_IN_FLIGHT);
    }
}
