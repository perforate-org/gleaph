//! Candid-shaped init args for the text index canister.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// Init args for the text index canister. `None` / omitted fields (or bare wasm installs
/// such as canbench, which send EMPTY argument bytes) fall back to the defaults recorded
/// in the handler instead of trapping.
#[derive(CandidType, Serialize, Deserialize, Debug, Default)]
pub struct TextCanisterInitArgs {
    /// Controller allowed to call `admin_flush` / `admin_merge_step` / the backfill and
    /// dictionary-admin endpoints. `None` leaves the canister without an admin caller
    /// until re-initialized — admin endpoints fail closed.
    pub controller: Option<Principal>,
    /// Pinned analyzer id (plan 0331): 1 = unicode-bigram, 2 = vibrato + ipadic lemma
    /// pipeline. Validated fail-closed (∈ {1, 2}) at the open and recorded into
    /// `TextMeta`. `None` defaults to the unicode-bigram pipeline, byte-compatibly with
    /// pre-0331 installs.
    pub analyzer_id: Option<u32>,
}
