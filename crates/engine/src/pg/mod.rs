//! PostgreSQL through tokio-postgres.
//!
//! The engine owns one small current-thread tokio runtime on its own thread (`quarry-pg`),
//! started on the first connection; every session's I/O runs there. The blocking
//! [`HumanConnection`] / [`AgentConnection`] handles hand it one future per call and wait for
//! the answer (with the call's timeout plus a margin). Cancel sends PostgreSQL's cancel request.
//! Sessions, cursors and the agent's guards: `session`. TLS modes: `tls`. SSH tunnels are a
//! later item: a spec with one is refused.

mod explain;
mod schema;
mod session;
mod tls;
mod types;

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use quarry_core::limits::{AGENT_TIMEOUT_MS_DEFAULT, AGENT_TIMEOUT_MS_MAX};
use quarry_core::{AgentAccess, ConnectionSpec, CursorId, EngineKind, Estimate, Page, PlanNode, SchemaTree, Secret, Target, TlsMode};
use tokio::runtime::Handle;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{Config, NoTls};

use crate::{
    AgentConnection, Browse, CancelToken, Cursor, Engine, EngineError, HumanConnection, QueryOptions, ReadStatement, SchemaFilter, TxOutcome,
};
use session::{AGENT_GUARDS, Canceller, Session, map_err};

/// TCP connect timeout, and the whole connect (TLS, auth, guards) bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TOTAL: Duration = Duration::from_secs(20);
/// Schema loads give up after this.
const SCHEMA_TIMEOUT_MS: u32 = 60_000;
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// Added to a call's own timeout before the caller stops waiting.
const MARGIN: Duration = Duration::from_secs(5);

/// The PostgreSQL engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct PgEngine;

fn runtime() -> Result<&'static Handle, EngineError> {
    static RT: OnceLock<Result<Handle, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let handle = rt.handle().clone();
        std::thread::Builder::new().name("quarry-pg".into()).spawn(move || rt.block_on(std::future::pending::<()>())).map_err(|e| e.to_string())?;
        Ok(handle)
    })
    .as_ref()
    .map_err(|e| EngineError::Connect(format!("cannot start the PostgreSQL runtime: {e}")))
}

/// Run `fut` on the engine's runtime and wait for it (at most `wait`; then `on_timeout` runs).
fn block<T: Send + 'static>(
    fut: impl Future<Output = Result<T, EngineError>> + Send + 'static,
    wait: Option<Duration>,
    on_timeout: impl FnOnce(),
) -> Result<T, EngineError> {
    let (tx, rx) = mpsc::sync_channel(1);
    runtime()?.spawn(async move {
        let _ = tx.send(fut.await);
    });
    match wait {
        None => rx.recv().map_err(|_| EngineError::Closed)?,
        Some(d) => match rx.recv_timeout(d) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                on_timeout();
                Err(EngineError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(EngineError::Closed),
        },
    }
}

/// A `postgres://` URL (or `key=value` string) as a target and its password. The password never
/// appears in an error.
pub fn parse_url(url: &str) -> Result<(Target, Option<Secret>), EngineError> {
    let cfg = Config::from_str(url).map_err(|_| EngineError::Connect("not a valid PostgreSQL URL".into()))?;
    let host = match cfg.get_hosts().first() {
        Some(Host::Tcp(h)) => h.clone(),
        #[cfg(unix)]
        Some(Host::Unix(p)) => p.to_string_lossy().into_owned(),
        None => "localhost".into(),
    };
    let tls = match cfg.get_ssl_mode() {
        SslMode::Disable => TlsMode::Disable,
        SslMode::Require => TlsMode::Require,
        _ => TlsMode::Prefer,
    };
    let user = cfg.get_user().unwrap_or("postgres").to_string();
    let target = Target::Postgres {
        host,
        port: cfg.get_ports().first().copied().unwrap_or(5432),
        db: cfg.get_dbname().map_or_else(|| user.clone(), str::to_string),
        user,
        tls,
        ssh: None,
    };
    let secret = cfg.get_password().map(|p| Secret::new(String::from_utf8_lossy(p).into_owned()));
    Ok((target, secret))
}

