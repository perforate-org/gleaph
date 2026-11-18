# 0132. Provision fee and reserve withdrawal

Date: 2026-11-19
Status: accepted (2026-11-19; implemented).

Related: [0127](0127-provision-deployment-record-and-service-config.md) (ledger and operator),
[0131](0131-provision-funding-integration.md) (admission quote and attached cycles).

## Context

Admissions accrue `ServiceLedger { platform_fees, compensation_reserve }` and keep those cycles in
the Provision canister: the `cost` portion pays resource work, while the fee and reserve remain
withheld. Until now there was no path to move the withheld cycles out, so the operating company
could account for revenue but never collect it.

ICP cycles can only leave a canister through the management `deposit_cycles` call to another
canister. They cannot be sent directly to a user principal.

## Decision

Provision exports one operator-authorized update:

```candid
withdraw_fees : (WithdrawArgs) -> (Result_16);
```

`WithdrawArgs` carries `kind` (`PlatformFees` or `CompensationReserve`), a destination **canister**,
an amount, and a 32-byte idempotency key. The configured `ServiceConfig.operator` is the only
authority. Anonymous and self destinations are rejected, and a zero amount is rejected.

The operation has three durable states in `WITHDRAWALS` (MemoryId 21), keyed by
`(authority, idempotency_key)`:

1. **Pending** — the amount is debited from the selected ledger pool in the same no-await message
   that inserts the record. A concurrent withdrawal observes the reduced ledger and cannot spend the
   same cycles.
2. **Completed** — set after `deposit_cycles` succeeds. An exact replay returns the recorded amount
   with `replayed = true` and performs no second transfer.
3. **Failed** — a failed transfer restores the exact amount to its pool and records the rejection.
   The same key then fails with `PreviousAttemptFailed`; retrying requires a new key.

The ledger fields are therefore *remaining withheld* values, not lifetime totals. A withdrawal
never touches deployment credit and never converts fiat.

## Consequences

- The operating company can collect fees and the compensation pool through a canister it controls,
  then convert cycles externally (for example through a cycles ledger/exchange). That conversion is
  outside Provision.
- A failed `deposit_cycles` leaves the accounting unchanged; a successful one cannot be replayed.
- Definitive management-call failures restore the pool. `SysUnknown`/`SysTransient` responses leave
  the withdrawal `Pending` and return `TransferPending`: the deposit may have landed, so restoring
  would allow a second transfer. The idempotency record and ledger are the reconciliation evidence.
- The handler catches call failures rather than trapping. An instruction-budget trap between the
  management call and the completion write remains an operational reconciliation boundary.
- No fiat, customer identity, or payment-provider data enters the canister.

## Test coverage

Store tests cover the ledger debit, exact replay, failure restoration, key reuse, zero amount, and
insufficient pool. Handler tests cover unauthorized callers, missing operator configuration,
anonymous destination, zero amount, and overdraft before any transfer. The native unit build cannot
execute an inter-canister management call; the PocketIC paid-admission flow remains the environment
that exercises the transfer itself.
