# Lean fixture freshness gate (2026-11-09)

Anchor timestamp: 2026-11-09 19:05:00 UTC +0900
Scope: plan 0374, implementing the freshness gate proposed in
`design/investigations/2026-11-09-lean-fixture-migration-classification.md` and answering its open
question (blocking now or advisory until re-anchoring). Parent commit: `ee891ee3`. No Lean model,
proof, build gate, production code or stable layout changed.

## Decision

Per-fixture source anchors with three statuses:

| status | meaning | checker |
| --- | --- | --- |
| `anchored` | the model was re-anchored to the manifest revision | changed/missing source is **blocking** (exit 1) |
| `pending-reanchor` | the known anchor and drift are recorded; the model is not yet re-anchored | changed/missing source is printed as an **advisory** (exit 0) |
| `obsolete` | the fixture models deleted code | reported and skipped |

The investigation's open question is answered: **advisory until each fixture is re-anchored, blocking
afterwards per fixture**. Flipping a manifest's status is the only change needed to activate blocking.

Initial assignment and measured drift:

| fixture | anchor | transcribed paths (anchor hash / HEAD) | status |
| --- | --- | --- | --- |
| `ic-stable-lara` | `dbd76f3d3441e804336d901798cf21b9d86ac563` | `src/slab_index.rs` `ddfd8168…` **changed**; `src/labeled/bucket_label_key.rs` `5f352ac2…` unchanged | `pending-reanchor` |
| `ic-stable-linear-hash-map` | `0da342d62b2a3c3b293fa7ff5ed21b9f577dd23d` | `src/control.rs` `74eeb24c…` **changed**; `src/map.rs` `e9149c6e…` **changed**; `src/header.rs` `b38b2449…` unchanged; `src/memory.rs` `7881cccc…` unchanged | `pending-reanchor` |
| `ic-stable-vec-deque` | `d80cc4c603e2dd50bd26ee77e2679b13f70f6dc0` | none (ADR 0086 deleted the modeled contiguous ring) | `obsolete` |

The drift is exactly the one the classification record measured; the gate now makes it visible on
every formal/source change instead of relying on a reader noticing a stale `SCOPE.md`.

## Mechanism

- `crates/<crate>/formal/SOURCES.sha256` header: `# schema: 1`, `# status:`,
  `# anchor:`, `# reviewed:`; entries are `<sha256>  <repo-relative path>`.
- `scripts/check-lean-sources.py`:
  - default discovery (`crates/*/formal/SOURCES.sha256`), hashing `git show HEAD:<path>`;
  - groups output into blocking / advisory / obsolete and exits 1 iff a `blocking` fixture (or a
    manifest error) is present;
  - `--manifest PATH` (repeatable) checks a single manifest (used for the negative control and
    obsolete check);
  - `--strict` treats `pending-reanchor` drift as blocking (fixture for the future flip);
  - `--update` rewrites the hashes at `HEAD` without touching `status`/`anchor`/`reviewed` (the
    explicit re-anchor step);
  - fails closed when no manifest is found.
- `.github/workflows/lean-freshness.yml` runs only `actions/checkout` + `python3
  scripts/check-lean-sources.py` on changes to `crates/*/formal/**`, the three transcribed crate
  `src` trees, the checker or the workflow. No elan/lake setup. `lean-verify.yml` (lhm
  `lake build` + axiom discipline) is unchanged and remains the heavy gate.

## Validation transcript

| Check | Result |
| --- | --- |
| `python3 scripts/check-lean-sources.py` | exit 0; advices `slab_index.rs`, `control.rs`, `map.rs`; vec-deque skipped; summary `0 blocking, 2 advisory/anchored-clean, 1 obsolete` |
| `--manifest crates/ic-stable-vec-deque/formal/SOURCES.sha256` | exit 0, obsolete note only |
| Negative control: temp `anchored` manifest with an all-zero hash for `slab_index.rs` | **exit 1**, `ERROR changed: crates/ic-stable-lara/src/slab_index.rs` |
| `--strict` | exit 1 (both pending fixtures become blocking), as designed |
| `--update` on a temp pending copy of the lara manifest | rewrites 2 hashes at HEAD; re-check of that temp manifest is clean (exit 0) |
| `python3 -m py_compile scripts/check-lean-sources.py` | pass |
| `ruby -ryaml -e 'YAML.load_file(...)'` on the new workflow | pass |
| `git diff --check` | pass |
| Changed-path manifest | checker, workflow, three `SOURCES.sha256`, three `SCOPE.md`, ADR 0104, this record |

No Lean build, PocketIC or canbench run applies: the gate hashes committed sources and changes no
proof, production code or stable layout.

## Documentation updates

- The three `formal/SCOPE.md` files lost their stale "No CI wiring yet (repository has no
  `.github/workflows/`)" claim and now state the gate, manifest path, status and next action
  (lara/lhm re-anchor; vec-deque restatement).
- ADR 0104 records the status policy and the advisory-to-blocking decision.

## Remaining work (later slices)

1. Re-anchor the surviving lara models (`Basic`, `SlotIndex`, `BucketLabelKey`) to current `HEAD`,
   then flip its manifest to `anchored` (blocking).
2. Re-anchor lhm `control.rs`/`map.rs` (the P3–P5 question in the classification record), then flip
   its manifest to `anchored`.
3. Restate the vec-deque model for the ADR 0086 segmented ring and give it a fresh manifest.
4. Retire the lara fixture when its Verus-migration criteria are met (ADR 0104), deleting its
   manifest with it.
