//! # Treasury Module
//!
//! Manages protocol fee collection and treasury for the StellarLend protocol.
//!
//! ## Overview
//! The treasury module provides a central place for:
//! - Configuring a protocol treasury address (admin-only)
//! - Viewing accumulated protocol reserves per asset
//! - Withdrawing reserves to a recipient (admin-only)
//! - Configuring fee percentages (interest spread fee, liquidation fee)
//!
//! ## Fee Sources
//! - **Borrow fees**: Collected at borrow time (`borrow_fee_bps` in `AssetParams`)
//! - **Interest spread**: A percentage of repaid interest goes to `ProtocolReserve`
//! - **Liquidation bonus fee**: A percentage of the liquidation incentive is retained
//! - **Flash loan fees**: Entire flash loan fee is credited to `ProtocolReserve`
//!
//! ## Access Control
//! All write operations require the caller to be the protocol admin.

#![allow(unused)]
use soroban_sdk::{contracterror, contracttype, Address, Env, Vec};

use crate::deposit::DepositDataKey;
use crate::events::{
    emit_event, emit_fee_config_updated, emit_reserves_claimed, emit_treasury_set,
};

/// Errors that can occur during treasury operations
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    /// Caller is not the protocol admin
    Unauthorized = 1,
    /// Amount must be greater than zero
    InvalidAmount = 2,
    /// Requested amount exceeds protocol reserves
    InsufficientReserve = 3,
    /// Overflow occurred during calculation
    Overflow = 4,
    /// Treasury address has not been configured
    TreasuryNotSet = 5,
    /// Fee value out of valid range (0–10000 bps)
    InvalidFee = 6,
    /// Voter has no vote-escrow voting power
    NoVotingPower = 7,
    /// Recipient is not registered with the allocator
    UnknownRecipient = 8,
    /// Recipient is already registered with the allocator
    DuplicateRecipient = 9,
    /// The allocation quarter has not ended yet
    QuarterNotEnded = 10,
    /// The quarter has already been disbursed for this asset
    AlreadyDisbursed = 11,
}

/// Storage keys for treasury-related data
#[contracttype]
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum TreasuryDataKey {
    /// Protocol treasury address
    TreasuryAddress,
    /// Fee configuration for interest and liquidation fees
    FeeConfig,
}

/// Fee configuration for protocol revenue collection
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TreasuryFeeConfig {
    /// Percentage of repaid interest routed to protocol reserve (in basis points)
    /// e.g. 1000 = 10% of interest paid goes to reserve
    pub interest_fee_bps: i128,
    /// Percentage of liquidation bonus retained by protocol (in basis points)
    /// e.g. 1000 = 10% of the liquidation incentive amount stays in reserve
    pub liquidation_fee_bps: i128,
}

/// Default: 10% of interest goes to reserve
const DEFAULT_INTEREST_FEE_BPS: i128 = 1000;
/// Default: 10% of liquidation bonus goes to reserve
const DEFAULT_LIQUIDATION_FEE_BPS: i128 = 1000;

/// Return the default fee configuration used when none has been set
pub fn default_fee_config() -> TreasuryFeeConfig {
    TreasuryFeeConfig {
        interest_fee_bps: DEFAULT_INTEREST_FEE_BPS,
        liquidation_fee_bps: DEFAULT_LIQUIDATION_FEE_BPS,
    }
}

// ============================================================================
// Treasury Address
// ============================================================================

/// Set the protocol treasury address (admin-only)
///
/// The treasury is the destination for claimed reserves when no explicit
/// recipient is provided to `claim_reserves`.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `caller` - Must be the protocol admin
/// * `treasury` - The new treasury address
pub fn set_treasury(env: &Env, caller: Address, treasury: Address) -> Result<(), TreasuryError> {
    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| TreasuryError::Unauthorized)?;

    env.storage()
        .persistent()
        .set(&TreasuryDataKey::TreasuryAddress, &treasury);

    emit_treasury_set(
        env,
        crate::events::TreasurySetEvent {
            admin: caller,
            treasury: treasury.clone(),
            timestamp: env.ledger().timestamp(),
        },
    );

    Ok(())
}

/// Return the configured treasury address, if any
pub fn get_treasury(env: &Env) -> Option<Address> {
    env.storage()
        .persistent()
        .get::<TreasuryDataKey, Address>(&TreasuryDataKey::TreasuryAddress)
}

