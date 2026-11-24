//! Timelocked multi-sig pause controller with an append-only audit trail
//! (issue #1781).
//!
//! A single signer can propose a pause, but cannot execute it: the proposer's
//! authorization counts as one approval and at least one other configured
//! signer must approve. Execution is only possible after `eta`, which must
//! itself be at least `min_delay_secs` in the future when the request is
//! proposed. Every transition and every rejection is written to an injectable
//! audit sink, so rejected abuse attempts are observable too.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Hard floor for the timelock delay (one hour).
pub const HARD_MIN_DELAY_SECS: u64 = 3_600;
/// Default timelock delay (48 hours), matching the timelock contract.
pub const DEFAULT_MIN_DELAY_SECS: u64 = 172_800;
/// Maximum scheduling horizon (30 days).
pub const MAX_DELAY_SECS: u64 = 2_592_000;
/// Default window after `eta` during which an approved request may execute.
pub const DEFAULT_EXECUTION_WINDOW_SECS: u64 = 86_400;
/// Maximum number of configured signers.
pub const MAX_SIGNERS: usize = 16;

/// Failures returned by the pause controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseError {
    /// The signer set is empty, too small or oversized.
    InvalidSignerSet,
    /// A signer identifier was empty or duplicated.
    DuplicateSigner,
    /// The threshold is not a strict multi-sig threshold.
    InvalidThreshold,
    /// Delays or the execution window are outside the allowed range.
    InvalidDelay,
    /// The caller is not a configured signer.
    Unauthorized,
    /// An actor identifier was empty.
    InvalidActor,
    /// The target name is not allowlisted.
    InvalidTarget,
    /// The ETA is outside the scheduling window.
    InvalidEta,
    /// A timestamp computation overflowed.
    ArithmeticOverflow,
    /// No request exists with the supplied id.
    RequestNotFound,
    /// The signer already approved this request.
    AlreadyApproved,
    /// Fewer than `threshold` distinct signers approved.
    ThresholdNotReached,
    /// The timelock has not elapsed yet.
    TooEarly,
    /// The execution window has closed.
    WindowClosed,
    /// The request was already executed or cancelled.
    AlreadyFinalized,
    /// The audit sink rejected the event; the transition was not committed.
    AuditFailure,
}

impl fmt::Display for PauseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSignerSet => "invalid signer set",
            Self::DuplicateSigner => "duplicate or empty signer",
            Self::InvalidThreshold => "threshold must form a multi-sig quorum",
            Self::InvalidDelay => "invalid timelock delay",
            Self::Unauthorized => "caller is not a configured signer",
            Self::InvalidActor => "actor identifier is empty",
            Self::InvalidTarget => "pause target is not allowlisted",
            Self::InvalidEta => "eta outside the scheduling window",
            Self::ArithmeticOverflow => "timestamp arithmetic overflowed",
            Self::RequestNotFound => "pause request not found",
            Self::AlreadyApproved => "signer already approved this request",
            Self::ThresholdNotReached => "multi-sig threshold not reached",
            Self::TooEarly => "timelock has not elapsed",
            Self::WindowClosed => "execution window has closed",
            Self::AlreadyFinalized => "pause request is already finalized",
            Self::AuditFailure => "audit sink rejected the event",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PauseError {}

/// Protocol surfaces that can be paused. Parsing is allowlisted so hostile
/// input cannot introduce an unknown switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PauseTarget {
    Deposits,
    Withdrawals,
    Borrows,
    Repayments,
    Liquidations,
    Transfers,
    OracleUpdates,
    All,
}

