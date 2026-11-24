//! Every value used by a liquidation decision is derived from a price that has
//! passed the shared oracle freshness gate. There is intentionally no public
//! accessor for a raw feed observation here.

use std::fmt;

use stellarlend_oracle_staleness::{
    GuardedOracleReads, PriceSource, StalenessError, StalenessPolicy, StalenessEvents,
};

/// Basis-point divisor used by thresholds, close factors and incentives.
pub const BPS_DIVISOR: u128 = 10_000;
/// Upper bound accepted for a liquidation incentive.
pub const MAX_INCENTIVE_BPS: u128 = 2_000;

/// Failures produced while checking whether a position may be liquidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidationCheckError {
    /// The oracle read did not satisfy the freshness policy.
    StaleOracle(StalenessError),
    /// An amount, price or configuration value was unusable.
    InvalidAmount,
    /// Arithmetic could not be represented without overflow.
    Overflow,
}

impl fmt::Display for LiquidationCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleOracle(error) => write!(f, "stale oracle: {error}"),
            Self::InvalidAmount => f.write_str("invalid liquidation amount"),
            Self::Overflow => f.write_str("liquidation arithmetic overflow"),
        }
    }
}

impl std::error::Error for LiquidationCheckError {}

impl From<StalenessError> for LiquidationCheckError {
    fn from(error: StalenessError) -> Self {
        Self::StaleOracle(error)
    }
}

/// Inputs that must be priced before a liquidation can be planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub collateral_asset: &'static str,
    pub collateral_amount: u128,
    pub debt_asset: &'static str,
    pub debt_amount: u128,
    /// Threshold in basis points applied to collateral when computing health.
    pub liquidation_threshold_bps: u128,
}

/// Result of a staleness-gated liquidation evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationDecision {
    pub health_factor_bps: u128,
    pub liquidatable: bool,
    pub collateral_value: u128,
    pub debt_value: u128,
    pub max_repay_value: u128,
    pub seize_value: u128,
}

/// The only consumer-facing surface for liquidation risk reads.
///
/// The wrapped [`GuardedOracleReads`] is private and every method obtains
/// prices through it, so a stale or missing observation can never reach the
/// health-factor or seize calculations.
pub struct LiquidationChecks<'a, S: PriceSource, E: StalenessEvents> {
    reads: GuardedOracleReads<'a, S, E>,
}

impl<'a, S: PriceSource, E: StalenessEvents> LiquidationChecks<'a, S, E> {
    pub fn new(
        source: &'a S,
        now: u64,
        policy: StalenessPolicy,
        events: &'a mut E,
    ) -> Result<Self, LiquidationCheckError> {
        // Validate once so a misconfigured policy cannot yield a decision.
        StalenessPolicy::new(policy.max_age_seconds)
            .map_err(LiquidationCheckError::StaleOracle)?;
        Ok(Self {
            reads: GuardedOracleReads::new(source, now, policy, events),
        })
    }

    /// Price collateral through the staleness gate.
    pub fn collateral_value(
        &mut self,
        asset: &str,
        amount: u128,
    ) -> Result<u128, LiquidationCheckError> {
        let price = self.reads.read(asset)?;
        let price = u128::try_from(price.value()).map_err(|_| LiquidationCheckError::InvalidAmount)?;
        amount.checked_mul(price).ok_or(LiquidationCheckError::Overflow)
    }

    /// Price debt through the staleness gate.
    pub fn debt_value(
        &mut self,
        asset: &str,
        amount: u128,
    ) -> Result<u128, LiquidationCheckError> {
        let price = self.reads.read(asset)?;
        let price = u128::try_from(price.value()).map_err(|_| LiquidationCheckError::InvalidAmount)?;
        amount.checked_mul(price).ok_or(LiquidationCheckError::Overflow)
    }

