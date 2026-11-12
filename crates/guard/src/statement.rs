//! What one top-level statement is: a read the agent may run, a write a human may approve, or
//! something refused on every path.

use quarry_core::{EngineKind, TableRef};
use sqlparser::ast::{CopySource, FromTable, ObjectName, ObjectNamePart, Statement, TableFactor, TableObject, TableWithJoins};

use crate::functions::{self, PragmaArg};
use crate::{ReadKind, WriteKind};

/// The verdict on a statement's own kind (its contents are judged by [`crate::walk`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    Read(ReadKind),
    Write(WriteKind),
    /// Refused even as a proposal (transaction control, session state, cursors, ATTACH).
    Never(&'static str),
}

/// Classify the statement itself. Anything not listed is a write of kind `Other`: never a read.
pub fn root(s: &Statement, engine: EngineKind) -> Root {
    use Statement as S;
    match s {
        S::Query(_) => Root::Read(ReadKind::Select),
        S::Explain { statement, analyze, options, .. } => match statement.as_ref() {
            S::Query(_) => {
                let in_options = options.as_ref().is_some_and(|o| o.iter().any(|u| u.name.value.eq_ignore_ascii_case("analyze")));
                Root::Read(ReadKind::Explain { analyze: *analyze || in_options })
            }
            inner => match root(inner, engine) {
                Root::Read(_) => Root::Never("EXPLAIN of anything but a query"),
                other => other,
            },
        },
        S::ShowVariable { .. } if engine == EngineKind::Postgres => Root::Read(ReadKind::Show),
        S::Pragma { name, value, is_eq } => pragma(name, value.is_some(), *is_eq, engine),

        S::Insert(_) => Root::Write(WriteKind::Insert),
        S::Update(_) => Root::Write(WriteKind::Update),
        S::Delete(_) => Root::Write(WriteKind::Delete),
        S::Merge(_) => Root::Write(WriteKind::Merge),
        S::Truncate(_) => Root::Write(WriteKind::Truncate),
        S::Copy { .. } | S::CopyIntoSnowflake { .. } | S::Unload { .. } | S::ExportData(_) | S::LoadData { .. } => Root::Write(WriteKind::Copy),
        S::Call(_) => Root::Write(WriteKind::Call),
        S::Lock(_) | S::LockTables { .. } => Root::Write(WriteKind::Lock),
        S::Vacuum(_) | S::Analyze(_) | S::RefreshMaterializedView(_) | S::OptimizeTable { .. } | S::Msck(_) => Root::Write(WriteKind::Maintenance),
        S::Grant(_)
        | S::Revoke(_)
        | S::GrantRole(_)
        | S::RevokeRole(_)
        | S::Deny(_)
        | S::AlterDefaultPrivileges(_)
        | S::CreateRole(_)
        | S::AlterRole { .. }
        | S::CreateUser(_)
        | S::AlterUser(_) => Root::Write(WriteKind::Dcl),
        S::CreateView(_)
        | S::CreateTable(_)
        | S::CreateVirtualTable { .. }
        | S::CreateIndex(_)
        | S::CreateSchema { .. }
        | S::CreateDatabase { .. }
        | S::CreateFunction(_)
        | S::CreateTrigger(_)
        | S::CreateProcedure { .. }
        | S::CreateSequence { .. }
        | S::CreateDomain(_)
        | S::CreateType { .. }
        | S::CreateExtension(_)
        | S::CreatePolicy(_)
        | S::CreateRule(_)
        | S::CreateServer(_)
        | S::CreateForeignTable(_)
        | S::CreateCollation(_)
        | S::CreateOperator(_)
        | S::CreateAggregate(_)
        | S::AlterTable(_)
        | S::AlterSchema(_)
        | S::AlterIndex { .. }
        | S::AlterView(_)
        | S::AlterFunction(_)
        | S::AlterType(_)
        | S::AlterTrigger(_)
        | S::AlterPolicy(_)
        | S::Drop { .. }
        | S::DropFunction(_)
        | S::DropDomain(_)
        | S::DropProcedure { .. }
        | S::DropPolicy(_)
        | S::DropRule(_)
        | S::DropTrigger(_)
        | S::DropExtension(_)
        | S::DropOperator(_)
        | S::Comment { .. }
        | S::RenameTable(_) => Root::Write(WriteKind::Ddl),

        S::StartTransaction { .. } | S::Commit { .. } | S::Rollback { .. } | S::Savepoint { .. } | S::ReleaseSavepoint { .. } => {
            Root::Never("transaction control: Quarry runs every agent read and every approved write in its own transaction")
        }
        S::Set(_) | S::Reset(_) | S::Discard { .. } | S::Use(_) => Root::Never("session settings are never changed for an agent"),
        S::LISTEN { .. } | S::UNLISTEN { .. } => Root::Never("LISTEN / UNLISTEN change the session"),
        S::Prepare { .. } | S::Execute { .. } | S::Deallocate { .. } => Root::Never("prepared statements are session state"),
        S::Declare { .. } | S::Fetch { .. } | S::Close { .. } | S::Open(_) => Root::Never("cursors are session state"),
        S::AttachDatabase { .. } | S::AttachDuckDBDatabase { .. } | S::DetachDuckDBDatabase { .. } => {
            Root::Never("ATTACH / DETACH change which databases the connection reaches")
        }
        S::Install { .. } | S::Load { .. } => Root::Never("loading extensions changes the connection"),
        S::ShowVariable { .. } | S::ExplainTable { .. } => Root::Never("not supported on this engine"),
        _ => Root::Write(WriteKind::Other),
    }
}