// ============================================================================
// Fee Configuration
// ============================================================================

/// Return the current fee configuration (falls back to defaults if unset)
pub fn get_fee_config(env: &Env) -> TreasuryFeeConfig {
    env.storage()
        .persistent()
        .get::<TreasuryDataKey, TreasuryFeeConfig>(&TreasuryDataKey::FeeConfig)
        .unwrap_or_else(default_fee_config)
}

/// Return the reserve factor from fee configuration (falls back to DEFAULT_RESERVE_FACTOR_BPS if not set)
pub fn get_reserve_factor_from_fee_config(env: &Env) -> i128 {
    let config = get_fee_config(env);
    config.interest_fee_bps
}

/// Update the protocol fee configuration (admin-only)
///
/// Fee values must be in range [0, 10000] basis points.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `caller` - Must be the protocol admin
/// * `config` - New fee configuration
pub fn set_fee_config(
    env: &Env,
    caller: Address,
    config: TreasuryFeeConfig,
) -> Result<(), TreasuryError> {
    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| TreasuryError::Unauthorized)?;

    if config.interest_fee_bps < 0 || config.interest_fee_bps > 10000 {
        return Err(TreasuryError::InvalidFee);
    }
    if config.liquidation_fee_bps < 0 || config.liquidation_fee_bps > 10000 {
        return Err(TreasuryError::InvalidFee);
    }

    env.storage()
        .persistent()
        .set(&TreasuryDataKey::FeeConfig, &config);

    emit_fee_config_updated(
        env,
        crate::events::FeeConfigUpdatedEvent {
            admin: caller,
            interest_fee_bps: config.interest_fee_bps,
            liquidation_fee_bps: config.liquidation_fee_bps,
            timestamp: env.ledger().timestamp(),
        },
    );

    Ok(())
}

// ============================================================================
// Reserve Balance
// ============================================================================

/// Return the accumulated protocol reserve for a given asset
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `asset` - The asset address (None for native XLM)
pub fn get_reserve_balance(env: &Env, asset: Option<Address>) -> i128 {
    env.storage()
        .persistent()
        .get::<DepositDataKey, i128>(&DepositDataKey::ProtocolReserve(asset))
        .unwrap_or(0)
}

/// Accrue reserves from interest payment
pub fn accrue_reserve(
    env: &Env,
    asset: Option<Address>,
    interest_amount: i128,
) -> Result<(i128, i128), TreasuryError> {
    if interest_amount <= 0 {
        return Ok((0, 0));
    }

    let reserve_factor = crate::reserve::get_reserve_factor(env, asset.clone());

    let reserve_amount = interest_amount
        .checked_mul(reserve_factor)
        .ok_or(TreasuryError::Overflow)?
        .checked_div(10000)
        .ok_or(TreasuryError::Overflow)?;

    let lender_amount = interest_amount
        .checked_sub(reserve_amount)
        .ok_or(TreasuryError::Overflow)?;

    if reserve_amount > 0 {
        let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
        let current_reserve = env
            .storage()
            .persistent()
            .get::<DepositDataKey, i128>(&reserve_key)
            .unwrap_or(0);
        let new_reserve = current_reserve
            .checked_add(reserve_amount)
            .ok_or(TreasuryError::Overflow)?;
        env.storage().persistent().set(&reserve_key, &new_reserve);
    }

    Ok((reserve_amount, lender_amount))
}

// ============================================================================
// Claim Reserves
// ============================================================================

