//! # Borrow-Rate Curve Memoization (issue #1640)
//!
//! Evaluating the piecewise borrow-rate curve on every call site inside a
//! single transaction is redundant: utilization and rate-curve parameters do
//! not change between invocations within the same ledger sequence.
//!
//! This module wraps [`crate::interest_rate`] with a **per-block memo-cache**
//! backed by Soroban's `temporary` storage tier.  Temporary entries live for
//! exactly one ledger; there is no need to invalidate them manually.
//!
//! ## Cache key
//! ```text
//! RateCacheKey::BorrowRate(sequence)   → i128  (borrow rate bps)
//! RateCacheKey::SupplyRate(sequence)   → i128  (supply rate bps)
//! RateCacheKey::Utilization(sequence)  → i128  (utilization bps)
//! ```
//!
//! ## Usage
//! Replace direct calls to `interest_rate::calculate_borrow_rate(env)` with
//! `rates::get_borrow_rate(env)`.  The first call within a ledger hits the
//! hot path and stores the result; every subsequent call returns the cached
//! value with a single temporary-storage read.
//!
//! ## Observability
//! [`RateCacheMetrics`] accumulates hit/miss/error counters in temporary
//! storage.  Callers can read them via [`get_cache_metrics`] for dashboards
//! and CI assertions.

use soroban_sdk::{contracterror, contracttype, Address, Env};
use stellarlend_math::rates::{RateCurve, RateModelKind};

use crate::events::emit_event;
use crate::interest_rate::{
    calculate_borrow_rate, calculate_supply_rate, calculate_utilization, InterestRateError,
};

// ── Storage keys ─────────────────────────────────────────────────────────────

/// Temporary-storage keys used by this module.
///
/// Each key is tagged with the current ledger `sequence` number so the entry
/// is logically scoped to a single block even before Soroban's TTL evicts it.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum RateCacheKey {
    /// Cached borrow rate for `sequence`.
    BorrowRate(u32),
    /// Cached supply rate for `sequence`.
    SupplyRate(u32),
    /// Cached utilization for `sequence`.
    Utilization(u32),
    /// Hit/miss/error counters (not keyed by sequence; accumulates forever
    /// within the transaction but is cheap — it's temporary storage).
    Metrics,
}

// ── Metrics ───────────────────────────────────────────────────────────────────

/// Lightweight hit/miss/error counters for observability.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RateCacheMetrics {
    /// Number of times a cached value was returned without recomputing.
    pub hits: u32,
    /// Number of times the cache was cold and the curve had to be evaluated.
    pub misses: u32,
    /// Number of times the underlying rate computation returned an error.
    pub errors: u32,
}

