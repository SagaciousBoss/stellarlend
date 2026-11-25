//! # Collateral factor hot-swap with a rollback window (issue #1530)
//!
//! The admin can change an asset's collateral factor while the protocol keeps
//! running. A hot-swap takes effect immediately, because it updates the same
//! `AssetParams.collateral_factor` record that the withdraw path reads. It also
//! records the previous factor and opens a rollback window. Within that window
//! the admin can restore the previous factor in one call if the new value
//! behaves badly. Once the window closes the swap is final.
//!
//! ## Lifecycle
//!
//! ```text
//! hot_swap(asset, new, window)   previous = current; current = new;
//!                                rollback_until = now + window
//! rollback(asset)                only while now < rollback_until;
//!                                current = previous; pending cleared
//! ```
//!
//! ## Rules
//!
//! - Factors are basis points in `[0, 10_000]`. A swap to the current value is
//!   rejected because it would be a no-op.
//! - The rollback window is at most [`MAX_ROLLBACK_WINDOW_SECONDS`]. A window of
//!   zero means the swap cannot be rolled back.
//! - A new hot-swap replaces the pending record. Rollback then restores the
//!   factor from before the most recent swap only.
//! - Rollback refuses to run if the live factor no longer equals the value the
//!   swap applied. That happens when `set_asset_params` changed it directly, and
//!   a rollback would otherwise overwrite that newer value.
//!
//! ## Security
//!
//! Hot-swap and rollback both require the protocol admin. Every state change
//! emits an event.

use soroban_sdk::{contracterror, contracttype, Address, Env};

use crate::admin::require_admin;
use crate::deposit::{AssetParams, DepositDataKey};
use crate::events::emit_event;

/// 100% in basis points.
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Longest rollback window a hot-swap may open: seven days.
pub const MAX_ROLLBACK_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Errors returned by the collateral hot-swap. Codes start at 800.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CollateralError {
    /// Caller is not the protocol admin.
    Unauthorized = 800,
    /// The asset has no `AssetParams` record to swap.
    AssetNotConfigured = 801,
    /// Collateral factor must be within `[0, 10_000]` basis points.
    InvalidFactor = 802,
    /// Rollback window exceeds `MAX_ROLLBACK_WINDOW_SECONDS`.
    InvalidWindow = 803,
    /// The new factor equals the current factor.
    NoChange = 804,
    /// There is no hot-swap to roll back for this asset.
    NoPendingSwap = 805,
    /// The rollback window has closed.
    RollbackWindowClosed = 806,
    /// The live factor changed after the swap, so a rollback would overwrite it.
    FactorChangedSinceSwap = 807,
}

/// Storage keys for pending hot-swaps.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum CollateralDataKey {
    /// The most recent hot-swap for an asset, while it can still be rolled back.
    PendingSwap(Address),
}

/// Record of the most recent hot-swap for one asset.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSwap {
    /// Factor in force before the swap.
    pub previous_factor: i128,
    /// Factor the swap applied.
    pub applied_factor: i128,
    /// Ledger timestamp at which the rollback window closes (exclusive).
    pub rollback_until: u64,
}

/// Change the collateral factor for `asset` now and open a rollback window of
/// `rollback_window_seconds` (admin-only). Returns the recorded swap.
pub fn hot_swap(
    env: &Env,
    caller: Address,
    asset: Address,
    new_factor_bps: i128,
    rollback_window_seconds: u64,
) -> Result<PendingSwap, CollateralError> {
    caller.require_auth();
    require_admin(env, &caller).map_err(|_| CollateralError::Unauthorized)?;

    if !(0..=BPS_DENOMINATOR).contains(&new_factor_bps) {
        return Err(CollateralError::InvalidFactor);
    }
    if rollback_window_seconds > MAX_ROLLBACK_WINDOW_SECONDS {
        return Err(CollateralError::InvalidWindow);
    }

    let params_key = DepositDataKey::AssetParams(asset.clone());
    let mut params = env
        .storage()
        .persistent()
        .get::<DepositDataKey, AssetParams>(&params_key)
        .ok_or(CollateralError::AssetNotConfigured)?;

    let previous_factor = params.collateral_factor;
    if previous_factor == new_factor_bps {
        return Err(CollateralError::NoChange);
    }

    let rollback_until = env
        .ledger()
        .timestamp()
        .saturating_add(rollback_window_seconds);

    params.collateral_factor = new_factor_bps;
    env.storage().persistent().set(&params_key, &params);

    let pending = PendingSwap {
        previous_factor,
        applied_factor: new_factor_bps,
        rollback_until,
    };
    env.storage()
        .persistent()
        .set(&CollateralDataKey::PendingSwap(asset.clone()), &pending);

    emit_event(
        env,
        "collateral_factor_swapped",
        "swap",
        (asset, previous_factor, new_factor_bps),
    );
    Ok(pending)
}

