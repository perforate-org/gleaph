import Lake
open Lake DSL

/-- Permanent Lean verification project for `ic-stable-linear-hash-map`.
See SCOPE.md for the verified contract and REPORT.md for findings. -/
package «lhm_formal»

@[default_target]
lean_lib «Lhm»
