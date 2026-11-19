# Provision recovery runbook

Status: current operational contract. Update this file when an operator command or its safety
conditions change.

This runbook covers inspection and recovery of Provision-owned deployments: recorded canisters,
failed-job leftovers, credit grants, and treasury withdrawals. It does not cover card/fiat
processing (the external order service is not in this repository).

## Authority

Run every command as the identity that owns the operation:

| Operation | Allowed caller |
|---|---|
| `get_deployment`, `get_deployment_canisters` | deployment owner, operator, billing authority, governance |
| `repair_failed_deployment_canisters` | operator, governance |
| `grant_credit` / `reverse_credit` | configured billing authority |
| `get_service_ledger`, `withdraw_fees`, `get_withdrawal` | operator (governance may read the ledger cross-checks) |

The operator CLI uses the Provision principal passed with `--provision`. Never reuse a billing
identity for treasury withdrawals; keep the keys separate.

## 1. Identify the deployment

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment show --deployment <DEPLOYMENT>
```

Record:

- controller mode (`Managed` / `SelfCustody`)
- owner
- operator
- current credit
- deployment id

A missing record is `UnknownDeployment`; do not create a replacement deployment by hand.

## 2. Check active canisters

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment reconcile --deployment <DEPLOYMENT>
```

The command compares every **active-job** canister with `canister_status`:

- expected controllers: `[Provision, operator]` for Managed, `[Provision, owner]` for SelfCustody
- `running=true`
- `controllers_match=true`

Any mismatch exits non-zero. It is read-only.

| Output | Meaning | Next |
|---|---|---|
| `status_unavailable` | management call failed or canister is gone | stop; investigate before any repair |
| `running=false` | canister is stopped | investigate; do not delete an active job's canister |
| `controllers_match=false` | unexpected controller set | do not repair automatically; escalate |
| `reconcile: ok` | all active canisters healthy | continue |

## 3. Find failed-job leftovers

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment reconcile --deployment <DEPLOYMENT> --include-failed
```

`failed_job_canister` means only a Failed job references the canister and it still exists.

`failed_job_canister_missing` means the Failed job references a canister that no longer exists;
this is usually already-cleaned state, not an orphan.

Do not delete a canister that appears in both the active and failed sets. The API reports it as
`active=true`; repair skips it.

## 4. Repair failed-job leftovers

Preview:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment repair --deployment <DEPLOYMENT>
```

Execute only after confirming every listed canister is `failed_job_canister`:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment repair --deployment <DEPLOYMENT> --yes
```

The endpoint re-scans the deployment immediately before each delete. Outcomes:

- `Deleted` — stopped and deleted
- `AlreadyAbsent` — nothing to delete
- `SkippedActive` — the canister became active again; preserved
- `Failed { reason }` — stop/delete failed; investigate and retry

After repair, run step 2 and step 3 again. A second repair should report no `failed_job_canister`
unless a new failure occurred.

## 5. Credit operations

Preview first. `--yes` executes.

```bash
# Grant for a confirmed bank transfer / invoice payment.
gleaph-operator --provision <PROVISION> -n ic --identity billing.pem \
  billing grant \
  --deployment <DEPLOYMENT> \
  --amount <CYCLES> \
  --idempotency-key <64-hex-external-payment-id> \
  --yes

# Full reversal of an unused grant.
gleaph-operator --provision <PROVISION> -n ic --identity billing.pem \
  billing reverse \
  --grant-authority <ORIGINAL-AUTHORITY> \
  --idempotency-key <same-64-hex-key> \
  --yes
```

Rules:

- The idempotency key is the external payment id. Never reuse it for a different amount or
  deployment.
- Reverse only the **unused full grant**. Provision rejects a partial or consumed reversal.
- A consumed grant is refunded in fiat only; do not attempt an on-chain partial reversal.
- A replay with the same key returns the previous result and does not move credit again.

## 6. Treasury

```bash
gleaph-operator --provision <PROVISION> -n ic --identity treasury.pem \
  treasury ledger
```

Withdraw only to a **canister** principal; cycles cannot be sent to an ordinary principal.

```bash
gleaph-operator --provision <PROVISION> -n ic --identity treasury.pem \
  treasury withdraw \
  --kind platform-fees \
  --destination <CANISTER> \
  --amount <CYCLES> \
  --idempotency-key <64-hex> \
  --yes
```

`--kind` is `platform-fees` or `compensation-reserve`.

If the withdrawal is `Pending` or the management call is uncertain, query it before retrying:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity treasury.pem \
  treasury status --idempotency-key <64-hex>
```

`Pending` means the ledger is reserved but the transfer is not confirmed. Do not issue a new
withdrawal for the same intent with a different key. Retry with the same key only after the
status is terminally failed.

## 7. Failed jobs

A job that fails during provisioning enters `CleanupPending` and Provision tries to stop and
delete the canisters it already created. If cleanup cannot complete, the job remains
`CleanupPending`; replaying the same request resumes cleanup.

Manual repair (step 4) is for canisters that remain after a job has reached `Failed`. Do not
manually delete canisters while a job is still active.

## 8. Do not

- delete a canister reported as `active=true`
- repair controller mismatches with `deployment repair`
- send cycles directly to a user principal
- reuse an idempotency key with different arguments
- perform a partial on-chain credit reversal
- delete a `CleanupPending` job's canisters by hand; replay cleanup first

## 9. Escalate

Stop and escalate when:

- an active canister is missing or stopped
- controllers do not match and the deployment is live
- repair returns `Failed` twice for the same canister
- a withdrawal stays `Pending` after a replay
- credit does not match the external payment record after a replayed grant

Record for every action:

- command and caller principal
- deployment id
- idempotency key
- before/after output
- timestamp