impl PauseTarget {
    /// Stable lower-case name used in audit events and runbooks.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deposits => "deposits",
            Self::Withdrawals => "withdrawals",
            Self::Borrows => "borrows",
            Self::Repayments => "repayments",
            Self::Liquidations => "liquidations",
            Self::Transfers => "transfers",
            Self::OracleUpdates => "oracle_updates",
            Self::All => "all",
        }
    }

    /// Parse an allowlisted target name.
    pub fn parse(value: &str) -> Result<Self, PauseError> {
        match value {
            "deposits" => Ok(Self::Deposits),
            "withdrawals" => Ok(Self::Withdrawals),
            "borrows" => Ok(Self::Borrows),
            "repayments" => Ok(Self::Repayments),
            "liquidations" => Ok(Self::Liquidations),
            "transfers" => Ok(Self::Transfers),
            "oracle_updates" => Ok(Self::OracleUpdates),
            "all" => Ok(Self::All),
            _ => Err(PauseError::InvalidTarget),
        }
    }
}

/// Configuration of the pause quorum and its timelock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PausePolicy {
    /// Stable signer identifiers (addresses or key names).
    pub signers: Vec<String>,
    /// Number of distinct approvals required to execute a request.
    pub threshold: usize,
    /// Minimum lead time before `eta`.
    pub min_delay_secs: u64,
    /// Maximum scheduling horizon from proposal time.
    pub max_delay_secs: u64,
    /// Time after `eta` during which execution is still allowed.
    pub execution_window_secs: u64,
}

impl Default for PausePolicy {
    fn default() -> Self {
        Self {
            signers: ["signer-a", "signer-b", "signer-c"]
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            threshold: 2,
            min_delay_secs: DEFAULT_MIN_DELAY_SECS,
            max_delay_secs: MAX_DELAY_SECS,
            execution_window_secs: DEFAULT_EXECUTION_WINDOW_SECS,
        }
    }
}

impl PausePolicy {
    /// Build a policy with the default execution window.
    pub fn new(
        signers: &[&str],
        threshold: usize,
        min_delay_secs: u64,
        max_delay_secs: u64,
    ) -> Result<Self, PauseError> {
        let policy = Self {
            signers: signers.iter().map(|name| (*name).to_string()).collect(),
            threshold,
            min_delay_secs,
            max_delay_secs,
            execution_window_secs: DEFAULT_EXECUTION_WINDOW_SECS,
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Validate every invariant before installing the policy.
    pub fn validate(&self) -> Result<(), PauseError> {
        if self.signers.len() < 2 || self.signers.len() > MAX_SIGNERS {
            return Err(PauseError::InvalidSignerSet);
        }
        let mut unique = BTreeSet::new();
        for signer in &self.signers {
            if signer.is_empty() || !unique.insert(signer.as_str()) {
                return Err(PauseError::DuplicateSigner);
            }
        }
        if self.threshold < 2 || self.threshold > self.signers.len() {
            return Err(PauseError::InvalidThreshold);
        }
        if self.min_delay_secs < HARD_MIN_DELAY_SECS
            || self.max_delay_secs < self.min_delay_secs
            || self.max_delay_secs > MAX_DELAY_SECS
            || self.execution_window_secs == 0
            || self.execution_window_secs > MAX_DELAY_SECS
        {
            return Err(PauseError::InvalidDelay);
        }
        Ok(())
    }

    /// Whether `actor` is one of the configured signers.
    pub fn is_signer(&self, actor: &str) -> bool {
        self.signers.iter().any(|signer| signer == actor)
    }
}

/// Lifecycle state of a pause request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Pending,
    Executed,
    Cancelled,
}

impl RequestState {
    /// Stable lower-case name used in audit events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Executed => "executed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A proposed pause switch change. The proposer is recorded as the first
/// approval; reaching the threshold still requires other distinct signers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseRequest {
    pub id: u64,
    pub target: PauseTarget,
    pub paused: bool,
    pub proposed_at: u64,
    pub eta: u64,
    pub proposer: String,
    pub approvals: Vec<String>,
    pub state: RequestState,
}

impl PauseRequest {
    /// Digest binding a signer's approval to these exact parameters.
    pub fn digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        fnv(&mut hash, &self.id.to_le_bytes());
        fnv(&mut hash, self.target.as_str().as_bytes());
        fnv(&mut hash, &[self.paused as u8]);
        fnv(&mut hash, &self.proposed_at.to_le_bytes());
        fnv(&mut hash, &self.eta.to_le_bytes());
        fnv(&mut hash, self.proposer.as_bytes());
        hash
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= *byte as u64;
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Observed outcome of a successfully executed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseDecision {
    pub request_id: u64,
    pub target: PauseTarget,
    pub paused: bool,
    pub executed_at: u64,
    pub approvals: usize,
    pub threshold: usize,
    pub digest: u64,
}

/// Kind of audit event emitted by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAuditEventKind {
    PolicyConfigured,
    Proposed,
    Approved,
    Rejected,
    Cancelled,
    Executed,
}

impl PauseAuditEventKind {
    /// Stable lower-case name used by indexers and alerting rules.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PolicyConfigured => "policy_configured",
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Executed => "executed",
        }
    }
}

