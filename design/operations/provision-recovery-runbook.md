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
| `expire_deployment_suspensions`, `resume_pending_cleanups` | operator, governance |
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

### Job history

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment jobs --deployment <DEPLOYMENT>
```

Each row is one canonical job:

- `state_name` (`Completed`, `Failed`, `CleanupPending`, `RouterRegistrationPending`, ...)
- `active_resource_index`
- `completed_effect_count`
- `resource_count`
- `graph_name` / `reserved_graph_id`
- `created_at_ns` / `last_transition_ns`

To inspect one job, use the request id:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment jobs --deployment <DEPLOYMENT> --request-id <64-hex>

gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment job --deployment <DEPLOYMENT> --request-id <64-hex>
```

`deployment job` prints the resources, canister ids, and artifact hashes; use it after the
summary identifies the failing request.

Use this before reconcile when a deployment is missing a resource or a request is stuck. A
`Failed` row explains why `deployment reconcile --include-failed` lists a canister as
`failed_job_canister`. A `CleanupPending` row means cleanup is still owned by the job; do not
repair it by hand.

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

Full triage order:

```text
deployment show
  -> deployment jobs
  -> deployment reconcile --include-failed
  -> deployment repair (preview)
  -> deployment repair --yes
  -> deployment reconcile
```

## 5. Ownership and controller mode

All three commands preview by default. Add `--yes` only after checking the printed current
record.

```bash
# Governance repair of a Managed deployment's owner (record only; controllers unchanged).
gleaph-operator --provision <PROVISION> -n ic --identity governance.pem \
  deployment set-owner --deployment <DEPLOYMENT> --owner <NEW_OWNER> --yes

# The deployment owner takes the controllers from the operator (Managed -> SelfCustody).
gleaph-operator --provision <PROVISION> -n ic --identity owner.pem \
  deployment take-ownership --deployment <DEPLOYMENT> --yes

# Governance transfer of a SelfCustody deployment's owner (controllers move to the new owner).
gleaph-operator --provision <PROVISION> -n ic --identity governance.pem \
  deployment transfer-self-custody --deployment <DEPLOYMENT> --owner <NEW_OWNER> --yes
```

Rules:

- `set-owner` is Managed-only; it does not change controllers.
- `take-ownership` is owner-only and moves every recorded canister from the operator to the owner.
- `transfer-self-custody` is governance-only and moves controllers from the old owner to the new
  owner.
- There is no SelfCustody -> Managed command; do not hand-edit the record.
- After any ownership change, run `deployment reconcile` and confirm the expected controller set.

## 6. Credit operations

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

Read grants for reconciliation:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity billing.pem \
  billing grants --deployment <DEPLOYMENT>

gleaph-operator --provision <PROVISION> -n ic --identity billing.pem \
  billing grant-status \
  --grant-authority <AUTHORITY> \
  --idempotency-key <64-hex>
```

`billing grants` is readable by the deployment owner/operator/billing authority/governance.
`billing grant-status` is restricted to the configured billing authority or governance; a
non-governance caller cannot read another authority's key.

Rules:

- The idempotency key is the external payment id. Never reuse it for a different amount or
  deployment.
- Reverse only the **unused full grant**. Provision rejects a partial or consumed reversal.
- A consumed grant is refunded in fiat only; do not attempt an on-chain partial reversal.
- A replay with the same key returns the previous result and does not move credit again.

## 7. Treasury

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

## 8. Suspend, resume, and delete

Suspension is reversible and keeps every canister. Deletion is irreversible and requires a zero
deployment credit.

### Suspend

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment suspend --deployment <DEPLOYMENT> --reason non-payment
```

Without `--yes` the current record is printed. With `--yes` every active recorded canister is
stopped and the record moves to `Suspended`. A non-terminal job is refused with `ActiveJob`; finish
or repair it first. A job in `RouterRegistrationPending` is waiting for the Router's registration
ACK: repeating the DDL that issued it (`CREATE TEXT INDEX`, `CREATE VECTOR INDEX`, `CREATE INDEX`,
`enable_ask`) sends the ACK once the Router catalog owns the canister.

