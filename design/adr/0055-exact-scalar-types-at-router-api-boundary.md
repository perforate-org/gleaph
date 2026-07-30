# 0055. Exact scalar types at the Router/API boundary

Date: 2026-07-30
Status: proposed
Last revised: 2026-07-30
Anchor timestamp: 2026-07-30 00:31:16 UTC +0000

## Context

Gleaph's GQL value system distinguishes scalar widths and representations, including
`Int8`/`Int16`/`Int32`, `Uint8`/`Uint16`/`Uint32`, and `Float16`/`Float32`. The current
`gleaph-gql-ic` Candid projection widens some of these values for convenience:

```text
Int8/Int16/Int32       -> Int64
Uint8/Uint16/Uint32    -> Uint64
Float16/Float32        -> Float64
Float128/Float256      -> ValueBinary
```

This preserves ordinary numeric values in several cases, but it does not preserve the GQL
type identity. That is insufficient for a typed Router API, prepared-query manifests, and
code generation. A generated client must distinguish an `Int32` parameter from an `Int64`
parameter and a `Float32` result from a `Float64` result.

The current Router/API wire is not a deployed compatibility contract. Existing SDK, CDK, and
codegen implementations are still under development, so preserving the historical widening
shape would create a permanent constraint without protecting existing users.

## Problem

The Router/API boundary currently has no single exact scalar contract. As a result:

- the GQL runtime type system, Candid result values, SDK `ApiValue`, and codegen `SemanticType`
  can disagree;
- generated language types cannot reflect the schema faithfully;
- `Float128` and `Float256` are treated as opaque fallback blobs rather than named scalar types;
- each runtime may independently decide whether a narrow scalar should be widened; and
- future Router metadata cannot safely describe the type of prepared parameters and result
  columns.

## Existing architecture assessment

The existing ownership boundaries are sufficient. `gleaph_gql::Value` remains the canonical
execution value model, `gleaph-gql-ic` owns its Internet Computer wire projection, SDK/CDK
packages own runtime conversion, and `crates/codegen` owns language-specific type generation.

The problem is the contract between those owners, not the absence of another value subsystem.
The current `IcWireValue` conversion is the appropriate owner for the change, but its widening
policy is not fit for a public typed API.

The compact GQL parameter blob already preserves the GQL value tags. This ADR aligns the
structured Candid projection and the manifest/codegen contract with that exact model; it does
not introduce a second execution value representation.

## Decision

### 1. Preserve scalar type identity at the Router/API boundary

Router/API values must preserve the exact GQL scalar variant. Implicit widening and opaque
fallback for supported scalar variants are prohibited.

The structured wire surface must expose:

```text
Int8, Int16, Int32, Int64, Int128, Int256
Uint8, Uint16, Uint32, Uint64, Uint128, Uint256
Float16, Float32, Float64, Float128, Float256
Decimal
```

The wire type may use a different storage representation where the platform lacks a native
primitive, but the variant and its representation contract must remain explicit.

### 2. Use explicit representations for non-Candid-native floating-point widths

The planned Candid projection is:

```rust
Float16(u16)      // canonical IEEE 754 binary16 bit pattern
Float32(f32)
Float64(f64)
Float128(Vec<u8>) // canonical Gleaph binary representation
Float256(Vec<u8>)
```

`Float16` is transported as its raw 16-bit bit pattern so conversion does not silently alter
NaN payloads, signed zero, or subnormal values. `Float128` and `Float256` use a versioned,
canonical binary representation owned by the GQL value codec. They must not be represented by
`Float64` or an untyped `ValueBinary` variant.

Integer widths that have a direct Candid representation use that representation. `Int256`,
`Uint256`, and arbitrary-precision `Decimal` may continue to use canonical decimal strings if
that is the selected Candid binding representation; this is an explicit representation, not
numeric widening.

### 3. Keep manifest types exact

The prepared-query manifest and `crates/codegen` IR must describe the exact scalar variants.
The current `SemanticType::Int64`/`Float64`-only model is replaced by a complete scalar model,
including all supported integer and floating-point widths.

The manifest version is incremented when this schema is implemented. A manifest containing an
unknown scalar variant or unsupported floating-point representation fails closed during
validation and generation.

