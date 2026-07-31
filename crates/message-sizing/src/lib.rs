#![no_std]

//! Adaptive sizing for prefix-shaped encoded messages.
//!
//! The crate deliberately does not depend on Candid or on any canister type. Callers provide a
//! measurement closure that builds and encodes the candidate payload. The returned hint is only an
//! optimization: every reuse still performs an authoritative measurement before the payload is
//! sent.

/// A sizing policy for a prefix-shaped payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizingPolicy {
    /// Hard encoded-byte limit. A candidate above this limit cannot be sent.
    pub hard_limit_bytes: usize,
    /// Preferred target below the hard limit. The initial estimate aims here so that later
    /// changes in entry shape retain headroom.
    pub target_bytes: usize,
    /// Number of entries to use for the first measurement when no reusable hint exists.
    pub sample_entries: usize,
}

/// Current portable ICP inter-canister request ceiling. Keep this as a policy input rather than
/// baking it into callers: if the platform raises the ceiling, only this constant needs updating.
pub const MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

/// Fixed headroom reserved below the current platform ceiling for envelope growth and policy
/// drift. The target is derived from the current ceiling minus this value.
pub const INTER_CANISTER_MESSAGE_HEADROOM_BYTES: usize = 500 * 1024;

pub const INTER_CANISTER_TARGET_PAYLOAD_BYTES: usize =
    MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES - INTER_CANISTER_MESSAGE_HEADROOM_BYTES;

pub const INTER_CANISTER_INITIAL_SAMPLE_ENTRIES: usize = 96;

impl SizingPolicy {
    pub const fn new(hard_limit_bytes: usize, target_bytes: usize, sample_entries: usize) -> Self {
        Self {
            hard_limit_bytes,
            target_bytes,
            sample_entries,
        }
    }

    /// The portable ICP inter-canister policy. Its target is always derived from the current
    /// platform ceiling minus the fixed headroom, so an increased ceiling is used automatically.
    pub const fn inter_canister() -> Self {
        Self::new(
            MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
            INTER_CANISTER_TARGET_PAYLOAD_BYTES,
            INTER_CANISTER_INITIAL_SAMPLE_ENTRIES,
        )
    }

    fn normalized(self) -> Self {
        let hard = self.hard_limit_bytes.max(1);
        Self {
            hard_limit_bytes: hard,
            target_bytes: self.target_bytes.max(1).min(hard),
            sample_entries: self.sample_entries.max(1),
        }
    }
}

/// A previously observed prefix count. Hints are shape-specific caller-owned optimization state;
/// they are never correctness state and must not be shared across unrelated payload shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeHint {
    pub entry_count: usize,
}

impl SizeHint {
    pub const fn new(entry_count: usize) -> Self {
        Self { entry_count }
    }
}

/// A measured prefix that fits the hard limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FittedPrefix {
    pub entry_count: usize,
    pub encoded_bytes: usize,
}

/// Measurement or admission failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitError<E> {
    /// The caller's candidate construction or encoding failed.
    Measure(E),
    /// Even one entry (or the fixed envelope) cannot fit the hard limit.
    NoEntryFits {
        encoded_bytes: usize,
        hard_limit_bytes: usize,
    },
}

/// Find a target-sized fitting prefix using a measured sample and an optional reusable hint.
///
/// `measure(count)` must return the encoded byte length for the complete payload containing the
/// first `count` entries. The function assumes prefix sizes are monotone for the supplied payload
/// shape, which is true for the Candid vectors used by the canister batch envelopes. The target is
/// intentionally below the hard limit; this avoids consuming transport-specific envelope overhead
/// that is not included in the canister argument measurement. Every returned result is based on an
/// actual final measurement; estimates and hints only reduce probes.
pub fn adaptive_fitting_prefix<E, F>(
    length: usize,
    hint: Option<SizeHint>,
    policy: SizingPolicy,
    mut measure: F,
) -> Result<Option<FittedPrefix>, FitError<E>>
where
    F: FnMut(usize) -> Result<usize, E>,
{
    if length == 0 {
        return Ok(None);
    }
    let policy = policy.normalized();

    let mut candidate = if let Some(hint) = hint {
        hint.entry_count.clamp(1, length)
    } else {
        let sample = policy.sample_entries.min(length).max(1);
        let base = measure(0).map_err(FitError::Measure)?;
        if base > policy.hard_limit_bytes {
            return Err(FitError::NoEntryFits {
                encoded_bytes: base,
                hard_limit_bytes: policy.hard_limit_bytes,
            });
        }
        let sample_bytes = measure(sample).map_err(FitError::Measure)?;
        estimate_count(length, policy, base, sample, sample_bytes)
    };

    let mut measured = measure(candidate).map_err(FitError::Measure)?;
    if hint.is_some() && measured > policy.hard_limit_bytes {
        // A cached count is shape-specific. If it no longer fits, discard it and re-estimate from
        // a fresh fixed-size sample instead of repeatedly retrying a stale hint.
        let sample = policy.sample_entries.min(length).max(1);
        let base = measure(0).map_err(FitError::Measure)?;
        if base > policy.hard_limit_bytes {
            return Err(FitError::NoEntryFits {
                encoded_bytes: base,
                hard_limit_bytes: policy.hard_limit_bytes,
            });
        }
        let sample_bytes = measure(sample).map_err(FitError::Measure)?;
        candidate = estimate_count(length, policy, base, sample, sample_bytes);
        measured = measure(candidate).map_err(FitError::Measure)?;
    }
    while measured > policy.hard_limit_bytes {
        if candidate == 1 {
            return Err(FitError::NoEntryFits {
                encoded_bytes: measured,
                hard_limit_bytes: policy.hard_limit_bytes,
            });
        }
        let next = proportional_count(candidate, policy.target_bytes, measured)
            .min(candidate.saturating_sub(1))
            .max(1);
        candidate = next;
        measured = measure(candidate).map_err(FitError::Measure)?;
    }

    Ok(Some(FittedPrefix {
        entry_count: candidate,
        encoded_bytes: measured,
    }))
}

