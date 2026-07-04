---
name: herdr-workflow
description: Coordinate Gleaph plans, implementation, iterative review, validation, final approval, commits, pane resets, and skill forward-testing across sibling herdr panes. Use whenever HERDR_ENV=1 and work is delegated among primary, implementation, review, or validation panes. This skill owns repository-specific collaboration policy; use the global herdr skill separately for CLI mechanics.
---

# Herdr Workflow

Keep the primary pane focused on planning, user interaction, and final approval. Delegate edits,
iterative review, and bounded validation to three sibling panes. Use the global `herdr` skill for
command syntax; do not duplicate its socket or pane-management reference here.

## Start a slice

1. Re-read pane ids with `herdr pane list`; ids may compact.
2. Verify implementation, review, and validation panes show fresh startup prompts. Reset a completed
   conversation before reuse.
3. Have the primary inspect the repository, choose one bounded slice, and create the plan.
4. Prime all three sibling conversations before implementation starts. Give the reviewer the plan
   path, implementation/primary pane ids, strict read-only skills, and exact finding/approval routes;
   give validation its role and tell it to remain idle until assigned an allowlist. Each setup turn
   must acknowledge readiness and end without polling.
5. Assign explicit pane ids and roles in every prompt.
6. Name every required repository skill by its exact path:
   `.agents/skills/<skill-name>/SKILL.md`. Instruct the pane to read it directly. Do not rely on
   `$CODEX_HOME` or global discovery for Gleaph-specific skills.

## Role ownership

### Primary pane

- Own architecture direction, plan scope, user communication, final approval, and commits.
- Do not duplicate implementation or routine fix iterations.
- Inspect the final diff and evidence independently before approval.
- Use the required safe commit helper; sibling panes do not commit.

### Implementation pane

- Read the plan, `AGENTS.md`, `implementation-integrity`, `code-quality`, and relevant domain skills.
- Own code, focused tests, design synchronization, and benchmark changes.
- Preserve unrelated worktree changes and never rewrite history.
- Keep one conversation for the entire slice, including review fixes.
- Notify the review pane only after edits, focused validation, and an honest report are complete.
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
- Send concrete findings to the implementation pane and re-review fixes.
- Do not notify the primary while actionable P1/P2 findings or required corrections remain.
- Notify the primary only after a consistent `APPROVE` verdict.

### Validation pane

- Start only after explicit assignment; do not wait or poll for future work.
- Use `cost-aware-validation` and an exact ordered command allowlist.
- Never edit or repair the worktree.
- Report actual terminal completion; background, `--no-run`, interrupted, and timed-out commands are
  not runtime passes.

## Notification chain

Every notification must be self-contained: name the slice/plan, sender and recipient pane ids, the
next action, and where the recipient must send findings or approval. Do not expect a fresh pane to
infer its role from "finished" alone.

Implementation completion must notify the current review pane immediately before its final answer:

```sh
herdr pane run <review-pane-id> "Implementation pane <implementation-pane-id> finished <plan-path>. Strictly review the report and actual diff. Send findings to <implementation-pane-id>; notify <primary-pane-id> only if APPROVE."
```

The review pane must explicitly run `herdr pane run <implementation-pane-id> "...findings..."` before
ending a non-approved turn. A report left only in reviewer scrollback is not communicated. After a
fix notification, re-review the current diff. On approval, explicitly notify the primary instead;
passive `agent_status` is not delivered into the active conversation.

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

For an opencode alternate-screen pane, require a unique `/private/tmp/` report before notification
because scrollback may omit the final response. If opencode emits empty commands, repeats a tool
schema error, or probes with unrelated `echo`/Python commands, interrupt it immediately. Preserve the
worktree and start a fresh session; do not let it diagnose the tool through repetition.

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

## Reset after commit

Reset implementation, review, and validation conversations before assigning another plan. For Codex,
send `/new`; if completion UI remains open, send Enter and verify a fresh startup prompt. If reset is
unsupported, close and recreate the pane. Old scrollback may remain visible; the fresh prompt/session
is the required signal.

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
