//! Oracle staleness enforcement for all risk-sensitive price reads.
//!
//! A raw [`PriceObservation`] is never treated as usable until it has passed
//! [`check_staleness`]. The check is total: it rejects absent, non-positive,
//! future-dated and stale observations instead of panicking or falling back to
//! an unverified value. Every accepted and rejected read emits a
//! [`StalenessEvent`] so operators can trace why a liquidation or borrow was
//! allowed or blocked.

use std::fmt;

/// Default maximum accepted age of a price observation (one hour).
pub const DEFAULT_MAX_AGE_SECONDS: u64 = 3_600;

/// Why an observation was not usable for a risk calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StalenessError {
    /// No observation was available for the requested asset.
    MissingPrice,
    /// Prices must be strictly positive to value collateral or debt.
    NonPositivePrice,
    /// The feed reported a timestamp from after the current ledger time.
    FutureTimestamp,
    /// The observation is older than the configured maximum age.
    Stale,
    /// The policy itself cannot be satisfied (for example, a zero window).
    InvalidPolicy,
}

impl fmt::Display for StalenessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingPrice => "missing oracle price",
            Self::NonPositivePrice => "oracle price must be positive",
            Self::FutureTimestamp => "oracle timestamp is in the future",
            Self::Stale => "oracle price is stale",
            Self::InvalidPolicy => "invalid staleness policy",
        };
        f.write_str(message)
    }
}

impl std::error::Error for StalenessError {}

/// A feed's raw output. This type deliberately has no arithmetic helpers: it
/// must be validated before a consumer can read `value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceObservation {
    pub value: i128,
    pub timestamp_seconds: u64,
}

impl PriceObservation {
    pub fn new(value: i128, timestamp_seconds: u64) -> Self {
        Self {
            value,
            timestamp_seconds,
        }
    }
}

/// Bounds accepted by [`check_staleness`]. An observation whose age is exactly
/// `max_age_seconds` is still fresh; one second older is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalenessPolicy {
    pub max_age_seconds: u64,
}

impl Default for StalenessPolicy {
    fn default() -> Self {
        Self {
            max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
        }
    }
}

impl StalenessPolicy {
    pub fn new(max_age_seconds: u64) -> Result<Self, StalenessError> {
        if max_age_seconds == 0 {
            return Err(StalenessError::InvalidPolicy);
        }
        Ok(Self { max_age_seconds })
    }
}

/// An observation that has been checked against the policy. Constructing this
/// type outside [`check_staleness`] is impossible because its fields are
/// private; consumers therefore cannot accidentally bypass the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreshPrice {
    value: i128,
    timestamp_seconds: u64,
    age_seconds: u64,
}

impl FreshPrice {
    pub fn value(&self) -> i128 {
        self.value
    }

    pub fn timestamp_seconds(&self) -> u64 {
        self.timestamp_seconds
    }

    pub fn age_seconds(&self) -> u64 {
        self.age_seconds
    }
}

/// Structured observability record for a freshness decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessEvent {
    Accepted {
        asset: String,
        value: i128,
        age_seconds: u64,
        max_age_seconds: u64,
    },
    Rejected {
        asset: String,
        reason: StalenessError,
        timestamp_seconds: Option<u64>,
        now: u64,
        max_age_seconds: u64,
    },
}

impl StalenessEvent {
    pub fn asset(&self) -> &str {
        match self {
            Self::Accepted { asset, .. } | Self::Rejected { asset, .. } => asset,
        }
    }
}

/// Sink for freshness decisions. Implementations can forward events to
/// contract events, logs or metrics without changing the check itself.
pub trait StalenessEvents {
    fn record(&mut self, event: StalenessEvent);
}

impl<T: StalenessEvents + ?Sized> StalenessEvents for &mut T {
    fn record(&mut self, event: StalenessEvent) {
        (**self).record(event);
    }
}

/// Sink that discards events, for callers that do not emit telemetry.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpEvents;

impl StalenessEvents for NoOpEvents {
    fn record(&mut self, _event: StalenessEvent) {}
}

/// In-memory sink used by contract tests and metrics exporters.
#[derive(Debug, Default)]
pub struct RecordingEvents {
    events: Vec<StalenessEvent>,
}