impl RateCacheMetrics {
    fn zero() -> Self {
        RateCacheMetrics {
            hits: 0,
            misses: 0,
            errors: 0,
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn load_metrics(env: &Env) -> RateCacheMetrics {
    env.storage()
        .temporary()
        .get::<RateCacheKey, RateCacheMetrics>(&RateCacheKey::Metrics)
        .unwrap_or_else(RateCacheMetrics::zero)
}

fn save_metrics(env: &Env, m: &RateCacheMetrics) {
    env.storage()
        .temporary()
        .set(&RateCacheKey::Metrics, m);
}

fn record_hit(env: &Env) {
    let mut m = load_metrics(env);
    m.hits = m.hits.saturating_add(1);
    save_metrics(env, &m);
}

fn record_miss(env: &Env) {
    let mut m = load_metrics(env);
    m.misses = m.misses.saturating_add(1);
    save_metrics(env, &m);
}

fn record_error(env: &Env) {
    let mut m = load_metrics(env);
    m.errors = m.errors.saturating_add(1);
    save_metrics(env, &m);
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Return the borrow rate (bps) for the current ledger, using the memo-cache.
///
/// # Cache behaviour
/// - **Hit**: ledger sequence unchanged since the last call → returns stored
///   value, increments `hits`.
/// - **Miss**: first call this ledger → evaluates the full curve, stores the
///   result in temporary storage, increments `misses`.
/// - **Error**: curve evaluation fails → propagates the error, increments
///   `errors`.
pub fn get_borrow_rate(env: &Env) -> Result<i128, InterestRateError> {
    let seq = env.ledger().sequence();
    let key = RateCacheKey::BorrowRate(seq);

    if let Some(cached) = env.storage().temporary().get::<RateCacheKey, i128>(&key) {
        record_hit(env);
        return Ok(cached);
    }

    record_miss(env);
    let rate = calculate_borrow_rate(env).map_err(|e| {
        record_error(env);
        e
    })?;
    env.storage().temporary().set(&key, &rate);
    Ok(rate)
}

/// Return the supply rate (bps) for the current ledger, using the memo-cache.
pub fn get_supply_rate(env: &Env) -> Result<i128, InterestRateError> {
    let seq = env.ledger().sequence();
    let key = RateCacheKey::SupplyRate(seq);

    if let Some(cached) = env.storage().temporary().get::<RateCacheKey, i128>(&key) {
        record_hit(env);
        return Ok(cached);
    }

    record_miss(env);
    let rate = calculate_supply_rate(env).map_err(|e| {
        record_error(env);
        e
    })?;
    env.storage().temporary().set(&key, &rate);
    Ok(rate)
}

/// Return the utilization (bps) for the current ledger, using the memo-cache.
pub fn get_utilization(env: &Env) -> Result<i128, InterestRateError> {
    let seq = env.ledger().sequence();
    let key = RateCacheKey::Utilization(seq);

    if let Some(cached) = env.storage().temporary().get::<RateCacheKey, i128>(&key) {
        record_hit(env);
        return Ok(cached);
    }

    record_miss(env);
    let util = calculate_utilization(env).map_err(|e| {
        record_error(env);
        e
    })?;
    env.storage().temporary().set(&key, &util);
    Ok(util)
}

/// Explicitly warm up all three cache entries for the current ledger in one
/// pass.  Call this at the top of any multi-step operation (e.g. a batch
/// liquidation) to ensure every downstream call within the same transaction
/// is a cache hit.
///
/// Returns `(borrow_rate, supply_rate, utilization)` in bps.
pub fn prime_cache(env: &Env) -> Result<(i128, i128, i128), InterestRateError> {
    let borrow = get_borrow_rate(env)?;
    let supply = get_supply_rate(env)?;
    let util = get_utilization(env)?;
    Ok((borrow, supply, util))
}

/// Invalidate any cached rate entries for the current ledger sequence.
///
/// Call this after an admin operation that changes curve parameters
/// mid-transaction (rare, but possible in test harnesses).  Under normal
/// protocol flow the cache auto-expires each ledger, so this is a no-op.
pub fn invalidate_cache(env: &Env) {
    let seq = env.ledger().sequence();
    env.storage()
        .temporary()
        .remove(&RateCacheKey::BorrowRate(seq));
    env.storage()
        .temporary()
        .remove(&RateCacheKey::SupplyRate(seq));
    env.storage()
        .temporary()
        .remove(&RateCacheKey::Utilization(seq));
}

/// Read the current hit/miss/error counters without modifying them.
pub fn get_cache_metrics(env: &Env) -> RateCacheMetrics {
    load_metrics(env)
}

// ── Per-pool dynamic rate curves (issue #1522) ────────────────────────────────
//
// Each pool may carry its own curve, so its borrow rate responds to that pool's
// utilization rather than to the global curve in `interest_rate`. Evaluation
// reuses `stellarlend_math::rates::RateCurve`, so the shape math has one
// implementation shared with the rest of the protocol.
//
// Rates are not memoized here: a pool curve is a pure function of its stored
// parameters and the utilization passed in, so the lookup is the only storage
// read on the hot path.

/// Largest borrow rate a pool curve may produce, in basis points (1,000% APR).
pub const MAX_POOL_RATE_BPS: i128 = 100_000;

const BPS_DENOMINATOR: i128 = 10_000;

/// Errors returned by the per-pool rate curves. Codes start at 900.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PoolRateError {
    /// Caller is not the protocol admin.
    Unauthorized = 900,
    /// Utilization must be within `[0, 10_000]` basis points.
    InvalidUtilization = 901,
    /// Curve parameters are outside their allowed ranges.
    InvalidCurve = 902,
    /// No curve has been configured for the pool.
    CurveNotFound = 903,
    /// An arithmetic operation overflowed.
    Overflow = 904,
}

/// Shape of a pool curve. Maps onto `RateModelKind` in the shared math crate.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PoolCurveShape {
    /// One slope across the whole utilization range.
    Linear = 0,
    /// Two segments joined at the kink.
    Kink = 1,
    /// Linear, plus a jump term above the kink.
    Jump = 2,
}

/// Borrow-rate curve for one pool. Every value is in basis points.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PoolRateCurve {
    /// Which curve shape to evaluate.
    pub shape: PoolCurveShape,
    /// Borrow rate at 0% utilization.
    pub base_rate_bps: i128,
    /// Utilization at which the curve steepens, in `(0, 10_000)`.
    pub kink_utilization_bps: i128,
    /// Slope below the kink.
    pub multiplier_bps: i128,
    /// Slope above the kink.
    pub jump_multiplier_bps: i128,
}

/// Storage keys for per-pool curves.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum PoolCurveKey {
    /// Curve for one pool, keyed by the pool's address.
    Curve(Address),
}

