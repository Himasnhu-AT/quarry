//! Handlers of the connection commands (`conn.*`, `table.open`). Harmless from any surface: they
//! open dialogs, connect or disconnect the person's connection, or open a tab. Saving, deleting
//! and agent access are human-only (`conn.save`, `conn.delete`), not commands.
//!
//! A command body cannot start a job or read settings in ashlar 0.2, so connection work is a
//! [`Request`] the window picks up on its next frame.

use ashlar::prelude::*;
use quarry_core::{ConnectionId, Tab, TabId, TabKind, TableRef, Workspace};

use crate::view::{Request, request};

type S = Session<Workspace>;
type R = Result<Value, CommandError>;

fn conn(p: &Value) -> Result<ConnectionId, CommandError> {
    p.get("connection")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ConnectionId::new)
        .ok_or_else(|| CommandError::failed("", "connection is required"))
}

pub fn conn_new(s: &mut S, _: &Value) -> R {
    request(s, Request::NewConnection);
    Ok(json!({"dialog": "connection"}))
}

pub fn conn_edit(s: &mut S, p: &Value) -> R {
    let connection = conn(p)?;
    request(s, Request::EditConnection { connection });
    Ok(json!({"dialog": "connection"}))
}

pub fn conn_open(s: &mut S, p: &Value) -> R {
    let connection = conn(p)?;
    request(s, Request::Connect { connection: connection.clone() });
    Ok(json!({"connection": connection, "status": "connecting"}))
}

pub fn conn_close(s: &mut S, p: &Value) -> R {
    let connection = conn(p)?;
    request(s, Request::Disconnect { connection: connection.clone() });
    Ok(json!({"connection": connection, "status": "closed"}))
}

pub fn conn_test(s: &mut S, p: &Value) -> R {
    let connection = conn(p)?;
    request(s, Request::Test { connection: connection.clone() });
    Ok(json!({"connection": connection, "status": "testing"}))
}

/// `table.open`: with `connection` and `table`, open a table tab; otherwise show the picker.
pub fn table_open(s: &mut S, p: &Value) -> R {
    let connection = p.get("connection").and_then(Value::as_str).filter(|c| !c.is_empty()).map(ConnectionId::new);
    let table = p.get("table").and_then(Value::as_str).and_then(TableRef::parse);
    match (connection, table) {
        (Some(c), Some(t)) => Ok(json!({"tab": open_table_tab(s, c, t)})),
        (c, _) => {
            request(s, Request::TablePicker { connection: c });
            Ok(json!({"dialog": "open_table"}))
        }
    }
}

/// Open (or focus) a table tab.
pub fn open_table_tab(s: &mut S, connection: ConnectionId, table: TableRef) -> TabId {
    if let Some(id) = s
        .doc()
        .tabs
        .iter()
        .find(|t| t.connection.as_ref() == Some(&connection) && matches!(&t.kind, TabKind::Table { table: x, .. } if *x == table))
        .map(|t| t.id)
    {
        s.edit_merged("Select Tab", "quarry.tab.select", |w| w.active = Some(id));
        return id;
    }
    s.edit("Open Table", |w| {
        let id = TabId(w.next_id());
        w.tabs.push(Tab { id, title: table.name.clone(), connection: Some(connection), kind: TabKind::Table { table, filter: None } });
        w.active = Some(id);
        id
    })
}

#[cfg(test)]
mod tests {
    use crate::ids;
    use crate::view::{REQUESTS, Request};
    use ashlar::command::Dispatch;
    use serde_json::json;

    #[test]
    fn connection_commands_queue_requests_and_table_open_opens_once() {
        let mut h = crate::app().headless();
        h.execute(ids::CONN_OPEN, json!({"connection": "fixture"})).unwrap();
        h.execute(ids::CONN_NEW, json!({})).unwrap();
        let q: Vec<Request> = h.session().view(REQUESTS).unwrap();
        assert_eq!(q, vec![Request::Connect { connection: quarry_core::ConnectionId::new("fixture") }, Request::NewConnection]);
        let a = h.execute(ids::TABLE_OPEN, json!({"connection": "fixture", "table": "main.orders"})).unwrap();
        let b = h.execute(ids::TABLE_OPEN, json!({"connection": "fixture", "table": "main.orders"})).unwrap();
        assert_eq!(a, b);
        assert_eq!(h.session().doc().tabs.len(), 1);
        assert!(h.execute(ids::CONN_OPEN, json!({"connection": ""})).is_err());
    }
}