### Resume

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment resume --deployment <DEPLOYMENT> --yes
```

Every recorded canister is started and the record moves to `Cancelled`. If a start fails, the
record stays `Suspended` and the failed canisters are reported; retry the same command.

### Expire the grace period

Run a bounded sweep from the operator cron. The wrapper validates the identity and limit, then
calls the operator:

```bash
GLEAPH_OPERATOR_IDENTITY=/etc/gleaph/ops.pem \
GLEAPH_PROVISION=<PROVISION> \
GLEAPH_NETWORK=ic \
SWEEP_LIMIT=100 \
  scripts/provision-sweep.sh expire
```

A crontab entry such as the following runs it hourly:

```cron
17 * * * * cd /opt/gleaph && GLEAPH_OPERATOR_IDENTITY=/etc/gleaph/ops.pem GLEAPH_PROVISION=<PROVISION> SWEEP_LIMIT=100 scripts/provision-sweep.sh expire >>/var/log/gleaph-expire.log 2>&1
```

The sweep only marks records whose `grace_until_ns` passed (`GraceExpired`); it never deletes.
`deployment termination` shows the state and `grace_expired_at_ns`. Review the expired list before
any deletion.

### Delete

1. Verify `deployment show` reports `credit_cycles = 0`.
2. If credit remains, reverse the unused grant (`billing reverse`) or consume it; deletion refuses
   positive credit with `CreditNotSettled`.
3. Suspend the deployment and confirm every canister is stopped; wait for the sweep to mark
   `GraceExpired`.
4. Preview, then repeat the deployment principal:

```bash
gleaph-operator --provision <PROVISION> -n ic --identity ops.pem \
  deployment delete --deployment <DEPLOYMENT> --confirm <DEPLOYMENT> --yes
```

Governance may add `--force` to bypass the 30-day grace period. The record moves through
`Deleting` to `Deleted`; absent canisters are skipped, and a failed delete leaves the remaining
list for retry. Billing grants and the deployment record remain as audit history.

## 9. Failed jobs

A job that fails during provisioning enters `CleanupPending` and Provision tries to stop and
delete the canisters it already created. If cleanup cannot complete, the job remains
`CleanupPending` and keeps those canisters until cleanup is resumed. Two entries resume it:

- a replay of the same request, and
- the operator cleanup sweep, which does not depend on the issuer retrying.

Run the sweep from the operator cron with the same wrapper as the grace sweep:

```bash
GLEAPH_OPERATOR_IDENTITY=/etc/gleaph/ops.pem \
GLEAPH_PROVISION=<PROVISION> \
GLEAPH_NETWORK=ic \
SWEEP_LIMIT=100 \
  scripts/provision-sweep.sh cleanup
```

```cron
47 * * * * cd /opt/gleaph && GLEAPH_OPERATOR_IDENTITY=/etc/gleaph/ops.pem GLEAPH_PROVISION=<PROVISION> SWEEP_LIMIT=100 scripts/provision-sweep.sh cleanup >>/var/log/gleaph-cleanup.log 2>&1
```

Each run attempts the oldest `CleanupPending` jobs once and prints one line per job:

- `completed` - every created canister is gone; the job is `Failed` and its intent claims are
  released.
- `still_pending: <canisters>` - a stop/delete call failed again; the listed canisters remain.

The command exits non-zero while any swept job is still pending, so alert on the cron job's exit
status. The sweep does not record why a delete failed: for a job that stays pending across runs,
check the listed canisters with `canister_status` and escalate (step 11).

Manual repair (step 4) is for canisters that remain after a job has reached `Failed`. Do not
manually delete canisters while a job is still active.

## 10. Do not

- delete a canister reported as `active=true`
- delete a deployment with positive credit; settle it first
- use `--force` without governance authority
- repair controller mismatches with `deployment repair`
- send cycles directly to a user principal
- reuse an idempotency key with different arguments
- perform a partial on-chain credit reversal
- delete a `CleanupPending` job's canisters by hand; run the cleanup sweep or replay the request first

## 11. Escalate

Stop and escalate when:

- an active canister is missing or stopped
- controllers do not match and the deployment is live
- repair returns `Failed` twice for the same canister
- the cleanup sweep reports the same job `still_pending` on consecutive runs
- a withdrawal stays `Pending` after a replay
- credit does not match the external payment record after a replayed grant

Record for every action:

- command and caller principal
- deployment id
- idempotency key
- before/after output
- timestamp
