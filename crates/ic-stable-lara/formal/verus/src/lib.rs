//! Frozen, workspace-independent mirror of `ic-stable-lara::log_head` for the Verus intake
//! harness (plan 0370, ADR 0104).
//!
//! The executable bodies are transcribed from the frozen blob recorded in `SCOPE.md`; contracts
//! state the implementation's **actual** accepted/rejected domains, not the documented domain. The
//! documented-domain mismatch is recorded in `SCOPE.md` and is not fixed here.
//!
//! Shape relaxations (also in `SCOPE.md`):
//!
//! - the type is declared inside `verus!` so it is spec-visible; the plain build declares the same
//!   field outside `verus!`;
//! - the frozen module's associated functions are mirrored as module-level free functions because
//!   this Verus release cannot attach `#[verus_spec]` to associated functions that return a
//!   datatype (the generated module-scope spec proxy fails name resolution; E0425). The bodies,
//!   contracts and constants are otherwise a faithful transcription.

#![cfg_attr(not(feature = "verus"), allow(dead_code, unused_variables))]

#[cfg(not(feature = "verus"))]
#[allow(unused_imports)]
use verus_spec_stub::*;
#[cfg(feature = "verus")]
#[allow(unused_imports)]
use vstd::prelude::*;

/// Head index into a per-segment overflow log, or `NONE` when absent.
#[cfg(not(feature = "verus"))]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct LogHead(pub u8);

#[cfg(feature = "verus")]
verus! {

/// Head index into a per-segment overflow log, or `NONE` when absent.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct LogHead(pub u8);

/// Sentinel: no overflow log chain (`u8::MAX` on wire).
pub const NONE: LogHead = LogHead(u8::MAX);

/// Maximum valid entry index (the frozen implementation rejects this value itself; see
/// `SCOPE.md`).
pub const MAX_VALID_INDEX: u8 = 169;

}

/// Sentinel: no overflow log chain (`u8::MAX` on wire).
#[cfg(not(feature = "verus"))]
pub const NONE: LogHead = LogHead(u8::MAX);

/// Maximum valid entry index (the frozen implementation rejects this value itself; see
/// `SCOPE.md`).
#[cfg(not(feature = "verus"))]
pub const MAX_VALID_INDEX: u8 = 169;

/// Creates a log head from an index; accepts `0..=168` and rejects `169..=u8::MAX`.
#[verus_spec(r => ensures
    match r {
        Some(h) => index < 169 && h.0 == index,
        None => index >= 169,
    },
)]
#[inline]
pub fn from_index(index: u8) -> Option<LogHead> {
    if index < MAX_VALID_INDEX {
        Some(LogHead(index))
    } else {
        None
    }
}

/// Legacy `i32` view: `-1` maps to `NONE`, `0..=168` are accepted, everything else is rejected.
#[verus_spec(r => ensures
    match r {
        Some(h) => if head < 0 {
            h.0 == u8::MAX
        } else {
            0 <= head && head < 169 && h.0 as int == head as int
        },
        None => head >= 169,
    },
)]
#[inline]
pub fn from_i32(head: i32) -> Option<LogHead> {
    if head < 0 {
        Some(NONE)
    } else if head < MAX_VALID_INDEX as i32 {
        Some(LogHead(head as u8))
    } else {
        None
    }
}

/// Legacy `i32` view (`-1` when the head is `NONE`).
#[verus_spec(r => ensures
    if head.0 == u8::MAX {
        r == -1
    } else {
        r as int == head.0 as int
    },
)]
#[inline]
pub fn to_i32(head: LogHead) -> i32 {
    if is_none(head) {
        -1
    } else {
        head.0 as i32
    }
}

/// Raw wire byte.
#[verus_spec(r => ensures r == head.0)]
#[inline]
pub fn as_byte(head: LogHead) -> u8 {
    head.0
}

/// `true` when the head is `NONE`.
#[verus_spec(r => ensures r == (head.0 == u8::MAX))]
#[inline]
pub fn is_none(head: LogHead) -> bool {
    head.0 == u8::MAX
}

/// Entry index when the head is not `NONE`.
#[verus_spec(r => ensures
    match r {
        Some(i) => i == head.0 && head.0 != u8::MAX,
        None => head.0 == u8::MAX,
    },
)]
#[inline]
pub fn index(head: LogHead) -> Option<u8> {
    if is_none(head) {
        None
    } else {
        Some(head.0)
    }
}

/// Decodes a wire byte: `u8::MAX` is `NONE`, every other byte is stored as-is (no domain
/// validation; this is the frozen implementation's behaviour).
#[verus_spec(r => ensures
    r.0 == if byte == u8::MAX { u8::MAX } else { byte },
)]
#[inline]
pub fn from_byte(byte: u8) -> LogHead {
    if byte == u8::MAX {
        NONE
    } else {
        LogHead(byte)
    }
}

/// Client: wire byte round-trips through `from_byte`/`as_byte`.
#[verus_spec]
fn wire_roundtrip(byte: u8) {
    let head = from_byte(byte);
    let wire = as_byte(head);
    proof! {
        assert(wire == byte);
    }
}

/// Client: an in-domain legacy head round-trips through `from_i32`/`to_i32`.
#[verus_spec(
    requires head == -1 || (0 <= head && head < 169),
)]
fn legacy_roundtrip(head: i32) {
    let decoded = from_i32(head);
    proof! {
        assert(decoded is Some);
    }
    let log_head = decoded.unwrap();
    let back = to_i32(log_head);
    proof! {
        assert(back == head);
    }
}

/// Client: the sentinel is `255` on the wire and `-1` in the legacy view.
#[verus_spec]
fn sentinel_views() {
    let byte = as_byte(NONE);
    let legacy = to_i32(NONE);
    let none = is_none(NONE);
    let index = index(NONE);
    proof! {
        assert(byte == 255);
        assert(legacy == -1);
        assert(none);
        assert(index is None);
    }
}

#[cfg(test)]
mod tests {
    //! Executable-behaviour tests for the plain build: they pin the actual domains the contracts
    //! describe (and the frozen implementation's real behaviour), including the boundary and
    //! all-negative quirks recorded in `SCOPE.md`.

    use super::*;

    #[test]
    fn index_boundary_matches_implementation() {
        assert_eq!(from_index(0).map(as_byte), Some(0));
        assert_eq!(from_index(168).map(as_byte), Some(168));
        assert!(from_index(169).is_none());
        assert!(from_index(u8::MAX).is_none());
    }

    #[test]
    fn every_negative_i32_maps_to_none() {
        assert_eq!(from_i32(-1).map(as_byte), Some(u8::MAX));
        assert_eq!(from_i32(-5).map(as_byte), Some(u8::MAX));
        assert_eq!(from_i32(168).map(as_byte), Some(168));
        assert!(from_i32(169).is_none());
    }

    #[test]
    fn sentinel_views_match_implementation() {
        assert_eq!(to_i32(NONE), -1);
        assert!(is_none(NONE));
        assert!(index(NONE).is_none());
        assert_eq!(to_i32(from_byte(7)), 7);
    }

    #[test]
    fn from_byte_does_not_validate_domain() {
        assert_eq!(as_byte(from_byte(u8::MAX)), u8::MAX);
        assert_eq!(index(from_byte(169)), Some(169));
        assert_eq!(index(from_byte(254)), Some(254));
    }
}
