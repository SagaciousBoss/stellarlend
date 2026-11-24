//! Deterministic decoder and invariants shared by the libFuzzer target and the
//! stable-toolchain integration tests.
//!
//! Inputs are raw little-endian `i128` parameters. Negative, zero and
//! `i128::MIN`/`i128::MAX` values are decoded unchanged so the harness explores
//! hostile arithmetic boundaries. Percentages are normalised into their
//! documented domain before being passed to the shared pool-math crate.

use stellarlend_pool_math::{
    accrue_index, health_factor_bps, max_borrow_amount, max_repayable, seize_amount,
    simple_interest, utilization_bps, BPS_DIVISOR, INDEX_SCALE, NO_DEBT_HEALTH_FACTOR,
};

/// Eight 128-bit parameters plus one 64-bit timestamp delta.
pub const INPUT_LEN: usize = 8 * 16 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolInput {
    pub total_supply: i128,
    pub total_borrows: i128,
    pub principal: i128,
    pub rate_bps: i128,
    pub threshold_bps: i128,
    pub close_factor_bps: i128,
    pub incentive_bps: i128,
    pub current_index: i128,
    pub elapsed_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvariantViolation(&'static str);

impl core::fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvariantViolation {}

fn read_i128(bytes: &[u8]) -> i128 {
    let mut buffer = [0_u8; 16];
    buffer.copy_from_slice(&bytes[..16]);
    i128::from_le_bytes(buffer)
}

fn read_i64(bytes: &[u8]) -> i64 {
    let mut buffer = [0_u8; 8];
    buffer.copy_from_slice(&bytes[..8]);
    i64::from_le_bytes(buffer)
}

/// Decode raw fuzzer bytes. Inputs shorter than [`INPUT_LEN`] are ignored.
pub fn decode(data: &[u8]) -> Option<PoolInput> {
    if data.len() < INPUT_LEN {
        return None;
    }
    let fields: [i128; 8] = core::array::from_fn(|index| read_i128(&data[index * 16..]));
    let elapsed_seconds = read_i64(&data[8 * 16..]);
    Some(PoolInput {
        total_supply: fields[0],
        total_borrows: fields[1],
        principal: fields[2],
        rate_bps: fields[3],
        threshold_bps: fields[4],
        close_factor_bps: fields[5],
        incentive_bps: fields[6],
        current_index: fields[7],
        elapsed_seconds,
    })
}

/// Map a raw value to the non-negative domain without overflowing on
/// `i128::MIN`.
fn non_negative(value: i128) -> i128 {
    if value == i128::MIN {
        i128::MAX
    } else {
        value.abs()
    }
}

/// Normalise a raw value into `0..=10_000` without overflowing.
fn bps(value: i128) -> i128 {
    value.rem_euclid(BPS_DIVISOR + 1)
}

/// Health-factor thresholds must be strictly positive to be meaningful.
fn positive_bps(value: i128) -> i128 {
    let normalised = bps(value);
    if normalised == 0 {
        1
    } else {
        normalised
    }
}

fn non_negative_elapsed(value: i64) -> i128 {
    if value == i64::MIN {
        i128::from(i64::MAX)
    } else {
        i128::from(value.abs())
    }
}

/// Run every pool-math operation against one decoded input.
///
/// Mathematical `Err` results are expected for hostile parameters and are not
/// failures. A failure means an operation panicked, returned a value outside
/// its documented bounds, or broke a monotonicity invariant.
pub fn run(data: &[u8]) -> Result<(), InvariantViolation> {
    let Some(input) = decode(data) else {
        return Ok(());
    };

    let total_supply = non_negative(input.total_supply);
    let total_borrows = non_negative(input.total_borrows);
    let principal = non_negative(input.principal);
    let rate_bps = bps(input.rate_bps);
    let threshold_bps = positive_bps(input.threshold_bps);
    let close_factor_bps = bps(input.close_factor_bps);
    let incentive_bps = bps(input.incentive_bps);
    let current_index = if input.current_index <= 0 {
        INDEX_SCALE
    } else {
        input.current_index
    };
    let elapsed = non_negative_elapsed(input.elapsed_seconds);

    if let Ok(utilization) = utilization_bps(total_borrows, total_supply) {
        if !(0..=BPS_DIVISOR).contains(&utilization) {
            return Err(InvariantViolation("utilization outside 0..=10000 bps"));
        }
    }

    if let Ok(accrued) = simple_interest(principal, rate_bps, elapsed) {
        let at_inception = simple_interest(principal, rate_bps, 0).unwrap_or(0);
        if accrued < 0 || accrued < at_inception {
            return Err(InvariantViolation("interest decreased or went negative"));
        }
    }

    if let Ok(next_index) = accrue_index(current_index, rate_bps, elapsed) {
        if next_index < current_index {
            return Err(InvariantViolation("lending index decreased"));
        }
    }

    if let Ok(health) = health_factor_bps(principal, total_borrows, threshold_bps) {
        if health < 0 {
            return Err(InvariantViolation("health factor went negative"));
        }
        if total_borrows == 0 && health != NO_DEBT_HEALTH_FACTOR {
            return Err(InvariantViolation("debt-free position lost its sentinel"));
        }
        let higher_collateral = principal.checked_add(1).unwrap_or(principal);
        if let (Ok(base), Ok(next)) = (
            health_factor_bps(principal, total_borrows, threshold_bps),
            health_factor_bps(higher_collateral, total_borrows, threshold_bps),
        ) {
            if next < base {
                return Err(InvariantViolation("health factor decreased with collateral"));
            }
        }
    }

    if let Ok(capacity) = max_borrow_amount(principal, close_factor_bps, total_borrows) {
        if capacity < 0 || capacity > principal {
            return Err(InvariantViolation("borrow capacity exceeded collateral"));
        }
    }

    if let Ok(seized) = seize_amount(total_borrows, incentive_bps) {
        if seized < total_borrows {
            return Err(InvariantViolation("seize amount was below repayment"));
        }
    }

    if let Ok(repayable) = max_repayable(total_borrows, close_factor_bps, principal) {
        if repayable < 0 || repayable > total_borrows || repayable > principal {
            return Err(InvariantViolation("repayable amount exceeded its bounds"));
        }
    }

    Ok(())
}

/// Deterministic pseudo-random input generator used by replayable tests.
pub fn deterministic_cases(count: usize) -> impl Iterator<Item = Vec<u8>> {
    (1..=count as u64).map(|seed| deterministic_input(seed))
}

/// Seeds that deliberately cover all-zero, all-ones and alternating patterns.
pub fn extreme_seeds() -> [u64; 6] {
    [
        0,
        1,
        u64::MAX,
        u64::MAX - 1,
        0xAAAA_AAAA_AAAA_AAAA,
        0x5555_5555_5555_5555,
    ]
}

/// Build one reproducible `INPUT_LEN`-byte case from a seed.
pub fn deterministic_input(seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut bytes = Vec::with_capacity(INPUT_LEN);
    for _ in 0..INPUT_LEN {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        bytes.push((state >> 33) as u8);
    }
    bytes
}

/// Build a case where every byte is `value`; used for saturation boundaries.
pub fn uniform_input(value: u8) -> Vec<u8> {
    vec![value; INPUT_LEN]
}
