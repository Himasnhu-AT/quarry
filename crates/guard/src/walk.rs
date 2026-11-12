//! The AST walk: every nested statement, query, select, expression and table expression of one
//! parsed statement, recording anything that is not provably a read.

use std::ops::ControlFlow;

use quarry_core::{EngineKind, TableRef};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName, ObjectNamePart, Query, Select, SelectItem, SetExpr, Statement,
    TableFactor, Value, Visit, Visitor,
};

use crate::{Policy, WriteKind, functions, is_sensitive, statement};

/// Deepest nesting of queries and expressions walked; deeper statements are refused `too_large`.
pub const MAX_DEPTH: usize = 64;

/// What the walk found. The first finding of each sort is kept (its message goes to the agent).
#[derive(Default)]
pub struct Findings {
    /// Something that writes or might (a nested DML statement, a lock, `SELECT … INTO`, an
    /// unsupported construct): never a read, but a human may approve it.
    pub write: Option<(WriteKind, String)>,
    /// A function outside the allow-list (or on the deny-list).
    pub denied: Option<String>,
    /// Refused on every path, proposals included.
    pub never: Option<String>,
    /// A column whose name is sensitive, used anywhere but as a bare output column.
    pub sensitive: Option<String>,
    /// Deeper than [`MAX_DEPTH`]: the walk stopped, nothing else here is complete.
    pub too_deep: bool,
    /// Tables written by nested statements and `SELECT … INTO`.
    pub tables: Vec<TableRef>,
}

/// Walk `root` (the statement itself is not judged here, only what it contains).
pub fn walk(root: &Statement, engine: EngineKind, policy: &Policy) -> Findings {
    let mut w = Walk { engine, policy, depth: 0, root_seen: false, bare_outputs: bare_outputs(root), found: Findings::default() };
    let _ = root.visit(&mut w);
    w.found
}

/// Walk one expression (a `view.filter` clause).
pub fn walk_expr(expr: &Expr, engine: EngineKind, policy: &Policy) -> Findings {
    let mut w = Walk { engine, policy, depth: 0, root_seen: true, bare_outputs: Vec::new(), found: Findings::default() };
    let _ = expr.visit(&mut w);
    w.found
}

struct Walk<'a> {
    engine: EngineKind,
    policy: &'a Policy,
    depth: usize,
    root_seen: bool,
    /// Addresses of the expressions that are bare columns of the outermost `SELECT`'s output:
    /// the only place a sensitive column may appear (its output name keeps it redacted).
    bare_outputs: Vec<usize>,
    found: Findings,
}

fn addr(e: &Expr) -> usize {
    e as *const Expr as usize
}

fn bare_outputs(root: &Statement) -> Vec<usize> {
    let Statement::Query(q) = root else { return Vec::new() };
    let SetExpr::Select(s) = q.body.as_ref() else { return Vec::new() };
    s.projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(e @ (Expr::Identifier(_) | Expr::CompoundIdentifier(_))) => Some(addr(e)),
            _ => None,
        })
        .collect()
}

impl Walk<'_> {
    fn enter(&mut self) -> ControlFlow<()> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            self.found.too_deep = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn write(&mut self, kind: WriteKind, why: impl Into<String>) {
        if self.found.write.is_none() {
            self.found.write = Some((kind, why.into()));
        }
    }

    fn deny(&mut self, why: impl Into<String>) {
        if self.found.denied.is_none() {
            self.found.denied = Some(why.into());
        }
    }

    fn never(&mut self, why: impl Into<String>) {
        if self.found.never.is_none() {
            self.found.never = Some(why.into());
        }
    }

    fn sensitive_ref(&mut self, e: &Expr, column: &str) {
        if self.found.sensitive.is_none() && is_sensitive(column, self.policy) && !self.bare_outputs.contains(&addr(e)) {
            self.found.sensitive = Some(format!(
                "column {column} is sensitive: it may only be selected as a plain output column (no alias, expression, filter or sort)"
            ));
        }
    }

    fn function(&mut self, name: &ObjectName, args: Option<&[FunctionArg]>) {
        let shown = name.to_string();
        let Some((schema, bare)) = normalize(name, self.engine) else {
            self.deny(format!("{shown} is not a plain function name"));
            return;
        };
        if functions::never(&bare) {
            self.never(format!("{shown} changes the connection itself"));
            return;
        }
        let catalog = match schema.as_deref() {
            None => true,
            Some(s) => self.engine == EngineKind::Postgres && s == "pg_catalog",
        };
        if catalog && self.engine == EngineKind::Postgres && bare == "pg_sleep" && short_sleep(args) {
            return;
        }
        if functions::denied(&bare) {
            self.deny(format!("{shown} has side effects (or reads the server's files or sessions)"));
            return;
        }
        if catalog && functions::allowed(&bare, self.engine) {
            return;
        }
        let full = match &schema {
            Some(s) => format!("{s}.{bare}"),
            None => bare.clone(),
        };
        let safe = self.policy.safe_functions.iter().any(|f| {
            let f = f.trim().to_lowercase();
            !f.is_empty() && f == full.to_lowercase()
        });
        if !safe {
            self.deny(format!("{shown} is not on the read-only function allow-list (the human can mark it safe)"));
        }
    }
}