/// Withdraw accumulated protocol reserves to a recipient (admin-only)
///
/// Transfers `amount` tokens from the on-chain reserve tracking to `recipient`.
/// In non-test environments, the actual token transfer is executed.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `caller` - Must be the protocol admin
/// * `asset` - The asset to withdraw (None for native XLM)
/// * `recipient` - Destination address for the withdrawn tokens
/// * `amount` - Amount to withdraw (must be ≤ current reserve balance)
///
/// # Errors
/// * `TreasuryError::Unauthorized` - caller is not admin
/// * `TreasuryError::InvalidAmount` - amount ≤ 0
/// * `TreasuryError::InsufficientReserve` - amount exceeds reserve balance
pub fn claim_reserves(
    env: &Env,
    caller: Address,
    asset: Option<Address>,
    recipient: Address,
    amount: i128,
) -> Result<(), TreasuryError> {
    // Reentrancy guard — prevents re-entry during token transfer
    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| TreasuryError::Unauthorized)?;

    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| TreasuryError::Unauthorized)?;

    if amount <= 0 {
        return Err(TreasuryError::InvalidAmount);
    }

    let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
    let current_reserve = env
        .storage()
        .persistent()
        .get::<DepositDataKey, i128>(&reserve_key)
        .unwrap_or(0);

    if amount > current_reserve {
        return Err(TreasuryError::InsufficientReserve);
    }

    let new_reserve = current_reserve
        .checked_sub(amount)
        .ok_or(TreasuryError::Overflow)?;
    env.storage().persistent().set(&reserve_key, &new_reserve);

    // Execute the actual token transfer outside of tests
    #[cfg(not(test))]
    if let Some(ref asset_addr) = asset {
        let token_client = soroban_sdk::token::Client::new(env, asset_addr);
        token_client.transfer(&env.current_contract_address(), &recipient, &amount);
    }

    emit_reserves_claimed(
        env,
        crate::events::ReservesClaimedEvent {
            admin: caller,
            asset: asset.clone(),
            recipient: recipient.clone(),
            amount,
            timestamp: env.ledger().timestamp(),
        },
    );

    Ok(())
}

// ============================================================================
// DAO Treasury Allocator (issue #1524)
// ============================================================================
//
// Each quarter the DAO splits a per-asset budget across registered recipients
// in proportion to the vote weight behind each one. A vote's weight is the
// voter's vote-escrow voting power when the vote is cast
// (`vote_escrow::voting_power`), so voting strength follows governance tokens
// the voter has locked.
//
// Quarter `q` covers the ledger timestamps `[q * QUARTER_SECONDS,
// (q + 1) * QUARTER_SECONDS)`:
//
//   1. The admin registers recipients and sets the quarterly budget per asset.
//   2. Voters vote for one recipient during the quarter. Voting again replaces
//      the earlier vote and refreshes its weight.
//   3. Once the quarter ends, anyone may call `disburse_quarter` once per
//      asset. Recipient `i` receives `payable * tally_i / total_weight`, rounded
//      down, where `payable = min(budget, protocol reserve)`. Rounding dust stays
//      in the reserve.
//
// Payouts come from the same `ProtocolReserve` that `claim_reserves` uses, so
// the allocator never pays out more than the treasury holds.

/// Length of one allocation quarter: 90 days.
pub const QUARTER_SECONDS: u64 = 90 * 24 * 60 * 60;

/// Storage keys for the DAO treasury allocator.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum AllocatorDataKey {
    /// Registered recipients, in registration order.
    Recipients,
    /// Quarterly budget for one asset (`None` = native XLM).
    QuarterBudget(Option<Address>),
    /// One voter's current vote for a quarter.
    Vote(u64, Address),
    /// Total vote weight behind one recipient in a quarter.
    Tally(u64, Address),
    /// Total vote weight cast in a quarter.
    TotalWeight(u64),
    /// Whether a quarter has been disbursed for an asset.
    Disbursed(u64, Option<Address>),
}

/// A voter's current vote for one quarter.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocatorVote {
    /// Recipient the vote supports.
    pub recipient: Address,
    /// Voting power the vote carried when it was cast.
    pub weight: i128,
}

/// One payout made by `disburse_quarter`.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disbursement {
    /// Recipient paid.
    pub recipient: Address,
    /// Amount paid, in the asset's smallest unit.
    pub amount: i128,
}

/// Quarter index that contains ledger `timestamp`.
pub fn quarter_of(timestamp: u64) -> u64 {
    timestamp / QUARTER_SECONDS
}

/// Register a recipient that can receive quarterly disbursements (admin-only).
pub fn register_recipient(
    env: &Env,
    caller: Address,
    recipient: Address,
) -> Result<(), TreasuryError> {
    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| TreasuryError::Unauthorized)?;

    let mut recipients = get_recipients(env);
    if recipients.iter().any(|r| r == recipient) {
        return Err(TreasuryError::DuplicateRecipient);
    }
    recipients.push_back(recipient.clone());
    env.storage()
        .persistent()
        .set(&AllocatorDataKey::Recipients, &recipients);

    emit_event(env, "treasury_recipient_registered", "recipient", recipient);
    Ok(())
}