fn pragma(name: &ObjectName, has_value: bool, is_eq: bool, engine: EngineKind) -> Root {
    if engine != EngineKind::Sqlite {
        return Root::Never("PRAGMA is SQLite only");
    }
    let last = name.0.last().and_then(|p| match p {
        ObjectNamePart::Identifier(id) => Some(id.value.to_lowercase()),
        ObjectNamePart::Function(_) => None,
    });
    let Some(last) = last else { return Root::Write(WriteKind::Pragma) };
    match functions::read_pragma(&last) {
        Some(PragmaArg::None) if !has_value && !is_eq => Root::Read(ReadKind::Pragma),
        Some(PragmaArg::Name) if !is_eq => Root::Read(ReadKind::Pragma),
        _ => Root::Write(WriteKind::Pragma),
    }
}

/// `TableRef` from `[[catalog.]schema.]name`, keeping each part as written.
pub fn table_ref(name: &ObjectName) -> TableRef {
    let parts: Vec<String> = name
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(id) => id.value.clone(),
            ObjectNamePart::Function(f) => f.to_string(),
        })
        .collect();
    match parts.as_slice() {
        [] => TableRef::new(None, String::new()),
        [n] => TableRef::new(None, n.clone()),
        [.., s, n] => TableRef::new(Some(s), n.clone()),
    }
}

fn relation(t: &TableWithJoins) -> Option<TableRef> {
    match &t.relation {
        TableFactor::Table { name, .. } => Some(table_ref(name)),
        _ => None,
    }
}

/// The tables `s` writes (best effort; for the approval card).
pub fn targets(s: &Statement) -> Vec<TableRef> {
    use Statement as S;
    match s {
        S::Insert(i) => match &i.table {
            TableObject::TableName(n) => vec![table_ref(n)],
            _ => Vec::new(),
        },
        S::Update(u) => relation(&u.table).into_iter().collect(),
        S::Delete(d) => {
            let from = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
            };
            if d.tables.is_empty() { from.iter().filter_map(relation).collect() } else { d.tables.iter().map(table_ref).collect() }
        }
        S::Merge(m) => match &m.table {
            TableFactor::Table { name, .. } => vec![table_ref(name)],
            _ => Vec::new(),
        },
        S::Truncate(t) => t.table_names.iter().map(|t| table_ref(&t.name)).collect(),
        S::Drop { names, .. } => names.iter().map(table_ref).collect(),
        S::CreateTable(c) => vec![table_ref(&c.name)],
        S::CreateView(c) => vec![table_ref(&c.name)],
        S::CreateIndex(c) => vec![table_ref(&c.table_name)],
        S::AlterTable(a) => vec![table_ref(&a.name)],
        S::Copy { source: CopySource::Table { table_name, .. }, .. } => vec![table_ref(table_name)],
        S::Explain { statement, .. } => targets(statement),
        _ => Vec::new(),
    }
}

/// Whether an UPDATE / DELETE has no WHERE (it touches every row).
pub fn unbounded(s: &Statement) -> bool {
    match s {
        Statement::Update(u) => u.selection.is_none(),
        Statement::Delete(d) => d.selection.is_none(),
        Statement::Truncate(_) => true,
        _ => false,
    }
}

/// `SELECT count(*) FROM <table> [WHERE …]` with the same WHERE as a single-table UPDATE /
/// DELETE: the rows it would touch, counted on the read-only connection.
pub fn count_sql(s: &Statement) -> Option<String> {
    let (table, selection) = match s {
        Statement::Update(u) if u.from.is_none() && u.table.joins.is_empty() => (u.table.to_string(), u.selection.as_ref()),
        Statement::Delete(d) if d.using.is_none() && d.tables.is_empty() => {
            let from = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
            };
            match from.as_slice() {
                [t] if t.joins.is_empty() => (t.to_string(), d.selection.as_ref()),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(match selection {
        Some(w) => format!("SELECT count(*) FROM {table} WHERE {w}"),
        None => format!("SELECT count(*) FROM {table}"),
    })
}
