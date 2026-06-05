# Rust Workflow

After meaningful code changes:

1. Run formatting.
2. Run tests.
3. Run relevant benchmarks.
4. If canbench results changed intentionally, update persisted results.
5. Summarize executed commands and unresolved failures.

Prefer targeted checks during development, then broader checks before completion.