fn config(spec: &ConnectionSpec, secret: Option<&Secret>, agent: bool) -> Result<(Config, TlsMode), EngineError> {
    let Target::Postgres { host, port, db, user, tls, ssh } = &spec.target else {
        return Err(EngineError::Connect("not a PostgreSQL connection".into()));
    };
    if ssh.is_some() {
        return Err(EngineError::Connect("SSH tunnels are not supported yet".into()));
    }
    let user = match (&spec.agent_access, agent) {
        (AgentAccess::ReadOnlyRole { role }, true) => role.as_str(),
        _ => user.as_str(),
    };
    let mut cfg = Config::new();
    cfg.host(host)
        .port(*port)
        .dbname(db)
        .user(user)
        .application_name(if agent { "quarry-agent" } else { "quarry" })
        .connect_timeout(CONNECT_TIMEOUT)
        .keepalives(true)
        .keepalives_idle(Duration::from_secs(60));
    if let Some(s) = secret {
        cfg.password(s.expose());
    }
    Ok((cfg, tls.clone()))
}

fn spawn_connection<F>(conn: F)
where
    F: Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            log::debug!("PostgreSQL connection ended: {e}");
        }
    });
}

async fn connect(mut cfg: Config, tls: TlsMode, agent: bool, read_only: bool) -> Result<Session, EngineError> {
    let (mode, connector) = tls::connector(&tls)?;
    cfg.ssl_mode(mode);
    let client = match &connector {
        None => {
            let (client, conn) = cfg.connect(NoTls).await.map_err(|e| EngineError::Connect(e.to_string()))?;
            spawn_connection(conn);
            client
        }
        Some(t) => {
            let (client, conn) = cfg.connect(t.clone()).await.map_err(|e| EngineError::Connect(e.to_string()))?;
            spawn_connection(conn);
            client
        }
    };
    let canceller = Canceller { token: client.cancel_token(), tls: connector };
    if agent {
        client.batch_execute(AGENT_GUARDS).await.map_err(map_err)?;
        let on: String = client.query_one("SHOW default_transaction_read_only", &[]).await.map_err(map_err)?.try_get(0).map_err(map_err)?;
        if on != "on" {
            return Err(EngineError::Connect("the agent session is not read-only".into()));
        }
    } else if read_only {
        client.batch_execute("SET default_transaction_read_only = on").await.map_err(map_err)?;
    }
    Ok(Session::new(client, canceller, agent))
}

impl Engine for PgEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Postgres
    }
    fn connect_human(&self, spec: &ConnectionSpec, secret: Option<&Secret>) -> Result<Box<dyn HumanConnection>, EngineError> {
        Ok(Box::new(PgConn::open(spec, secret, false)?))
    }
    fn connect_agent(&self, spec: &ConnectionSpec, secret: Option<&Secret>) -> Result<Box<dyn AgentConnection>, EngineError> {
        if !spec.agent_access.allows_agent() {
            return Err(EngineError::Connect("agent access is off for this connection".into()));
        }
        Ok(Box::new(PgConn::open(spec, secret, true)?))
    }
}

type Shared = Arc<tokio::sync::Mutex<Session>>;

/// A connection handle: the session lives on the engine's runtime.
pub(crate) struct PgConn {
    session: Shared,
    canceller: Canceller,
    busy: Arc<AtomicBool>,
    agent: bool,
}