/// Registered recipients, in registration order.
pub fn get_recipients(env: &Env) -> Vec<Address> {
    env.storage()
        .persistent()
        .get::<AllocatorDataKey, Vec<Address>>(&AllocatorDataKey::Recipients)
        .unwrap_or_else(|| Vec::new(env))
}

/// Set the budget that each quarter may disburse for `asset` (admin-only).
pub fn set_quarter_budget(
    env: &Env,
    caller: Address,
    asset: Option<Address>,
    amount: i128,
) -> Result<(), TreasuryError> {
    caller.require_auth();
    crate::admin::require_admin(env, &caller).map_err(|_| TreasuryError::Unauthorized)?;
    if amount < 0 {
        return Err(TreasuryError::InvalidAmount);
    }

    env.storage()
        .persistent()
        .set(&AllocatorDataKey::QuarterBudget(asset.clone()), &amount);
    emit_event(
        env,
        "treasury_quarter_budget_set",
        "budget",
        (asset, amount),
    );
    Ok(())
}

/// Quarterly budget for `asset`. Zero when unset.
pub fn get_quarter_budget(env: &Env, asset: Option<Address>) -> i128 {
    env.storage()
        .persistent()
        .get::<AllocatorDataKey, i128>(&AllocatorDataKey::QuarterBudget(asset))
        .unwrap_or(0)
}

/// Cast or replace the caller's vote for `recipient` in the current quarter.
///
/// The weight is the caller's vote-escrow voting power now. Voting again in the
/// same quarter removes the earlier vote's weight first, so each voter counts
/// once. Returns the weight recorded.
pub fn vote_for_recipient(
    env: &Env,
    voter: Address,
    recipient: Address,
) -> Result<i128, TreasuryError> {
    voter.require_auth();
    if !is_registered(env, &recipient) {
        return Err(TreasuryError::UnknownRecipient);
    }
    let weight =
        crate::vote_escrow::voting_power(env, &voter).map_err(|_| TreasuryError::Overflow)?;
    if weight <= 0 {
        return Err(TreasuryError::NoVotingPower);
    }

    let quarter = quarter_of(env.ledger().timestamp());
    let vote_key = AllocatorDataKey::Vote(quarter, voter.clone());
    if let Some(previous) = env
        .storage()
        .persistent()
        .get::<AllocatorDataKey, AllocatorVote>(&vote_key)
    {
        remove_weight(env, quarter, &previous.recipient, previous.weight)?;
    }

    add_weight(env, quarter, &recipient, weight)?;
    env.storage().persistent().set(
        &vote_key,
        &AllocatorVote {
            recipient: recipient.clone(),
            weight,
        },
    );

    emit_event(
        env,
        "treasury_vote_cast",
        "vote",
        (voter, recipient, weight),
    );
    Ok(weight)
}

