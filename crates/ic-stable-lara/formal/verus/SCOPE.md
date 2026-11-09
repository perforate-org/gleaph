# Frozen mirror scope: `ic-stable-lara::log_head`

This directory is a **workspace-independent Verus fixture** for the intake harness (plan 0370,
ADR 0104). It is not a production crate, is excluded from the repository workspace by the empty
`[workspace]` table in `Cargo.toml`, and is never a dependency of any production crate.

## Frozen input

- Re-frozen by plan 0371 from parent commit `c2b0b9a1`; the slice commit is identified in
  `design/evidence/log-head-169-boundary-2026-11-09.md`.
- Source: `crates/ic-stable-lara/src/log_head.rs`, 136 lines.
- Blob SHA-256: `cc59d575a6b8eefb7447d1e2699839f6013c7111f8534d5efe178bd30ff013d9`
  (supersedes the plan-0370 freeze `f4c63cc4…`, 91 lines).
- The mirror is a transcription of that blob. Production code is untouched by this harness.

## Resolved contract question (plan 0371)

The implementation previously rejected index 169 because both constructors compared with
`< MAX_VALID_INDEX`, contradicting the documentation, `MAX_VALID_INDEX`, the Lean model, the
`0..=169` comment in `slab_index.rs`, `try_encode_log_head`'s `i < 170`, and record validation.
Plan 0371 decided the inclusive boundary and fixed the implementation:

- `from_index`: accepts `0..=169`; rejects `170..=u8::MAX`.
- `from_i32`: accepts **every negative `i32`** (mapping all of them to `NONE`, documented), accepts
  `0..=169`, rejects everything `>= 170`.
- `from_byte`: still accepts every wire byte without domain validation (`255` maps to `NONE`;
  `169..=254` become heads whose `index()` returns `Some`); record validation owns the domain.
- Regression tests pin both boundaries (`169` accepted, `170` rejected) and the all-negative rule.
- The Lean model already states this contract (`maxLogEntries = 170`, `idx < maxLogEntries`).

The contracts in `src/lib.rs` state this resolved domain.

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
