//! Overflow-safe lending pool math.
//!
//! Every public operation returns a `Result` instead of panicking on hostile
//! parameters. Products are computed with a 256-bit intermediate, so inputs up
//! to `u128::MAX` remain well defined, and every division rejects a zero
//! denominator. This crate is dependency-free and is the library under test
//! for the `pool_math` fuzz target.

use core::fmt;

/// Basis-point divisor.
pub const BPS: u128 = 10_000;
/// Seconds used to annualise interest rates.
pub const SECONDS_PER_YEAR: u64 = 31_536_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolMathError {
    DivisionByZero,
    Overflow,
    InvalidInput,
}

impl fmt::Display for PoolMathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::Overflow => f.write_str("arithmetic overflow"),
            Self::InvalidInput => f.write_str("invalid pool parameter"),
        }
    }
}

impl std::error::Error for PoolMathError {}

pub type Result<T> = core::result::Result<T, PoolMathError>;

/// Full 256-bit product of two `u128` values, returned as low and high limbs.
/// Implemented with 64-bit partial products because `u128::widening_mul` is not
/// stable on all supported toolchains.
fn mul_256(value: u128, numerator: u128) -> (u128, u128) {
    const MASK: u128 = u64::MAX as u128;

    let a0 = value & MASK;
    let a1 = value >> 64;
    let b0 = numerator & MASK;
    let b1 = numerator >> 64;

    let p0 = a0 * b0;
    let p1 = a0 * b1;
    let p2 = a1 * b0;
    let p3 = a1 * b1;

    let limb0 = p0 & MASK;
    let sum1 = (p0 >> 64) + (p1 & MASK) + (p2 & MASK);
    let limb1 = sum1 & MASK;
    let carry1 = sum1 >> 64;
    let sum2 = (p1 >> 64) + (p2 >> 64) + (p3 & MASK) + carry1;
    let limb2 = sum2 & MASK;
    let carry2 = sum2 >> 64;
    let limb3 = (p3 >> 64) + carry2;

    (limb0 | (limb1 << 64), limb2 | (limb3 << 64))
}

/// Divide a 256-bit value `(hi, lo)` by a 128-bit denominator.
///
/// Returns `None` when the denominator is zero or the quotient does not fit in
/// `u128`. The caller guarantees `hi < denominator` before calling.
fn div_256_by_u128(hi: u128, lo: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 || hi >= denominator {
        return None;
    }

    let mut remainder = hi;
    let mut quotient = 0_u128;
    for bit in (0..128).rev() {
        let next = ((lo >> bit) & 1) as u128;
        // `overflowing_shl` only flags a shift count >= 128, not truncated
        // bits, so detect the 129th bit explicitly.
        let overflow = (remainder >> 127) != 0;
        let shifted = remainder << 1;
        let candidate = shifted | next;
        if overflow || candidate >= denominator {
            remainder = candidate.wrapping_sub(denominator);
            quotient = (quotient << 1) | 1;
        } else {
            remainder = candidate;
            quotient <<= 1;
        }
    }
    Some(quotient)
}

/// `value * numerator / denominator` using a 256-bit intermediate.
pub fn checked_mul_div(value: u128, numerator: u128, denominator: u128) -> Result<u128> {
    if denominator == 0 {
        return Err(PoolMathError::DivisionByZero);
    }
    if value == 0 || numerator == 0 {
        return Ok(0);
    }
    let (lo, hi) = mul_256(value, numerator);
    if hi >= denominator {
        return Err(PoolMathError::Overflow);
    }
    div_256_by_u128(hi, lo, denominator).ok_or(PoolMathError::Overflow)
}

/// Utilisation in basis points. Borrowing more than the supplied amount is a
/// configuration error rather than a value that can be silently clamped.
pub fn utilization_bps(total_borrows: u128, total_supply: u128) -> Result<u128> {
    if total_supply == 0 {
        return Err(PoolMathError::DivisionByZero);
    }
    if total_borrows > total_supply {
        return Err(PoolMathError::InvalidInput);
    }
    checked_mul_div(total_borrows, BPS, total_supply)
}

/// Simple interest accrual for `elapsed_seconds`, rounded down.
pub fn accrue_interest(
    principal: u128,
    rate_bps: u128,
    elapsed_seconds: u64,
) -> Result<u128> {
    let periodic = checked_mul_div(principal, rate_bps, BPS)?;
    let interest = checked_mul_div(periodic, elapsed_seconds as u128, SECONDS_PER_YEAR as u128)?;
    principal.checked_add(interest).ok_or(PoolMathError::Overflow)
}

/// Output of a constant-product swap after charging `fee_bps` on the input.
/// The result is always strictly smaller than `reserve_out`.
pub fn swap_output(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u128,
) -> Result<u128> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(PoolMathError::DivisionByZero);
    }
    if fee_bps > BPS {
        return Err(PoolMathError::InvalidInput);
    }
    let effective_in = checked_mul_div(amount_in, BPS - fee_bps, BPS)?;
    let denominator = reserve_in
        .checked_add(effective_in)
        .ok_or(PoolMathError::Overflow)?;
    checked_mul_div(reserve_out, effective_in, denominator)
}