/// Restore the factor that was in force before the latest hot-swap for `asset`
/// (admin-only). Allowed only while the rollback window is open. Returns the
/// restored factor.
pub fn rollback(env: &Env, caller: Address, asset: Address) -> Result<i128, CollateralError> {
    caller.require_auth();
    require_admin(env, &caller).map_err(|_| CollateralError::Unauthorized)?;

    let pending_key = CollateralDataKey::PendingSwap(asset.clone());
    let pending = env
        .storage()
        .persistent()
        .get::<CollateralDataKey, PendingSwap>(&pending_key)
        .ok_or(CollateralError::NoPendingSwap)?;

    if env.ledger().timestamp() >= pending.rollback_until {
        return Err(CollateralError::RollbackWindowClosed);
    }

    let params_key = DepositDataKey::AssetParams(asset.clone());
    let mut params = env
        .storage()
        .persistent()
        .get::<DepositDataKey, AssetParams>(&params_key)
        .ok_or(CollateralError::AssetNotConfigured)?;

    if params.collateral_factor != pending.applied_factor {
        return Err(CollateralError::FactorChangedSinceSwap);
    }

    params.collateral_factor = pending.previous_factor;
    env.storage().persistent().set(&params_key, &params);
    env.storage().persistent().remove(&pending_key);

    emit_event(
        env,
        "collateral_factor_rolled_back",
        "rollback",
        (asset, pending.applied_factor, pending.previous_factor),
    );
    Ok(pending.previous_factor)
}

/// Live collateral factor for `asset`, if the asset has an `AssetParams` record.
pub fn get_collateral_factor(env: &Env, asset: Address) -> Option<i128> {
    env.storage()
        .persistent()
        .get::<DepositDataKey, AssetParams>(&DepositDataKey::AssetParams(asset))
        .map(|params| params.collateral_factor)
}