impl PgConn {
    fn open(spec: &ConnectionSpec, secret: Option<&Secret>, agent: bool) -> Result<Self, EngineError> {
        let (cfg, tls) = config(spec, secret, agent)?;
        let read_only = spec.human_read_only;
        let fut = async move {
            match tokio::time::timeout(CONNECT_TOTAL, connect(cfg, tls, agent, read_only)).await {
                Ok(r) => r,
                Err(_) => Err(EngineError::Connect("timed out".into())),
            }
        };
        let session = block(fut, Some(CONNECT_TOTAL + MARGIN), || {})?;
        let canceller = session.canceller.clone();
        Ok(Self { session: Arc::new(tokio::sync::Mutex::new(session)), canceller, busy: Arc::new(AtomicBool::new(false)), agent })
    }

    fn cancel_now(&self) {
        let c = self.canceller.clone();
        if let Ok(h) = runtime() {
            h.spawn(async move { c.cancel().await });
        }
    }

    /// Run a session call, waiting at most its timeout plus a margin.
    fn call<T, Fut>(&self, timeout_ms: Option<u32>, f: impl FnOnce(Shared) -> Fut) -> Result<T, EngineError>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        self.busy.store(true, Ordering::SeqCst);
        let wait = timeout_ms.map(|t| Duration::from_millis(u64::from(t)) + MARGIN);
        let r = block(f(Arc::clone(&self.session)), wait, || self.cancel_now());
        self.busy.store(false, Ordering::SeqCst);
        r
    }

    fn agent_opts(opts: &QueryOptions) -> QueryOptions {
        QueryOptions { timeout_ms: Some(opts.timeout_ms.unwrap_or(AGENT_TIMEOUT_MS_DEFAULT).clamp(1, AGENT_TIMEOUT_MS_MAX)), ..*opts }
    }

    fn default_timeout(&self) -> Option<u32> {
        self.agent.then_some(AGENT_TIMEOUT_MS_DEFAULT)
    }

    fn run_query(&self, sql: String, opts: QueryOptions) -> Result<Cursor, EngineError> {
        self.call(opts.timeout_ms, move |s| async move { s.lock().await.query(&sql, opts).await })
    }

    fn run_explain(&self, sql: String, analyze: bool) -> Result<PlanNode, EngineError> {
        let t = self.default_timeout();
        self.call(t, move |s| async move { s.lock().await.explain(&sql, analyze, t).await })
    }
}

impl Drop for PgConn {
    fn drop(&mut self) {
        // finish the cursor transaction (the person's commits) before the session goes
        let s = Arc::clone(&self.session);
        if let Ok(h) = runtime() {
            h.spawn(async move { s.lock().await.end_tx().await });
        }
    }
}

impl Browse for PgConn {
    fn schema(&mut self, filter: &SchemaFilter) -> Result<SchemaTree, EngineError> {
        let filter = filter.clone();
        let t = Some(self.default_timeout().unwrap_or(SCHEMA_TIMEOUT_MS));
        self.call(t, move |s| async move { s.lock().await.schema(&filter, t).await })
    }
    fn fetch(&mut self, cursor: CursorId, first_row: u64, rows: u32) -> Result<Page, EngineError> {
        // the cursor's own timeout applies inside
        self.call(None, move |s| async move { s.lock().await.fetch(cursor, first_row, rows).await })
    }
    fn close(&mut self, cursor: CursorId) {
        let s = Arc::clone(&self.session);
        let closing = async move {
            s.lock().await.close(cursor).await;
            Ok(())
        };
        let _ = block(closing, Some(CLOSE_WAIT), || {});
    }
    fn cancel_token(&self) -> CancelToken {
        let c = self.canceller.clone();
        let busy = Arc::clone(&self.busy);
        CancelToken::new(move || {
            if busy.load(Ordering::SeqCst)
                && let Ok(h) = runtime()
            {
                let c = c.clone();
                h.spawn(async move { c.cancel().await });
            }
        })
    }
}