/// Structured audit record. Rejections reuse the same shape with a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseAuditEvent<'a> {
    pub kind: PauseAuditEventKind,
    pub actor: &'a str,
    pub request_id: Option<u64>,
    pub target: Option<PauseTarget>,
    pub paused: Option<bool>,
    pub eta: Option<u64>,
    pub timestamp: u64,
    pub approvals: usize,
    pub threshold: usize,
    pub digest: Option<u64>,
    pub trace_id: &'a str,
    pub reason: Option<&'a str>,
}

/// Sink for pause audit events. A failing sink aborts the transition.
pub trait PauseAudit {
    fn record(&mut self, event: PauseAuditEvent<'_>) -> Result<(), PauseError>;
}

/// Audit sink used only by tests and dry-run tooling.
#[derive(Debug, Default)]
pub struct NoOpAudit;

impl PauseAudit for NoOpAudit {
    fn record(&mut self, _event: PauseAuditEvent<'_>) -> Result<(), PauseError> {
        Ok(())
    }
}

/// Owned copy of an audit event for assertions and log shipping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedAuditEvent {
    pub kind: PauseAuditEventKind,
    pub actor: String,
    pub request_id: Option<u64>,
    pub target: Option<PauseTarget>,
    pub paused: Option<bool>,
    pub eta: Option<u64>,
    pub timestamp: u64,
    pub approvals: usize,
    pub threshold: usize,
    pub digest: Option<u64>,
    pub trace_id: String,
    pub reason: Option<String>,
}

impl<'a> From<PauseAuditEvent<'a>> for OwnedAuditEvent {
    fn from(event: PauseAuditEvent<'a>) -> Self {
        Self {
            kind: event.kind,
            actor: event.actor.to_string(),
            request_id: event.request_id,
            target: event.target,
            paused: event.paused,
            eta: event.eta,
            timestamp: event.timestamp,
            approvals: event.approvals,
            threshold: event.threshold,
            digest: event.digest,
            trace_id: event.trace_id.to_string(),
            reason: event.reason.map(str::to_string),
        }
    }
}

/// In-memory audit sink that records every event in order.
#[derive(Debug, Default)]
pub struct RecordingAudit {
    events: Vec<OwnedAuditEvent>,
}

impl RecordingAudit {
    /// Events recorded so far.
    pub fn events(&self) -> &[OwnedAuditEvent] {
        &self.events
    }

    /// Consume the sink and return the recorded events.
    pub fn into_events(self) -> Vec<OwnedAuditEvent> {
        self.events
    }
}

impl PauseAudit for RecordingAudit {
    fn record(&mut self, event: PauseAuditEvent<'_>) -> Result<(), PauseError> {
        self.events.push(event.into());
        Ok(())
    }
}

/// Stateful pause controller. All methods take `now` explicitly, keeping the
/// timelock deterministic and free of hidden clock reads.
pub struct PauseController<'a> {
    policy: PausePolicy,
    audit: &'a mut dyn PauseAudit,
    requests: BTreeMap<u64, PauseRequest>,
    next_id: u64,
    paused: BTreeMap<PauseTarget, bool>,
}