/// Pay out a finished quarter's budget for `asset`, split by vote weight.
///
/// Anyone may call this once the quarter has ended. Each quarter and asset can
/// be disbursed once; the flag is set before any transfer. The payable amount is
/// `min(budget, protocol reserve)`. Returns the payouts made.
pub fn disburse_quarter(
    env: &Env,
    asset: Option<Address>,
    quarter: u64,
) -> Result<Vec<Disbursement>, TreasuryError> {
    // Reentrancy guard — prevents re-entry during token transfers
    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| TreasuryError::Unauthorized)?;

    let quarter_end = quarter
        .checked_add(1)
        .and_then(|next| next.checked_mul(QUARTER_SECONDS))
        .ok_or(TreasuryError::Overflow)?;
    if env.ledger().timestamp() < quarter_end {
        return Err(TreasuryError::QuarterNotEnded);
    }

    let done_key = AllocatorDataKey::Disbursed(quarter, asset.clone());
    if env.storage().persistent().has(&done_key) {
        return Err(TreasuryError::AlreadyDisbursed);
    }
    env.storage().persistent().set(&done_key, &true);

    let mut payouts: Vec<Disbursement> = Vec::new(env);
    let total = total_weight(env, quarter);
    let budget =
        get_quarter_budget(env, asset.clone()).min(get_reserve_balance(env, asset.clone()));
    if total <= 0 || budget <= 0 {
        return Ok(payouts);
    }

    let mut paid: i128 = 0;
    for recipient in get_recipients(env).iter() {
        let tally = tally_of(env, quarter, &recipient);
        if tally <= 0 {
            continue;
        }
        let amount = budget.checked_mul(tally).ok_or(TreasuryError::Overflow)? / total;
        if amount <= 0 {
            continue;
        }
        paid = paid.checked_add(amount).ok_or(TreasuryError::Overflow)?;
        payouts.push_back(Disbursement { recipient, amount });
    }

    if paid > 0 {
        let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
        let reserve = env
            .storage()
            .persistent()
            .get::<DepositDataKey, i128>(&reserve_key)
            .unwrap_or(0);
        let new_reserve = reserve.checked_sub(paid).ok_or(TreasuryError::Overflow)?;
        env.storage().persistent().set(&reserve_key, &new_reserve);
    }

    // Execute the token transfers outside of tests, as `claim_reserves` does.
    #[cfg(not(test))]
    if let Some(ref asset_addr) = asset {
        let token_client = soroban_sdk::token::Client::new(env, asset_addr);
        for payout in payouts.iter() {
            token_client.transfer(
                &env.current_contract_address(),
                &payout.recipient,
                &payout.amount,
            );
        }
    }

    emit_event(
        env,
        "treasury_quarter_disbursed",
        "disbursed",
        (asset, quarter, paid),
    );
    Ok(payouts)
}

/// Total vote weight behind `recipient` in `quarter`.
pub fn get_recipient_tally(env: &Env, quarter: u64, recipient: Address) -> i128 {
    tally_of(env, quarter, &recipient)
}

fn is_registered(env: &Env, recipient: &Address) -> bool {
    get_recipients(env).iter().any(|r| r == *recipient)
}

fn tally_of(env: &Env, quarter: u64, recipient: &Address) -> i128 {
    env.storage()
        .persistent()
        .get::<AllocatorDataKey, i128>(&AllocatorDataKey::Tally(quarter, recipient.clone()))
        .unwrap_or(0)
}

fn total_weight(env: &Env, quarter: u64) -> i128 {
    env.storage()
        .persistent()
        .get::<AllocatorDataKey, i128>(&AllocatorDataKey::TotalWeight(quarter))
        .unwrap_or(0)
}

fn add_weight(
    env: &Env,
    quarter: u64,
    recipient: &Address,
    weight: i128,
) -> Result<(), TreasuryError> {
    let tally = tally_of(env, quarter, recipient)
        .checked_add(weight)
        .ok_or(TreasuryError::Overflow)?;
    let total = total_weight(env, quarter)
        .checked_add(weight)
        .ok_or(TreasuryError::Overflow)?;
    env.storage()
        .persistent()
        .set(&AllocatorDataKey::Tally(quarter, recipient.clone()), &tally);
    env.storage()
        .persistent()
        .set(&AllocatorDataKey::TotalWeight(quarter), &total);
    Ok(())
}

fn remove_weight(
    env: &Env,
    quarter: u64,
    recipient: &Address,
    weight: i128,
) -> Result<(), TreasuryError> {
    let tally = tally_of(env, quarter, recipient)
        .checked_sub(weight)
        .ok_or(TreasuryError::Overflow)?;
    let total = total_weight(env, quarter)
        .checked_sub(weight)
        .ok_or(TreasuryError::Overflow)?;
    env.storage()
        .persistent()
        .set(&AllocatorDataKey::Tally(quarter, recipient.clone()), &tally);
    env.storage()
        .persistent()
        .set(&AllocatorDataKey::TotalWeight(quarter), &total);
    Ok(())
}

#[cfg(test)]
mod allocator_tests {
    use super::*;
    use crate::vote_escrow::{EscrowLock, VoteEscrowKey, MAX_LOCK_SECONDS};
    use soroban_sdk::{
        contract, contractimpl,
        testutils::{Address as _, Ledger as _},
    };

    #[contract]
    struct TreasuryTestContract;

    #[contractimpl]
    impl TreasuryTestContract {
        pub fn marker() {}
    }

    const START: u64 = 1_000;

    struct Fixture {
        env: Env,
        contract: Address,
        admin: Address,
    }