impl HumanConnection for PgConn {
    fn query(&mut self, sql: &str, opts: &QueryOptions) -> Result<Cursor, EngineError> {
        self.run_query(sql.to_string(), *opts)
    }
    fn explain(&mut self, sql: &str, analyze: bool) -> Result<PlanNode, EngineError> {
        self.run_explain(sql.to_string(), analyze)
    }
    fn execute_in_tx(&mut self, statements: &[String], check: Option<Estimate>) -> Result<TxOutcome, EngineError> {
        let statements = statements.to_vec();
        self.call(None, move |s| async move { s.lock().await.execute_in_tx(&statements, check).await })
    }
}

fn check_engine(stmt: &ReadStatement) -> Result<(), EngineError> {
    if stmt.engine() == EngineKind::Postgres {
        Ok(())
    } else {
        Err(EngineError::Database { message: "the statement was classified for another engine".into(), position: None })
    }
}

impl AgentConnection for PgConn {
    fn query(&mut self, stmt: &ReadStatement, opts: &QueryOptions) -> Result<Cursor, EngineError> {
        check_engine(stmt)?;
        self.run_query(stmt.sql().to_string(), Self::agent_opts(opts))
    }
    fn explain(&mut self, stmt: &ReadStatement, analyze: bool) -> Result<PlanNode, EngineError> {
        check_engine(stmt)?;
        let sql = if matches!(stmt.kind(), quarry_guard::ReadKind::Explain { .. }) { crate::strip_explain(stmt.sql()) } else { stmt.sql() };
        self.run_explain(sql.to_string(), analyze)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_without_leaking_the_password() {
        let (t, s) = parse_url("postgres://alice:hunter2@db.local:6543/app?sslmode=require").unwrap();
        assert_eq!(
            t,
            Target::Postgres { host: "db.local".into(), port: 6543, db: "app".into(), user: "alice".into(), tls: TlsMode::Require, ssh: None }
        );
        assert_eq!(s.map(|s| s.expose().to_string()).as_deref(), Some("hunter2"));
        let e = parse_url("postgres://alice:hunter2@db.local:notaport/app").err().unwrap();
        assert!(!e.to_string().contains("hunter2"));
    }

    #[test]
    fn ssh_and_wrong_targets_are_refused() {
        let mut spec = ConnectionSpec {
            id: quarry_core::ConnectionId::new("x"),
            name: "x".into(),
            target: Target::Postgres {
                host: "127.0.0.1".into(),
                port: 1,
                db: "x".into(),
                user: "x".into(),
                tls: TlsMode::Disable,
                ssh: Some(quarry_core::SshSpec { host: "h".into(), port: 22, user: "u".into(), auth: quarry_core::SshAuth::Agent }),
            },
            color: quarry_core::ConnColor::None,
            agent_access: AgentAccess::ReadOnlyRole { role: "reader".into() },
            sensitive_patterns: vec![],
            human_read_only: false,
        };
        assert!(matches!(config(&spec, None, true), Err(EngineError::Connect(m)) if m.contains("SSH")));
        if let Target::Postgres { ssh, .. } = &mut spec.target {
            *ssh = None;
        }
        let (cfg, _) = config(&spec, None, true).unwrap();
        assert_eq!(cfg.get_user(), Some("reader"));
        let (cfg, _) = config(&spec, None, false).unwrap();
        assert_eq!(cfg.get_user(), Some("x"));
    }

    #[test]
    fn nothing_listening_is_a_connect_error_not_a_hang() {
        // port 1 on loopback: refused at once on every OS
        let spec = ConnectionSpec {
            id: quarry_core::ConnectionId::new("x"),
            name: "x".into(),
            target: Target::Postgres { host: "127.0.0.1".into(), port: 1, db: "x".into(), user: "x".into(), tls: TlsMode::Disable, ssh: None },
            color: quarry_core::ConnColor::None,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec![],
            human_read_only: false,
        };
        assert!(matches!(PgEngine.connect_human(&spec, None).err(), Some(EngineError::Connect(_))));
    }
}
