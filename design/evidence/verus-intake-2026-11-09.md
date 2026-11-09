# Verus intake harness — evidence record (2026-11-09)

Anchor timestamp: 2026-11-09 12:30:00 UTC+0900
Scope: plan 0370, ADR 0104. Proves one frozen real module with a pinned Verus release, at zero cost
to the production workspace. Production adoption, CI wiring, and the 169 contract decision are out
of scope.

## Re-baseline against the plan

| Plan fact (at authoring) | At execution | Consequence |
| --- | --- | --- |
| `HEAD = 1159e777` | `HEAD = 5ed8753f347148356b81e24db603cdd2ccd57d5b` | re-frozen at execution HEAD |
| mirror blob `f4c63cc4…`, 91 lines | identical (`git show HEAD:… \| shasum -a 256`) | frozen digest holds |
| 52 workspace packages | 56 | member-count criterion re-baselined to "56 before and after; no manifest/lock change by this slice" |
| clean worktree except listed paths | 104 pre-existing dirty entries, including untracked `crates/graph-view-core/src/label_layout.rs` | workspace-wide `cargo fmt --all -- --check` is currently blocked by that pre-existing file (see Failures); all slice-owned files pass their own formatting check |

## Input freeze

- Source: `$REPO/crates/ic-stable-lara/src/log_head.rs`, 91 lines, blob SHA-256
  `f4c63cc44bddeaca891944bf800bae40774315a2c0a03fb4335b9885d8279103`.
- Mirror: `$REPO/crates/ic-stable-lara/formal/verus/src/lib.rs` (196 lines), workspace-independent
  (empty `[workspace]`), never a production dependency.
- Contract question recorded in `formal/verus/SCOPE.md`: documented `{NONE} ∪ [0,170)` versus the
  implementation's `< MAX_VALID_INDEX` comparison; `from_index`/`from_i32` reject 169; `from_i32`
  maps **every** negative to `NONE`; `from_byte` does not validate the domain (169..=254 become
  heads whose `index()` returns `Some`); the unit test is vacuous; the Lean transcription keeps
  `< 170`. Production behaviour is mirrored as-is; no fix is made here.

## Provisioning

- Release `0.2026.10.18.7d82994`; asset `verus-0.2026.10.18.7d82994-arm64-macos.zip`,
  451,327,093 bytes, SHA-256 `3fc56c58277b5f0999368f7b3a2b07ba4eb9ec2965454ce7a9f2769c0464d32b`.
- `scripts/setup-verus-stack.sh` is idempotent and prints
  `pinned Verus stack already provisioned: $REPO/.tools/verus-0.2026.10.18.7d82994` when present.
  This run **seeded** `.tools/` from the independent project's already-provisioned stack (same
  release, same digest) instead of downloading 451 MB; the cold path is the same script and was
  exercised end-to-end in `perforate-org/verus-ic` (`docs/evidence/2026-11-09-stack-provisioning.md`).
- Stack size on disk: 1.7 GB. `.tools/` is git-ignored.
- Launcher `scripts/verus-0.2026.10.18` pins `PATH`, an isolated `CARGO_HOME`, and
  `RUSTUP_TOOLCHAIN=1.99.1-aarch64-apple-darwin`. Identity output:

  ```
  Verus
    Version: 0.2026.10.18.7d82994
    Profile: release
    Platform: macos_aarch64
    Toolchain: 1.99.1-aarch64-apple-darwin (overridden by environment variable RUSTUP_TOOLCHAIN)
  ```

  `cargo verus --help` works; `cargo verus --version` does not exist in this release.

## Fixture shape and contracts

`$REPO/crates/ic-stable-lara/formal/verus/` contains `Cargo.toml` (empty `[workspace]`, feature
`verus` → `vstd = "=0.0.0-2026-10-18-0147"`, local no-op `verus-spec-stub`), `Cargo.lock`,
`src/lib.rs`, `SCOPE.md`, and `stub/`. The type is declared inside `verus!` (plain build: a plain
duplicate) so it is spec-visible; associated functions are mirrored as free functions and associated
constants as module constants because this release cannot attach attribute-style specs to associated
functions returning datatypes (details under Failures). `SCOPE.md` lists every relaxation.

