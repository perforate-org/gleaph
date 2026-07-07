---
name: herdr-workflow
description: Coordinate Gleaph plans, implementation, iterative review, validation, final approval, commits, pane resets, and skill forward-testing across sibling herdr panes. Use whenever HERDR_ENV=1 and work is delegated among primary, implementation, review, or validation panes. This skill owns repository-specific collaboration policy; use the global herdr skill separately for CLI mechanics.
---

# Herdr Workflow

Keep the primary pane focused on unresolved architecture decisions and the final approval/commit
gate while supervisory delegation is still being proven. A designated supervisor owns routine
pane connection and the complete plan-to-validation chain; implementation, review, and validation
communicate directly with the supervisor or with each other as defined below. Do not use the primary
as a message relay. Use the global `herdr` skill for command syntax; do not duplicate its socket or
pane-management reference here.

## Start a slice

1. Re-read pane ids with `herdr pane list`; ids may compact.
2. Verify implementation, review, and validation panes show fresh startup prompts. Reset a completed
   conversation before reuse.
3. Have the primary or user establish only unresolved architecture/product direction. Designate one
   supervisor to inspect the repository, choose the bounded slice within accepted contracts, write
   the plan, and drive the remaining cross-pane chain. Once delegation maturity is explicitly
   granted, the supervisor may choose routine next slices without a primary turn.
4. Have the supervisor prime implementation, review, and validation conversations before
   implementation starts. Give the reviewer the plan
   path, implementation/primary pane ids, strict read-only skills, and exact finding/approval routes;
   give validation its role and tell it to remain idle until assigned an allowlist. Each setup turn
   must acknowledge readiness and end without polling.
5. Assign explicit pane ids and roles in every prompt.
6. Name every required repository skill by its exact path:
   `.agents/skills/<skill-name>/SKILL.md`. Instruct the pane to read it directly. Do not rely on
   `$CODEX_HOME` or global discovery for Gleaph-specific skills.

## Role ownership

### Primary pane

- Own only architecture/scope decisions escalated by the supervisor, user communication that cannot
  be delegated, independent final inspection, and commits while the delegation gate remains active.
- Do not connect routine panes, relay plan approvals, duplicate implementation/review iterations,
  or arm handoff watchers; those are supervisor responsibilities.
- Inspect the final diff and evidence independently before approval.
- Use the required safe commit helper; sibling panes do not commit.

### Supervisor pane

- Own plan drafting, plan-review iterations, implementation assignment, review/fix routing,
  validation assignment, and completion-evidence collation for the designated slice.
- Escalate to the primary only for a material architecture/scope decision, an unresolved blocker, or
  a final-approval candidate. Do not relay routine stage transitions to the primary.
- Inspect pane reports and actual repository state before advancing stages. Reviewer approval alone
  does not authorize validation or final handoff when the diff or plan remains inconsistent.
- Never commit. The primary retains final inspection and commit authority.
- Receive routine notifications directly from implementation, review, validation, and watchers.
  Notify the primary only for a real architecture/scope blocker or a complete final-approval
  candidate. A plan APPROVE, implementation start, review round, checkpoint, or validation start is
  not a primary notification.
- The supervisor owns plan decisions and acceptance of plan revisions, but may delegate mechanical
  plan-file editing to a fresh implementation/plan-editor pane when its provider is unreliable for
  patch-heavy turns. The handoff must say `PLAN ONLY`, forbid product-code edits, point to the durable
  review queue, and return the revised plan to the supervisor and reviewer. This is not authorization
  to begin implementation; product work starts only after explicit plan APPROVE.

### Implementation pane

- Read the plan, `AGENTS.md`, `implementation-integrity`, `code-quality`, and relevant domain skills.
- Own code, focused tests, design synchronization, and benchmark changes.
- Preserve unrelated worktree changes and never rewrite history.
- Keep one conversation for the entire slice, including review fixes.
- Notify the review pane only after edits, focused validation, and an honest report are complete.
- If the task is truncated, blocked, or only partially completed, notify the assigning supervisor
  explicitly before ending; never ask an unnamed "user" for missing instructions and leave the
  workflow idle. State exactly what completed, what did not, and which durable queue or prompt
  portion is missing.