/// LP tokens minted for depositing `amount` into a pool.
pub fn liquidity_tokens_for_deposit(
    amount: u128,
    total_supply: u128,
    reserve: u128,
) -> Result<u128> {
    if reserve == 0 {
        return if total_supply == 0 {
            Ok(amount)
        } else {
            Err(PoolMathError::DivisionByZero)
        };
    }
    if total_supply == 0 {
        return Ok(amount);
    }
    checked_mul_div(amount, total_supply, reserve)
}

/// Underlying amount returned for burning `shares` LP tokens.
pub fn withdraw_amount_for_shares(
    shares: u128,
    total_supply: u128,
    reserve: u128,
) -> Result<u128> {
    if total_supply == 0 || reserve == 0 {
        return Err(PoolMathError::DivisionByZero);
    }
    if shares > total_supply {
        return Err(PoolMathError::InvalidInput);
    }
    checked_mul_div(shares, reserve, total_supply)
}

/// Health factor in basis points: `collateral * threshold / debt`.
pub fn health_factor_bps(
    collateral_value: u128,
    debt_value: u128,
    threshold_bps: u128,
) -> Result<u128> {
    if threshold_bps > BPS {
        return Err(PoolMathError::InvalidInput);
    }
    if debt_value == 0 {
        return Ok(u128::MAX);
    }
    checked_mul_div(collateral_value, threshold_bps, debt_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_handles_maximum_width_operands() {
        assert_eq!(
            checked_mul_div(u128::MAX, u128::MAX, u128::MAX).unwrap(),
            u128::MAX
        );
        assert_eq!(
            checked_mul_div(u128::MAX, 2, 2).unwrap(),
            u128::MAX
        );
        assert_eq!(checked_mul_div(7, 3, 2).unwrap(), 10);
        assert_eq!(checked_mul_div(7, 0, 0).unwrap_err(), PoolMathError::DivisionByZero);
        assert_eq!(
            checked_mul_div(u128::MAX, u128::MAX, 1).unwrap_err(),
            PoolMathError::Overflow
        );
    }

    #[test]
    fn utilization_is_bounded_and_total() {
        assert_eq!(utilization_bps(250, 1_000).unwrap(), 2_500);
        assert_eq!(utilization_bps(1_000, 1_000).unwrap(), BPS);
        assert_eq!(utilization_bps(0, 1).unwrap(), 0);
        assert_eq!(utilization_bps(1, 0).unwrap_err(), PoolMathError::DivisionByZero);
        assert_eq!(
            utilization_bps(1_001, 1_000).unwrap_err(),
            PoolMathError::InvalidInput
        );
    }

    #[test]
    fn interest_accrues_monotonically_and_never_panics() {
        let one_year = accrue_interest(1_000_000, 500, SECONDS_PER_YEAR).unwrap();
        assert_eq!(one_year, 1_050_000);
        assert!(accrue_interest(1_000_000, 500, 0).unwrap() < one_year);
        assert_eq!(
            accrue_interest(u128::MAX, u128::MAX, u64::MAX).unwrap_err(),
            PoolMathError::Overflow
        );
    }

    #[test]
    fn swap_output_respects_fees_and_reserves() {
        let output = swap_output(1_000, 10_000, 20_000, 100).unwrap();
        assert!(output > 0 && output < 20_000);
        assert!(swap_output(1_000, 10_000, 20_000, 0).unwrap() > output);
        assert_eq!(swap_output(1_000, 0, 20_000, 0).unwrap_err(), PoolMathError::DivisionByZero);
        assert_eq!(
            swap_output(1_000, 10_000, 20_000, BPS + 1).unwrap_err(),
            PoolMathError::InvalidInput
        );
        assert_eq!(
            swap_output(u128::MAX, 1, 1, 0).unwrap_err(),
            PoolMathError::Overflow
        );
    }

    #[test]
    fn liquidity_math_round_trips() {
        let minted = liquidity_tokens_for_deposit(1_000, 5_000, 10_000).unwrap();
        assert_eq!(minted, 500);
        assert_eq!(liquidity_tokens_for_deposit(100, 0, 0).unwrap(), 100);
        assert_eq!(
            liquidity_tokens_for_deposit(100, 1, 0).unwrap_err(),
            PoolMathError::DivisionByZero
        );
        assert_eq!(withdraw_amount_for_shares(minted, 5_000 + minted, 10_000 + 1_000).unwrap(), 1_000);
        assert_eq!(
            withdraw_amount_for_shares(5_001, 5_000, 10_000).unwrap_err(),
            PoolMathError::InvalidInput
        );
    }

    #[test]
    fn health_factor_handles_zero_debt_and_bad_thresholds() {
        assert_eq!(health_factor_bps(1_000, 0, 9_000).unwrap(), u128::MAX);
        assert_eq!(health_factor_bps(1_000, 1_000, 9_000).unwrap(), 9_000);
        assert_eq!(
            health_factor_bps(1_000, 1_000, BPS + 1).unwrap_err(),
            PoolMathError::InvalidInput
        );
    }
}