/// The most recent hot-swap for `asset`, if one is recorded.
pub fn get_pending_swap(env: &Env, asset: Address) -> Option<PendingSwap> {
    env.storage()
        .persistent()
        .get::<CollateralDataKey, PendingSwap>(&CollateralDataKey::PendingSwap(asset))
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{contract, contractimpl, testutils::Address as _, testutils::Ledger as _};

    #[contract]
    struct CollateralTestContract;

    #[contractimpl]
    impl CollateralTestContract {
        pub fn marker() {}
    }

    const START: u64 = 1_000;
    const WINDOW: u64 = 24 * 60 * 60;

    struct Fixture {
        env: Env,
        contract: Address,
        admin: Address,
        asset: Address,
    }

    fn set_params(env: &Env, asset: &Address, collateral_factor: i128) {
        env.storage().persistent().set(
            &DepositDataKey::AssetParams(asset.clone()),
            &AssetParams {
                deposit_enabled: true,
                collateral_factor,
                max_deposit: 1_000_000,
                borrow_fee_bps: 0,
            },
        );
    }

    fn fixture() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|ledger| ledger.timestamp = START);
        let contract = env.register(CollateralTestContract, ());
        let admin = Address::generate(&env);
        let asset = Address::generate(&env);
        env.as_contract(&contract, || {
            crate::admin::set_admin(&env, admin.clone(), None).unwrap();
            set_params(&env, &asset, 7_500);
        });
        Fixture {
            env,
            contract,
            admin,
            asset,
        }
    }

    #[test]
    fn hot_swap_takes_effect_immediately_and_records_previous_factor() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            let swap = hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, WINDOW).unwrap();
            assert_eq!(swap.previous_factor, 7_500);
            assert_eq!(swap.applied_factor, 6_000);
            assert_eq!(swap.rollback_until, START + WINDOW);

            // The live value read by the withdraw path changes at once.
            assert_eq!(get_collateral_factor(&f.env, f.asset.clone()), Some(6_000));
            assert_eq!(get_pending_swap(&f.env, f.asset.clone()), Some(swap));
        });
    }

    #[test]
    fn rollback_inside_window_restores_previous_factor() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, WINDOW).unwrap();

            f.env
                .ledger()
                .with_mut(|ledger| ledger.timestamp = START + WINDOW - 1);
            let restored = rollback(&f.env, f.admin.clone(), f.asset.clone()).unwrap();
            assert_eq!(restored, 7_500);
            assert_eq!(get_collateral_factor(&f.env, f.asset.clone()), Some(7_500));
            assert_eq!(get_pending_swap(&f.env, f.asset.clone()), None);
        });
    }

    #[test]
    fn rollback_at_window_close_is_rejected_and_factor_stays_swapped() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, WINDOW).unwrap();

            f.env
                .ledger()
                .with_mut(|ledger| ledger.timestamp = START + WINDOW);
            assert_eq!(
                rollback(&f.env, f.admin.clone(), f.asset.clone()),
                Err(CollateralError::RollbackWindowClosed)
            );
            assert_eq!(get_collateral_factor(&f.env, f.asset.clone()), Some(6_000));
        });
    }

    #[test]
    fn zero_window_swap_cannot_be_rolled_back() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, 0).unwrap();
            assert_eq!(
                rollback(&f.env, f.admin.clone(), f.asset.clone()),
                Err(CollateralError::RollbackWindowClosed)
            );
        });
    }

    #[test]
    fn rollback_without_a_pending_swap_is_rejected() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                rollback(&f.env, f.admin.clone(), f.asset.clone()),
                Err(CollateralError::NoPendingSwap)
            );
        });
    }

    #[test]
    fn rollback_refuses_to_overwrite_a_factor_changed_outside_the_swap() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, WINDOW).unwrap();
            // A direct write through set_asset_params bypasses the swap record.
            set_params(&f.env, &f.asset, 4_000);

            assert_eq!(
                rollback(&f.env, f.admin.clone(), f.asset.clone()),
                Err(CollateralError::FactorChangedSinceSwap)
            );
            assert_eq!(get_collateral_factor(&f.env, f.asset.clone()), Some(4_000));
        });
    }

    #[test]
    fn newer_swap_replaces_pending_record() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 6_000, WINDOW).unwrap();
            let second = hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 5_000, WINDOW).unwrap();
            assert_eq!(second.previous_factor, 6_000);

            // Rollback reverts only the latest swap.
            let restored = rollback(&f.env, f.admin.clone(), f.asset.clone()).unwrap();
            assert_eq!(restored, 6_000);
        });
    }

    #[test]
    fn out_of_range_factor_is_rejected() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                hot_swap(
                    &f.env,
                    f.admin.clone(),
                    f.asset.clone(),
                    BPS_DENOMINATOR + 1,
                    WINDOW
                ),
                Err(CollateralError::InvalidFactor)
            );
            assert_eq!(
                hot_swap(&f.env, f.admin.clone(), f.asset.clone(), -1, WINDOW),
                Err(CollateralError::InvalidFactor)
            );
            assert_eq!(get_collateral_factor(&f.env, f.asset.clone()), Some(7_500));
        });
    }

    #[test]
    fn window_above_maximum_is_rejected() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                hot_swap(
                    &f.env,
                    f.admin.clone(),
                    f.asset.clone(),
                    6_000,
                    MAX_ROLLBACK_WINDOW_SECONDS + 1
                ),
                Err(CollateralError::InvalidWindow)
            );
        });
    }

    #[test]
    fn swap_to_current_factor_is_a_rejected_no_op() {
        let f = fixture();
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                hot_swap(&f.env, f.admin.clone(), f.asset.clone(), 7_500, WINDOW),
                Err(CollateralError::NoChange)
            );
        });
    }

    #[test]
    fn unconfigured_asset_and_non_admin_are_rejected() {
        let f = fixture();
        let stranger = Address::generate(&f.env);
        let unknown = Address::generate(&f.env);
        f.env.as_contract(&f.contract, || {
            assert_eq!(
                hot_swap(&f.env, f.admin.clone(), unknown.clone(), 6_000, WINDOW),
                Err(CollateralError::AssetNotConfigured)
            );
            assert_eq!(
                hot_swap(&f.env, stranger.clone(), f.asset.clone(), 6_000, WINDOW),
                Err(CollateralError::Unauthorized)
            );
            assert_eq!(
                rollback(&f.env, stranger.clone(), f.asset.clone()),
                Err(CollateralError::Unauthorized)
            );
        });
    }
}
