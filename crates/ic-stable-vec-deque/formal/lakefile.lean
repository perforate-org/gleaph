import Lake
open Lake DSL

/-- Permanent Lean verification project for `ic-stable-vec-deque`.
See SCOPE.md for the verified contract and REPORT.md for findings. -/
package «svd_formal»

@[default_target]
lean_lib «Svd»