/// Configure the borrow-rate curve for `pool` (admin-only). Replaces any
/// existing curve for that pool.
pub fn set_pool_curve(
    env: &Env,
    caller: Address,
    pool: Address,
    curve: PoolRateCurve,
) -> Result<(), PoolRateError> {
    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| PoolRateError::Unauthorized)?;
    validate_pool_curve(&curve)?;

    env.storage()
        .persistent()
        .set(&PoolCurveKey::Curve(pool.clone()), &curve);
    emit_event(env, "pool_rate_curve_set", "curve", (pool, curve));
    Ok(())
}

/// The configured curve for `pool`, if any.
pub fn get_pool_curve(env: &Env, pool: Address) -> Option<PoolRateCurve> {
    env.storage()
        .persistent()
        .get::<PoolCurveKey, PoolRateCurve>(&PoolCurveKey::Curve(pool))
}

/// Borrow rate (bps) for `pool` at `utilization_bps`, using the pool's own
/// curve. Fails with `CurveNotFound` when the pool has no curve.
pub fn pool_borrow_rate(
    env: &Env,
    pool: Address,
    utilization_bps: i128,
) -> Result<i128, PoolRateError> {
    if !(0..=BPS_DENOMINATOR).contains(&utilization_bps) {
        return Err(PoolRateError::InvalidUtilization);
    }
    let curve = get_pool_curve(env, pool).ok_or(PoolRateError::CurveNotFound)?;
    evaluate_pool_curve(&curve, utilization_bps)
}

/// Evaluate `curve` at `utilization_bps` without touching storage.
pub fn evaluate_pool_curve(
    curve: &PoolRateCurve,
    utilization_bps: i128,
) -> Result<i128, PoolRateError> {
    if !(0..=BPS_DENOMINATOR).contains(&utilization_bps) {
        return Err(PoolRateError::InvalidUtilization);
    }
    to_math_curve(curve)
        .borrow_rate(utilization_bps)
        .map_err(|_| PoolRateError::Overflow)
}

/// Utilization in basis points for a pool with `total_borrows` out of
/// `total_deposits`. Returns 0 for an empty pool and caps at 100%.
pub fn pool_utilization_bps(
    total_borrows: i128,
    total_deposits: i128,
) -> Result<i128, PoolRateError> {
    if total_borrows < 0 || total_deposits < 0 {
        return Err(PoolRateError::InvalidUtilization);
    }
    if total_deposits == 0 {
        return Ok(0);
    }
    let scaled = total_borrows
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(PoolRateError::Overflow)?;
    Ok((scaled / total_deposits).min(BPS_DENOMINATOR))
}

fn validate_pool_curve(curve: &PoolRateCurve) -> Result<(), PoolRateError> {
    if curve.base_rate_bps < 0 || curve.multiplier_bps < 0 || curve.jump_multiplier_bps < 0 {
        return Err(PoolRateError::InvalidCurve);
    }
    // A kink at 0 or at 100% would divide by zero in the segment math.
    if curve.kink_utilization_bps <= 0 || curve.kink_utilization_bps >= BPS_DENOMINATOR {
        return Err(PoolRateError::InvalidCurve);
    }
    let ceiling = curve
        .base_rate_bps
        .checked_add(curve.multiplier_bps)
        .and_then(|sum| sum.checked_add(curve.jump_multiplier_bps))
        .ok_or(PoolRateError::Overflow)?;
    if ceiling > MAX_POOL_RATE_BPS {
        return Err(PoolRateError::InvalidCurve);
    }
    Ok(())
}

