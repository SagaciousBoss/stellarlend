# Insurance fund, pool rate curves, treasury allocator and collateral hot-swap

Four self-contained modules in `stellar-lend/contracts/hello-world/src/`. Each module
exposes plain Rust functions that check the caller, validate inputs, use checked
arithmetic, and emit an event for every state change. None of them changes existing
entrypoints; wiring them into `#[contractimpl]` is a separate decision.

| Module | Issue | What it does |
| --- | --- | --- |
| `insurance.rs` | #1520 | Per-asset insurance fund that takes a configurable share of each contribution |
| `rates.rs` (pool curves) | #1522 | Borrow-rate curve per pool, evaluated at that pool's utilization |
| `treasury.rs` (allocator) | #1524 | Quarterly, vote-weighted split of a per-asset budget across recipients |
| `collateral.rs` | #1530 | Immediate collateral-factor change with a bounded rollback window |

All four require the protocol admin for configuration (`admin::require_admin`).
Error codes are unique per module: insurance 700s, collateral 800s, pool rates 900s,
treasury allocator 7 to 11 on `TreasuryError`.

## Insurance fund (`insurance.rs`)

Every contribution for an asset is split once:

```text
to_fund    = amount * fund_split_bps / 10_000     (rounded down)
to_reserve = amount - to_fund
```

- `set_fund_split(admin, asset, fund_bps)`: `fund_bps` is in `[0, 10_000]`. An unset
  split is 0, so the fund is opt-in per asset.
- `route_contribution(asset, amount)`: credits the fund and the existing
  `ProtocolReserve` and returns the `ContributionSplit`. Rounding dust goes to the
  reserve, so `to_fund + to_reserve == amount`.
- `cover_loss(admin, asset, loss)`: pays out at most the fund balance and returns
  `LossCoverage { covered, shortfall }`. The balance never goes negative.

## Pool rate curves (`rates.rs`)

Each pool can have its own curve. The curve shape is evaluated by the shared
`stellarlend_math::rates::RateCurve`, so the math is not duplicated.

| Field | Meaning (basis points) |
| --- | --- |
| `shape` | `Linear`, `Kink` or `Jump` |
| `base_rate_bps` | Rate at 0% utilization |
| `kink_utilization_bps` | Must be in `(0, 10_000)` |
| `multiplier_bps` | Slope below the kink |
| `jump_multiplier_bps` | Slope above the kink (`Kink` and `Jump` shapes) |

- Configuration is rejected when any value is negative, the kink is 0 or 100%, or
  `base + multiplier + jump` exceeds `MAX_POOL_RATE_BPS` (100,000 = 1,000% APR).
- `pool_borrow_rate(pool, utilization_bps)` returns `CurveNotFound` for a pool without a
  curve. Utilization outside `[0, 10_000]` is rejected.
- `pool_utilization_bps(total_borrows, total_deposits)` returns 0 for an empty pool and
  caps at 100%.

## DAO treasury allocator (`treasury.rs`)

Quarters are 90 days: quarter `q` covers `[q * QUARTER_SECONDS, (q + 1) * QUARTER_SECONDS)`.

1. The admin calls `register_recipient` and `set_quarter_budget(asset, amount)`.
2. A voter calls `vote_for_recipient(recipient)` during the quarter. The weight is the
   voter's vote-escrow voting power (`vote_escrow::voting_power`) at that moment. Voting
   again in the same quarter replaces the earlier vote and refreshes its weight.
3. After the quarter ends, anyone calls `disburse_quarter(asset, quarter)` once per asset.
   Recipient `i` receives `payable * tally_i / total_weight`, rounded down, where
   `payable = min(budget, ProtocolReserve)`.

Payouts reduce the same `ProtocolReserve` that `claim_reserves` uses, so the allocator
cannot pay more than the treasury holds. The quarter is marked disbursed before any
transfer. A quarter with no votes pays nothing, and its unspent budget stays in the
reserve.

## Collateral factor hot-swap (`collateral.rs`)

`hot_swap(admin, asset, new_factor_bps, rollback_window_seconds)` updates the live
`AssetParams.collateral_factor`, which the withdraw path reads, so the change applies
immediately. It records the previous factor and sets `rollback_until = now + window`.

- Factors are in `[0, 10_000]`. A swap to the current factor is rejected.
- The window is at most 7 days. A window of 0 cannot be rolled back.
- `rollback(admin, asset)` restores the previous factor only while `now < rollback_until`.
  It refuses to run if the live factor no longer equals the value the swap applied,
  which happens when `set_asset_params` changed it directly. This prevents a rollback
  from overwriting a newer value.
- A new hot-swap replaces the pending record, so rollback reverts only the latest swap.

## Verification status

These modules have unit tests in the same files. Those tests have not been compiled or
run in the commit that introduced this page; run the contract suite with
`scripts/testing/run-contract-tests.sh` before merging.
