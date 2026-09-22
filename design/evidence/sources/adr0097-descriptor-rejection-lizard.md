## Lizard complexity review

Mode: working tree (not an index-only review)
Baseline: 7735a59875e573cb3d45ee0a3e4ec786ba275a9d
Lizard: 1.24.0
Scope: crates/ic-stable-lara/src/labeled/bucket_store.rs
Commands: lizard --csv -i -1 <isolated baseline/current bucket_store.rs>
No project-specific settings found under scripts/ or .github/; no new threshold or exclusions.

### Summary

- One source file; two directly touched functions.
- Maximum touched-function CCN: 24 -> 24.
- Aggregate touched-function CCN: 24 -> 27 (new boundary test: 3).
- Assessment: focused. Production control flow/signature are unchanged; only write ordering moves.

### Material changes

| Function | CCN | NLOC | Tokens | Parameters | Assessment |
| --- | ---: | ---: | ---: | ---: | --- |
| insert_label_bucket_at | 24 -> 24 | 70 -> 70 | 511 -> 511 | 5 -> 5 | Existing metadata preflight precedes write |
| in_place_descriptor_count_rejection_keeps_backed_rows_unchanged | new 3 | new 67 | new 521 | 0 | Real bound, three positions, full-byte oracle |

### Decisions

Keep the existing fallible preparation before canonical writes and the owner-level rejection test.
The segment-rewrite arm already checks the same LabeledVertex-owned count rule before rewrite.
No duplicate guard, API, cache, persisted state or abstraction is introduced. Separate pinned
performance evidence from this correctness change; no compactor optimization is in the patch.

### Validation

Final library: 659 passed. Library clippy -D warnings and scoped rustfmt pass.
The benchmark comparison uses pinned 3ea7dc739/5ad7f5a61, not the descriptor correction.
Metrics do not measure performance or constitute independent review/landing approval.
