//! Oracle freshness primitives shared by every risk-sensitive consumer.
//!
//! The only way to obtain a usable price from [`GuardedOracleReads`] is
//! [`check_staleness`], which rejects missing, non-positive, future-dated and
//! stale observations before the value can reach liquidation or borrow math.

pub mod staleness;

pub use staleness::{
    check_staleness, age_seconds, FreshPrice, GuardedOracleReads, NoOpEvents, PriceObservation,
    PriceSource, RecordingEvents, StalenessError, StalenessEvent, StalenessEvents, StalenessPolicy,
    DEFAULT_MAX_AGE_SECONDS,
};
