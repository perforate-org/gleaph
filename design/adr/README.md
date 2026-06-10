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
Last revised: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
```

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-labeled-segment-slide.md) | Labeled edge physical layer uses PMA leaf segment slide | accepted |
| [0002](0002-federated-row-batch-merge.md) | Federated row-batch merge on router (`rows_blob`) | accepted |
| [0003](0003-federated-aggregate-merge.md) | Federated aggregate merge and index fast path | accepted |
| [0004](0004-label-index.md) | Label index: sieve + telemetry; vertex export only when needed | accepted |

When adding an ADR, link it from the relevant design doc and update this table.
