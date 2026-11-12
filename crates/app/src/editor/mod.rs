//! The SQL editor (`shell` stream): an `egui::TextEdit` with SQL highlighting
//! (`code_view::highlight_job`) and a line-number gutter, run statement / selection / all on the
//! person's own connection, the inline error banner, and the query history.
//!
//! Running is a human-only action (`query.run`, `query.run_all`): [`start_run`] takes a
//! [`HumanTicket`] by value, and a ticket exists only for a real click or key press while no
//! agent input is being injected. Nothing registered (no command, no MCP tool) reaches it.

pub mod commands;
pub mod history;
pub mod sql;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ashlar::egui::{self, FontId, Rect, Stroke, pos2, vec2};
use ashlar::jobs::JobId;
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use ashlar::ui::code_view::{SyntaxTheme, highlight_job};
use ashlar::ui::tokens;
use quarry_core::limits::PAGE_ROWS;
use quarry_core::{Column, ConnectionId, CursorId, Page, Tab, TabId, TabKind, Total, Workspace};
use quarry_engine::QueryOptions;

use crate::human::{HumanTicket, RunScope};
use crate::sidebar::connections::{Human, lock};
use crate::theme;

/// The first page of a finished run.
#[derive(Clone, Debug)]
pub struct Rows {
    pub columns: Arc<[Column]>,
    pub total: Total,
    pub first: Page,
    pub cursor: CursorId,
}

/// Why a run failed; `position` is the server's 1-based character offset in the statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunError {
    pub message: String,
    pub position: Option<u32>,
    /// Where the failing statement starts in the buffer (bytes).
    pub statement_start: usize,
}

/// One finished run of a tab.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub connection: ConnectionId,
    /// The statements that ran (joined by `;\n`), for the history.
    pub sql: String,
    pub elapsed_ms: u64,
    pub statements: usize,
    pub result: Result<Rows, RunError>,
}

/// A tab's run.
#[derive(Clone, Debug)]
pub enum RunState {
    /// `job` is `None` in scenes.
    Running {
        job: Option<JobId>,
        connection: ConnectionId,
    },
    Done(Outcome),
}

/// A tab's editor state that is not the document: cursor, cached highlighting, its run.
#[derive(Default)]
pub struct TabState {
    /// Char indices of the selection (`primary`, `secondary`).
    pub cursor: Option<(usize, usize)>,
    pub run: Option<RunState>,
    /// A table tab: what its grid shows (`connection|table|filter`); a change reloads it.
    pub table_key: Option<String>,
    /// A table tab: the filter field's text while the person types (`None`: shows the tab's).
    pub filter_text: Option<String>,
    highlight: Option<(u64, egui::text::LayoutJob)>,
    request_focus: bool,
}

impl TabState {
    pub fn focus(&mut self) {
        self.request_focus = true;
    }
}

/// Finished jobs land here; the view drains it each frame.
pub type Inbox = Arc<Mutex<Vec<(TabId, Outcome)>>>;

/// The statements `scope` runs from `buffer` with selection `cursor` (char indices): `(start
/// byte, text)` pairs.
pub fn statements_for(buffer: &str, cursor: Option<(usize, usize)>, scope: RunScope) -> Vec<(usize, String)> {
    let pick = |r: std::ops::Range<usize>| buffer.get(r.clone()).map(|s| (r.start, s.to_string()));
    let (a, b) =
        cursor.map(|(p, s)| (sql::byte_of_char(buffer, p.min(s)), sql::byte_of_char(buffer, p.max(s)))).unwrap_or((buffer.len(), buffer.len()));
    match scope {
        RunScope::All => sql::statements(buffer).into_iter().filter_map(pick).collect(),
        RunScope::Selection if a < b => buffer
            .get(a..b)
            .map(|sel| sql::statements(sel).into_iter().filter_map(|r| sel.get(r.clone()).map(|s| (a + r.start, s.to_string()))).collect())
            .unwrap_or_default(),
        // ⌘↵ with a selection runs the selection
        RunScope::Statement if a < b => statements_for(buffer, cursor, RunScope::Selection),
        RunScope::Statement | RunScope::Selection => sql::statement_at(buffer, a).and_then(pick).into_iter().collect(),
    }
}