### 4. Keep language conversion below the API contract

The API contract preserves the Gleaph type. Each runtime and generator maps that type to an
appropriate language representation:

| Gleaph type | Rust | TypeScript/JavaScript |
| --- | --- | --- |
| `Int8`/`Int16`/`Int32` | `i8`/`i16`/`i32` | `number` |
| `Int64`/`Uint64` | `i64`/`u64` or runtime bigint wrapper | `bigint` |
| `Float16`/`Float32`/`Float64` | exact runtime float wrapper or native supported type | `number` with exact runtime conversion |
| `Float128`/`Float256` | Gleaph runtime float wrapper | Gleaph runtime float wrapper |

Where a language cannot represent a scalar natively, the SDK/CDK runtime owns the wrapper and
conversion rules. Generated code must not silently substitute a wider native type.

### 5. Treat this as a clean pre-release wire change

No compatibility decoder, dual wire shape, or legacy widening path is required. Once implemented,
the old widening variants and their tests are removed. Router, SDK, CDK, codegen, and generated
fixtures move to the exact contract as one change set.

## Alternatives considered

### Preserve widening and keep exact types only in the manifest

Rejected. The generated schema would claim `Int32` while the runtime response exposes `Int64`,
requiring every runtime to maintain an implicit conversion boundary and making the public API
internally inconsistent.

### Keep `ValueBinary` for Float128/Float256

Rejected. A typed API cannot distinguish a supported `Float128` column from an arbitrary opaque
GQL value. The canonical floating-point representation must be named and versioned.

### Add a new versioned exact endpoint while retaining the old endpoint

Unnecessary for the current pre-release state. It would introduce two public contracts and
duplicate SDK/CDK testing without protecting deployed consumers.

### Normalize all values to the largest native type

Rejected. It loses schema information, weakens generated type safety, and makes future scalar
extensions depend on the least expressive existing wire type.

## Consequences

Positive consequences:

- Router, GQL, SDK/CDK, manifest, and generated code share one scalar vocabulary.
- Prepared-query code generation can emit faithful parameter and result types.
- Float widths and special IEEE representations are not silently lost.
- Runtime conversion remains owned by SDK/CDK rather than duplicated by generators.
- The compact parameter blob and structured result wire have consistent scalar semantics.

Accepted costs and risks:

- `IcWireValue`, Candid IDLs, SDK types, and generated fixtures change together.
- Dedicated runtime wrappers are required for `Float16`, `Float128`, and `Float256`.
- Canonical binary encoding for `Float128` and `Float256` must be specified and tested before
  the Router API is accepted.
- Native language mappings cannot always be one-to-one; codegen must expose those limitations
  rather than hiding them through widening.

## Migration and implementation order

This is a clean pre-release replacement, not a compatibility migration. Implement it in this
order:

1. specify canonical representations for `Float16`, `Float128`, and `Float256` in the GQL value
   codec;
2. replace `IcWireValue` scalar variants and conversion tests;
3. update Router prepared result types and Candid declarations;
4. update JS/TS SDK `ApiValue`, IDL, and runtime conversion;
5. expand `crates/codegen` `SemanticType`, bump the manifest version, and update all profiles;
6. update Rust SDK/CDK and Motoko runtime helpers; and
7. add round-trip and boundary-value tests across every scalar family, including nested records
   and lists.

## Design documentation impact

- ADR 0053 must reference this ADR for the prepared-query result schema and semantic type
  decision.
- `design/architecture/overview.md` need not duplicate the scalar table; API wire details remain
  owned by this ADR and the relevant crate documentation.
- `crates/gql-ic/src/wire.rs`, SDK IDL/type documentation, and codegen manifest documentation
  must be updated in the implementation change.

## Required acceptance criteria

The ADR can move from proposed only when:

- every supported GQL scalar has a distinct structured API variant;
- no supported scalar conversion widens or falls back to `ValueBinary`;
- canonical float representations have round-trip tests;
- manifest validation and every generator profile preserve scalar width; and
- Router, SDK/CDK, and generated-code fixtures pass against the same Candid contract.