impl fmt::Debug for PauseController<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PauseController")
            .field("policy", &self.policy)
            .field("requests", &self.requests)
            .field("next_id", &self.next_id)
            .field("paused", &self.paused)
            .finish()
    }
}

impl<'a> PauseController<'a> {
    /// Install a validated policy and audit its configuration.
    pub fn new(policy: PausePolicy, audit: &'a mut dyn PauseAudit) -> Result<Self, PauseError> {
        policy.validate()?;
        let controller = Self {
            policy,
            audit,
            requests: BTreeMap::new(),
            next_id: 1,
            paused: BTreeMap::new(),
        };
        controller.audit.record(PauseAuditEvent {
            kind: PauseAuditEventKind::PolicyConfigured,
            actor: "system",
            request_id: None,
            target: None,
            paused: None,
            eta: None,
            timestamp: 0,
            approvals: 0,
            threshold: controller.policy.threshold,
            digest: None,
            trace_id: "pause_policy",
            reason: None,
        })?;
        Ok(controller)
    }

    /// Installed policy.
    pub fn policy(&self) -> &PausePolicy {
        &self.policy
    }

    /// All requests, including finalized ones, in id order.
    pub fn requests(&self) -> impl Iterator<Item = &PauseRequest> {
        self.requests.values()
    }

    /// Current state of one switch. Unset targets default to running.
    pub fn is_paused(&self, target: PauseTarget) -> bool {
        self.paused.get(&target).copied().unwrap_or(false)
    }

