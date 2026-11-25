# Pause Runbook

## Purpose

Protocol pause switches must never be flipped by a single unrecorded
administrator action. This runbook covers the timelocked, multi-party pause
flow implemented by `contracts/pause/src/lib.rs` (on-chain guard) and
`backend/src/services/pauseService.ts` (proposal/signature coordination).
Issue #1781 is enforced by these canonical implementations on `main`.

## Policy

| Parameter | Production value | Purpose |
| --- | --- | --- |
| Signers | Distinct operator addresses recorded on the proposal | Prevents one actor from supplying both approvals |
| Threshold | 2 approvals (`requiredSignatures = 2`) | Proposal counts as the first approval; one other signer is required |
| Timelock | 1 hour (`TIMELOCK_DELAY_SECONDS = 3600` / `delaySeconds = 3600`) | Mandatory lead time between proposal and execution |
| Replay protection | `executed` flag on the proposal | A finalized proposal cannot be executed again |
| Audit surface | `PROP_PAUS` and `EXEC_PAUS` contract events plus the proposal record | Every observed transition is queryable |

The on-chain guard authenticates both the proposer and the executor with
`require_auth`. The coordinator rejects execution until the signature count
reaches the threshold and `Date.now() >= executableAt`. Do not shorten the
delay or lower the threshold for incidents; open a new proposal instead.

## Roles

- **Proposer** — an administrator who calls `propose_pause` / `propose`. The
  proposal records their address as the first signature.
- **Approver** — a second, distinct administrator who calls `signProposal`.
  Duplicate signatures are ignored, not double-counted.
- **Executor** — a signer who calls `canExecute` and then `execute_pause` only
  after the timelock and quorum checks pass.

There is no single-key bypass. Guardians and emergency scripts must go through
the same proposal, signature and timelock flow.

## Procedure

1. **Confirm the incident and target.** Record the incident or change ticket
   id and the pool id. Use the pool-scoped proposal key so unrelated pools are
   unaffected.
2. **Propose.** The proposer calls `propose_pause(env, proposer, pool_id)` (or
   `TimelockedPauseService.propose(poolId, proposer)`). Record the returned
   `execution_time` / `executableAt` and the ticket id in the operations
   channel.
3. **Collect the second approval.** A distinct signer calls `signProposal`.
   Confirm `canExecute(poolId)` reports `ready: true`; if not, use the returned
   reason (`Insufficient signatures`, `Timelock delay window has not
   expired`, `Proposal not found`, or `Already executed`).
4. **Wait out the timelock.** Do not bypass the delay. The guard independently
   rejects early execution with `TimelockActive`.
5. **Execute.** An authenticated signer calls
   `execute_pause(env, caller, pool_id)`. The guard re-checks the stored
   proposal, rejects replays (`AlreadyExecuted`), marks the proposal executed,
   and emits `EXEC_PAUS`.
6. **Verify.** Confirm the `EXEC_PAUS` event and the proposal's `executed`
   flag. Attach the pool id, execution time and ticket id to the incident
   record.
7. **Unpause.** Repeat the same flow; unpausing is never a single-signer
   action.

## Cancellation

A pending proposal is not executed until the quorum and timelock both pass.
If it is no longer needed, stop signing it and create a new proposal for the
current intent. Once `execute_pause` succeeds the proposal is final and any
retry returns `AlreadyExecuted`.

## Audit trail

| Event / record | Emitted when | Key fields |
| --- | --- | --- |
| `PROP_PAUS` | A proposal is stored | `pool_id`, `proposer`, `execution_time` |
| `EXEC_PAUS` | A quorum executes after the timelock | `pool_id`, `caller`, `now` |
| `PauseProposal` (coordinator) | Proposal / signature state changes | `poolId`, `proposer`, `proposedAt`, `executableAt`, `executed`, `signatures` |

Alert on `PROP_PAUS` events that do not reach `EXEC_PAUS` within the expected
window, and page on repeated `TimelockActive` or `InsufficientSignatures`
attempts. Retain the ticket id alongside the pool id so an auditor can tie an
event to the decision that authorised it.

## Failure modes

| Symptom | Cause | Response |
| --- | --- | --- |
| `NotAdmin` | Caller is not authenticated | Use an authorised signer; investigate if unexpected |
| `Insufficient signatures` | Fewer than two distinct approvals | Collect another distinct signer |
| `TimelockActive` / `Timelock delay window has not expired` | Execution too early | Wait; do not bypass |
| `ProposalNotFound` | No proposal exists for the pool | Propose first |
| `AlreadyExecuted` | Replay of a finalized proposal | No action; start a new proposal if needed |

Policy changes (signer rotation, threshold or delay) must go through the same
governance/timelock process used for other sensitive configuration. Do not
weaken the policy while an incident is open.