- Prefer the native `apply_patch` tool for manual edits. When the model/provider does not expose that
  tool, the shell `apply_patch` executable is the only permitted fallback: include the patch text
  directly in the tool call, do not stage it in `/tmp` or another file, and do not substitute
  `cat`, `sed -i`, or a Python writer. Inspect the actual diff after every fallback patch. After two
  failed patch attempts, stop and route editing to a pane whose tool path works instead of probing
  more writers.

  Use the patch grammar exactly; do not infer or probe it. A shell fallback may use a literal heredoc
  only as `apply_patch` standard input:

  ```sh
  apply_patch <<'PATCH'
  *** Begin Patch
  *** Add File: path/to/new-file
  +new contents
  *** Update File: path/to/existing-file
  @@
   unchanged context
  -old contents
  +new contents
  *** Delete File: path/to/obsolete-file
  *** End Patch
  PATCH
  ```

  Every added file-content line starts with `+`. Update hunks contain context (unchanged) lines,
  removal (`-`) lines, or addition (`+`) lines. Omit operations that are not needed; do not add diff
  `---`/`+++` headers or a trailing `@@` after hunk content.

### Review pane

- Use `adversarial-test-review` strict read-only mode plus relevant architecture/design skills.
- Review the plan, base, actual diff, and evidence; do not approve from the report.
- Send plan-review findings to the designated supervisor, which owns plan revisions. Send
  implementation-diff findings to the implementation pane and re-review its fixes. Never route a
  rejected plan to implementation as authorization to start product work.
- Do not notify the primary while actionable P1/P2 findings or required corrections remain.
- Notify the primary only after a consistent `APPROVE` verdict.
- When `HERDR_ENV=1`, use only the `herdr` CLI for pane discovery and delivery. Never probe for or
  fall back to tmux. If a long findings report would exceed the handoff limit, save it under
  `/private/tmp/` and send the supervisor a short `herdr pane run` message pointing to the report.

### Validation pane

- Start only after explicit assignment; do not wait or poll for future work.
- Use `cost-aware-validation` and an exact ordered command allowlist.
- Never edit or repair the worktree.
- Report actual terminal completion; background, `--no-run`, interrupted, and timed-out commands are
  not runtime passes.
- Before ending, execute `herdr pane run <assigning-supervisor> "..."` with the aggregate verdict,
  completed/failed/incomplete/not-run counts, key test totals, and confirmation that no edits were
  made. Writing the report only in validation scrollback is not delivery. If the notification cannot
  be verified, state the routing failure instead of silently ending.

## Notification chain

Every notification must be self-contained: name the slice/plan, sender and recipient pane ids, the
next action, and where the recipient must send findings or approval. Do not expect a fresh pane to
infer its role from "finished" alone.

Do not report a task as dispatched merely because the prompt was drafted or described in prose.
Use `herdr pane run <target> <text>` as the atomic text-plus-Enter path, then perform one bounded
verification that the recipient shows the delivered prompt or has entered `working` or `done`.
`idle` alone proves neither delivery nor corruption. If delivery is not observable, retry the same
`pane run` once, then report routing failure; do not keep submitting keys or leave the chain silently
idle. Never declare delivery from a command exit code alone.

Do not auto-reset a target with Escape, Ctrl+C, or `/new` merely because it remains `idle`; a short
task may already have completed. Reset only under the post-commit policy or after positive evidence
of a corrupted conversation. When `pane run` is unavailable and a literal-text path is required,
use `herdr agent send` followed by `herdr pane send-keys <target> Return`, apply the same bounded
verification and single retry, then report failure.