    /// Propose a switch change. The proposer's approval is recorded, so at
    /// least one other signer must still approve before execution.
    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &mut self,
        actor: &str,
        target: PauseTarget,
        paused: bool,
        eta: u64,
        now: u64,
        trace_id: &str,
    ) -> Result<u64, PauseError> {
        self.assert_signer(actor, None, now, trace_id)?;

        let min_eta = now
            .checked_add(self.policy.min_delay_secs)
            .ok_or(PauseError::ArithmeticOverflow)
            .or_else(|error| {
                Err(self.reject(
                    actor,
                    None,
                    Some(target),
                    now,
                    trace_id,
                    error,
                    "timestamp_overflow",
                ))
            })?;
        let max_eta = now
            .checked_add(self.policy.max_delay_secs)
            .ok_or(PauseError::ArithmeticOverflow)
            .or_else(|error| {
                Err(self.reject(
                    actor,
                    None,
                    Some(target),
                    now,
                    trace_id,
                    error,
                    "timestamp_overflow",
                ))
            })?;
        if eta < min_eta || eta > max_eta {
            return Err(self.reject(
                actor,
                None,
                Some(target),
                now,
                trace_id,
                PauseError::InvalidEta,
                "eta_outside_window",
            ));
        }

        if self.next_id == u64::MAX {
            return Err(self.reject(
                actor,
                None,
                Some(target),
                now,
                trace_id,
                PauseError::ArithmeticOverflow,
                "request_id_exhausted",
            ));
        }
        let id = self.next_id;
        let request = PauseRequest {
            id,
            target,
            paused,
            proposed_at: now,
            eta,
            proposer: actor.to_string(),
            approvals: vec![actor.to_string()],
            state: RequestState::Pending,
        };
        let digest = request.digest();
        self.emit(PauseAuditEvent {
            kind: PauseAuditEventKind::Proposed,
            actor,
            request_id: Some(id),
            target: Some(target),
            paused: Some(paused),
            eta: Some(eta),
            timestamp: now,
            approvals: request.approvals.len(),
            threshold: self.policy.threshold,
            digest: Some(digest),
            trace_id,
            reason: None,
        })?;

        self.next_id = id + 1;
        self.requests.insert(id, request);
        Ok(id)
    }

    /// Record a second (or later) distinct signer's approval.
    pub fn approve(
        &mut self,
        actor: &str,
        request_id: u64,
        now: u64,
        trace_id: &str,
    ) -> Result<(), PauseError> {
        self.assert_signer(actor, Some(request_id), now, trace_id)?;
        if !self.requests.contains_key(&request_id) {
            return Err(self.reject(
                actor,
                Some(request_id),
                None,
                now,
                trace_id,
                PauseError::RequestNotFound,
                "request_not_found",
            ));
        }
        let request = self
            .requests
            .get(&request_id)
            .expect("checked above")
            .clone();
        self.assert_pending(&request, actor, now, trace_id)?;
        if now > request.eta {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::WindowClosed,
                "approval_window_closed",
            ));
        }
        if request.approvals.iter().any(|signer| signer == actor) {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::AlreadyApproved,
                "duplicate_approval",
            ));
        }

        self.emit(PauseAuditEvent {
            kind: PauseAuditEventKind::Approved,
            actor,
            request_id: Some(request_id),
            target: Some(request.target),
            paused: Some(request.paused),
            eta: Some(request.eta),
            timestamp: now,
            approvals: request.approvals.len() + 1,
            threshold: self.policy.threshold,
            digest: Some(request.digest()),
            trace_id,
            reason: None,
        })?;
        self.requests
            .get_mut(&request_id)
            .expect("checked above")
            .approvals
            .push(actor.to_string());
        Ok(())
    }

    /// Execute a fully approved request after its ETA. The executor must be
    /// one of the approving signers.
    pub fn execute(
        &mut self,
        actor: &str,
        request_id: u64,
        now: u64,
        trace_id: &str,
    ) -> Result<PauseDecision, PauseError> {
        self.assert_signer(actor, Some(request_id), now, trace_id)?;
        if !self.requests.contains_key(&request_id) {
            return Err(self.reject(
                actor,
                Some(request_id),
                None,
                now,
                trace_id,
                PauseError::RequestNotFound,
                "request_not_found",
            ));
        }
        let request = self
            .requests
            .get(&request_id)
            .expect("checked above")
            .clone();
        self.assert_pending(&request, actor, now, trace_id)?;
        if now < request.eta {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::TooEarly,
                "timelock_not_elapsed",
            ));
        }
        let deadline = request
            .eta
            .checked_add(self.policy.execution_window_secs)
            .ok_or(PauseError::ArithmeticOverflow)
            .or_else(|error| {
                Err(self.reject(
                    actor,
                    Some(request_id),
                    Some(request.target),
                    now,
                    trace_id,
                    error,
                    "deadline_overflow",
                ))
            })?;
        if now > deadline {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::WindowClosed,
                "execution_window_closed",
            ));
        }
        if request.approvals.len() < self.policy.threshold {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::ThresholdNotReached,
                "quorum_not_reached",
            ));
        }
        if !request.approvals.iter().any(|signer| signer == actor) {
            return Err(self.reject(
                actor,
                Some(request_id),
                Some(request.target),
                now,
                trace_id,
                PauseError::Unauthorized,
                "executor_not_an_approver",
            ));
        }

        let digest = request.digest();
        let decision = PauseDecision {
            request_id,
            target: request.target,
            paused: request.paused,
            executed_at: now,
            approvals: request.approvals.len(),
            threshold: self.policy.threshold,
            digest,
        };
        self.emit(PauseAuditEvent {
            kind: PauseAuditEventKind::Executed,
            actor,
            request_id: Some(request_id),
            target: Some(request.target),
            paused: Some(request.paused),
            eta: Some(request.eta),
            timestamp: now,
            approvals: request.approvals.len(),
            threshold: self.policy.threshold,
            digest: Some(digest),
            trace_id,
            reason: None,
        })?;

        self.requests
            .get_mut(&request_id)
            .expect("checked above")
            .state = RequestState::Executed;
        self.paused.insert(request.target, request.paused);
        Ok(decision)
    }

    /// Cancel a pending request. Any configured signer may do so.
    pub fn cancel(
        &mut self,
        actor: &str,
        request_id: u64,
        now: u64,
        trace_id: &str,
    ) -> Result<(), PauseError> {
        self.assert_signer(actor, Some(request_id), now, trace_id)?;
        if !self.requests.contains_key(&request_id) {
            return Err(self.reject(
                actor,
                Some(request_id),
                None,
                now,
                trace_id,
                PauseError::RequestNotFound,
                "request_not_found",
            ));
        }
        let request = self
            .requests
            .get(&request_id)
            .expect("checked above")
            .clone();
        self.assert_pending(&request, actor, now, trace_id)?;

        self.emit(PauseAuditEvent {
            kind: PauseAuditEventKind::Cancelled,
            actor,
            request_id: Some(request_id),
            target: Some(request.target),
            paused: Some(request.paused),
            eta: Some(request.eta),
            timestamp: now,
            approvals: request.approvals.len(),
            threshold: self.policy.threshold,
            digest: Some(request.digest()),
            trace_id,
            reason: None,
        })?;
        self.requests
            .get_mut(&request_id)
            .expect("checked above")
            .state = RequestState::Cancelled;
        Ok(())
    }

    fn assert_pending(
        &mut self,
        request: &PauseRequest,
        actor: &str,
        now: u64,
        trace_id: &str,
    ) -> Result<(), PauseError> {
        if request.state == RequestState::Pending {
            Ok(())
        } else {
            Err(self.reject(
                actor,
                Some(request.id),
                Some(request.target),
                now,
                trace_id,
                PauseError::AlreadyFinalized,
                "request_already_finalized",
            ))
        }
    }

    fn assert_signer(
        &mut self,
        actor: &str,
        request_id: Option<u64>,
        now: u64,
        trace_id: &str,
    ) -> Result<(), PauseError> {
        if actor.is_empty() {
            return Err(self.reject(
                actor,
                request_id,
                None,
                now,
                trace_id,
                PauseError::InvalidActor,
                "empty_actor",
            ));
        }
        if !self.policy.is_signer(actor) {
            return Err(self.reject(
                actor,
                request_id,
                None,
                now,
                trace_id,
                PauseError::Unauthorized,
                "not_a_signer",
            ));
        }
        Ok(())
    }

    fn emit(&mut self, event: PauseAuditEvent<'_>) -> Result<(), PauseError> {
        self.audit
            .record(event)
            .map_err(|_| PauseError::AuditFailure)
    }

    #[allow(clippy::too_many_arguments)]
    fn reject(
        &mut self,
        actor: &str,
        request_id: Option<u64>,
        target: Option<PauseTarget>,
        now: u64,
        trace_id: &str,
        error: PauseError,
        reason: &'static str,
    ) -> PauseError {
        let event = PauseAuditEvent {
            kind: PauseAuditEventKind::Rejected,
            actor,
            request_id,
            target,
            paused: None,
            eta: None,
            timestamp: now,
            approvals: 0,
            threshold: self.policy.threshold,
            digest: None,
            trace_id,
            reason: Some(reason),
        };
        let _ = self.emit(event);
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller<'a>(
        audit: &'a mut dyn PauseAudit,
    ) -> (PauseController<'a>, u64, u64, u64) {
        let policy = PausePolicy::default();
        let now = 10_000;
        let eta = now + policy.min_delay_secs;
        let deadline = eta + policy.execution_window_secs;
        (
            PauseController::new(policy, audit).unwrap(),
            now,
            eta,
            deadline,
        )
    }

    #[test]
    fn policy_requires_a_real_multisig_and_sane_delays() {
        assert_eq!(
            PausePolicy::new(&["only"], 1, 3_600, MAX_DELAY_SECS).unwrap_err(),
            PauseError::InvalidSignerSet
        );
        assert_eq!(
            PausePolicy::new(&["a", "b"], 1, 3_600, MAX_DELAY_SECS).unwrap_err(),
            PauseError::InvalidThreshold
        );
        assert_eq!(
            PausePolicy::new(&["a", "a", "b"], 2, 3_600, MAX_DELAY_SECS).unwrap_err(),
            PauseError::DuplicateSigner
        );
        assert_eq!(
            PausePolicy::new(&["a", "b"], 2, 3_599, MAX_DELAY_SECS).unwrap_err(),
            PauseError::InvalidDelay
        );
        assert_eq!(
            PausePolicy::new(&["a", "b"], 2, 3_600, 3_599).unwrap_err(),
            PauseError::InvalidDelay
        );
        assert!(PausePolicy::new(&["a", "b"], 2, 3_600, MAX_DELAY_SECS).is_ok());
    }

    #[test]
    fn target_parsing_is_allowlisted() {
        assert_eq!(
            PauseTarget::parse("borrows"),
            Ok(PauseTarget::Borrows)
        );
        assert_eq!(
            PauseTarget::parse("all"),
            Ok(PauseTarget::All)
        );
        assert_eq!(
            PauseTarget::parse("anything"),
            Err(PauseError::InvalidTarget)
        );
        assert_eq!(PauseTarget::parse(""), Err(PauseError::InvalidTarget));
    }

    #[test]
    fn proposal_enforces_signer_and_timelock_window() {
        let mut audit = RecordingAudit::default();
        let (mut controller, now, eta, _) = controller(&mut audit);

        assert_eq!(
            controller.propose("mallory", PauseTarget::Borrows, true, eta, now, "t1"),
            Err(PauseError::Unauthorized)
        );
        assert_eq!(
            controller.propose("signer-a", PauseTarget::Borrows, true, eta - 1, now, "t2"),
            Err(PauseError::InvalidEta)
        );
        assert_eq!(
            controller.propose(
                "signer-a",
                PauseTarget::Borrows,
                true,
                now + MAX_DELAY_SECS + 1,
                now,
                "t3",
            ),
            Err(PauseError::InvalidEta)
        );
        assert_eq!(
            controller.propose("signer-a", PauseTarget::Borrows, true, eta, now, "t4"),
            Ok(1)
        );
        assert_eq!(controller.requests().count(), 1);
        assert!(!controller.is_paused(PauseTarget::Borrows));
    }

    #[test]
    fn two_distinct_signers_are_required_and_execution_waits_for_eta() {
        let mut audit = RecordingAudit::default();
        let (mut controller, now, eta, _) = controller(&mut audit);
        let id = controller
            .propose("signer-a", PauseTarget::Borrows, true, eta, now, "t5")
            .unwrap();

        // The proposer alone cannot execute, even after the timelock.
        assert_eq!(
            controller.execute("signer-a", id, eta, "t6"),
            Err(PauseError::ThresholdNotReached)
        );
        assert_eq!(
            controller.approve("signer-a", id, now, "t7"),
            Err(PauseError::AlreadyApproved)
        );
        controller.approve("signer-b", id, now, "t8").unwrap();
        assert_eq!(
            controller.execute("signer-b", id, eta - 1, "t9"),
            Err(PauseError::TooEarly)
        );

        let decision = controller.execute("signer-b", id, eta, "t10").unwrap();
        assert_eq!(decision.request_id, id);
        assert_eq!(decision.target, PauseTarget::Borrows);
        assert!(decision.paused);
        assert_eq!(decision.approvals, 2);
        assert_eq!(decision.threshold, 2);
        assert!(controller.is_paused(PauseTarget::Borrows));
    }

    #[test]
    fn finality_and_execution_window_are_enforced() {
        let mut audit = RecordingAudit::default();
        let (mut controller, now, eta, deadline) = controller(&mut audit);
        let id = controller
            .propose("signer-a", PauseTarget::Withdrawals, true, eta, now, "t11")
            .unwrap();
        controller.approve("signer-b", id, now, "t12").unwrap();
        assert_eq!(
            controller.execute("signer-b", id, deadline + 1, "t13"),
            Err(PauseError::WindowClosed)
        );
        assert!(!controller.is_paused(PauseTarget::Withdrawals));

        let id2 = controller
            .propose("signer-a", PauseTarget::Withdrawals, true, eta, now, "t14")
            .unwrap();
        controller.approve("signer-c", id2, now, "t15").unwrap();
        controller.cancel("signer-c", id2, now, "t16").unwrap();
        assert_eq!(
            controller.execute("signer-c", id2, eta, "t17"),
            Err(PauseError::AlreadyFinalized)
        );
        assert_eq!(
            controller.cancel("signer-c", id2, now, "t18"),
            Err(PauseError::AlreadyFinalized)
        );
        assert_eq!(
            controller.requests().nth(1).unwrap().state,
            RequestState::Cancelled
        );
    }

    #[test]
    fn unauthorized_executor_is_rejected_even_with_quorum() {
        let mut audit = RecordingAudit::default();
        let policy = PausePolicy::new(&["a", "b", "c", "d"], 2, 3_600, MAX_DELAY_SECS).unwrap();
        let mut controller = PauseController::new(policy, &mut audit).unwrap();
        let now = 1_000;
        let eta = now + HARD_MIN_DELAY_SECS;
        let id = controller
            .propose("a", PauseTarget::Transfers, true, eta, now, "t19")
            .unwrap();
        controller.approve("b", id, now, "t20").unwrap();
        assert_eq!(
            controller.execute("c", id, eta, "t21"),
            Err(PauseError::Unauthorized)
        );
        assert!(!controller.is_paused(PauseTarget::Transfers));
    }

    #[test]
    fn every_transition_and_rejection_is_audited() {
        let mut audit = RecordingAudit::default();
        let (mut controller, now, eta, _) = controller(&mut audit);
        assert_eq!(
            controller.propose("mallory", PauseTarget::All, true, eta, now, "trace"),
            Err(PauseError::Unauthorized)
        );
        let id = controller
            .propose("signer-a", PauseTarget::All, true, eta, now, "trace")
            .unwrap();
        controller.approve("signer-b", id, now, "trace").unwrap();
        controller.execute("signer-b", id, eta, "trace").unwrap();

        let events = audit.events();
        assert_eq!(events[0].kind, PauseAuditEventKind::PolicyConfigured);
        assert_eq!(events[1].kind, PauseAuditEventKind::Rejected);
        assert_eq!(events[1].actor, "mallory");
        assert_eq!(events[1].reason.as_deref(), Some("not_a_signer"));
        assert_eq!(events[2].kind, PauseAuditEventKind::Proposed);
        assert_eq!(events[3].kind, PauseAuditEventKind::Approved);
        assert_eq!(events[4].kind, PauseAuditEventKind::Executed);
        assert_eq!(events[4].request_id, Some(id));
        assert_eq!(events[4].target, Some(PauseTarget::All));
        assert_eq!(events[4].approvals, 2);
        assert_eq!(events[4].threshold, 2);
        assert_eq!(events[4].trace_id, "trace");
        assert_eq!(events[2].digest, events[4].digest);
        assert!(events.iter().all(|event| event.timestamp <= eta));
    }

    #[test]
    fn digest_binds_every_request_field() {
        let base = PauseRequest {
            id: 1,
            target: PauseTarget::Borrows,
            paused: true,
            proposed_at: 100,
            eta: 200,
            proposer: "signer-a".to_string(),
            approvals: vec!["signer-a".to_string()],
            state: RequestState::Pending,
        };
        let mut changed = base.clone();
        changed.paused = false;
        assert_ne!(base.digest(), changed.digest());
        changed = base.clone();
        changed.eta += 1;
        assert_ne!(base.digest(), changed.digest());
        changed = base.clone();
        changed.target = PauseTarget::Withdrawals;
        assert_ne!(base.digest(), changed.digest());
    }

    #[test]
    fn audit_failure_aborts_the_transition() {
        struct FailingAudit;
        impl PauseAudit for FailingAudit {
            fn record(&mut self, _event: PauseAuditEvent<'_>) -> Result<(), PauseError> {
                Err(PauseError::AuditFailure)
            }
        }
        let mut audit = FailingAudit;
        let policy = PausePolicy::default();
        assert_eq!(
            PauseController::new(policy, &mut audit).unwrap_err(),
            PauseError::AuditFailure
        );
    }
}
