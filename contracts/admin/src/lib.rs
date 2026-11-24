//! Administrative pause controls (issue #1781).
//!
//! The pause controller is the only supported way to change a protocol pause
//! switch. It requires a configured multi-sig quorum, a timelock delay and an
//! audit event for every accepted or rejected transition.

pub mod pause;

pub use pause::{
    NoOpAudit, OwnedAuditEvent, PauseAudit, PauseAuditEvent, PauseAuditEventKind, PauseController,
    PauseDecision, PauseError, PausePolicy, PauseRequest, PauseTarget, RecordingAudit,
    RequestState, DEFAULT_EXECUTION_WINDOW_SECS, DEFAULT_MIN_DELAY_SECS, HARD_MIN_DELAY_SECS,
    MAX_DELAY_SECS, MAX_SIGNERS,
};
