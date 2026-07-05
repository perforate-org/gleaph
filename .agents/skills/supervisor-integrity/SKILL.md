---
name: supervisor-integrity
description: Supervise Gleaph work from repository inspection and bounded planning through delegated implementation, independent review, validation, final approval, and safe commits. Use alongside herdr-workflow, not instead of it, when an agent owns or shadows the primary/coordinator role, prepares plans or ADRs, decides whether a change is ready to land, recovers an interrupted supervisory session, or improves cross-stage supervisory quality from observed failures and successful runs.
---

# Supervisor Integrity

Protect the repository-level decision loop. A supervisor does not merely relay pane reports: it
establishes scope, preserves ownership boundaries, verifies evidence, decides readiness, and lands
only the intended change.

Use this with `herdr-workflow` for pane mechanics and with the relevant architecture, design, test,
benchmark, validation, or document-date skills for domain judgment.

## 1. Establish current truth

Before planning or resuming work:

1. Read `AGENTS.md`, `.agents/skills/INDEX.md`, `.agents/skills/herdr-workflow/SKILL.md`, and the
   relevant active contracts. Read the `plan` skill before creating, numbering, or auditing plans.
2. Inspect `git status --short`, current `HEAD`, recent commits, ignored plan files, and live pane ids.
3. Compare repository state with the last known report. Treat an unexpected `HEAD` move, new commit,
   or unrelated worktree diff as a stop condition until its owner and scope are understood.
4. Read sibling pane output before reassigning work. Reset a completed conversation before reuse.
5. Distinguish confirmed repository state from conversation-only proposals.

Preserve skill paths exactly as assigned. Gleaph project skills live under
`.agents/skills/<name>/SKILL.md`; global coordination skills such as `plan` and `herdr` may live under
`/Users/yota/.agents/skills/<name>/SKILL.md`. Do not silently substitute the repository skill root for
an absolute global path. If a skill path fails, perform one bounded filesystem discovery, use the
discovered path once, and stop with the exact error if it still cannot be read.

Never infer that a plan, ADR, validation run, or commit exists merely because another pane discussed
or claimed it. Verify the artifact.

Enforce the native-first, reviewable `apply_patch` workflow and its bounded shell fallback from
`herdr-workflow`. Do not allow supervised panes to switch from a failed patch path to unstructured
writers or temporary replacement files. Treat two failed patch attempts as a routing decision, not
an invitation to probe more editing mechanisms.

`.agents/plans/` is intentionally ignored process state in this repository. Inspect it for the active
handoff and numbering, but do not treat its ignored/untracked status as repository damage or a reason
to block unrelated reviewed work. Duplicate ordinals and stray artifacts are hygiene findings; they
become blocking only when they make the active plan ambiguous or would misdirect an implementation.
If a protected or unwritable plan cannot be updated, report that fact and the real on-disk status;
never claim its TODOs changed.

## 2. Plan one reviewable boundary

Use the `plan` skill. A plan must name:

- the canonical state and its owner;
- the enforcing write/API boundary;
- dependency and execution direction;
- derived-state convergence or rollback semantics;
- observable completion criteria;
- exact focused validation and explicit non-goals.

Prefer one coherent boundary over a roadmap-sized implementation. Split work when it combines
independent architecture decisions, ownership domains, or validation loops.

### Keep numbering domains separate

- Determine the next plan ordinal from `.agents/plans/`.
- Resolve duplicate candidate ordinals before creating a new plan, then use the zero-padded
  frontmatter/template required by the `plan` skill.
- Determine the next ADR number independently from `design/adr/README.md` and existing ADR files.
- Never derive an ADR number from a plan number.
- Update the ADR index when adding an ADR.
- A conversation proposal is not an ADR. Write the file, status, date, rationale, consequences, and
  links before reporting that it exists.

### ADR gate

Use `adr-review` for hard-to-reverse decisions involving persistence, topology, canister boundaries,
authorization, public APIs, consistency, or operational control. Keep unresolved alternatives out of
an accepted decision. Mark them proposed or planned and identify the evidence still required.

Use `document-date-accuracy` whenever an ADR, plan, gap ledger, or active design document contains
dates, timestamps, relative-time wording, or current/latest claims.

Use concrete terms—data owner, invariant enforcer, API surface, source of truth, controller,
dependency direction, state transition—not vague umbrella words.

## 3. Keep one cross-stage decision owner

Use `herdr-workflow` for pane roles, prompts, notifications, validation allowlists, resets, and tool
mechanics. This skill only adds the supervisor's cross-stage decisions: scope, prerequisite handling,
final evidence inspection, approval, and commit authority.

When the primary designates this pane as the active slice supervisor, drive the chain continuously:
write the plan, obtain independent plan approval, assign implementation, route review fixes, assign
bounded validation, and inspect the resulting evidence and diff. Notify the primary only for a true
architecture/scope blocker or a final-approval candidate. The supervisor does not commit; primary
final inspection and commit authority remain separate.

