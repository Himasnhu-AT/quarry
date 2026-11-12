//! The write-approval contract (contract 4, SECURITY.md layer C).
//!
//! An agent never writes. `db.propose_write` stores a [`Proposal`] in state `pending`; only the
//! human, in the window, moves it on:
//!
//! ```text
//!            approve (Human)       start (System)       finish (System)
//!   pending ────────────────▶ approved ─────────▶ running ──────────────▶ done
//!      │                                              │
//!      │ deny (Human)                                 │ fail (System)
//!      ├──────────────▶ denied                        └──────────────▶ failed
//!      │ expire (System, after PROPOSAL_TTL_MS)
//!      └──────────────▶ expired
//! ```
//!
//! [`transition`] is the whole rule as a pure function: every other (state, event, actor) is
//! refused. [`Actor::Agent`] causes no transition at all. `System` is the app itself (the expiry
//! timer and the executor job that runs an approved write on the human's connection); `Human` is
//! a real click or key press in the window (see `crates/app/src/human.rs`), never MCP, the CLI or
//! the control port.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::limits::{MAX_KEPT_PROPOSALS, MAX_PENDING_PROPOSALS, PROPOSAL_TTL_MS};
use crate::{ConnectionId, ProposalId, Refusal, RefusalCode, TableRef, Timestamp};

/// Where a proposal is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Running,
    Done,
    Failed,
    Denied,
    Expired,
}

impl ProposalStatus {
    pub const ALL: [ProposalStatus; 7] = [
        ProposalStatus::Pending,
        ProposalStatus::Approved,
        ProposalStatus::Running,
        ProposalStatus::Done,
        ProposalStatus::Failed,
        ProposalStatus::Denied,
        ProposalStatus::Expired,
    ];

    /// No event leaves this state.
    pub fn is_terminal(self) -> bool {
        matches!(self, ProposalStatus::Done | ProposalStatus::Failed | ProposalStatus::Denied | ProposalStatus::Expired)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProposalStatus::Pending => "pending",
            ProposalStatus::Approved => "approved",
            ProposalStatus::Running => "running",
            ProposalStatus::Done => "done",
            ProposalStatus::Failed => "failed",
            ProposalStatus::Denied => "denied",
            ProposalStatus::Expired => "expired",
        }
    }
}

/// Who does something: the person at the window, an AI agent (any MCP / CLI / control-port
/// caller), or the app itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Human,
    Agent,
    System,
}

impl Actor {
    pub const ALL: [Actor; 3] = [Actor::Human, Actor::Agent, Actor::System];
}

/// Something that happens to a proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum ProposalEvent {
    /// The human approved the exact statement on the card.
    Approve,
    /// The human denied it.
    Deny,
    /// Nobody decided within [`PROPOSAL_TTL_MS`].
    Expire,
    /// The executor began the transaction on the human's connection.
    Start,
    /// The transaction committed.
    Finish { rows_affected: u64 },
    /// The statement failed, or the row count differed from the estimate beyond its tolerance
    /// and the transaction rolled back.
    Fail { error: String },
}

impl ProposalEvent {
    pub fn name(&self) -> &'static str {
        match self {
            ProposalEvent::Approve => "approve",
            ProposalEvent::Deny => "deny",
            ProposalEvent::Expire => "expire",
            ProposalEvent::Start => "start",
            ProposalEvent::Finish { .. } => "finish",
            ProposalEvent::Fail { .. } => "fail",
        }
    }

    /// The single (from, to, actor) this event allows.
    fn rule(&self) -> (ProposalStatus, ProposalStatus, Actor) {
        use ProposalStatus as S;
        match self {
            ProposalEvent::Approve => (S::Pending, S::Approved, Actor::Human),
            ProposalEvent::Deny => (S::Pending, S::Denied, Actor::Human),
            ProposalEvent::Expire => (S::Pending, S::Expired, Actor::System),
            ProposalEvent::Start => (S::Approved, S::Running, Actor::System),
            ProposalEvent::Finish { .. } => (S::Running, S::Done, Actor::System),
            ProposalEvent::Fail { .. } => (S::Running, S::Failed, Actor::System),
        }
    }
}