Contracts state the **actual** behaviour (7 functions) plus 3 client proofs:

| Item | Contract (summary) |
| --- | --- |
| `from_index(index)` | `Some(h)` iff `index < 169`, with `h.0 == index`; `None` iff `index >= 169` |
| `from_i32(head)` | `head < 0` → `Some(NONE)`; `0..=168` → `Some(LogHead(head))`; `>= 169` → `None` |
| `to_i32(head)` | `NONE` → `-1`; otherwise the raw byte as `i32` |
| `as_byte(head)` | the raw byte |
| `is_none(head)` | `head.0 == 255` |
| `index(head)` | `Some(head.0)` iff not `NONE`, else `None` |
| `from_byte(byte)` | `255` → `NONE`, every other byte stored as-is |
| `wire_roundtrip(byte)` | client: `as_byte(from_byte(byte)) == byte` |
| `legacy_roundtrip(head)` | client (`head == -1 ∨ 0 <= head < 169`): `to_i32(from_i32(head)) == head` |
| `sentinel_views()` | client: `NONE` is `255` on the wire, `-1` in the legacy view, `is_none`, `index() == None` |

## Proof evidence (proved)

Command (from the fixture directory):

```sh
bash ../../../../scripts/verus-0.2026.10.18 cargo verus verify --features verus
```

