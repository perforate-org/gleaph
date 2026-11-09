# Frozen mirror scope: `ic-stable-lara::log_head`

This directory is a **workspace-independent Verus fixture** for the intake harness (plan 0370,
ADR 0104). It is not a production crate, is excluded from the repository workspace by the empty
`[workspace]` table in `Cargo.toml`, and is never a dependency of any production crate.

## Frozen input

- Repository HEAD at freeze: `5ed8753f347148356b81e24db603cdd2ccd57d5b` (the plan was authored against
  `1159e77`; the blob is unchanged, so the digest below is the identity that matters).
- Source: `crates/ic-stable-lara/src/log_head.rs`, 91 lines.
- Blob SHA-256: `f4c63cc44bddeaca891944bf800bae40774315a2c0a03fb4335b9885d8279103`
  (`git show HEAD:crates/ic-stable-lara/src/log_head.rs | shasum -a 256`).
- The mirror is a transcription of that blob. Production code is untouched by this harness.

## Recorded contract question (not fixed here)

The module documents `0..170` valid heads plus `NONE`, and defines
`MAX_VALID_INDEX: u8 = 169` as the "maximum valid entry index", but the implementation rejects
169 in both constructors because both compare with `< MAX_VALID_INDEX`:

- `from_index`: accepts `0..=168`; rejects `169..=u8::MAX`.
- `from_i32`: accepts **every negative `i32`** (mapping all of them to `NONE`, not only `-1`),
  accepts `0..=168`, rejects everything `>= 169`.
- `from_byte`: accepts every wire byte without domain validation (`255` maps to `NONE`; `169..=254`
  become heads whose `index()` returns `Some`).
- The unit test `round_trip_i32` compares `from_i32(169)` with `from_index(169)`; both are `None`,
  so the equality is vacuous and the boundary is untested.
- The Lean model (per `design/investigations/2026-11-09-lean-fixture-migration-classification.md`)
  transcribes the older `< 170` comparison.
- Call sites: `crates/ic-stable-lara/src/slab_index.rs:209` feeds `from_i32(...).expect(...)` and
  `slab_index.rs:236` decodes through `from_byte`; `try_encode_log_head` separately checks `i < 170`.

The contracts in `src/lib.rs` state the **actual** implementation domain. The mismatch is recorded
for a separate owner-reviewed slice; this harness does not decide it.

## Shape relaxations (transcription is otherwise faithful)

1. **Free functions instead of associated functions.** This Verus release (0.2026.10.18) cannot
   attach `#[verus_spec]` to an associated function whose return type is a datatype: the generated
   module-scope spec proxy fails name resolution (`E0425: cannot find function ...`). The mirror
   exposes `from_index`/`from_i32`/`to_i32`/`as_byte`/`is_none`/`index`/`from_byte` as module-level
   functions with identical bodies and contracts.
2. **Module constants inside `verus!`.** `NONE` and `MAX_VALID_INDEX` are module constants declared
   in the `verus!` block (plain build: plain duplicates), because associated constants of a
   `verus!`-declared type are not supported in verified code in this release.
3. **`pub` field.** The field is `pub u8` rather than private so the contracts can read it; the
   frozen module keeps it private and exposes accessors.
4. **`const fn` dropped.** The frozen constructors are `const fn`; the mirror uses `fn` because
   attribute-style contracts are not applied to `const fn` in this release.
5. **Build modes.** `--features verus` selects the real `vstd` macros and the `verus!` type
   declaration; the default build uses the local no-op `verus-spec-stub` macros and a plain type
   declaration, so `cargo check` type-checks the executable bodies with specifications erased.

## Build and verify

```sh
cd crates/ic-stable-lara/formal/verus
bash ../../../../scripts/verus-0.2026.10.18 cargo verus verify --features verus   # proof run
cargo check --manifest-path Cargo.toml                                            # plain build
```
