//! SQLite through rusqlite with the bundled library (M1: `engine` stream). Stub.

use quarry_core::{ConnectionSpec, EngineKind, Secret};

use crate::{AgentConnection, Engine, EngineError, HumanConnection};

/// The SQLite engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteEngine;

/// The bundled SQLite library version (`3.x.y`).
pub fn library_version() -> &'static str {
    rusqlite::version()
}

impl Engine for SqliteEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }
    fn connect_human(&self, _spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn HumanConnection>, EngineError> {
        Err(EngineError::NotYet)
    }
    fn connect_agent(&self, _spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn AgentConnection>, EngineError> {
        Err(EngineError::NotYet)
    }
}
