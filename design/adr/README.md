# Architecture Decision Records (ADR)

## Purpose

Capture **significant, hard-to-reverse** decisions with context and consequences. Design docs explain steady-state; ADRs explain *why* we chose it.

## When to write an ADR

- Federation placement authority on router
- Plan blob as execution IR (vs re-parse GQL on graph)
- `PlanRow` dense layout + indexed merge
- Prepared query security model
- Breaking wire format changes

Skip ADRs for routine features, bug fixes, or choices already obvious from code.

## Format

Use numbered files: `NNNN-short-title.md`

```markdown
# NNNN. Title

Date: YYYY-MM-DD
Status: proposed | accepted | deprecated | superseded by NNNN

## Context
## Decision
## Consequences
## Alternatives considered
```

## Index

| ADR | Title | Status |
|-----|-------|--------|
| — | *(none yet)* | — |

When adding an ADR, link it from the relevant design doc and update this table.