The supervisor also does not repair product code or tests directly, including after an implementation
pane fails, truncates a prompt, or exits early. Preserve and inspect partial state, then reset/recreate
an implementation pane and reassign a concise durable queue. If no safe implementation pane is
available, escalate to the primary instead of switching to ad hoc `cat`, `head`/`tail`, `sed`, or
Python writers. Supervisory recovery must not create a second unreviewed implementation path.

Declare which pane is the active supervisor for the slice. Two supervisors must not independently
create plans, approve, or commit against the same worktree. A non-primary supervisor must notify the
designated primary before ending a supervisory turn, using a direct `herdr pane run <primary> "..."`
message as required by `herdr-workflow`, and must not wrap that message in `printf`, `echo`, command
substitution, or another shell command.

## 4. Treat reports as leads, not evidence

Reviewer approval is necessary but not sufficient. Before final approval, independently inspect:

1. plan completion criteria;
2. actual base-to-final diff and every newly committed change since the slice began;
3. public/API and persistence boundaries;
4. tests against plausible wrong implementations;
5. active design status, gap ledger, dates, and benchmark artifacts;
6. validation transcript and skipped checks;
7. worktree scope, untracked files, ignored plans, and background work.
8. ignored plan frontmatter statuses versus body checklists and the final report; all three must
   describe the same completion state.

Re-open exact corrected lines after every blocking review cycle. A regression test is unresolved if
restoring the original defect would still pass its assertions.

Do not approve when:

- any required P1/P2 correction remains;
- runtime evidence is only `--no-run`, interrupted, delegated without a terminal result, or timed out;
- canonical mutation can occur before all fallible validation;
- a derived-state failure can silently lose work or be reported as all-or-nothing failure;
- docs claim implemented behavior that remains planned;
- unexpected commits or unrelated files have not been reviewed.

## 5. Commit as a controlled state transition

Commit only after final review and validation evidence satisfy the plan **and** the user or designated
primary explicitly authorizes this supervisor to commit. Approval to inspect or coordinate is not
commit authority.

1. Re-check `git status --short` and `git diff --check`.
2. Stage exact intended paths; inspect the staged file list and staged diff summary.
3. Separate product changes from agent-workflow/skill changes when ownership is clearer that way.
4. Commit only through:
   `/bin/sh /Users/yota/.local/bin/codex-git-commit commit -m MESSAGE`.
5. Amend only a commit created in the same authorized session, with explicit user/primary approval,
   through `/bin/sh /Users/yota/.local/bin/codex-git-commit amend -m MESSAGE`.
   Never invoke `git commit`, `git rebase`, `git reset`, or `git replace` directly.
6. Verify the resulting commit id, subject, file list, and clean worktree.
7. Follow `herdr-workflow` to reset delegated conversations before the next slice.

Never let a sibling implementation/review/validation pane commit unless the user explicitly changes
the ownership model for that slice.

## 6. Stop conditions

Stop and notify the primary/user rather than improvising when:

- `HEAD` changes unexpectedly or another supervisor commits concurrently;
- a prerequisite architecture decision is unresolved;
- the proposed fix materially expands the plan;
- the same tool/error failure occurs twice;
- an agent emits empty commands, unrelated probes, contradictory claims, or loses context;
- validation exceeds repository time budgets;
- external/current facts are being treated as settled without authoritative verification.

Preserve useful context before resetting a malfunctioning pane. Do not allow it to diagnose tool
failures through repeated empty or speculative commands.
For a repeated local path/tool error, stop before a third attempt. Do not browse the web for a local
path typo or keep trying equivalent shell/Python reads after the correct path was already discovered.

## 7. Continuous supervisory improvement

Treat every slice as a forward-test of planning, coordination, approval, and commit discipline.

After completion:

1. Compare the plan, pane behavior, findings, validation, final diff, and commit outcome.
2. Classify each issue as task-specific or reusable.
3. Update the smallest owning skill for reusable failures or successful procedures; do not pile all
   guidance into this skill.
4. Keep tool coordination in `herdr-workflow`, implementation defects in
   `implementation-integrity`, assertion defects in `adversarial-test-review`, validation behavior in
   `cost-aware-validation`, and only cross-stage supervisory decisions here.
5. Validate changed skills with `quick_validate.py` and forward-test them on the next real slice.
6. Report whether the forward-test passed, partially passed, or exposed another gap.

Do not encode one-off file names, current expected findings, or a single incident's answer into a
skill. Improve general decision rules.

## Completion report

Report concisely:

- selected boundary and why it was sized that way;
- plans/ADRs created or corrected;
- implementation, review, and validation evidence actually observed;
- primary final-review findings and their disposition;
- commits created and worktree state;
- pane reset status;
- material skipped checks or unresolved risks;
- supervisory skill changes and forward-test status.