/// Why a transition was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TransitionError {
    /// This actor may never cause this event (checked first: an agent learns nothing about state).
    #[error("{actor:?} may not {event} a proposal")]
    WrongActor { event: String, actor: Actor },
    /// The event does not apply in this state.
    #[error("cannot {event} a proposal that is {}", from.as_str())]
    WrongState { event: String, from: ProposalStatus },
    /// The proposal expired before the decision arrived (it is now `expired`).
    #[error("the proposal expired")]
    Expired,
    #[error("no proposal {0}")]
    NotFound(ProposalId),
}

/// The proposal state machine: the next state, or why not. Pure; exhaustively tested.
pub fn transition(from: ProposalStatus, event: &ProposalEvent, actor: Actor) -> Result<ProposalStatus, TransitionError> {
    let (allowed_from, to, who) = event.rule();
    if actor != who {
        return Err(TransitionError::WrongActor { event: event.name().into(), actor });
    }
    if from != allowed_from {
        return Err(TransitionError::WrongState { event: event.name().into(), from });
    }
    Ok(to)
}

/// The estimated effect of a write, from the read-only connection (EXPLAIN, and for UPDATE /
/// DELETE a capped `SELECT count(*)` with the same WHERE).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub rows: u64,
    /// From a count (true) or the planner (false).
    pub exact: bool,
    /// The executor rolls back when the affected rows differ from `rows` by more than this.
    pub tolerance: u64,
}

impl Estimate {
    pub fn accepts(&self, rows_affected: u64) -> bool {
        rows_affected.abs_diff(self.rows) <= self.tolerance
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ProposalOutcome {
    Done { rows_affected: u64 },
    Failed { error: String },
}

/// A write an agent asked for (`db.propose_write`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: ProposalId,
    pub connection: ConnectionId,
    /// The exact statement(s), as the agent sent them; shown unformatted on the card.
    pub sql: String,
    pub reason: String,
    /// Tables the statement writes (from the guard's parse).
    #[serde(default)]
    pub tables: Vec<TableRef>,
    #[serde(default)]
    pub estimate: Option<Estimate>,
    pub status: ProposalStatus,
    pub requested_at: Timestamp,
    pub expires_at: Timestamp,
    #[serde(default)]
    pub decided_at: Option<Timestamp>,
    /// `Human` for approve/deny, `System` for expire.
    #[serde(default)]
    pub decided_by: Option<Actor>,
    #[serde(default)]
    pub result: Option<ProposalOutcome>,
}

impl Proposal {
    /// A new pending proposal that expires [`PROPOSAL_TTL_MS`] after `now`.
    pub fn new(
        id: ProposalId,
        connection: ConnectionId,
        sql: impl Into<String>,
        reason: impl Into<String>,
        tables: Vec<TableRef>,
        now: Timestamp,
    ) -> Self {
        Self {
            id,
            connection,
            sql: sql.into(),
            reason: reason.into(),
            tables,
            estimate: None,
            status: ProposalStatus::Pending,
            requested_at: now,
            expires_at: now.plus_ms(PROPOSAL_TTL_MS),
            decided_at: None,
            decided_by: None,
            result: None,
        }
    }

    /// Whether a pending proposal is past its deadline at `now`.
    pub fn is_due(&self, now: Timestamp) -> bool {
        self.status == ProposalStatus::Pending && now >= self.expires_at
    }

