---
name: code-quality
description: Keep implementations and reviews simple, cohesive, proportionate, and maintainable over time. Use when adding features, changing APIs, growing modules or functions, introducing helpers or abstractions, adding parameters or flags, refactoring, or reviewing diffs for accidental complexity, duplication, code bloat, unclear naming, excessive coupling, or high change amplification.
---

# Code Quality

Preserve long-term readability and changeability after correctness and architecture are satisfied.
Prefer the smallest design that expresses the domain invariant clearly; do not trade correctness for
brevity or create abstraction merely to reduce line count.

## Before implementation

Identify the smallest existing owner that can absorb the behavior. Search for an existing type,
helper, state machine, validation path, or domain vocabulary before creating another one. State:

- the single responsibility being changed;
- why the existing API is insufficient;
- which old code becomes obsolete;
- the expected net concepts, parameters, and branches added.

If a feature requires several unrelated responsibilities, split the slice before coding.

## Complexity controls

### Functions

- Keep one level of abstraction per function. Separate policy decisions from mechanical encoding,
  persistence, or transport.
- Prefer guard clauses and named decisions over deep nesting. Three nested control-flow levels are a
  review signal; go deeper only when the domain shape genuinely nests.
- Split a function when its name needs “and”, its locals serve unrelated phases, or a reader must
  remember distant mutable state. Do not split a linear operation into tiny forwarding helpers that
  obscure execution flow.
- Avoid boolean mode flags. Use separate operations or a meaningful enum when callers select
  different behavior.

### Parameters and APIs

- Zero to four meaningful parameters is normally easy to understand. Five or six requires an API
  shape review. Seven or more requires redesign or an explicit justification.
- Group parameters only when they form a stable domain concept with its own invariants. Do not hide
  an oversized signature in an unstructured `Options`, `Context`, or “bag of fields”.
- Pass canonical identifiers or domain values, not several parallel primitives that can disagree.
- Keep visibility minimal. Constructors establish invariants; public fields must not permit invalid
  states that write boundaries later trust.
- Avoid expanding a widely used signature when a narrower owner-specific method can carry the new
  behavior.

### Types, modules, and abstractions

- Introduce a type when it owns an invariant, eliminates invalid combinations, or provides a stable
  vocabulary. Do not introduce one only to rename a tuple used once.
- Prefer exhaustive enums for real variants; avoid parallel booleans and optional fields whose valid
  combinations require comments.
- A module should have one recognizable owner and reason to change. Split it when independent
  responsibilities repeatedly change separately, not merely because the file is long.
- Avoid generic frameworks for one current use case. Generalize after two concrete uses reveal the
  shared invariant.
- Delete superseded helpers, compatibility branches, imports, tests, comments, and docs in the same
  patch. New code should not leave two paths owning the same behavior.

## Duplication and size

Distinguish repeated code from repeated knowledge:

- Duplicate domain rules, bounds, schema definitions, and error decisions are blocking; centralize
  them at the owner.
- Small duplicated mechanical code can be clearer than a parameterized helper with many modes.
- Extract only when callers share semantics, not merely similar syntax.
- Before proposing extraction, enumerate the callers' behavioral differences. Reject a shared helper
  that needs optional payloads, boolean modes, callbacks, or erased diagnostics merely to unify two
  flows. Prefer waiting for a third concrete use when the common invariant is not yet stable.
- Review large positive diffs for dead scaffolding, repeated fixtures, copied match arms, and comments
  that restate code. Ask what can be removed without weakening the contract.
- Prefer a bounded feature diff. If implementation size is surprising relative to the behavior,
  explain the irreducible complexity or reslice it.

## Review procedure

Review the actual diff and report concrete maintenance consequences, not aesthetic preferences:

1. Trace the main execution path; flag indirection that makes it harder to follow.
2. List new/changed function signatures and count semantically meaningful parameters.
3. Find new flags, optional combinations, wildcard matches, clones, conversions, and duplicated
   validation.
4. Check whether invalid states can be constructed and whether ownership is obvious from names and
   visibility.
5. Search for old paths and concepts that should have been deleted.
6. Compare code growth with delivered behavior and tests. Identify accidental versus essential
   complexity.
7. Propose the smallest simplification that preserves invariants and performance.

Every finding must quote an inspected symbol/path and describe the current behavior accurately. Do
not infer an uninspected helper, stale variant, or caller from naming. Re-check the exact code before
reporting a P1/P2.

Calibrate common false positives:

- A staged constructor may intentionally validate logical invariants while the owning write boundary
  validates persistence limits. Flag it only when callers can mistake the stage, bypass the owner, or
  construct state that another API incorrectly trusts.
- Re-deriving cheap data from canonical state is often the correct SSOT tradeoff. Require hot-path,
  benchmark, or change-divergence evidence before calling recomputation a material defect.
- Named scenario helpers in lifecycle tests can make contract order explicit. Do not inline them
  solely to reduce lines; flag wrappers only when they hide state dependencies or add no semantic
  vocabulary.
- Variant-specific branches may preserve precise errors. Do not replace them with a broad category
  helper if that would weaken diagnostics or exhaustive handling.

Use severity based on consequence:

- `P1`: complexity permits invalid state, divergent ownership, unsafe partial behavior, or an API
  that cannot be used correctly.
- `P2`: material maintainability cost, excessive coupling/signature growth, duplicated rules, or
  avoidable framework/bloat likely to cause future defects.
- `P3`: localized naming, layout, minor indirection, or removable noise.

Do not block on arbitrary line counts alone. Cite the responsibility mix, state burden, caller cost,
or change amplification that makes the size harmful.

## Implementation handoff

Before review, state:

- abstractions/types introduced and why existing concepts were insufficient;
- signatures with five or more parameters and their justification or redesign;
- obsolete code removed;
- intentional duplication and why extraction would be worse;
- remaining complexity and the later condition that would justify generalization.