    /// Evaluate a position. Both collateral and debt prices are freshly read
    /// in this call; cached values from a previous evaluation are never used.
    pub fn evaluate(
        &mut self,
        position: &Position,
        requested_repay_value: u128,
        close_factor_bps: u128,
        incentive_bps: u128,
    ) -> Result<LiquidationDecision, LiquidationCheckError> {
        if close_factor_bps > BPS_DIVISOR || incentive_bps > MAX_INCENTIVE_BPS {
            return Err(LiquidationCheckError::InvalidAmount);
        }
        if position.liquidation_threshold_bps > BPS_DIVISOR {
            return Err(LiquidationCheckError::InvalidAmount);
        }

        let collateral_value = self.collateral_value(
            position.collateral_asset,
            position.collateral_amount,
        )?;
        let debt_value = self.debt_value(position.debt_asset, position.debt_amount)?;

        if debt_value == 0 {
            return Ok(LiquidationDecision {
                health_factor_bps: u128::MAX,
                liquidatable: false,
                collateral_value,
                debt_value,
                max_repay_value: 0,
                seize_value: 0,
            });
        }

        let health_factor_bps = mul_div(
            collateral_value,
            position.liquidation_threshold_bps,
            debt_value,
        )?;
        let liquidatable = health_factor_bps < BPS_DIVISOR;
        let close_factor_value = mul_div(debt_value, close_factor_bps, BPS_DIVISOR)?;
        let max_repay_value = if requested_repay_value == 0 {
            close_factor_value
        } else {
            requested_repay_value.min(close_factor_value)
        };
        let seize_value = mul_div(
            max_repay_value,
            BPS_DIVISOR + incentive_bps,
            BPS_DIVISOR,
        )?
        .min(collateral_value);

        Ok(LiquidationDecision {
            health_factor_bps,
            liquidatable,
            collateral_value,
            debt_value,
            max_repay_value,
            seize_value,
        })
    }
}