/// `(schema, name)` with each part folded like the engine folds it, or `None` for anything but
/// one or two plain identifiers.
fn normalize(name: &ObjectName, engine: EngineKind) -> Option<(Option<String>, String)> {
    let mut parts = Vec::with_capacity(2);
    for p in &name.0 {
        let ObjectNamePart::Identifier(id) = p else { return None };
        let v = match (engine, id.quote_style) {
            (EngineKind::Postgres, Some(_)) => id.value.clone(),
            _ => id.value.to_lowercase(),
        };
        parts.push(v);
    }
    match parts.as_slice() {
        [n] => Some((None, n.clone())),
        [s, n] => Some((Some(s.clone()), n.clone())),
        _ => None,
    }
}

/// `pg_sleep(n)` with a literal `0 <= n <= 1` second.
fn short_sleep(args: Option<&[FunctionArg]>) -> bool {
    let Some([FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v)))]) = args else { return false };
    match &v.value {
        Value::Number(n, _) => n.parse::<f64>().is_ok_and(|s| (0.0..=1.0).contains(&s)),
        _ => false,
    }
}

impl Visitor for Walk<'_> {
    type Break = ();

    fn pre_visit_statement(&mut self, s: &Statement) -> ControlFlow<()> {
        if !self.root_seen {
            self.root_seen = true;
            return ControlFlow::Continue(());
        }
        if !matches!(s, Statement::Query(_)) {
            match statement::root(s, self.engine) {
                statement::Root::Never(why) => self.never(why),
                statement::Root::Read(_) => self.write(WriteKind::Other, "a statement nested inside another is not a plain query"),
                statement::Root::Write(kind) => self.write(kind, format!("contains {}", kind.as_str())),
            }
            self.found.tables.extend(statement::targets(s));
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        self.enter()?;
        if !q.locks.is_empty() {
            self.write(WriteKind::Lock, "FOR UPDATE / FOR SHARE locks rows");
        }
        if q.for_clause.is_some() || q.settings.is_some() || q.format_clause.is_some() || !q.pipe_operators.is_empty() {
            self.write(WriteKind::Other, "a query clause this engine does not support");
        }
        if let SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) = q.body.as_ref() {
            self.write(WriteKind::Other, "a query whose body changes data");
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _q: &Query) -> ControlFlow<()> {
        self.leave();
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, s: &Select) -> ControlFlow<()> {
        if let Some(into) = &s.into {
            self.write(WriteKind::Ddl, "SELECT … INTO creates a table");
            for t in &into.targets {
                if let Expr::Identifier(id) = t {
                    self.found.tables.push(TableRef::new(None, id.value.clone()));
                } else if let Expr::CompoundIdentifier(ids) = t {
                    self.found.tables.push(statement::table_ref(&ObjectName::from(ids.clone())));
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
        match t {
            TableFactor::Table { name, args: Some(a), .. } => self.function(name, Some(&a.args)),
            TableFactor::Function { name, args, .. } => self.function(name, Some(args)),
            TableFactor::Table { .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. }
            | TableFactor::UNNEST { .. }
            | TableFactor::TableFunction { .. } => {}
            _ => self.write(WriteKind::Other, "a table expression this engine does not support"),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        self.enter()?;
        match e {
            Expr::Function(f) => {
                let args = match &f.args {
                    FunctionArguments::List(l) => Some(l.args.as_slice()),
                    _ => None,
                };
                self.function(&f.name, args);
            }
            Expr::BinaryOp { op: BinaryOperator::Custom(o), .. } => self.deny(format!("operator {o} is not a built-in operator")),
            Expr::BinaryOp { op: BinaryOperator::PGCustomBinaryOperator(o), .. } => {
                self.deny(format!("OPERATOR({}) may run any function", o.join(".")));
            }
            Expr::Identifier(id) => self.sensitive_ref(e, &id.value),
            Expr::CompoundIdentifier(ids) => {
                if let Some(last) = ids.last() {
                    self.sensitive_ref(e, &last.value);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, _e: &Expr) -> ControlFlow<()> {
        self.leave();
        ControlFlow::Continue(())
    }
}
