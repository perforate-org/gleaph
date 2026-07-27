//! Legacy edge-label weight profile support has been removed (ADR 0051 Phase B).
//!
//! The weight-specific encodings (`WeightRawU16`, `WeightLinearU16`, `WeightLogU16`,
//! `WeightBinary16`) and the `GLEAPH.WEIGHT(e)` runtime function no longer exist.
//! Use ordinary `INLINE` scalar properties and `e.<property>` access instead.