impl RecordingEvents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> &[StalenessEvent] {
        &self.events
    }

    pub fn accepted(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, StalenessEvent::Accepted { .. }))
            .count()
    }

    pub fn rejected(&self) -> usize {
        self.events.len() - self.accepted()
    }

    pub fn rejections(&self, reason: StalenessError) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, StalenessEvent::Rejected { reason: found, .. } if *found == reason))
            .count()
    }
}

impl StalenessEvents for RecordingEvents {
    fn record(&mut self, event: StalenessEvent) {
        self.events.push(event);
    }
}

/// Non-decreasing clock difference. A future timestamp is rejected separately
/// by [`check_staleness`], so saturating here cannot turn a bad feed into a
/// fresh one.
pub fn age_seconds(now: u64, timestamp_seconds: u64) -> u64 {
    now.saturating_sub(timestamp_seconds)
}

/// Validate `observation` and return a price that is safe for risk math.
pub fn check_staleness<E: StalenessEvents>(
    asset: &str,
    observation: Option<PriceObservation>,
    now: u64,
    policy: StalenessPolicy,
    events: &mut E,
) -> Result<FreshPrice, StalenessError> {
    if policy.max_age_seconds == 0 {
        events.record(StalenessEvent::Rejected {
            asset: asset.to_owned(),
            reason: StalenessError::InvalidPolicy,
            timestamp_seconds: None,
            now,
            max_age_seconds: policy.max_age_seconds,
        });
        return Err(StalenessError::InvalidPolicy);
    }

    let price = observation.ok_or_else(|| {
        events.record(StalenessEvent::Rejected {
            asset: asset.to_owned(),
            reason: StalenessError::MissingPrice,
            timestamp_seconds: None,
            now,
            max_age_seconds: policy.max_age_seconds,
        });
        StalenessError::MissingPrice
    })?;

    let failure = if price.value <= 0 {
        Some(StalenessError::NonPositivePrice)
    } else if price.timestamp_seconds > now {
        Some(StalenessError::FutureTimestamp)
    } else {
        None
    };

    if let Some(reason) = failure {
        events.record(StalenessEvent::Rejected {
            asset: asset.to_owned(),
            reason,
            timestamp_seconds: Some(price.timestamp_seconds),
            now,
            max_age_seconds: policy.max_age_seconds,
        });
        return Err(reason);
    }

    let age = age_seconds(now, price.timestamp_seconds);
    if age > policy.max_age_seconds {
        events.record(StalenessEvent::Rejected {
            asset: asset.to_owned(),
            reason: StalenessError::Stale,
            timestamp_seconds: Some(price.timestamp_seconds),
            now,
            max_age_seconds: policy.max_age_seconds,
        });
        return Err(StalenessError::Stale);
    }

    events.record(StalenessEvent::Accepted {
        asset: asset.to_owned(),
        value: price.value,
        age_seconds: age,
        max_age_seconds: policy.max_age_seconds,
    });
    Ok(FreshPrice {
        value: price.value,
        timestamp_seconds: price.timestamp_seconds,
        age_seconds: age,
    })
}

/// Minimal feed interface. Implementations must not expose validated prices;
/// they only supply timestamped observations.
pub trait PriceSource {
    fn observation(&self, asset: &str) -> Option<PriceObservation>;
}

/// The single entry point for risk-sensitive consumers. Every read runs the
/// same policy and emits the same telemetry; callers never see the raw source.
pub struct GuardedOracleReads<'a, S: PriceSource, E: StalenessEvents> {
    source: &'a S,
    now: u64,
    policy: StalenessPolicy,
    events: &'a mut E,
}