    fn fixture() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|ledger| ledger.timestamp = START);
        let contract = env.register(TreasuryTestContract, ());
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

    /// Give `owner` a lock that outlasts the boost window, so its voting power
    /// stays at the full 2x `amount` for every quarter used in these tests.
    fn lock(f: &Fixture, owner: &Address, amount: i128) {
        f.env.as_contract(&f.contract, || {
            f.env.storage().persistent().set(
                &VoteEscrowKey::Lock(owner.clone()),
                &EscrowLock {
                    amount,
                    unlock_time: START + 2 * MAX_LOCK_SECONDS,
                },
            );
        });
    }

    fn seed_reserve(env: &Env, amount: i128) {
        env.storage()
            .persistent()
            .set(&DepositDataKey::ProtocolReserve(None), &amount);
    }

    fn set_time(f: &Fixture, timestamp: u64) {
        f.env
            .ledger()
            .with_mut(|ledger| ledger.timestamp = timestamp);
    }

    #[test]
    fn quarter_disburses_budget_in_proportion_to_vote_weight() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let r2 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        let bob = Address::generate(&f.env);
        lock(&f, &alice, 1_000); // weight 2_000
        lock(&f, &bob, 3_000); // weight 6_000

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            register_recipient(&f.env, f.admin.clone(), r2.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 9_000).unwrap();
            seed_reserve(&f.env, 10_000);

            assert_eq!(
                vote_for_recipient(&f.env, alice.clone(), r1.clone()),
                Ok(2_000)
            );
            assert_eq!(
                vote_for_recipient(&f.env, bob.clone(), r2.clone()),
                Ok(6_000)
            );
        });

        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            let payouts = disburse_quarter(&f.env, None, 0).unwrap();
            assert_eq!(payouts.len(), 2);
            // 9_000 * 2_000 / 8_000 = 2_250 and 9_000 * 6_000 / 8_000 = 6_750.
            assert_eq!(
                payouts.get(0).unwrap(),
                Disbursement {
                    recipient: r1.clone(),
                    amount: 2_250
                }
            );
            assert_eq!(
                payouts.get(1).unwrap(),
                Disbursement {
                    recipient: r2.clone(),
                    amount: 6_750
                }
            );
            assert_eq!(get_reserve_balance(&f.env, None), 1_000);
        });
    }

    #[test]
    fn revote_moves_weight_instead_of_counting_twice() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let r2 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        lock(&f, &alice, 1_000); // weight 2_000

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            register_recipient(&f.env, f.admin.clone(), r2.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 1_000).unwrap();
            seed_reserve(&f.env, 1_000);

            vote_for_recipient(&f.env, alice.clone(), r1.clone()).unwrap();
            vote_for_recipient(&f.env, alice.clone(), r2.clone()).unwrap();
            assert_eq!(get_recipient_tally(&f.env, 0, r1.clone()), 0);
            assert_eq!(get_recipient_tally(&f.env, 0, r2.clone()), 2_000);
            assert_eq!(total_weight(&f.env, 0), 2_000);
        });

        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            let payouts = disburse_quarter(&f.env, None, 0).unwrap();
            assert_eq!(payouts.len(), 1);
            assert_eq!(
                payouts.get(0).unwrap(),
                Disbursement {
                    recipient: r2.clone(),
                    amount: 1_000
                }
            );
        });
    }

    #[test]
    fn payout_is_capped_by_the_reserve_balance() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let r2 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        let bob = Address::generate(&f.env);
        lock(&f, &alice, 1_000); // weight 2_000
        lock(&f, &bob, 3_000); // weight 6_000

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            register_recipient(&f.env, f.admin.clone(), r2.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 9_000).unwrap();
            seed_reserve(&f.env, 4_000);
            vote_for_recipient(&f.env, alice.clone(), r1.clone()).unwrap();
            vote_for_recipient(&f.env, bob.clone(), r2.clone()).unwrap();
        });

        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            let payouts = disburse_quarter(&f.env, None, 0).unwrap();
            // Payable is min(9_000, 4_000) = 4_000, split 1:3.
            assert_eq!(payouts.get(0).unwrap().amount, 1_000);
            assert_eq!(payouts.get(1).unwrap().amount, 3_000);
            assert_eq!(get_reserve_balance(&f.env, None), 0);
        });
    }

    #[test]
    fn quarter_cannot_be_disbursed_before_it_ends() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        lock(&f, &alice, 1_000);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 500).unwrap();
            seed_reserve(&f.env, 500);
            vote_for_recipient(&f.env, alice.clone(), r1.clone()).unwrap();
            assert_eq!(
                disburse_quarter(&f.env, None, 0),
                Err(TreasuryError::QuarterNotEnded)
            );
        });

        // The failed attempt must not consume the quarter.
        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            assert_eq!(disburse_quarter(&f.env, None, 0).unwrap().len(), 1);
        });
    }

    #[test]
    fn quarter_disburses_only_once() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        lock(&f, &alice, 1_000);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 500).unwrap();
            seed_reserve(&f.env, 500);
            vote_for_recipient(&f.env, alice.clone(), r1.clone()).unwrap();
        });

        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            disburse_quarter(&f.env, None, 0).unwrap();
            assert_eq!(
                disburse_quarter(&f.env, None, 0),
                Err(TreasuryError::AlreadyDisbursed)
            );
        });
    }

    #[test]
    fn empty_quarter_pays_nothing_and_leaves_reserve_intact() {
        let f = fixture();
        let r1 = Address::generate(&f.env);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 500).unwrap();
            seed_reserve(&f.env, 500);
        });

        set_time(&f, QUARTER_SECONDS);
        f.env.as_contract(&f.contract, || {
            assert_eq!(disburse_quarter(&f.env, None, 0).unwrap().len(), 0);
            assert_eq!(get_reserve_balance(&f.env, None), 500);
        });
    }

    #[test]
    fn votes_count_only_toward_the_quarter_they_were_cast_in() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let r2 = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        lock(&f, &alice, 1_000);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            register_recipient(&f.env, f.admin.clone(), r2.clone()).unwrap();
            set_quarter_budget(&f.env, f.admin.clone(), None, 1_000).unwrap();
            seed_reserve(&f.env, 1_000);
            vote_for_recipient(&f.env, alice.clone(), r1.clone()).unwrap(); // quarter 0
        });

        // Vote again in quarter 1; quarter 0's tally must not change.
        set_time(&f, QUARTER_SECONDS + 10);
        f.env.as_contract(&f.contract, || {
            vote_for_recipient(&f.env, alice.clone(), r2.clone()).unwrap();
            assert_eq!(get_recipient_tally(&f.env, 0, r1.clone()), 2_000);
            assert_eq!(get_recipient_tally(&f.env, 1, r2.clone()), 2_000);
            assert_eq!(get_recipient_tally(&f.env, 0, r2.clone()), 0);
        });
    }

    #[test]
    fn voter_must_hold_power_and_vote_for_a_registered_recipient() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let stranger_recipient = Address::generate(&f.env);
        let alice = Address::generate(&f.env);
        let no_lock = Address::generate(&f.env);
        lock(&f, &alice, 1_000);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            assert_eq!(
                vote_for_recipient(&f.env, alice.clone(), stranger_recipient.clone()),
                Err(TreasuryError::UnknownRecipient)
            );
            assert_eq!(
                vote_for_recipient(&f.env, no_lock.clone(), r1.clone()),
                Err(TreasuryError::NoVotingPower)
            );
        });
    }

    #[test]
    fn duplicate_registration_and_non_admin_actions_are_rejected() {
        let f = fixture();
        let r1 = Address::generate(&f.env);
        let stranger = Address::generate(&f.env);

        f.env.as_contract(&f.contract, || {
            register_recipient(&f.env, f.admin.clone(), r1.clone()).unwrap();
            assert_eq!(
                register_recipient(&f.env, f.admin.clone(), r1.clone()),
                Err(TreasuryError::DuplicateRecipient)
            );
            assert_eq!(
                register_recipient(&f.env, stranger.clone(), Address::generate(&f.env)),
                Err(TreasuryError::Unauthorized)
            );
            assert_eq!(
                set_quarter_budget(&f.env, stranger.clone(), None, 100),
                Err(TreasuryError::Unauthorized)
            );
            assert_eq!(
                set_quarter_budget(&f.env, f.admin.clone(), None, -1),
                Err(TreasuryError::InvalidAmount)
            );
        });
    }
}
