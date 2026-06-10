---
name: document-date-accuracy
description: Use when creating, editing, or reviewing documents that contain dates, relative time, timelines, deadlines, schedules, release dates, changelogs, design docs, reports, plans, or words such as today, yesterday, tomorrow, recent, latest, current, now, as of, last, next, soon. Ensures date-sensitive claims are anchored to the OS current date, converted to exact calendar dates, and verified when they may be stale.
---

# Document Date Accuracy

## Goal

Make dates in documents exact, current, and honest.

Use this skill before writing or reviewing any document that includes calendar dates,
relative time, timelines, release notes, milestones, deadlines, schedules, or
current-state claims.

## Anchor Date

Get the current date from the OS or platform runtime. Do not rely on model memory.

Preferred commands:

macOS / Linux / Unix shell:

    date +"%Y-%m-%d %H:%M:%S %Z %z"

Windows PowerShell:

    Get-Date -Format "yyyy-MM-dd HH:mm:ss zzz"

If neither command is available, use the current date provided by the execution
environment and state the timezone explicitly.

Use the command appropriate for the active shell:

- POSIX shell, zsh, bash: `date +"%Y-%m-%d %H:%M:%S %Z %z"`
- PowerShell: `Get-Date -Format "yyyy-MM-dd HH:mm:ss zzz"`

## Date-Sensitive Terms

Search for and review terms such as:

- today
- tomorrow
- yesterday
- this week
- next week
- last week
- recently
- latest
- current
- now
- as of
- soon
- upcoming
- previous
- last
- next
- deadline
- release
- milestone
- schedule

Replace ambiguous relative wording with exact dates when possible.

Good:

- `today, June 10, 2026`
- `as of June 10, 2026`
- `planned for 2026-07-01`

Avoid:

- `today`
- `recently`
- `latest`
- `soon`
- `next month`

## Workflow

1. Get the OS anchor date with the `date` command.

2. Identify all date-sensitive claims in the document.

3. Classify each claim:
   - Stable historical fact
   - Current-state claim
   - Future plan or deadline
   - Inferred date
   - Relative date based on the anchor date

4. Convert relative dates to absolute dates.
   - `today` becomes the OS anchor date.
   - `tomorrow`, `yesterday`, `next week`, and similar phrases must be resolved
     against the OS anchor date.

5. Verify unstable claims.
   - For repository/project documents, prefer git tags, commits, changelogs,
     issues, PR metadata, source files, or design docs.
   - For external facts, use authoritative primary sources when possible.
   - If a claim could have changed recently, verify it before writing it as fact.

6. Mark uncertainty clearly.
   - Use `planned`, `tentative`, `expected`, `last verified on`, or `inferred from`
     when the date is not confirmed.
   - Do not present planned behavior as already shipped.
   - Do not present stale or unverified current-state claims as current.

7. Run a final date pass.
   - Re-scan the document for date-sensitive terms.
   - Confirm all relative dates are anchored.
   - Confirm all current-state claims have either been verified or marked with
     an `as of` date.
   - Confirm date formats are consistent.

## Output Requirements

When completing the document task, report:

- OS anchor date used
- Date-sensitive claims changed or verified
- Dates left uncertain, tentative, or source-limited
- Sources checked, when verification was needed
