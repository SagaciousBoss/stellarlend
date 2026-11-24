use std::collections::HashMap;

use stellarlend_oracle_staleness::{
    check_staleness, GuardedOracleReads, PriceObservation, PriceSource, RecordingEvents,
    StalenessError, StalenessEvent, StalenessPolicy,
};

struct Feed(HashMap<&'static str, PriceObservation>);

impl PriceSource for Feed {
    fn observation(&self, asset: &str) -> Option<PriceObservation> {
        self.0.get(asset).copied()
    }
}

#[test]
fn multi_asset_read_fails_closed_when_one_feed_is_stale() {
    let feed = Feed(HashMap::from([
        ("XLM", PriceObservation::new(110, 9_900)),
        ("USDC", PriceObservation::new(1, 9_000)),
    ]));
    let mut events = RecordingEvents::new();
    let mut reads = GuardedOracleReads::new(&feed, 10_000, StalenessPolicy::new(500).unwrap(), &mut events);

    let first = reads.read("XLM").unwrap();
    assert_eq!(first.value(), 110);
    assert_eq!(reads.read("USDC").unwrap_err(), StalenessError::Stale);

    assert_eq!(events.accepted(), 1);
    assert_eq!(events.rejected(), 1);
    assert!(events.events().iter().any(|event| matches!(
        event,
        StalenessEvent::Rejected { asset, reason: StalenessError::Stale, .. } if asset == "USDC"
    )));
}

#[test]
fn no_observation_can_be_promoted_without_a_check() {
    let mut events = RecordingEvents::new();
    let result = check_staleness(
        "BTC",
        Some(PriceObservation::new(i128::MAX, u64::MAX)),
        0,
        StalenessPolicy::default(),
        &mut events,
    );
    assert_eq!(result.unwrap_err(), StalenessError::FutureTimestamp);
    assert_eq!(events.rejected(), 1);
}