Keep pane messages short enough to survive terminal and input limits. Do not embed long review
reports, large code blocks, or multi-page fix queues in `herdr pane run`. Write detailed instructions
to a unique durable file under `/private/tmp/`, then send a concise message naming that path, scope,
sender, recipient, and return route. As a default, keep the inline handoff below 1,500 characters.
After delivery, inspect the recipient's visible prompt once when truncation would be costly.
Never expand the durable file back into the command with `$(cat ...)`, backticks, or equivalent, and
never use a temporary prompt file merely to inject its full contents. The recipient reads the named
file itself.

When the user requires uninterrupted multi-stage chaining, do not rely solely on the delegated
agent's final self-notification. Arm one event-driven fallback in a plain shell pane with
`herdr wait agent-status <recipient> --status done --timeout <budget>`; on completion it wakes the
supervisor with `herdr pane run`, and on timeout it sends a **checkpoint alert**, not an automatic
stop instruction. At that checkpoint, inspect the pane's recent unwrapped output and repository
state once. If the agent is making coherent progress, leave it running and arm the next bounded
checkpoint. Interrupt or restart only for a real stop condition such as repeated no-progress,
malfunctioning tool loops, contradictory scope, an exceeded command-specific runtime budget, or
orphaned background work. Use one watcher per active handoff. This is a status-event fallback, not a
polling loop, and it does not make an unverified result a pass.

Implementation completion must notify the current review pane immediately before its final answer:

```sh
herdr pane run <review-pane-id> "Implementation pane <implementation-pane-id> finished <plan-path>. Strictly review the report and actual diff. Send findings to <implementation-pane-id>; notify <primary-pane-id> only if APPROVE."
```

The review pane must explicitly run `herdr pane run <implementation-pane-id> "...findings..."` before
ending a non-approved turn. A report left only in reviewer scrollback is not communicated. After a
fix notification, re-review the current diff. On approval, explicitly notify the designated
supervisor. The supervisor assigns validation, inspects its result, and notifies the primary only
when the slice is a final-approval candidate. Passive `agent_status` is not delivered into the active
conversation.

If validation is needed, the reviewer or primary assigns it with a bounded command list. Validation
reports its result to the named assigning pane with completed, failed, incomplete, and not-run
commands. Read recent unwrapped output and use only bounded waits for an expected completion
notification.

## Keep panes responsive

- Never keep reviewer or validation turns alive by polling, sleeping, or repeatedly calling
  `herdr wait` while another pane works. End the setup turn and remain idle until notified.
- Keep long PocketIC, workspace, and canbench work outside the primary pane, but enforce the
  repository's five-minute no-output and ten-minute turn budgets.
- Do not treat an agent-status transition as proof; read the final report and terminal result.
- Do not use `--test-threads=1` unless the user explicitly requests it.
- Keep supervisor turns checkpointed and bounded: write the plan or durable review queue before a
  long synthesis, then send concise stage prompts instead of accumulating several stages in one
  streamed response. Reaching a ten-minute supervisory checkpoint is not by itself a reason to
  interrupt a healthy turn; inspect progress and continue it when useful work is still advancing.
- On repeated `stream disconnected before completion`, `unexpected EOF`, or missing
  `response.completed`, inspect the provider/proxy log once and preserve the current on-disk
  checkpoint. Do not keep sending `continue` to the same conversation. Restart the pane with a
  user-approved alternative provider/model, then resume from the artifact and sibling reports. Do
  not silently consume a constrained premium model or downgrade an architecture/review role to a
  lightweight local model. Prefer shorter checkpointed turns on the designated capable cloud model
  when no equivalent direct provider is configured. Retry or idle-timeout tuning is only a secondary
  mitigation when the upstream repeatedly closes HTTP 200 streams.

For an opencode alternate-screen pane, require a unique `/private/tmp/` report before notification
because scrollback may omit the final response. If opencode emits empty commands, repeats a tool
schema error, or probes with unrelated `echo`/Python commands, interrupt it immediately. Preserve the
worktree and start a fresh session; do not let it diagnose the tool through repetition.