/// Start running `statements` of `tab` on the person's connection: `query.run` / `query.run_all`,
/// human-only. The ticket is consumed.
pub fn start_run(
    cx: &mut Cx<'_, Workspace>,
    ticket: HumanTicket,
    tab: TabId,
    connection: ConnectionId,
    human: Human,
    statements: Vec<(usize, String)>,
    inbox: &Inbox,
) -> Option<JobId> {
    let _consumed: HumanTicket = ticket;
    if statements.is_empty() {
        return None;
    }
    log::info!("human action {} ({} statement(s))", crate::ids::QUERY_RUN, statements.len());
    let inbox = Arc::clone(inbox);
    let conn = connection.clone();
    let label = format!("Running {} statement(s)", statements.len());
    let job = cx.jobs.job("query", label).spawn((human, statements), move |(human, statements), progress| {
        let started = Instant::now();
        let count = statements.len();
        let mut last: Result<Rows, RunError> = Err(RunError { message: "nothing ran".into(), position: None, statement_start: 0 });
        let mut ran = Vec::new();
        for (start, text) in statements {
            if progress.is_cancelled() {
                last = Err(RunError { message: "cancelled".into(), position: None, statement_start: start });
                break;
            }
            ran.push(text.clone());
            let mut h = lock(&human);
            let r = h.query(&text, &QueryOptions::default()).and_then(|cur| {
                let first = h.fetch(cur.id, 0, PAGE_ROWS)?;
                Ok(Rows { columns: cur.columns, total: cur.total, first, cursor: cur.id })
            });
            drop(h);
            match r {
                Ok(rows) => last = Ok(rows),
                Err(e) => {
                    let position = match &e {
                        quarry_engine::EngineError::Database { position, .. } => *position,
                        _ => None,
                    };
                    last = Err(RunError { message: e.to_string(), position, statement_start: start });
                    break;
                }
            }
        }
        let outcome = Outcome {
            connection: conn,
            sql: ran.join(";\n"),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            statements: count,
            result: last,
        };
        inbox.lock().unwrap_or_else(PoisonError::into_inner).push((tab, outcome));
        Ok(apply(|_: &mut Session<Workspace>| Ok(Value::Null)))
    });
    Some(job)
}

/// Draw the editor of an editor tab; edits go into the document as merged undo steps.
pub fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, tab: &Tab, st: &mut TabState) {
    let t = tokens::current(ui.ctx());
    let TabKind::Editor { sql: doc_sql } = &tab.kind else {
        if let TabKind::Table { table, filter } = &tab.kind {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let r = ui.label(egui::RichText::new(table.to_string()).monospace().color(t.text.secondary));
                automation::register(ui, &r, "quarry.editor.table", &tab.title, Role::Label);
                ui.add_space(12.0);
                ui.label(egui::RichText::new("WHERE").monospace().color(t.text.disabled));
                filter_field(ui, cx, tab, filter.as_deref(), st);
            });
        }
        return;
    };
    let mut text = doc_sql.clone();
    let theme_syn = SyntaxTheme::from_tokens(&t);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        t.dark.hash(&mut h);
        h.finish()
    };
    let cache = &mut st.highlight;
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let s = buf.as_str();
        let mut job = match cache {
            Some((h, job)) if *h == hash && s.len() == job.text.len() => job.clone(),
            _ => {
                let job = highlight_job(Some("sql"), s, FontId::monospace(theme::MONO_SIZE), &theme_syn);
                *cache = Some((hash, job.clone()));
                job
            }
        };
        job.wrap.max_width = wrap;
        ui.painter().layout_job(job)
    };
    let gutter = 40.0;
    let avail = ui.available_rect_before_wrap();
    egui::ScrollArea::vertical().id_salt(("quarry.editor.scroll", tab.id.0)).auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            let (gutter_rect, _) = ui.allocate_exact_size(vec2(gutter, avail.height().max(40.0)), egui::Sense::hover());
            let edit = egui::TextEdit::multiline(&mut text)
                .id(egui::Id::new(("quarry.editor", tab.id.0)))
                .code_editor()
                .frame(egui::Frame::NONE)
                .hint_text("-- write SQL here; Cmd+Enter runs the statement under the cursor")
                .desired_width(f32::INFINITY)
                .desired_rows(8)
                .lock_focus(true)
                .layouter(&mut layouter);
            let out = edit.show(ui);
            automation::register(ui, &out.response.response, "quarry.editor", &tab.title, Role::Field);
            if st.request_focus {
                out.response.response.request_focus();
                st.request_focus = false;
            }
            if let Some(r) = out.cursor_range {
                st.cursor = Some((r.primary.index.into(), r.secondary.index.into()));
            }
            // line numbers
            let p = ui.painter();
            let mut line = 1usize;
            let mut new_line = true;
            for row in &out.galley.rows {
                let y = out.galley_pos.y + row.pos.y;
                if new_line {
                    let g = p.layout_no_wrap(line.to_string(), FontId::monospace(11.0), t.text.disabled);
                    let x = gutter_rect.right() - 10.0 - g.size().x;
                    p.galley(pos2(x, y + (row.rect().height() - g.size().y) / 2.0), g, t.text.disabled);
                    line += 1;
                }
                new_line = row.ends_with_newline;
            }
            p.line_segment(
                [
                    pos2(gutter_rect.right() - 4.0, gutter_rect.top()),
                    pos2(gutter_rect.right() - 4.0, gutter_rect.bottom().max(out.galley_pos.y + out.galley.rect.height())),
                ],
                Stroke::new(1.0, t.separator),
            );
            if out.response.response.changed() {
                let id = tab.id;
                let new_text = text.clone();
                cx.edit_merged("Edit SQL", &format!("quarry.sql.{}", id.0), move |w| {
                    if let Some(TabKind::Editor { sql }) = w.tab_mut(id).map(|t| &mut t.kind) {
                        *sql = new_text;
                    }
                });
            }
        });
    });
}