fn mul_div(
    value: u128,
    numerator: u128,
    denominator: u128,
) -> Result<u128, LiquidationCheckError> {
    if denominator == 0 {
        return Err(LiquidationCheckError::InvalidAmount);
    }
    let quotient = value / denominator;
    let remainder = value % denominator;
    let whole = quotient
        .checked_mul(numerator)
        .ok_or(LiquidationCheckError::Overflow)?;
    let fraction = remainder
        .checked_mul(numerator)
        .ok_or(LiquidationCheckError::Overflow)?
        / denominator;
    whole
        .checked_add(fraction)
        .ok_or(LiquidationCheckError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellarlend_oracle_staleness::{PriceObservation, RecordingEvents, StalenessEvent};

    struct Feed(Vec<(&'static str, PriceObservation)>);

    impl PriceSource for Feed {
        fn observation(&self, asset: &str) -> Option<PriceObservation> {
            self.0
                .iter()
                .find(|(name, _)| *name == asset)
                .map(|(_, observation)| *observation)
        }
    }

    fn feed(collateral_at: u64, debt_at: u64) -> Feed {
        Feed(vec![
            ("XLM", PriceObservation::new(100, collateral_at)),
            ("USDC", PriceObservation::new(1, debt_at)),
        ])
    }

    fn position() -> Position {
        Position {
            collateral_asset: "XLM",
            collateral_amount: 120,
            debt_asset: "USDC",
            debt_amount: 100,
            liquidation_threshold_bps: 9_000,
        }
    }

    #[test]
    fn evaluates_fresh_positions_and_emits_one_event_per_read() {
        let source = feed(9_950, 9_990);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::new(100).unwrap(), &mut events)
                .unwrap();

        let decision = checks.evaluate(&position(), 0, 5_000, 500).unwrap();
        assert_eq!(decision.collateral_value, 12_000);
        assert_eq!(decision.debt_value, 100);
        assert_eq!(decision.health_factor_bps, 1_080_000);
        assert!(!decision.liquidatable);
        assert_eq!(decision.max_repay_value, 50);
        assert_eq!(decision.seize_value, 52);
        assert_eq!(events.accepted(), 2);
        assert_eq!(events.rejected(), 0);
    }

    #[test]
    fn unhealthy_position_is_liquidatable() {
        let source = feed(9_950, 9_990);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::default(), &mut events)
                .unwrap();
        let mut position = position();
        position.collateral_amount = 105;
        position.debt_amount = 10_000;

        let decision = checks.evaluate(&position, 0, 10_000, 1_000).unwrap();
        assert!(decision.liquidatable);
        assert!(decision.health_factor_bps < BPS_DIVISOR);
        assert_eq!(decision.max_repay_value, 10_000);
        assert_eq!(decision.seize_value, 10_500);
    }

    #[test]
    fn stale_collateral_blocks_the_decision() {
        let source = feed(9_000, 9_990);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::new(100).unwrap(), &mut events)
                .unwrap();

        assert_eq!(
            checks.evaluate(&position(), 0, 5_000, 500).unwrap_err(),
            LiquidationCheckError::StaleOracle(StalenessError::Stale)
        );
        assert_eq!(events.accepted(), 0);
        assert_eq!(events.rejections(StalenessError::Stale), 1);
    }

    #[test]
    fn stale_debt_blocks_even_when_collateral_is_fresh() {
        let source = feed(9_950, 8_000);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::new(100).unwrap(), &mut events)
                .unwrap();

        assert_eq!(
            checks.debt_value("USDC", 100).unwrap_err(),
            LiquidationCheckError::StaleOracle(StalenessError::Stale)
        );
        assert_eq!(
            events.events(),
            &[StalenessEvent::Rejected {
                asset: "USDC".to_owned(),
                reason: StalenessError::Stale,
                timestamp_seconds: Some(8_000),
                now: 10_000,
                max_age_seconds: 100,
            }]
        );
    }

    #[test]
    fn missing_and_future_prices_fail_closed() {
        let empty = Feed(vec![]);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&empty, 10_000, StalenessPolicy::default(), &mut events).unwrap();
        assert_eq!(
            checks.collateral_value("XLM", 1).unwrap_err(),
            LiquidationCheckError::StaleOracle(StalenessError::MissingPrice)
        );

        let future = Feed(vec![("XLM", PriceObservation::new(1, 10_001))]);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&future, 10_000, StalenessPolicy::default(), &mut events).unwrap();
        assert_eq!(
            checks.collateral_value("XLM", 1).unwrap_err(),
            LiquidationCheckError::StaleOracle(StalenessError::FutureTimestamp)
        );
    }

    #[test]
    fn zero_debt_is_not_liquidatable() {
        let source = feed(9_950, 9_990);
        let mut events = RecordingEvents::new();
        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::default(), &mut events).unwrap();
        let mut position = position();
        position.debt_amount = 0;

        let decision = checks.evaluate(&position, 0, 5_000, 0).unwrap();
        assert!(!decision.liquidatable);
        assert_eq!(decision.health_factor_bps, u128::MAX);
        assert_eq!(decision.max_repay_value, 0);
    }

    #[test]
    fn invalid_configuration_and_overflow_are_rejected() {
        let source = feed(9_950, 9_990);
        let mut events = RecordingEvents::new();
        assert!(LiquidationChecks::new(
            &source,
            10_000,
            StalenessPolicy { max_age_seconds: 0 },
            &mut events
        )
        .is_err());

        let mut checks =
            LiquidationChecks::new(&source, 10_000, StalenessPolicy::default(), &mut events).unwrap();

        assert_eq!(
            checks.evaluate(&position(), 0, 10_001, 0).unwrap_err(),
            LiquidationCheckError::InvalidAmount
        );
        assert_eq!(
            checks.evaluate(&position(), 0, 5_000, 2_001).unwrap_err(),
            LiquidationCheckError::InvalidAmount
        );

        let mut huge = position();
        huge.collateral_amount = u128::MAX;
        assert_eq!(
            checks.evaluate(&huge, 0, 5_000, 0).unwrap_err(),
            LiquidationCheckError::Overflow
        );
    }
}
