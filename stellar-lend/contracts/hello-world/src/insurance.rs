//! # On-chain insurance fund with per-asset contribution split (issue #1520)
//!
//! Part of every protocol contribution (for example the reserve share of
//! interest or fees) can be diverted into a per-asset insurance fund. The admin
//! sets, for each asset, the share of each contribution that goes to the fund,
//! in basis points. The remainder goes to the protocol reserve, so every
//! contribution is split exactly once and no amount is lost to rounding.
//!
//! ## Accounting
//!
//! ```text
//! to_fund    = amount * fund_split_bps / 10_000     (rounded down)
//! to_reserve = amount - to_fund
//! ```
//!
//! An asset with no configured split contributes nothing to the fund (split 0),
//! so the fund is opt-in per asset.
//!
//! ## Covering losses
//!
//! [`cover_loss`] draws on an asset's fund balance to offset a loss, such as bad
//! debt. It covers at most the balance and reports the uncovered remainder as a
//! shortfall. The balance never goes negative and a shortfall is never dropped.
//!
//! ## Security
//!
//! - Setting a split and covering a loss both require the protocol admin.
//! - Splits must lie in `[0, 10_000]` and amounts must be strictly positive.
//! - All arithmetic is checked.
//! - Every state change emits an event for off-chain monitoring.

use soroban_sdk::{contracterror, contracttype, Address, Env};

use crate::admin::require_admin;
use crate::deposit::DepositDataKey;
use crate::events::emit_event;

/// 100% in basis points.
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Errors returned by the insurance fund. Codes start at 700.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum InsuranceError {
    /// Caller is not the protocol admin.
    Unauthorized = 700,
    /// Amount must be strictly positive.
    InvalidAmount = 701,
    /// Fund split must be within `[0, 10_000]` basis points.
    InvalidSplit = 702,
    /// An arithmetic operation overflowed.
    Overflow = 703,
}

/// Storage keys for the insurance fund. `None` identifies native XLM.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum InsuranceDataKey {
    /// Share of each contribution routed to the fund, in basis points.
    FundSplit(Option<Address>),
    /// Current fund balance for an asset.
    FundBalance(Option<Address>),
}

/// How one contribution was divided between the fund and the reserve.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionSplit {
    /// Amount credited to the insurance fund.
    pub to_fund: i128,
    /// Amount credited to the protocol reserve.
    pub to_reserve: i128,
}

/// Result of covering a loss from the fund.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LossCoverage {
    /// Amount paid out of the fund.
    pub covered: i128,
    /// Part of the loss the fund could not cover.
    pub shortfall: i128,
}

/// Set the share of each contribution for `asset` that is routed to the fund
/// (admin-only). `fund_bps` must lie in `[0, 10_000]`.
pub fn set_fund_split(
    env: &Env,
    caller: Address,
    asset: Option<Address>,
    fund_bps: i128,
) -> Result<(), InsuranceError> {
    caller.require_auth();
    require_admin(env, &caller).map_err(|_| InsuranceError::Unauthorized)?;
    if !(0..=BPS_DENOMINATOR).contains(&fund_bps) {
        return Err(InsuranceError::InvalidSplit);
    }

    env.storage()
        .persistent()
        .set(&InsuranceDataKey::FundSplit(asset.clone()), &fund_bps);
    emit_event(env, "insurance_split_set", "split", (asset, fund_bps));
    Ok(())
}

/// Divide a contribution of `amount` for `asset` between the fund and the
/// reserve, and credit both. Returns how the amount was split.
pub fn route_contribution(
    env: &Env,
    asset: Option<Address>,
    amount: i128,
) -> Result<ContributionSplit, InsuranceError> {
    if amount <= 0 {
        return Err(InsuranceError::InvalidAmount);
    }

    let fund_bps = get_fund_split(env, asset.clone());
    let to_fund = amount
        .checked_mul(fund_bps)
        .ok_or(InsuranceError::Overflow)?
        / BPS_DENOMINATOR;
    let to_reserve = amount
        .checked_sub(to_fund)
        .ok_or(InsuranceError::Overflow)?;

    if to_fund > 0 {
        let balance = get_fund_balance(env, asset.clone());
        let new_balance = balance
            .checked_add(to_fund)
            .ok_or(InsuranceError::Overflow)?;
        env.storage()
            .persistent()
            .set(&InsuranceDataKey::FundBalance(asset.clone()), &new_balance);
    }

    if to_reserve > 0 {
        let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
        let reserve = env
            .storage()
            .persistent()
            .get::<DepositDataKey, i128>(&reserve_key)
            .unwrap_or(0);
        let new_reserve = reserve
            .checked_add(to_reserve)
            .ok_or(InsuranceError::Overflow)?;
        env.storage().persistent().set(&reserve_key, &new_reserve);
    }

    emit_event(
        env,
        "insurance_contribution",
        "routed",
        (asset, to_fund, to_reserve),
    );
    Ok(ContributionSplit {
        to_fund,
        to_reserve,
    })
}