    /// Apply `event` by `actor` at `now`. A decision on a proposal past its deadline expires it
    /// instead and fails with [`TransitionError::Expired`]. Refused events change nothing.
    pub fn apply(&mut self, event: ProposalEvent, actor: Actor, now: Timestamp) -> Result<(), TransitionError> {
        let to = transition(self.status, &event, actor)?;
        if matches!(event, ProposalEvent::Approve | ProposalEvent::Deny) && self.is_due(now) {
            self.status = ProposalStatus::Expired;
            self.decided_at = Some(now);
            self.decided_by = Some(Actor::System);
            return Err(TransitionError::Expired);
        }
        match &event {
            ProposalEvent::Approve | ProposalEvent::Deny | ProposalEvent::Expire => {
                self.decided_at = Some(now);
                self.decided_by = Some(actor);
            }
            ProposalEvent::Start => {}
            ProposalEvent::Finish { rows_affected } => self.result = Some(ProposalOutcome::Done { rows_affected: *rows_affected }),
            ProposalEvent::Fail { error } => self.result = Some(ProposalOutcome::Failed { error: error.clone() }),
        }
        self.status = to;
        Ok(())
    }
}

impl fmt::Display for ProposalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The proposals of one app process: ids, the pending cap, expiry. Pure (the app owns one and
/// passes the clock in).
#[derive(Clone, Debug, Default)]
pub struct ProposalBook {
    next: u64,
    /// Ascending by id.
    items: Vec<Proposal>,
}

impl ProposalBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a new pending proposal (what `db.propose_write` does). Refused with
    /// [`RefusalCode::RateLimited`] while [`MAX_PENDING_PROPOSALS`] are pending.
    pub fn propose(
        &mut self,
        connection: ConnectionId,
        sql: impl Into<String>,
        reason: impl Into<String>,
        tables: Vec<TableRef>,
        now: Timestamp,
    ) -> Result<ProposalId, Refusal> {
        self.expire_due(now);
        if self.pending().count() >= MAX_PENDING_PROPOSALS {
            return Err(Refusal::new(RefusalCode::RateLimited, format!("{MAX_PENDING_PROPOSALS} proposals are already waiting for a decision")));
        }
        self.next = self.next.saturating_add(1);
        let id = ProposalId(self.next);
        self.items.push(Proposal::new(id, connection, sql, reason, tables, now));
        self.trim();
        Ok(id)
    }

    pub fn get(&self, id: ProposalId) -> Option<&Proposal> {
        self.items.binary_search_by_key(&id, |p| p.id).ok().and_then(|i| self.items.get(i))
    }

    /// Apply an event to proposal `id` (see [`Proposal::apply`]).
    pub fn apply(&mut self, id: ProposalId, event: ProposalEvent, actor: Actor, now: Timestamp) -> Result<&Proposal, TransitionError> {
        let i = self.items.binary_search_by_key(&id, |p| p.id).map_err(|_| TransitionError::NotFound(id))?;
        let p = self.items.get_mut(i).ok_or(TransitionError::NotFound(id))?;
        p.apply(event, actor, now)?;
        Ok(p)
    }

    /// Record the estimate of a pending proposal (computed on the read-only connection).
    pub fn set_estimate(&mut self, id: ProposalId, estimate: Estimate) -> Result<(), TransitionError> {
        let p = self.items.iter_mut().find(|p| p.id == id).ok_or(TransitionError::NotFound(id))?;
        if p.status != ProposalStatus::Pending {
            return Err(TransitionError::WrongState { event: "estimate".into(), from: p.status });
        }
        p.estimate = Some(estimate);
        Ok(())
    }

    /// Expire every pending proposal past its deadline; their ids.
    pub fn expire_due(&mut self, now: Timestamp) -> Vec<ProposalId> {
        let mut out = Vec::new();
        for p in self.items.iter_mut().filter(|p| p.is_due(now)) {
            if p.apply(ProposalEvent::Expire, Actor::System, now).is_ok() {
                out.push(p.id);
            }
        }
        out
    }

    pub fn pending(&self) -> impl Iterator<Item = &Proposal> {
        self.items.iter().filter(|p| p.status == ProposalStatus::Pending)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Proposal> {
        self.items.iter()
    }

    /// Drop the oldest finished proposals beyond [`MAX_KEPT_PROPOSALS`].
    fn trim(&mut self) {
        let mut excess = self.items.len().saturating_sub(MAX_KEPT_PROPOSALS);
        self.items.retain(|p| {
            if excess > 0 && p.status.is_terminal() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProposalStatus as S;

    fn events() -> Vec<ProposalEvent> {
        vec![
            ProposalEvent::Approve,
            ProposalEvent::Deny,
            ProposalEvent::Expire,
            ProposalEvent::Start,
            ProposalEvent::Finish { rows_affected: 3 },
            ProposalEvent::Fail { error: "boom".into() },
        ]
    }

    /// The whole allowed table. Everything not listed must be refused.
    const ALLOWED: &[(S, &str, Actor, S)] = &[
        (S::Pending, "approve", Actor::Human, S::Approved),
        (S::Pending, "deny", Actor::Human, S::Denied),
        (S::Pending, "expire", Actor::System, S::Expired),
        (S::Approved, "start", Actor::System, S::Running),
        (S::Running, "finish", Actor::System, S::Done),
        (S::Running, "fail", Actor::System, S::Failed),
    ];

    #[test]
    fn every_state_event_actor_combination_matches_the_table() {
        let mut allowed = 0;
        for from in S::ALL {
            for event in events() {
                for actor in Actor::ALL {
                    let want = ALLOWED.iter().find(|(f, e, a, _)| *f == from && *e == event.name() && *a == actor).map(|r| r.3);
                    let got = transition(from, &event, actor);
                    match want {
                        Some(to) => {
                            assert_eq!(got, Ok(to), "{from:?} --{}/{actor:?}-->", event.name());
                            allowed += 1;
                        }
                        None => assert!(got.is_err(), "{from:?} --{}/{actor:?}--> should be refused, got {got:?}", event.name()),
                    }
                }
            }
        }
        assert_eq!(allowed, ALLOWED.len());
        assert_eq!(S::ALL.len() * events().len() * Actor::ALL.len(), 126);
    }

    #[test]
    fn the_agent_causes_no_transition_from_any_state() {
        for from in S::ALL {
            for event in events() {
                assert_eq!(
                    transition(from, &event, Actor::Agent),
                    Err(TransitionError::WrongActor { event: event.name().into(), actor: Actor::Agent })
                );
            }
        }
    }

    #[test]
    fn only_the_human_decides_and_only_the_system_runs() {
        for from in S::ALL {
            for actor in [Actor::Agent, Actor::System] {
                assert!(transition(from, &ProposalEvent::Approve, actor).is_err());
                assert!(transition(from, &ProposalEvent::Deny, actor).is_err());
            }
            for e in [
                ProposalEvent::Start,
                ProposalEvent::Expire,
                ProposalEvent::Finish { rows_affected: 0 },
                ProposalEvent::Fail { error: String::new() },
            ] {
                assert!(transition(from, &e, Actor::Human).is_err(), "human may not {}", e.name());
            }
        }
    }

    #[test]
    fn terminal_states_accept_nothing() {
        for from in S::ALL.into_iter().filter(|s| s.is_terminal()) {
            for event in events() {
                for actor in Actor::ALL {
                    assert!(transition(from, &event, actor).is_err(), "{from:?} {}", event.name());
                }
            }
        }
        assert!(!S::Pending.is_terminal() && !S::Approved.is_terminal() && !S::Running.is_terminal());
    }

    #[test]
    fn wrong_actor_is_reported_before_wrong_state() {
        assert!(matches!(transition(S::Done, &ProposalEvent::Approve, Actor::Agent), Err(TransitionError::WrongActor { .. })));
        assert!(matches!(transition(S::Done, &ProposalEvent::Approve, Actor::Human), Err(TransitionError::WrongState { from: S::Done, .. })));
    }

    fn p(now: u64) -> Proposal {
        Proposal::new(
            ProposalId(1),
            ConnectionId::new("local"),
            "DELETE FROM t WHERE id = 1",
            "cleanup",
            vec![TableRef::new(None, "t")],
            Timestamp(now),
        )
    }

    #[test]
    fn a_full_run_records_decision_and_result() {
        let mut x = p(1_000);
        assert_eq!(x.expires_at, Timestamp(1_000 + PROPOSAL_TTL_MS));
        x.apply(ProposalEvent::Approve, Actor::Human, Timestamp(2_000)).unwrap();
        assert_eq!((x.status, x.decided_by, x.decided_at), (S::Approved, Some(Actor::Human), Some(Timestamp(2_000))));
        x.apply(ProposalEvent::Start, Actor::System, Timestamp(2_001)).unwrap();
        x.apply(ProposalEvent::Finish { rows_affected: 1 }, Actor::System, Timestamp(2_002)).unwrap();
        assert_eq!(x.status, S::Done);
        assert_eq!(x.result, Some(ProposalOutcome::Done { rows_affected: 1 }));

        let mut y = p(0);
        y.apply(ProposalEvent::Approve, Actor::Human, Timestamp(1)).unwrap();
        y.apply(ProposalEvent::Start, Actor::System, Timestamp(2)).unwrap();
        y.apply(ProposalEvent::Fail { error: "rows over estimate".into() }, Actor::System, Timestamp(3)).unwrap();
        assert_eq!((y.status, y.result), (S::Failed, Some(ProposalOutcome::Failed { error: "rows over estimate".into() })));
    }

    #[test]
    fn a_late_decision_expires_the_proposal() {
        for event in [ProposalEvent::Approve, ProposalEvent::Deny] {
            let mut x = p(0);
            assert_eq!(x.apply(event, Actor::Human, Timestamp(PROPOSAL_TTL_MS)), Err(TransitionError::Expired));
            assert_eq!((x.status, x.decided_by), (S::Expired, Some(Actor::System)));
        }
        let mut x = p(0);
        x.apply(ProposalEvent::Approve, Actor::Human, Timestamp(PROPOSAL_TTL_MS - 1)).unwrap();
        assert_eq!(x.status, S::Approved);
    }

    #[test]
    fn a_refused_event_changes_nothing() {
        let mut x = p(0);
        let before = x.clone();
        assert!(x.apply(ProposalEvent::Approve, Actor::Agent, Timestamp(1)).is_err());
        assert!(x.apply(ProposalEvent::Start, Actor::System, Timestamp(1)).is_err());
        assert_eq!(x, before);
    }

    #[test]
    fn the_book_caps_pending_and_expires_on_time() {
        let mut b = ProposalBook::new();
        let c = ConnectionId::new("local");
        let ids: Vec<ProposalId> =
            (0..MAX_PENDING_PROPOSALS).map(|_| b.propose(c.clone(), "DELETE FROM t", "r", vec![], Timestamp(0)).unwrap()).collect();
        assert_eq!(ids.first(), Some(&ProposalId(1)));
        let refused = b.propose(c.clone(), "DELETE FROM t", "r", vec![], Timestamp(1)).unwrap_err();
        assert_eq!(refused.code, RefusalCode::RateLimited);
        b.apply(ProposalId(1), ProposalEvent::Deny, Actor::Human, Timestamp(5)).unwrap();
        assert!(b.propose(c.clone(), "x", "r", vec![], Timestamp(6)).is_ok());
        assert_eq!(b.expire_due(Timestamp(PROPOSAL_TTL_MS)).len(), MAX_PENDING_PROPOSALS - 1);
        assert_eq!(b.get(ProposalId(2)).map(|p| p.status), Some(S::Expired));
        assert_eq!(
            b.apply(ProposalId(999), ProposalEvent::Deny, Actor::Human, Timestamp(0)).unwrap_err(),
            TransitionError::NotFound(ProposalId(999))
        );
        let last = ProposalId(MAX_PENDING_PROPOSALS as u64 + 1);
        b.set_estimate(last, Estimate { rows: 3, exact: true, tolerance: 0 }).unwrap();
        assert!(b.set_estimate(ProposalId(1), Estimate { rows: 3, exact: true, tolerance: 0 }).is_err(), "decided: estimate is frozen");
        assert!(Estimate { rows: 10, exact: false, tolerance: 2 }.accepts(12) && !Estimate { rows: 10, exact: false, tolerance: 2 }.accepts(13));
    }
}