/// A table tab's filter: Enter applies it through `grid.filter` (guard-checked), empty clears.
fn filter_field(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, tab: &Tab, applied: Option<&str>, st: &mut TabState) {
    let mut text = st.filter_text.clone().unwrap_or_else(|| applied.unwrap_or("").to_string());
    let r = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id(egui::Id::new(("quarry.grid.filter", tab.id.0)))
            .font(FontId::monospace(theme::MONO_SIZE))
            .hint_text("status = 'paid'  (Cmd+F, Enter applies)")
            .desired_width(ui.available_width().min(520.0)),
    );
    automation::register(ui, &r, "quarry.grid.filter", "Filter", Role::Field);
    if cx.view::<u64>(crate::grid::commands::FILTER_FOCUS) == Some(tab.id.0) {
        r.request_focus();
        cx.set_view(crate::grid::commands::FILTER_FOCUS, &0u64);
    }
    if r.changed() {
        st.filter_text = Some(text.clone());
    }
    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        st.filter_text = None;
        cx.dispatch(crate::ids::GRID_FILTER, json!({"tab": tab.id, "where": text}));
    }
}

/// The inline error banner under the editor, with the error's line and column when the server
/// gave a position.
pub fn error_banner(ui: &mut egui::Ui, buffer: &str, e: &RunError) {
    let where_ =
        e.position.and_then(|p| sql::line_col(buffer, e.statement_start, p)).map(|(l, c)| format!(" (line {l}, column {c})")).unwrap_or_default();
    let text = format!("{}{where_}", e.message);
    let frame = egui::Frame::new()
        .fill(theme::ERROR.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, theme::ERROR.gamma_multiply(0.6)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(12, 8));
    let r = frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::strong("ERROR", 12.0).color(theme::ERROR));
                ui.label(egui::RichText::new(&text).monospace().color(theme::TEXT));
            });
        })
        .response;
    automation::register(ui, &r, "quarry.editor.error", &text, Role::Alert);
}

/// A thin progress strip while a run is in flight (reduced motion: static).
pub fn running_strip(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 2.0), egui::Sense::hover());
    let time = ui.input(|i| i.time) as f32;
    let reduced = ashlar::ui::motion::prefs(ui.ctx()).reduced;
    let w = rect.width() * 0.25;
    let x = if reduced { rect.left() } else { rect.left() + (time * 0.6).fract() * (rect.width() + w) - w };
    let bar = Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), pos2((x + w).min(rect.right()), rect.bottom()));
    ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
    if !reduced {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_pick_the_right_statements() {
        let buf = "SELECT 1;\nSELECT 2;\nSELECT 3";
        let texts = |v: Vec<(usize, String)>| v.into_iter().map(|(_, s)| s).collect::<Vec<_>>();
        assert_eq!(texts(statements_for(buf, Some((12, 12)), RunScope::Statement)), ["SELECT 2"]);
        assert_eq!(texts(statements_for(buf, None, RunScope::All)), ["SELECT 1", "SELECT 2", "SELECT 3"]);
        // a selection across two statements runs both, with their buffer offsets
        let sel = statements_for(buf, Some((0, 19)), RunScope::Statement);
        assert_eq!(sel, vec![(0, "SELECT 1".to_string()), (10, "SELECT 2".to_string())]);
        assert_eq!(texts(statements_for("", None, RunScope::Statement)), Vec::<String>::new());
        assert_eq!(texts(statements_for(buf, None, RunScope::Statement)), ["SELECT 3"], "no cursor: the last statement");
    }
}
