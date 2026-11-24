//! Deterministic decoder and invariants shared by the libFuzzer target and the
//! stable-toolchain integration tests.
//!
//! Fuzz inputs are interpreted as raw little-endian parameters, including
//! `u128::MAX`-scale values, so the harness explores arithmetic boundaries that
//! ordinary unit tests rarely reach.

use stellarlend_pool_math::{
    accrue_interest, health_factor_bps, liquidity_tokens_for_deposit, swap_output,
    utilization_bps, withdraw_amount_for_shares, BPS,
};

/// Eight 128-bit parameters plus one 64-bit timestamp delta.
pub const INPUT_LEN: usize = 8 * 16 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolInput {
    pub total_supply: u128,
    pub total_borrows: u128,
    pub reserve_in: u128,
    pub reserve_out: u128,
    pub amount: u128,
    pub rate_bps: u128,
    pub threshold_bps: u128,
    pub fee_bps: u128,
    pub elapsed_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvariantViolation(&'static str);

impl core::fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvariantViolation {}

fn read_u128(bytes: &[u8]) -> u128 {
    let mut value = 0_u128;
    for (index, byte) in bytes.iter().take(16).enumerate() {
        value |= (*byte as u128) << (8 * index);
    }
    value
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().take(8).enumerate() {
        value |= (*byte as u64) << (8 * index);
    }
    value
}

/// Decode raw fuzzer bytes. Inputs shorter than [`INPUT_LEN`] are ignored.
pub fn decode(data: &[u8]) -> Option<PoolInput> {
    if data.len() < INPUT_LEN {
        return None;
    }
    let fields: [u128; 8] = core::array::from_fn(|index| read_u128(&data[index * 16..]));
    let elapsed_seconds = read_u64(&data[8 * 16..]);
    Some(PoolInput {
        total_supply: fields[0],
        total_borrows: fields[1],
        reserve_in: fields[2],
        reserve_out: fields[3],
        amount: fields[4],
        rate_bps: fields[5],
        threshold_bps: fields[6],
        fee_bps: fields[7],
        elapsed_seconds,
    })
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

    // Keep percentage parameters inside their documented domain while leaving
    // amounts and reserves at their raw, potentially extreme values.
    let rate_bps = input.rate_bps % (BPS + 1);
    let threshold_bps = input.threshold_bps % (BPS + 1);
    let fee_bps = input.fee_bps % (BPS + 1);
    let total_borrows = if input.total_supply == 0 || input.total_supply == u128::MAX {
        input.total_borrows
    } else {
        input.total_borrows % (input.total_supply + 1)
    };

    if let Ok(utilization) = utilization_bps(total_borrows, input.total_supply) {
        if utilization > BPS {
            return Err(InvariantViolation("utilization exceeded 10000 bps"));
        }
    }

    if let Ok(accrued) = accrue_interest(input.amount, rate_bps, input.elapsed_seconds) {
        if accrued < input.amount {
            return Err(InvariantViolation("interest decreased principal"));
        }
    }

    if let Ok(output) = swap_output(
        input.amount,
        input.reserve_in,
        input.reserve_out,
        fee_bps,
    ) {
        if output >= input.reserve_out {
            return Err(InvariantViolation("swap drained the output reserve"));
        }
    }

    let minted = liquidity_tokens_for_deposit(
        input.amount,
        input.total_supply,
        input.reserve_out,
    )
    .unwrap_or(0);
    if let Ok(withdrawn) =
        withdraw_amount_for_shares(minted, input.total_supply.saturating_add(minted), input.reserve_out.saturating_add(input.amount))
    {
        if withdrawn > input.reserve_out.saturating_add(input.amount) {
            return Err(InvariantViolation("withdrawal exceeded reserve"));
        }
    }

    if health_factor_bps(input.amount, total_borrows, threshold_bps).is_ok() {
        let higher = input.amount.checked_add(1).unwrap_or(input.amount);
        if let (Ok(base), Ok(next)) = (
            health_factor_bps(input.amount, total_borrows, threshold_bps),
            health_factor_bps(higher, total_borrows, threshold_bps),
        ) {
            if next < base {
                return Err(InvariantViolation("health factor decreased with collateral"));
            }
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
    [0, 1, u64::MAX, u64::MAX - 1, 0xAAAA_AAAA_AAAA_AAAA, 0x5555_5555_5555_5555]
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
