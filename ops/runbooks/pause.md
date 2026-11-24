# Pause Runbook

## Purpose

Every protocol pause switch is controlled by `contracts/admin/src/pause.rs`. A
single admin key can no longer pause the protocol: a request must be proposed
and approved by a multi-sig quorum, wait out a timelock, and be executed inside
a bounded window. Every accepted and rejected transition is written to an audit
sink so abuse attempts are visible.

## Policy

| Parameter | Production value | Purpose |
| --- | --- | --- |
| Signers | 3 configured signers | Distinct, named keys held by independent operators |
| Threshold | 2 of 3 | At least two distinct signers must approve |
| Minimum delay | 48 hours (`172_800` s) | Lead time before `eta` |
| Maximum horizon | 30 days (`2_592_000` s) | Bounds how far ahead a request may be scheduled |
| Execution window | 24 hours after `eta` | Prevents stale approvals from being replayed |
| Targets | Allowlisted `deposits`, `withdrawals`, `borrows`, `repayments`, `liquidations`, `transfers`, `oracle_updates`, `all` | No free-form switch names |

The policy is validated on installation: fewer than two signers, a threshold
below two, duplicate/empty signers, a delay below one hour, or a missing
execution window are rejected before any request can be created.

## Roles

- **Proposer** — a configured signer who schedules a request. The proposal
  counts as that signer's approval.
- **Approver** — another configured signer who approves the exact request
  digest. Approvals are one per signer and cannot be replayed.
- **Executor** — an approving signer who calls `execute` after `eta` and before
  the execution window closes.

There is no single-key emergency bypass. Guardians and the timelock executor
cannot pause a switch through this controller unless they are configured
signers and satisfy the quorum.

## Procedure

1. **Confirm the incident and target.** Record the incident or change ticket id
   and choose an allowlisted target from the table above. Prefer the narrowest
   target that contains the incident.
2. **Propose.** A signer calls `propose(actor, target, paused, eta, now,
   trace_id)`. Use the ticket id as `trace_id`. `eta` must be between
   `now + 48h` and `now + 30d`. The call returns a request id and publishes a
   `proposed` audit event containing the digest.
3. **Collect approvals.** At least one additional distinct signer calls
   `approve(actor, request_id, now, trace_id)` before `eta`. Each approval is
   bound to the request digest; changing the target, direction, timestamps or
   proposer produces a different digest and a new request.
4. **Announce.** Post the request id, target, direction (`paused` / `unpaused`),
   `eta` and digest in the operations channel. Monitoring alerts on `proposed`
   and `approved` events use the same fields.
5. **Execute after the timelock.** Once `now >= eta` and `now <= eta + 24h`, an
   approving signer calls `execute`. The controller re-checks the quorum and
   the window, flips the switch, and emits `executed`.
6. **Verify.** Confirm `is_paused(target)` reflects the intended state and that
   the `executed` event's `digest` matches the proposal. Attach the request id
   and `trace_id` to the incident record.
7. **Unpause.** Repeat the same flow with `paused = false`. Unpausing is never
   a single-signer action.

## Cancellation

Any configured signer may call `cancel(actor, request_id, now, trace_id)` while
the request is pending. Cancellation is final; start a new request if the
action is still needed. A cancelled or executed request cannot be approved or
executed again.

## Audit trail

Every event has this stable shape:

```
kind | actor | request_id | target | paused | eta | timestamp
     | approvals | threshold | digest | trace_id | reason
```

| `kind` | When it is emitted |
| --- | --- |
| `policy_configured` | A policy is installed |
| `proposed` | A request is accepted |
| `approved` | A distinct signer approves |
| `rejected` | Any call fails authorization, timing, state or input checks |
| `cancelled` | A pending request is cancelled |
| `executed` | A quorum executes after the timelock |

`rejected` events carry a machine-readable `reason` such as `not_a_signer`,
`eta_outside_window`, `quorum_not_reached` or `timelock_not_elapsed`. Alert on
any `rejected` event for a pause target, and page on repeated `not_a_signer`
or `quorum_not_reached` events.

## Failure modes

| Symptom | Cause | Response |
| --- | --- | --- |
| `Unauthorized` / `not_a_signer` | Caller is not configured | Use a configured signer; investigate if unexpected |
| `InvalidEta` | ETA too soon or beyond the horizon | Re-propose with a valid ETA |
| `TooEarly` | Timelock has not elapsed | Wait; do not bypass |
| `WindowClosed` | Execution happened too late | Re-propose |
| `ThresholdNotReached` | Only the proposer approved | Collect another distinct signer |
| `AlreadyApproved` | Duplicate approval | No action; already counted |
| `AlreadyFinalized` | Replay of an executed/cancelled request | No action; start a new request |
| `AuditFailure` | Sink rejected the event | Treat the switch as unchanged; restore the sink and retry the original call |

If the policy itself must change (signer rotation, threshold or delay), install
a new `PausePolicy` through the same governance/timelock process used for other
sensitive configuration and audit it as `policy_configured`. Do not weaken the
policy while an incident is open.