fn to_math_curve(curve: &PoolRateCurve) -> RateCurve {
    RateCurve {
        kind: match curve.shape {
            PoolCurveShape::Linear => RateModelKind::Linear,
            PoolCurveShape::Kink => RateModelKind::Kink,
            PoolCurveShape::Jump => RateModelKind::Jump,
        },
        base_rate_bps: curve.base_rate_bps,
        kink_utilization_bps: curve.kink_utilization_bps,
        multiplier_bps: curve.multiplier_bps,
        jump_multiplier_bps: curve.jump_multiplier_bps,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::Env;

    use crate::deposit::{DepositDataKey, ProtocolAnalytics};
    use crate::interest_rate::{
        initialize_interest_rate_config, InterestRateDataKey,
        InterestRateConfig, InterestRateModelKind,
    };
    use crate::admin;

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Bootstrap the minimum state the rate-curve needs to compute a result.
    fn setup(env: &Env) {
        use soroban_sdk::testutils::Address as _;
        let admin_addr = soroban_sdk::Address::generate(env);
        admin::set_admin(env, admin_addr.clone(), None).unwrap();
        initialize_interest_rate_config(env, admin_addr).unwrap();

        // Seed analytics so utilization is non-zero (50%)
        env.storage().persistent().set(
            &DepositDataKey::ProtocolAnalytics,
            &ProtocolAnalytics {
                total_deposits: 10_000,
                total_borrows: 5_000,
                total_value_locked: 10_000,
            },
        );
    }

    // ── Happy path ────────────────────────────────────────────────────────

    #[test]
    fn first_call_is_a_miss_and_warms_cache() {
        let env = Env::default();
        setup(&env);

        let rate = get_borrow_rate(&env).expect("borrow rate");
        assert!(rate > 0, "rate should be positive at 50% utilization");

        let metrics = get_cache_metrics(&env);
        assert_eq!(metrics.misses, 1, "first call must be a miss");
        assert_eq!(metrics.hits, 0);
        assert_eq!(metrics.errors, 0);
    }

    #[test]
    fn second_call_same_sequence_is_a_hit() {
        let env = Env::default();
        setup(&env);

        let rate1 = get_borrow_rate(&env).unwrap();
        let rate2 = get_borrow_rate(&env).unwrap();

        assert_eq!(rate1, rate2, "cached value must equal computed value");

        let metrics = get_cache_metrics(&env);
        assert_eq!(metrics.misses, 1);
        assert_eq!(metrics.hits, 1, "second call must be a hit");
    }

    #[test]
    fn supply_and_utilization_caches_are_independent() {
        let env = Env::default();
        setup(&env);

        get_borrow_rate(&env).unwrap();  // miss
        get_supply_rate(&env).unwrap();  // miss
        get_utilization(&env).unwrap();  // miss

        get_borrow_rate(&env).unwrap();  // hit
        get_supply_rate(&env).unwrap();  // hit
        get_utilization(&env).unwrap();  // hit

        let m = get_cache_metrics(&env);
        assert_eq!(m.misses, 3);
        assert_eq!(m.hits, 3);
    }

    #[test]
    fn prime_cache_returns_all_three_values_and_warms_all_entries() {
        let env = Env::default();
        setup(&env);

        let (b, s, u) = prime_cache(&env).unwrap();
        assert!(b > 0);
        assert!(s > 0);
        assert!(u > 0);

        // All subsequent single-value calls should be hits.
        get_borrow_rate(&env).unwrap();
        get_supply_rate(&env).unwrap();
        get_utilization(&env).unwrap();

        let m = get_cache_metrics(&env);
        // prime_cache: 3 misses (each function sees cold cache)
        // 3 explicit follow-up calls: 3 hits
        assert_eq!(m.misses, 3);
        assert_eq!(m.hits, 3);
    }

    #[test]
    fn new_ledger_sequence_causes_cache_miss() {
        let env = Env::default();
        setup(&env);

        get_borrow_rate(&env).unwrap(); // miss at sequence N

        // Advance to the next ledger.
        env.ledger().with_mut(|li| {
            li.sequence_number += 1;
        });

        get_borrow_rate(&env).unwrap(); // miss at sequence N+1

        let m = get_cache_metrics(&env);
        assert_eq!(m.misses, 2, "each new sequence must be a fresh miss");
    }

    #[test]
    fn invalidate_cache_forces_next_call_to_miss() {
        let env = Env::default();
        setup(&env);

        get_borrow_rate(&env).unwrap(); // miss
        get_borrow_rate(&env).unwrap(); // hit

        invalidate_cache(&env);

        get_borrow_rate(&env).unwrap(); // miss again after invalidation

        let m = get_cache_metrics(&env);
        assert_eq!(m.misses, 2);
        assert_eq!(m.hits, 1);
    }

    // ── Failure modes ─────────────────────────────────────────────────────

    #[test]
    fn error_increments_error_counter_and_does_not_populate_cache() {
        let env = Env::default();
        // Deliberately skip setup() so there is no InterestRateConfig in
        // storage — calculate_borrow_rate will return InvalidParameter.

        let result = get_borrow_rate(&env);
        assert!(result.is_err(), "must fail without config");

        let m = get_cache_metrics(&env);
        assert_eq!(m.errors, 1, "error counter must be incremented");
        assert_eq!(m.misses, 1, "miss counted before error propagated");
        assert_eq!(m.hits, 0);

        // Cache must be cold — another call must also be a miss (and error).
        let _ = get_borrow_rate(&env);
        let m2 = get_cache_metrics(&env);
        assert_eq!(m2.errors, 2);
        assert_eq!(m2.misses, 2);
    }

    #[test]
    fn cache_returns_zero_utilization_when_no_deposits() {
        let env = Env::default();
        use soroban_sdk::testutils::Address as _;
        let admin_addr = soroban_sdk::Address::generate(&env);
        admin::set_admin(&env, admin_addr.clone(), None).unwrap();
        initialize_interest_rate_config(&env, admin_addr).unwrap();
        // No analytics stored → utilization defaults to 0.

        let u = get_utilization(&env).unwrap();
        assert_eq!(u, 0, "empty pool must have 0% utilization");

        let m = get_cache_metrics(&env);
        assert_eq!(m.misses, 1);
        assert_eq!(m.hits, 0);
    }

    #[test]
    fn metrics_saturate_instead_of_overflow() {
        // Ensure saturating_add protects the counters under extreme use.
        let m = RateCacheMetrics {
            hits: u32::MAX,
            misses: u32::MAX,
            errors: u32::MAX,
        };
        // These should not panic.
        let _ = m.hits.saturating_add(1);
        let _ = m.misses.saturating_add(1);
        let _ = m.errors.saturating_add(1);
    }
}

#[cfg(test)]
mod pool_curve_tests {
    use super::*;
    use soroban_sdk::{contract, contractimpl, testutils::Address as _};

    #[contract]
    struct RatesTestContract;

    #[contractimpl]
    impl RatesTestContract {
        pub fn marker() {}
    }

    struct Fixture {
        env: Env,
        contract: Address,
        admin: Address,
    }

    fn fixture() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register(RatesTestContract, ());
        let admin = Address::generate(&env);
        env.as_contract(&contract, || {
            crate::admin::set_admin(&env, admin.clone(), None).unwrap();
        });
        Fixture {
            env,
            contract,
            admin,
        }
    }

    fn kink_curve() -> PoolRateCurve {
        PoolRateCurve {
            shape: PoolCurveShape::Kink,
            base_rate_bps: 100,
            kink_utilization_bps: 8_000,
            multiplier_bps: 2_000,
            jump_multiplier_bps: 10_000,
        }
    }

    #[test]
    fn kink_curve_is_piecewise_below_and_above_the_kink() {
        let curve = kink_curve();
        // Below the kink: base + u / kink * multiplier.
        assert_eq!(evaluate_pool_curve(&curve, 0), Ok(100));
        assert_eq!(evaluate_pool_curve(&curve, 4_000), Ok(1_100));
        // At the kink: base + multiplier.
        assert_eq!(evaluate_pool_curve(&curve, 8_000), Ok(2_100));
        // Above the kink: steeper by jump_multiplier over the excess range.
        assert_eq!(evaluate_pool_curve(&curve, 9_000), Ok(7_100));
    }

    #[test]
    fn linear_curve_uses_a_single_slope() {
        let curve = PoolRateCurve {
            shape: PoolCurveShape::Linear,
            base_rate_bps: 0,
            kink_utilization_bps: 8_000,
            multiplier_bps: 1_000,
            jump_multiplier_bps: 0,
        };
        assert_eq!(evaluate_pool_curve(&curve, 5_000), Ok(500));
        assert_eq!(evaluate_pool_curve(&curve, 10_000), Ok(1_000));
    }

    #[test]
    fn jump_curve_adds_jump_term_only_above_the_kink() {
        let curve = PoolRateCurve {
            shape: PoolCurveShape::Jump,
            base_rate_bps: 100,
            kink_utilization_bps: 8_000,
            multiplier_bps: 2_000,
            jump_multiplier_bps: 10_000,
        };
        // Linear part: 100 + 9_000 * 2_000 / 10_000 = 1_900; jump: 1_000 * 10_000 / 2_000 = 5_000.
        assert_eq!(evaluate_pool_curve(&curve, 9_000), Ok(6_900));
        // Below the kink there is no jump term: 100 + 4_000 * 2_000 / 10_000 = 900.
        assert_eq!(evaluate_pool_curve(&curve, 4_000), Ok(900));
    }

    #[test]
    fn each_pool_prices_from_its_own_curve() {
        let f = fixture();
        let pool_a = Address::generate(&f.env);
        let pool_b = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            set_pool_curve(&f.env, f.admin.clone(), pool_a.clone(), kink_curve()).unwrap();
            set_pool_curve(
                &f.env,
                f.admin.clone(),
                pool_b.clone(),
                PoolRateCurve {
                    shape: PoolCurveShape::Linear,
                    base_rate_bps: 500,
                    kink_utilization_bps: 8_000,
                    multiplier_bps: 0,
                    jump_multiplier_bps: 0,
                },
            )
            .unwrap();

            // 100 + 5_000 * 2_000 / 8_000 = 1_350
            assert_eq!(pool_borrow_rate(&f.env, pool_a.clone(), 5_000), Ok(1_350));
            assert_eq!(pool_borrow_rate(&f.env, pool_b.clone(), 5_000), Ok(500));
        });
    }

    #[test]
    fn invalid_curves_are_rejected() {
        let f = fixture();
        let pool = Address::generate(&f.env);
        let base = kink_curve();
        let bad_curves = [
            PoolRateCurve {
                kink_utilization_bps: 0,
                ..base
            },
            PoolRateCurve {
                kink_utilization_bps: BPS_DENOMINATOR,
                ..base
            },
            PoolRateCurve {
                base_rate_bps: -1,
                ..base
            },
            PoolRateCurve {
                multiplier_bps: -1,
                ..base
            },
            PoolRateCurve {
                jump_multiplier_bps: -1,
                ..base
            },
            PoolRateCurve {
                base_rate_bps: MAX_POOL_RATE_BPS,
                multiplier_bps: 1,
                ..base
            },
        ];
        f.env.as_contract(&f.contract, || {
            for curve in bad_curves {
                assert_eq!(
                    set_pool_curve(&f.env, f.admin.clone(), pool.clone(), curve),
                    Err(PoolRateError::InvalidCurve)
                );
            }
            assert_eq!(get_pool_curve(&f.env, pool.clone()), None);
        });
    }

    #[test]
    fn utilization_outside_range_is_rejected() {
        let f = fixture();
        let pool = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                pool_borrow_rate(&f.env, pool.clone(), -1),
                Err(PoolRateError::InvalidUtilization)
            );
            assert_eq!(
                pool_borrow_rate(&f.env, pool.clone(), BPS_DENOMINATOR + 1),
                Err(PoolRateError::InvalidUtilization)
            );
        });
    }

    #[test]
    fn pool_without_a_curve_reports_not_found() {
        let f = fixture();
        let pool = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                pool_borrow_rate(&f.env, pool.clone(), 5_000),
                Err(PoolRateError::CurveNotFound)
            );
        });
    }

    #[test]
    fn non_admin_cannot_set_a_curve() {
        let f = fixture();
        let stranger = Address::generate(&f.env);
        let pool = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                set_pool_curve(&f.env, stranger.clone(), pool.clone(), kink_curve()),
                Err(PoolRateError::Unauthorized)
            );
        });
    }

    #[test]
    fn pool_utilization_is_borrows_over_deposits_capped_at_full() {
        assert_eq!(pool_utilization_bps(5_000, 10_000), Ok(5_000));
        assert_eq!(pool_utilization_bps(0, 0), Ok(0));
        assert_eq!(pool_utilization_bps(15_000, 10_000), Ok(BPS_DENOMINATOR));
        assert_eq!(
            pool_utilization_bps(-1, 10),
            Err(PoolRateError::InvalidUtilization)
        );
    }
}