/// Offset a `loss` for `asset` from the fund (admin-only). Covers at most the
/// current fund balance and reports any remainder as a shortfall.
pub fn cover_loss(
    env: &Env,
    caller: Address,
    asset: Option<Address>,
    loss: i128,
) -> Result<LossCoverage, InsuranceError> {
    caller.require_auth();
    require_admin(env, &caller).map_err(|_| InsuranceError::Unauthorized)?;
    if loss <= 0 {
        return Err(InsuranceError::InvalidAmount);
    }

    let balance = get_fund_balance(env, asset.clone());
    let covered = loss.min(balance);
    let shortfall = loss.checked_sub(covered).ok_or(InsuranceError::Overflow)?;

    if covered > 0 {
        let new_balance = balance
            .checked_sub(covered)
            .ok_or(InsuranceError::Overflow)?;
        env.storage()
            .persistent()
            .set(&InsuranceDataKey::FundBalance(asset.clone()), &new_balance);
    }

    emit_event(
        env,
        "insurance_loss_covered",
        "coverage",
        (asset, covered, shortfall),
    );
    Ok(LossCoverage { covered, shortfall })
}

/// Configured fund split for `asset` in basis points. Zero when unset.
pub fn get_fund_split(env: &Env, asset: Option<Address>) -> i128 {
    env.storage()
        .persistent()
        .get::<InsuranceDataKey, i128>(&InsuranceDataKey::FundSplit(asset))
        .unwrap_or(0)
}