Result: `verification results:: 13 verified, 0 errors`; dependency `vstd` 2060 verified, 0 errors
(not part of the mirror's claim). `--time` on a warm run: total-time 1030 ms, verification-time
580 ms, SMT 5 ms / 10,733 rlimit, 1 thread. Cold run additionally compiles `vstd` and the Verus
support crates. No `unknown`, timeout, or skipped VC was reported.

Plain build evidence (tests are erased by the stub but behaviour is executed):

```sh
cargo check --manifest-path crates/ic-stable-lara/formal/verus/Cargo.toml   # 0 warnings
cargo test  --manifest-path crates/ic-stable-lara/formal/verus/Cargo.toml   # 4 passed
cargo fmt   --manifest-path crates/ic-stable-lara/formal/verus/Cargo.toml -- --check
```

The 4 tests pin the executable behaviour the contracts describe: boundary `0..=168` accepted,
169/255 rejected, every negative `i32` → `NONE`, sentinel views, and `from_byte` accepting
out-of-domain bytes (169/254).

## Mutant matrix (spec strength)

One baseline and 12 targeted mutants, each a scratch copy under `verif/verus-mutants/` (git-ignored)
verified with the same launcher; verification must reject the mutant. Result: **12/12 rejected**,
baseline verified (`13 verified, 0 errors`).

| mutant | targeted clause | observed |
| --- | --- | --- |
| from-index-accepts-169 | `from_index` rejects 169 | postcondition not satisfied |
| from-index-rejects-168 | `from_index` accepts 168 | postcondition not satisfied |
| from-i32-negative-wrong | negatives map to `NONE` | postcondition not satisfied |
| from-i32-accepts-169 | `from_i32` rejects 169 | postcondition not satisfied |
| to-i32-sentinel-wrong | `NONE` → `-1` | postcondition not satisfied |
| as-byte-wrong | raw byte returned | postcondition not satisfied |
| is-none-wrong | `255` detection | postcondition not satisfied |
| index-wrong | stored byte returned | postcondition not satisfied |
| from-byte-wrong | `255` → `NONE` | postcondition not satisfied |
| wire-client-wrong | wire round-trip client | assertion failed |
| legacy-client-wrong | legacy round-trip client | assertion failed |
| sentinel-client-wrong | sentinel views client | assertion failed |

Spec-strength gap found and fixed: the first `from_index` contract only said
`None => index >= 169` and `Some => h.0 == index`, so the `<= 169` mutant **verified** (it accepts
169 and still satisfies both implications). The contract was strengthened to
`Some => index < 169 && h.0 == index`, after which the mutant is rejected. Recorded here because the
initial acceptance is exactly the vacuity class this harness exists to catch.

## Trust cards and labels

| Result | Label | Basis |
| --- | --- | --- |
| mirror function contracts, client proofs | **proved** | `cargo verus verify --features verus`, 13/0 |
| plain-build executable behaviour | **trusted-and-tested** | 4 tests execute the same bodies with specs erased |
| no-op `verus-spec-stub` macros | **trusted-and-tested** | plain `cargo check` + tests; erasure is the only effect |
| dual type/constant declaration (verus! vs plain) | **trusted-and-tested** | both build modes compile and the behaviour tests pass |
| launcher/provisioning scripts | **trusted-and-tested** | idempotent rerun and identity output above |
| transcription fidelity to the frozen blob | **assumed** | manually reviewed against the 91-line blob and its hash; no mechanical equivalence proof |
| Verus, `vstd`, z3 toolchain | **assumed** | pinned release and digest; toolchain correctness is outside the harness |

No `assume`, `admit`, `external_body`, or `uninterp` appears in the fixture.

## Failures and tool limitations observed

1. `#[verus_spec]` on associated functions returning a datatype fails with
   `E0425: cannot find function …` (the generated module-scope spec proxy cannot resolve the method
   name). Worked around with free functions; recorded as a relaxation.
2. Associated constants of a `verus!`-declared type are unsupported in verified code
   (`… is not supported`); moved to module constants inside the `verus!` block.
3. A private field makes the type opaque for field access in an ensures clause
   (`disallowed: field expression for an opaque datatype`); the mirror uses `pub u8` and records it.
4. Bare `requires …` is invalid outside `verus!`; attribute-style requires must be inside
   `#[verus_spec(...)]`.
5. Calling an exec function inside `proof! { … }` fails (`cannot call function … with mode exec`);
   the client calls exec functions first and asserts on locals.
6. `cargo verus verify` without `--features verus` fails with
   `verus_builtin crate was not imported`; the feature-gated stub pattern is required.
7. The workspace's 14-day registry policy is not enforced by this cargo invocation
   (`warning: ignoring registry.global-min-publish-age without -Zmin-publish-age`); the pinned
   `vstd` is 22 days old at disposition, so the pin is inside the policy anyway.
8. `cargo fmt --all -- --check` at the repository root currently fails on the pre-existing untracked
   file `crates/graph-view-core/src/label_layout.rs` (not part of this slice and not in its diff).
   All slice-owned files pass `cargo fmt --manifest-path … -- --check`.

## Isolation and regression evidence

- `.gitignore` now ignores `.tools/` and `verif/`; `git check-ignore -v` confirms both, including
  `.tools/verus-0.2026.10.18.7d82994` and `verif/verus-mutants`.
- `cargo metadata --no-deps --format-version 1` reports **56** workspace packages before and after;
  the fixture is excluded by its own `[workspace]` table.
- No workspace manifest, `Cargo.lock`, or production source file was changed by this slice. The
  worktree still contains the pre-existing unrelated modifications; the slice diff inventory is
  `scripts/setup-verus-stack.sh`, `scripts/verus-0.2026.10.18`, `.gitignore`,
  `crates/ic-stable-lara/formal/verus/**`, and this record.
- `cargo check -p ic-stable-lara --tests` passes (9.6 s); `git diff --check` passes.
- No PocketIC, canbench, or production test run is required: no production code or benchmark
  assumption changes.

## Handoff to the independent `verus-ic` project

The independent spike (its plan 0001, now closed and tagged `v0.1.0`) already established what this
harness confirms: feature-gated stub macros are needed for a token-free release build; attribute
macros are usable only for token-free executable functions; wasm32 verification works though
`--target` is undocumented; async contracts work on this release despite the stale feature table
(measured there in gate 4); unsafe pointer glue can be proved with zero trusted declarations, but
the permission model and caller-side permission creation stay in the TCB. The tool limitations
above are new inputs for its upgrade checklist.

## Next decision

The harness verdict is **pass**: one real module is proved, mutation-controlled, and isolated at a
known cost. The next step is a separately reviewed production-adoption slice (leaf crate plus a
path-filtered CI job) and, independently, the 169 contract decision with a non-vacuous regression
test. Neither is part of this record.