impl<'a, S: PriceSource, E: StalenessEvents> GuardedOracleReads<'a, S, E> {
    pub fn new(source: &'a S, now: u64, policy: StalenessPolicy, events: &'a mut E) -> Self {
        Self {
            source,
            now,
            policy,
            events,
        }
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn policy(&self) -> StalenessPolicy {
        self.policy
    }

    /// Read one asset through the staleness gate.
    pub fn read(&mut self, asset: &str) -> Result<FreshPrice, StalenessError> {
        check_staleness(
            asset,
            self.source.observation(asset),
            self.now,
            self.policy,
            self.events,
        )
    }

    /// Read several assets, failing the whole operation if any one is stale.
    pub fn read_all<'b>(
        &mut self,
        assets: impl IntoIterator<Item = &'b str>,
    ) -> Result<Vec<FreshPrice>, StalenessError> {
        assets.into_iter().map(|asset| self.read(asset)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSource {
        observations: Vec<(&'static str, PriceObservation)>,
    }

    impl PriceSource for TestSource {
        fn observation(&self, asset: &str) -> Option<PriceObservation> {
            self.observations
                .iter()
                .find(|(name, _)| *name == asset)
                .map(|(_, observation)| *observation)
        }
    }

    fn check(
        observation: Option<PriceObservation>,
        now: u64,
        policy: StalenessPolicy,
    ) -> (Result<FreshPrice, StalenessError>, RecordingEvents) {
        let mut events = RecordingEvents::new();
        let result = check_staleness("XLM", observation, now, policy, &mut events);
        (result, events)
    }

    #[test]
    fn accepts_fresh_price_at_the_boundary() {
        let (result, events) = check(
            Some(PriceObservation::new(100, 900)),
            1_000,
            StalenessPolicy::new(100).unwrap(),
        );
        let price = result.unwrap();
        assert_eq!(price.value(), 100);
        assert_eq!(price.age_seconds(), 100);
        assert_eq!(price.timestamp_seconds(), 900);
        assert_eq!(events.accepted(), 1);
        assert_eq!(events.rejected(), 0);
        assert_eq!(
            events.events(),
            &[StalenessEvent::Accepted {
                asset: "XLM".to_owned(),
                value: 100,
                age_seconds: 100,
                max_age_seconds: 100,
            }]
        );
    }

    #[test]
    fn rejects_one_second_past_the_window() {
        let (result, events) = check(
            Some(PriceObservation::new(100, 899)),
            1_000,
            StalenessPolicy::new(100).unwrap(),
        );
        assert_eq!(result.unwrap_err(), StalenessError::Stale);
        assert_eq!(events.rejections(StalenessError::Stale), 1);
        assert_eq!(events.accepted(), 0);
    }

    #[test]
    fn rejects_missing_non_positive_and_future_prices() {
        let policy = StalenessPolicy::default();
        assert_eq!(
            check(None, 1_000, policy).0.unwrap_err(),
            StalenessError::MissingPrice
        );
        assert_eq!(
            check(Some(PriceObservation::new(0, 999)), 1_000, policy)
                .0
                .unwrap_err(),
            StalenessError::NonPositivePrice
        );
        assert_eq!(
            check(Some(PriceObservation::new(-5, 999)), 1_000, policy)
                .0
                .unwrap_err(),
            StalenessError::NonPositivePrice
        );
        assert_eq!(
            check(Some(PriceObservation::new(10, 1_001)), 1_000, policy)
                .0
                .unwrap_err(),
            StalenessError::FutureTimestamp
        );
        let (_, events) = check(Some(PriceObservation::new(10, 1_001)), 1_000, policy);
        assert_eq!(events.rejections(StalenessError::FutureTimestamp), 1);
    }

    #[test]
    fn rejects_invalid_policy_without_publishing_a_price() {
        let (result, events) = check(
            Some(PriceObservation::new(100, 1_000)),
            1_000,
            StalenessPolicy { max_age_seconds: 0 },
        );
        assert_eq!(result.unwrap_err(), StalenessError::InvalidPolicy);
        assert_eq!(events.rejections(StalenessError::InvalidPolicy), 1);
        assert_eq!(StalenessPolicy::new(0).unwrap_err(), StalenessError::InvalidPolicy);
    }

    #[test]
    fn guarded_reads_validate_every_asset() {
        let source = TestSource {
            observations: vec![
                ("XLM", PriceObservation::new(120, 1_000)),
                ("USDC", PriceObservation::new(1, 950)),
            ],
        };
        let mut events = RecordingEvents::new();
        let mut reads = GuardedOracleReads::new(
            &source,
            1_000,
            StalenessPolicy::new(100).unwrap(),
            &mut events,
        );

        assert_eq!(reads.read_all(["XLM", "USDC"]).unwrap().len(), 2);
        assert_eq!(reads.read("MISSING").unwrap_err(), StalenessError::MissingPrice);
        assert_eq!(events.accepted(), 2);
        assert_eq!(events.rejected(), 1);
    }

    #[test]
    fn display_messages_are_stable() {
        assert_eq!(StalenessError::Stale.to_string(), "oracle price is stale");
        assert_eq!(age_seconds(1_000, 2_000), 0);
    }
}