/// Current fund balance for `asset`. Zero when nothing has been contributed.
pub fn get_fund_balance(env: &Env, asset: Option<Address>) -> i128 {
    env.storage()
        .persistent()
        .get::<InsuranceDataKey, i128>(&InsuranceDataKey::FundBalance(asset))
        .unwrap_or(0)
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{contract, contractimpl, testutils::Address as _};

    #[contract]
    struct InsuranceTestContract;

    #[contractimpl]
    impl InsuranceTestContract {
        pub fn marker() {}
    }

    struct Fixture {
        env: Env,
        contract: Address,
        admin: Address,
        asset: Address,
    }

    fn fixture() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register(InsuranceTestContract, ());
        let admin = Address::generate(&env);
        let asset = Address::generate(&env);
        env.as_contract(&contract, || {
            crate::admin::set_admin(&env, admin.clone(), None).unwrap();
        });
        Fixture {
            env,
            contract,
            admin,
            asset,
        }
    }

    #[test]
    fn split_routes_exact_amounts_to_fund_and_reserve() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            set_fund_split(&f.env, f.admin.clone(), Some(f.asset.clone()), 2_500).unwrap();

            let split = route_contribution(&f.env, Some(f.asset.clone()), 1_000).unwrap();
            assert_eq!(split.to_fund, 250);
            assert_eq!(split.to_reserve, 750);

            assert_eq!(get_fund_balance(&f.env, Some(f.asset.clone())), 250);
            let reserve = f
                .env
                .storage()
                .persistent()
                .get::<DepositDataKey, i128>(&DepositDataKey::ProtocolReserve(Some(
                    f.asset.clone(),
                )))
                .unwrap_or(0);
            assert_eq!(reserve, 750);
        });
    }

    #[test]
    fn rounding_remainder_stays_with_reserve_so_nothing_is_lost() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            // 33.33% of 10 rounds down to 3; the remaining 7 goes to the reserve.
            set_fund_split(&f.env, f.admin.clone(), Some(f.asset.clone()), 3_333).unwrap();
            let split = route_contribution(&f.env, Some(f.asset.clone()), 10).unwrap();
            assert_eq!(split.to_fund, 3);
            assert_eq!(split.to_reserve, 7);
            assert_eq!(split.to_fund + split.to_reserve, 10);
        });
    }

    #[test]
    fn unset_split_sends_everything_to_reserve() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            let split = route_contribution(&f.env, Some(f.asset.clone()), 500).unwrap();
            assert_eq!(split.to_fund, 0);
            assert_eq!(split.to_reserve, 500);
            assert_eq!(get_fund_balance(&f.env, Some(f.asset.clone())), 0);
        });
    }

    #[test]
    fn full_split_sends_everything_to_fund() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            set_fund_split(&f.env, f.admin.clone(), None, BPS_DENOMINATOR).unwrap();
            let split = route_contribution(&f.env, None, 400).unwrap();
            assert_eq!(split.to_fund, 400);
            assert_eq!(split.to_reserve, 0);
        });
    }

    #[test]
    fn split_outside_basis_point_range_is_rejected() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                set_fund_split(&f.env, f.admin.clone(), None, BPS_DENOMINATOR + 1),
                Err(InsuranceError::InvalidSplit)
            );
            assert_eq!(
                set_fund_split(&f.env, f.admin.clone(), None, -1),
                Err(InsuranceError::InvalidSplit)
            );
            assert_eq!(get_fund_split(&f.env, None), 0);
        });
    }

    #[test]
    fn non_admin_cannot_change_split() {
        let f = fixture();
        let stranger = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                set_fund_split(&f.env, stranger.clone(), None, 1_000),
                Err(InsuranceError::Unauthorized)
            );
        });
    }

    #[test]
    fn non_positive_contribution_is_rejected() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                route_contribution(&f.env, None, 0),
                Err(InsuranceError::InvalidAmount)
            );
            assert_eq!(
                route_contribution(&f.env, None, -5),
                Err(InsuranceError::InvalidAmount)
            );
        });
    }

    #[test]
    fn loss_is_covered_up_to_balance_and_remainder_is_a_shortfall() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            set_fund_split(&f.env, f.admin.clone(), Some(f.asset.clone()), 2_500).unwrap();
            route_contribution(&f.env, Some(f.asset.clone()), 1_000).unwrap(); // fund = 250

            let cover = cover_loss(&f.env, f.admin.clone(), Some(f.asset.clone()), 400).unwrap();
            assert_eq!(cover.covered, 250);
            assert_eq!(cover.shortfall, 150);
            assert_eq!(get_fund_balance(&f.env, Some(f.asset.clone())), 0);
        });
    }

    #[test]
    fn loss_within_balance_is_fully_covered() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            set_fund_split(&f.env, f.admin.clone(), None, BPS_DENOMINATOR).unwrap();
            route_contribution(&f.env, None, 300).unwrap();

            let cover = cover_loss(&f.env, f.admin.clone(), None, 120).unwrap();
            assert_eq!(cover.covered, 120);
            assert_eq!(cover.shortfall, 0);
            assert_eq!(get_fund_balance(&f.env, None), 180);
        });
    }

    #[test]
    fn empty_fund_reports_full_loss_as_shortfall() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            let cover = cover_loss(&f.env, f.admin.clone(), None, 75).unwrap();
            assert_eq!(cover.covered, 0);
            assert_eq!(cover.shortfall, 75);
        });
    }

    #[test]
    fn non_admin_cannot_cover_loss_and_non_positive_loss_is_rejected() {
        let f = fixture();
        let stranger = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                cover_loss(&f.env, stranger.clone(), None, 10),
                Err(InsuranceError::Unauthorized)
            );
            assert_eq!(
                cover_loss(&f.env, f.admin.clone(), None, 0),
                Err(InsuranceError::InvalidAmount)
            );
        });
    }

    #[test]
    fn splits_and_balances_are_isolated_per_asset() {
        let f = fixture();
        let other = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            set_fund_split(&f.env, f.admin.clone(), Some(f.asset.clone()), 5_000).unwrap();
            route_contribution(&f.env, Some(f.asset.clone()), 100).unwrap();

            assert_eq!(get_fund_split(&f.env, Some(other.clone())), 0);
            assert_eq!(get_fund_balance(&f.env, Some(other.clone())), 0);
            assert_eq!(get_fund_balance(&f.env, Some(f.asset.clone())), 50);
        });
    }
}
