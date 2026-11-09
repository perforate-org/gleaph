# LogHead 169 boundary decision (2026-11-09)

Anchor timestamp: 2026-11-09 13:50:00 UTC+0900
Scope: plan 0371, resolving the contract question ADR 0104 defers. Parent commit: `c2b0b9a1`.
No canister, persistence-format, Candid, or benchmark surface changes.

## Decision

`from_index` and `from_i32` accept `0..=169` (inclusive of `MAX_VALID_INDEX`) and reject `170..`.
`from_i32` keeps its existing all-negative → `NONE` mapping, now documented as such. `from_byte`
keeps its deliberate non-validation (record validation owns the domain at rehydrate).

## Why (the six sites already assumed 169 was valid)

| Site | Before |
| --- | --- |
| `log_head.rs` module doc | `0..170` valid |
| `MAX_VALID_INDEX` | `169`, documented "maximum valid entry index" |
| `slab_index.rs:17` | wire field `0..=169` valid |
| `try_encode_log_head` | independent `i < 170` bound (accepted 169) |
| `labeled/record.rs` rehydrate | rejects encoded heads `>= 170`; allows log length up to 170 |
| Lean `Lar/LogHead.lean` | `maxLogEntries := 170`, `idx < maxLogEntries`, round-trip theorems over `0..=169` |

Only the two constructors compared with `< MAX_VALID_INDEX`, and the unit test compared
`from_i32(169)` with `from_index(169)` (both `None`), so it could not notice. Change the constructor,
not the documentation or the Lean model: the intent is `0..=169`, and accepting 169 also removes a
latent panic path for a value record validation already admits.

## Changes

- `crates/ic-stable-lara/src/log_head.rs`
  - `from_index`: `index <= MAX_VALID_INDEX`; doc comment states `0..=169`.
  - `from_i32`: `head <= MAX_VALID_INDEX as i32`; doc comment states "any negative → `NONE`,
    `0..=169` valid, `>= 170` rejected".
  - `from_byte`: doc comment now states that it does not validate the domain.
  - Tests: the vacuous `round_trip_i32` is replaced by a loop over `0..=169` plus non-vacuous
    boundary tests (`169` accepted, `170`/255 rejected by both constructors, all negatives → `NONE`).
- `crates/ic-stable-lara/src/slab_index.rs`
  - `try_encode_log_head` uses `LogHead::MAX_VALID_INDEX` instead of the duplicated literal `170`;
    the wire-field bound now has one owner.
  - Four tests pin the encoder to the constructors (`169` encoded, `170` rejected, decode agrees
    over `0..=169`, `-1` sentinel round-trips).
- `crates/ic-stable-lara/formal/verus/` (mirror re-frozen)
  - `SCOPE.md`: new blob `cc59d575a6b8eefb7447d1e2699839f6013c7111f8534d5efe178bd30ff013d9`
    (136 lines; supersedes plan 0370's `f4c63cc4…`, 91 lines); the contract question is recorded as
    resolved.
  - Contracts: `from_index` `Some` iff `index <= 169`; `from_i32` `Some(NONE)` for negatives,
    `Some` for `0..=169`, `None` for `>= 170`; boundary tests and client domain flipped to `<= 169`.
- ADR 0104 migration note updated to record the resolution.

## Test transcript

```sh
cargo test -p ic-stable-lara log_head
# test result: ok. 7 passed; 0 failed; 0 ignored; 794 filtered out
#   none_is_max_u8, round_trip_i32, from_index_accepts_max_valid_index,
#   from_index_rejects_above_max, from_i32_accepts_max_valid_index,
#   from_i32_rejects_above_max, negative_i32_maps_to_none

cargo test -p ic-stable-lara overflow_log
# test result: ok. 11 passed; 0 failed; 0 ignored; 794 filtered out
#   4 new boundary tests + 7 pre-existing overflow-log integration tests

cargo check -p ic-stable-lara --tests          # pass
cargo fmt -p ic-stable-lara -- --check         # pass
git diff --check                               # pass
cargo metadata --no-deps ...                   # 56 packages (unchanged; fixture still excluded)
```

Verus mirror (re-frozen):

```sh
cd crates/ic-stable-lara/formal/verus
bash ../../../../scripts/verus-0.2026.10.18 cargo verus verify --features verus
# verification results:: 13 verified, 0 errors
cargo test --manifest-path Cargo.toml
# test result: ok. 4 passed
```

Mutant matrix (baseline + 12 targeted mutants, scratch copies under `verif/verus-mutants-169/`):
baseline verified; **12/12 rejected** on the intended clause — `from-index-rejects-169`,
`from-index-accepts-170`, `from-i32-negative-wrong`, `from-i32-accepts-170`, `to-i32-sentinel-wrong`,
`as-byte-wrong`, `is-none-wrong`, `index-wrong`, `from-byte-wrong`, and the three client mutants.

## Lean correspondence (no Lean edit)

The Lean model already states the decided contract; this slice makes Rust match it. The
correspondence is by name: `maxLogEntries = 170` with `idx < maxLogEntries`,
`logHeadFromI32_roundtrip`, `tryEncodeOverflowLogByte_none_iff` (fails exactly at `>= 170`), and
`decode_of_tryEncode`. No Lean build was run (model unchanged); the classification record remains
the owner of the transcription-freshness roadmap.

## Compatibility and risk

- Wire encoding is unchanged: `169` was always encodable by record validation and by
  `try_encode_log_head`; only the panicking/fallible constructor path rejected it.
- Persisted descriptors that were valid remain valid; no migration is required.
- `encode_overflow_log_byte` has no production callers; its `expect` can no longer fire for 169.
- Behaviour change is limited to previously rejected inputs `169` (now accepted) and the
  now-documented negative rule; `>= 170` stays fail-closed everywhere.
- No new trusted site, persistence layout, or Candid surface.

## Isolation

Changed paths are exactly: the two source files, the mirror (`src/lib.rs`, `SCOPE.md`), ADR 0104 and
this record. The pre-existing dirty worktree (including `crates/graph-view-core/**` and the Rune
console paths) is untouched; workspace-wide `cargo fmt --all -- --check` remains blocked by the
pre-existing untracked file recorded in `design/evidence/verus-intake-2026-11-09.md`, while
`cargo fmt -p ic-stable-lara -- --check` passes for this slice.