## Plan drafting and editing

The supervisor is the **plan author by default** for the active slice. The supervisor
drafts `.agents/plans/NNNN-*.md` directly, runs the plan validator, and only then
routes the plan to the review pane. Mechanical plan-file editing is not a reviewer's
or implementation pane's job; routing it there adds round-trips, fragments the
supervisor's decision ownership, and turns the implementation pane into a writer of
plans it does not own.

When the supervisor's primary tool path is unavailable (e.g., `apply_patch` is denied
or `exec_command` writes need user approval that the supervisor cannot self-approve),
the supervisor falls back to the following sequence under the `use_default` sandbox:

1. Write the plan body (or the new frontmatter) to a unique file under `/private/tmp/`
   via `cat > file << 'EOF' ... EOF`. The `/private/tmp/` write does not require
   escalated permissions in the workspace-write sandbox.
2. Move or append into the canonical plan path using shell primitives that the
   `use_default` sandbox allows (`cat file > target`, `cat file >> target`,
   `head -n K target > /tmp/trimmed` then `cat /tmp/trimmed > target`). Avoid
   `sed -i` and `perl -i` because BSD/macOS `sed` does not support the
   `'-i expression'` form and `perl` triggers an approval prompt on some hosts.
3. After every move/append, run the plan validator (`/Users/yota/.agents/skills/plan/scripts/validate_plan.py`)
   against the canonical path. Do not advance the chain on a validator failure even
   if the on-disk size looks right.

The supervisor may still delegate mechanical plan editing to a fresh
implementation/plan-editor pane when its provider is unreliable for the long-form
plan, as the existing `Role ownership / Supervisor` section already allows. The
delegation is the exception, not the default.

**Never** have the implementation pane draft the plan unless the supervisor
explicitly delegates the whole draft with a `PLAN ONLY` instruction. If the
implementation pane returns a plan draft unprompted, treat it as a routing
violation and reset/recreate the pane; do not adopt the draft as the canonical plan.

# herdr-workflow patch: Oracle approval gate for plan review iterations

## Oracle approval as a hard gate for plan-approval candidates

A plan that has cleared strict read-only plan review (w1:p9) is **not** yet a
final-approval candidate. Before the supervisor routes the plan to
implementation, the plan must also clear an independent oracle consult via the
`oracle` skill (GPT-5.5 browser mode by default). The oracle verdict is recorded
in `/private/tmp/oracle-<slug>-<YYYYMMDD>.md` and cited in the eventual commit
scope or final candidate.

Required oracle consults before plan -> implementation:

1. **post plan-review APPROVE** (mandatory). The oracle reads the plan file plus
   the latest review verdict and confirms that the plan text is faithful to the
   review findings. Verdict shape: `APPROVE` / `APPROVE with corrections` /
   `NOT APPROVE`. P1/P2/P3 corrections from the oracle are returned to the
   supervisor for plan revision; the supervisor writes the next revision and
   re-runs the review + oracle loop.
2. **post implementation-review APPROVE** (mandatory). The oracle cross-checks
   the actual diff against the approved plan and the implementation review
   verdict, and reports any drift (uncommitted files, unrelated modifications,
   missing items).
3. **post validation run** (recommended, mandatory for final-approval
   candidates). The oracle cross-checks the commit scope against the plan
   completion criteria and the validation output. The oracle verdict is the
   final gate before the supervisor (or primary, per the active delegation
   model) commits the slice.

If the oracle consult cannot start (sandbox `EPERM` on
`/Users/yota/.oracle/sessions/`, OpenAI API `insufficient_quota`, etc.), the
supervisor cites the most recent completed oracle session for the slice and
documents the failure in the slice's primary-findings-queue file. The oracle
verdict becomes advisory-only; the slice still requires w1:p9 approval +
bounded validation + primary inspection. The oracle failure is a forward-test
input for the next session: try a non-conflicting slug, switch to API mode, or
re-run from a shell with an unsandboxed home directory.

