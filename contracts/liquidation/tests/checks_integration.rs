use std::collections::HashMap;

use stellarlend_liquidation_checks::{
    LiquidationCheckError, LiquidationChecks, Position,
};
use stellarlend_oracle_staleness::{
    PriceObservation, PriceSource, RecordingEvents, StalenessError, StalenessPolicy,
};

struct Feed(HashMap<&'static str, PriceObservation>);

impl PriceSource for Feed {
    fn observation(&self, asset: &str) -> Option<PriceObservation> {
        self.0.get(asset).copied()
    }
}

fn position() -> Position {
    Position {
        collateral_asset: "XLM",
        collateral_amount: 100,
        debt_asset: "USDC",
        debt_amount: 9_500,
        liquidation_threshold_bps: 9_000,
    }
}

#[test]
fn batch_evaluation_fails_closed_if_any_read_is_stale() {
    let fresh = Feed(HashMap::from([
        ("XLM", PriceObservation::new(100, 9_950)),
        ("USDC", PriceObservation::new(1, 9_950)),
    ]));
    let mut events = RecordingEvents::new();
    {
        let mut checks =
            LiquidationChecks::new(&fresh, 10_000, StalenessPolicy::new(100).unwrap(), &mut events)
                .unwrap();
        let first = checks.evaluate(&position(), 0, 5_000, 500).unwrap();
        assert!(first.liquidatable);
    }

    // A later evaluation with a degraded debt feed must not reuse the earlier
    // fresh value.
    let degraded = Feed(HashMap::from([
        ("XLM", PriceObservation::new(100, 9_950)),
        ("USDC", PriceObservation::new(1, 9_050)),
    ]));
    let err = {
        let mut checks = LiquidationChecks::new(
            &degraded,
            10_000,
            StalenessPolicy::new(100).unwrap(),
            &mut events,
        )
        .unwrap();
        checks.evaluate(&position(), 0, 5_000, 500).unwrap_err()
    };
    assert_eq!(
        err,
        LiquidationCheckError::StaleOracle(StalenessError::Stale)
    );
    // First evaluation: two accepted reads. Second: fresh collateral, stale debt.
    assert_eq!(events.accepted(), 3);
    assert_eq!(events.rejected(), 1);
}

#[test]
fn evaluation_uses_distinct_assets_and_rejects_unknown_collateral() {
    let feed = Feed(HashMap::from([("USDC", PriceObservation::new(1, 9_950))]));
    let mut events = RecordingEvents::new();
    let mut checks =
        LiquidationChecks::new(&feed, 10_000, StalenessPolicy::new(100).unwrap(), &mut events)
            .unwrap();

    assert_eq!(
        checks.evaluate(&position(), 0, 5_000, 0).unwrap_err(),
        LiquidationCheckError::StaleOracle(StalenessError::MissingPrice)
    );
    assert_eq!(events.rejected(), 1);
}
