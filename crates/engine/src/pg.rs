//! PostgreSQL through tokio-postgres (M1: `engine` stream). Stub.

use quarry_core::{ConnectionSpec, EngineKind, Secret};

use crate::{AgentConnection, Engine, EngineError, HumanConnection};

/// The PostgreSQL engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct PgEngine;

impl Engine for PgEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Postgres
    }
    fn connect_human(&self, _spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn HumanConnection>, EngineError> {
        Err(EngineError::NotYet)
    }
    fn connect_agent(&self, _spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn AgentConnection>, EngineError> {
        Err(EngineError::NotYet)
    }
}
