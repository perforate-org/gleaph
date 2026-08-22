//! Sole owner of the Vector MemoryId 7 subject map.
//!
//! MemoryId 7 is a destructive pre-release cutover from the clustered hash map to the linear
//! hash map. Static construction never opens the region: fresh install calls strict `create` and
//! post-upgrade calls seed-free exact `open`. Every point access and physical scan is routed
//! through this owner so an unavailable region cannot be mistaken for an empty subject catalog.
//! The heap-owner state machine, binding protocol, and reset ticket come from the shared region
//! owner ([`super::region_store`]); only the scan machinery below is subject-specific.

use super::memory;
#[cfg(any(test, feature = "canbench"))]
use super::region_store::RegionResetTicket;
use super::region_store::{RegionError, RegionOwner};
use crate::records::{
    FixedSubjectMapEntry, SubjectKey, SubjectScanCursor, SubjectScanCursorError, SubjectScanScope,
};
use ic_stable_linear_hash_map::{ScanError, ScanPage};
use std::cell::RefCell;

/// Opaque subject-region reset ticket. Construction is private so consumers cannot fabricate a
/// reset fence; the coordinated Vector owner must acquire every other coupled region handle
/// before committing it.
#[cfg(any(test, feature = "canbench"))]
pub(crate) struct SubjectResetTicket(RegionResetTicket);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SubjectScanPage {
    pub entries: Vec<(SubjectKey, FixedSubjectMapEntry)>,
    pub next_cursor: SubjectScanCursor,
    pub examined_slots: u64,
    pub exhausted: bool,
}

type SubjectOwner = RegionOwner<SubjectKey, FixedSubjectMapEntry>;

thread_local! {
    static SUBJECT_STORE: RefCell<SubjectOwner> = RefCell::new(SubjectOwner::default());
}

/// Folds a typed cursor-validation failure into the shared scan vocabulary. Malformed,
/// version-mismatched, scope-mismatched, and stale cursors all fence the scan identically;
/// restart/pressure/in-progress conditions pass through for the structural consumers above.
fn cursor_error(error: SubjectScanCursorError) -> RegionError {
    RegionError::Scan(match error {
        SubjectScanCursorError::Malformed
        | SubjectScanCursorError::VersionMismatch
        | SubjectScanCursorError::ScopeMismatch
        | SubjectScanCursorError::Lhm(ScanError::InvalidCursor) => ScanError::InvalidCursor,
        SubjectScanCursorError::Lhm(error) => error,
    })
}

pub(crate) fn create_for_install(hash_seed: u64) -> Result<(), RegionError> {
    SUBJECT_STORE
        .with_borrow_mut(|owner| owner.create_for_install(memory::subject_memory(), hash_seed))
}

pub(crate) fn open_after_upgrade() -> Result<(), RegionError> {
    SUBJECT_STORE.with_borrow_mut(|owner| owner.open_after_upgrade(memory::subject_memory()))
}

/// Rebinds the production subject owner against the same MemoryId 7 bytes for a native upgrade
/// persistence test. This mirrors the canister's post-upgrade exact-open path without creating or
/// resetting the subject map.
#[cfg(test)]
pub(crate) fn reopen_for_test() -> Result<(), RegionError> {
    SUBJECT_STORE.with_borrow_mut(|owner| owner.reopen_for_test(memory::subject_memory()))
}

pub(crate) fn get(key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, RegionError> {
    SUBJECT_STORE.with_borrow(|owner| owner.get(key))
}

pub(crate) fn insert(
    key: SubjectKey,
    value: FixedSubjectMapEntry,
) -> Result<Option<FixedSubjectMapEntry>, RegionError> {
    SUBJECT_STORE.with_borrow(|owner| owner.insert(key, value))
}

pub(crate) fn remove(key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, RegionError> {
    SUBJECT_STORE.with_borrow(|owner| owner.remove(key))
}

pub(crate) fn scan_start(scope: SubjectScanScope) -> Result<SubjectScanCursor, RegionError> {
    let cursor = SUBJECT_STORE.with_borrow(|owner| {
        let map = owner.map()?;
        map.scan_start().map_err(RegionError::Scan)
    })?;
    Ok(SubjectScanCursor::from_lhm(scope, cursor))
}

pub(crate) fn scan_step(
    scope: SubjectScanScope,
    cursor: SubjectScanCursor,
    physical_slot_budget: u64,
) -> Result<SubjectScanPage, RegionError> {
    cursor.validate(scope).map_err(cursor_error)?;
    let page: ScanPage<SubjectKey, FixedSubjectMapEntry> = SUBJECT_STORE.with_borrow(|owner| {
        let map = owner.map()?;
        map.scan_step(
            cursor.lhm_cursor().map_err(cursor_error)?,
            physical_slot_budget,
        )
        .map_err(RegionError::Scan)
    })?;
    let next_cursor = SubjectScanCursor::from_lhm(scope, page.next_cursor());
    let entries = page.entries().to_vec();
    Ok(SubjectScanPage {
        entries,
        next_cursor,
        examined_slots: page.examined_slots(),
        exhausted: page.exhausted(),
    })
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn prepare_reset(expected_incarnation: u64) -> Result<SubjectResetTicket, RegionError> {
    SUBJECT_STORE
        .with_borrow(|owner| owner.prepare_reset(expected_incarnation))
        .map(SubjectResetTicket)
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn commit_reset(ticket: SubjectResetTicket) -> Result<u64, RegionError> {
    SUBJECT_STORE.with_borrow(|owner| owner.commit_reset(ticket.0))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn bind_for_fixture(hash_seed: u64) -> Result<(), RegionError> {
    SUBJECT_STORE
        .with_borrow_mut(|owner| owner.bind_for_fixture(memory::subject_memory(), hash_seed))
}

/// Returns the current owner incarnation for test/benchmark fixture coordination.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn incarnation_for_test_or_bench() -> Result<u64, RegionError> {
    SUBJECT_STORE.with_borrow(RegionOwner::incarnation_for_test_or_bench)
}

#[cfg(test)]
pub(crate) fn is_empty_for_test() -> Result<bool, RegionError> {
    SUBJECT_STORE.with_borrow(RegionOwner::is_empty_for_test)
}