## Routing for oracle consults

- The supervisor (w1:pE) is the only pane that starts an oracle consult.
- The oracle result is a `verdict + findings + caveats` block. The supervisor
  uses this block to decide whether to advance the slice, revise the plan, or
  escalate to the primary.
- The supervisor **does not** echo the oracle transcript into the next
  notification verbatim; instead, the supervisor cites the recording
  (`/private/tmp/oracle-<slug>-<YYYYMMDD>.md`) and the verdict line. The
  receiving pane reads the recording if it needs the full text.

## Reusable rule

Plan-text review alone is not enough. The supervisor's chain is:

plan -> w1:p9 strict read-only review -> oracle consult -> w1:p3 implementation
   -> w1:p9 implementation review -> oracle consult (diff cross-check) ->
   bounded validation -> oracle consult (commit scope) -> commit.

If any oracle consult is missing, the slice is not a final-approval candidate
even if w1:p9 approved it.
## Why the supervisor owns plan drafting

- The supervisor is the single decision owner for scope, prerequisite, and
  validation gate. Plan text is the durable form of that decision; if the
  implementation pane drafts it, the decision ownership leaks.
- Reviewers and validators inspect the plan, not the implementation pane's
  scrollback. A plan drafted in scrollback is not reviewable.
- Plan-text drift across rounds (wrong predecessor commit hash, helper name
  drift, lock-owner tuple wording) is easier to catch when one author writes the
  whole file end-to-end with the validator and the on-disk worktree visible.
## Final approval and commit

1. Require reviewer approval and read its evidence.
2. Review every material gap discovered by implementation, review, or validation. Fix it, create an
   independently reviewable prerequisite slice, or record it in `design/implementation-gaps.md`
   before commit; do not accept terminal scrollback or a temporary report as durable tracking.
3. Have the primary inspect the actual final diff, active docs, benchmark artifacts, skipped checks,
   and validation status.
4. Run only lightweight integrity checks still needed for the commit.
5. Commit only after primary approval. Separate product and agent-workflow commits when that makes
   ownership clearer.
6. Do not amend or rewrite commits outside the authority granted by the user and repository policy.

During the delegation-proving period, the primary performs steps 3-5. After the user explicitly
grants mature-supervisor authority, the supervisor may perform final inspection and the safe commit
itself, and the primary becomes an on-demand escalation pane. Do not infer that authority from a
streak alone; the streak only makes the supervisor eligible for the user's decision.

## Reset after commit

Reset implementation, review, and validation conversations before assigning another plan. For Codex,
send `/new`; if completion UI remains open, send Enter and verify a fresh startup prompt. If reset is
unsupported, close and recreate the pane. Old scrollback may remain visible; the fresh prompt/session
is the required signal.

A startup banner alone is not proof that `/new` created a new conversation. Verify the new session
with `/status` or equivalent evidence: a new session id and near-zero token usage. If the prior
session id, continuation command, or accumulated token count remains, run `/exit` and launch a new
Codex process in the same pane (or recreate the pane), then verify again before dispatching work.

## Improve skills from pane behavior

Treat each sibling run as a forward-test of the skills it used.

- Distinguish task-specific mistakes from reusable process failures.
- For reusable failures—editing during review, validation scope creep, partial writes, weakened
  assertions, false completion claims, tool loops, or contradictory verdicts—update the smallest
  owning skill in the same iteration.
- Preserve tool-specific coordination rules in this skill; keep implementation, review, validation,
  architecture, and design rules tool-independent in their owning skills.
- Validate changed skills with `quick_validate.py`, then observe the next real pane run.
- Fold generalizable successful procedures back into the owning skill as well.
- Do not encode one-off file names, current findings, or expected answers into a skill.

Report material skill changes and whether the next forward-test passed, partially passed, or exposed
another gap.