fn estimate_count(
    length: usize,
    policy: SizingPolicy,
    base_bytes: usize,
    sample_count: usize,
    sample_bytes: usize,
) -> usize {
    if sample_bytes <= base_bytes {
        return sample_count.min(length).max(1);
    }
    let variable_bytes = sample_bytes.saturating_sub(base_bytes);
    let available = policy.target_bytes.saturating_sub(base_bytes);
    let estimate = ((available as u128).saturating_mul(sample_count as u128)
        / variable_bytes as u128) as usize;
    estimate.clamp(1, length)
}

fn proportional_count(count: usize, target_bytes: usize, measured_bytes: usize) -> usize {
    if measured_bytes == 0 {
        return count.saturating_mul(2).max(count.saturating_add(1));
    }
    ((count as u128).saturating_mul(target_bytes as u128) / measured_bytes as u128) as usize
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    fn linear_measure(
        base: usize,
        per_entry: usize,
        limit: usize,
    ) -> impl FnMut(usize) -> Result<usize, ()> {
        move |count| {
            Ok(base
                .saturating_add(count.saturating_mul(per_entry))
                .min(limit + 1))
        }
    }

    #[test]
    fn estimates_a_target_sized_fitting_prefix() {
        let result = adaptive_fitting_prefix(
            2_000,
            None,
            SizingPolicy::new(2_000, 1_500, 10),
            linear_measure(100, 10, 2_000),
        )
        .expect("measurement")
        .expect("non-empty");
        assert_eq!(result.entry_count, 140);
        assert_eq!(result.encoded_bytes, 1_500);
    }

    #[test]
    fn oversized_hint_is_reestimated_from_a_fresh_sample() {
        let result = adaptive_fitting_prefix(
            500,
            Some(SizeHint::new(400)),
            SizingPolicy::new(2_000, 1_500, 10),
            linear_measure(100, 10, 2_000),
        )
        .expect("measurement")
        .expect("non-empty");
        assert_eq!(result.entry_count, 140);
        assert_eq!(result.encoded_bytes, 1_500);
    }

    #[test]
    fn reports_when_one_entry_does_not_fit() {
        let error = adaptive_fitting_prefix(10, None, SizingPolicy::new(100, 75, 4), |count| {
            Ok::<_, ()>(200 + count * 10)
        })
        .expect_err("one entry must be rejected");
        assert_eq!(
            error,
            FitError::NoEntryFits {
                encoded_bytes: 200,
                hard_limit_bytes: 100
            }
        );
    }

    #[test]
    fn keeps_a_cached_hint_as_a_starting_point() {
        let mut calls = Vec::new();
        let result = adaptive_fitting_prefix(
            100,
            Some(SizeHint::new(8)),
            SizingPolicy::new(1_000, 750, 4),
            |count| {
                calls.push(count);
                Ok::<_, ()>(100 + count * 50)
            },
        )
        .expect("measurement")
        .expect("non-empty");
        assert_eq!(result.entry_count, 8);
        assert_eq!(result.encoded_bytes, 500);
        assert_eq!(calls.first().copied(), Some(8));
    }

    #[test]
    fn inter_canister_target_is_derived_from_current_limit_and_headroom() {
        assert_eq!(
            INTER_CANISTER_TARGET_PAYLOAD_BYTES,
            MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES - INTER_CANISTER_MESSAGE_HEADROOM_BYTES
        );
        assert_eq!(
            SizingPolicy::inter_canister().target_bytes,
            INTER_CANISTER_TARGET_PAYLOAD_BYTES
        );
    }
}
