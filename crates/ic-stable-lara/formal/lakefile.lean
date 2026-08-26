import Lake
open Lake DSL

/-- Permanent Lean verification project for `ic-stable-lara` (Labeled LARA,
Stage 1: record/slot arithmetic). See SCOPE.md for the verified contract and
REPORT.md for findings. -/
package «lara_formal»

@[default_target]
lean_lib «Lar»
