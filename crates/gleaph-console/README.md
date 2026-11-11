# gleaph-console

Gleaph's desktop console, built on [Rune](https://github.com/anomalyco/rune) through
`graph-view-rune`.

## Status: workspace member (2026-11-09)

Both crates are workspace members and the gate passes: `cargo metadata`,
`cargo test -p graph-view-core -p graph-view-rune -p gleaph-console` (242 + 11 + 14, 3 ignored),
`cargo fmt --all -- --check`, and
`cargo clippy -p graph-view-rune -p gleaph-console --all-targets -- -D warnings`.
`cargo run -p gleaph-console -- --snap out.png --at 1.0 --scale 2` renders headless.

The integration needed one repository-wide change: `cargo update`, because Rune's tree requires
newer patch versions of crates the lock had pinned (`hashbrown`, `bitflags`,
`unicode-segmentation`, and the cascading set behind them). See
`plans/rune-console-integration-2026-11-08.md` for the record.

## Where the data comes from

`schema.json` is the console's view of the graph schema: node labels, relationship types and result
columns (with units). `schema.rs` parses it; `graph-view-rune` receives the label vocabulary as
`LabelTables`, so the adapter knows no Gleaph label names. Results are a
`gleaph_result_table::ResultTable`; the rows are still a fixture until `GraphExplorerSession` is
wired to a running graph (that needs a canister endpoint and identity).

## Where the working prototype is

The buildable copy lives in the Rune checkout (`examples/graph-console`) because that is the only
place with Rune's path dependencies; `plans/rune-adapter/` holds its archive and snapshots. The
files here are the same sources restructured into the two crates this integration needs.
