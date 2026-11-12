//! Handlers of the editor and tab commands (`tab.close`, `tab.select`, `query.cancel`,
//! `query.format`, `history.show`). None runs SQL: running is human-only (`query.run`).

use ashlar::prelude::*;
use quarry_core::{TabId, TabKind, Workspace};

use crate::view::{Request, request};

type S = Session<Workspace>;
type R = Result<Value, CommandError>;

fn tab_param(s: &S, p: &Value) -> Result<TabId, CommandError> {
    let id = p.get("tab").and_then(Value::as_u64).map(TabId).or(s.doc().active).ok_or_else(|| CommandError::failed("", "no tab"))?;
    if s.doc().tab(id).is_none() {
        return Err(CommandError::failed("", format!("no tab {}", id.0)));
    }
    Ok(id)
}

pub fn tab_close(s: &mut S, p: &Value) -> R {
    let id = tab_param(s, p)?;
    s.edit("Close Tab", |w| {
        let at = w.tabs.iter().position(|t| t.id == id).unwrap_or(0);
        w.tabs.retain(|t| t.id != id);
        w.charts.retain(|c| c.tab != Some(id));
        if w.active == Some(id) {
            w.active = w.tabs.get(at.min(w.tabs.len().saturating_sub(1))).map(|t| t.id);
        }
    });
    Ok(json!({"closed": id}))
}

pub fn tab_select(s: &mut S, p: &Value) -> R {
    let id = tab_param(s, p)?;
    if s.doc().active != Some(id) {
        s.edit_merged("Select Tab", "quarry.tab.select", |w| w.active = Some(id));
    }
    Ok(json!({"tab": id}))
}

pub fn query_cancel(s: &mut S, p: &Value) -> R {
    let tab = p.get("tab").and_then(Value::as_u64).map(TabId).or(s.doc().active);
    request(s, Request::Cancel { tab });
    Ok(json!({"cancelled": tab}))
}

pub fn query_format(s: &mut S, p: &Value) -> R {
    let id = tab_param(s, p)?;
    let Some(TabKind::Editor { sql }) = s.doc().tab(id).map(|t| &t.kind) else {
        return Err(CommandError::failed("", "not an editor tab"));
    };
    let formatted = super::sql::format(sql);
    if &formatted != sql {
        s.edit("Format SQL", |w| {
            if let Some(TabKind::Editor { sql }) = w.tab_mut(id).map(|t| &mut t.kind) {
                *sql = formatted;
            }
        });
    }
    Ok(json!({"tab": id}))
}

pub fn history_show(s: &mut S, p: &Value) -> R {
    let search = p.get("search").and_then(Value::as_str).map(str::to_string);
    request(s, Request::History { search });
    Ok(json!({"dialog": "history"}))
}

#[cfg(test)]
mod tests {
    use crate::ids;
    use ashlar::command::Dispatch;
    use quarry_core::TabKind;
    use serde_json::json;

    #[test]
    fn tabs_close_select_and_format() {
        let mut h = crate::app().headless();
        h.execute(ids::TAB_NEW_EDITOR, json!({})).unwrap();
        h.execute(ids::TAB_NEW_EDITOR, json!({})).unwrap();
        assert_eq!(h.session().doc().active.map(|t| t.0), Some(2));
        h.execute(ids::TAB_SELECT, json!({"tab": 1})).unwrap();
        assert_eq!(h.session().doc().active.map(|t| t.0), Some(1));
        h.session_mut().edit("t", |w| {
            if let Some(TabKind::Editor { sql }) = w.tabs.first_mut().map(|t| &mut t.kind) {
                *sql = "select 1 from t where x = 'a'".into();
            }
        });
        h.execute(ids::QUERY_FORMAT, json!({})).unwrap();
        assert!(matches!(&h.session().doc().tabs[0].kind, TabKind::Editor { sql } if sql == "SELECT 1\nFROM t\nWHERE x = 'a'\n"));
        h.execute(ids::TAB_CLOSE, json!({"tab": 1})).unwrap();
        assert_eq!(h.session().doc().active.map(|t| t.0), Some(2));
        assert!(h.execute(ids::TAB_CLOSE, json!({"tab": 9})).is_err());
        h.execute(ids::HISTORY_SHOW, json!({"search": "orders"})).unwrap();
        h.execute(ids::QUERY_CANCEL, json!({})).unwrap();
    }
}
