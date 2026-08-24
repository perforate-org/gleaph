//! Candid-shaped init args for the text index canister.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// v0 init args: the controller principal for admin guards. `None` (or omission via
/// anonymous default) leaves the canister without an admin caller until re-initialized —
/// admin endpoints fail closed.
#[derive(CandidType, Serialize, Deserialize, Debug, Default)]
pub struct TextCanisterInitArgs {
    /// Controller allowed to call `admin_flush` / `admin_merge_step`.
    pub controller: Option<Principal>,
}
