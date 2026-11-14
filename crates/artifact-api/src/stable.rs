//! Candid `Storable` encodings for the shared artifact/release wire types.
//!
//! Provision owns the stable maps, but a `Storable` impl must live with its type (orphan rule).
//! The `stable` feature keeps `ic-stable-structures` out of client builds while giving the
//! canister one encoding for every shared key and value.

use candid::{Decode, Encode};
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;

use crate::types::{
    ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome, ArtifactId, ArtifactMetadata,
    ArtifactUpload, ArtifactUploadState, InstallError, ReleaseActivateResult, ReleaseError,
    ReleaseId, ReleaseInstallResult, ReleaseManifest,
};

macro_rules! candid_storable {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl Storable for $ty {
                const BOUND: Bound = Bound::Unbounded;

                fn to_bytes(&self) -> Cow<'_, [u8]> {
                    Cow::Owned(Encode!(self).expect(concat!("encode ", stringify!($ty))))
                }

                fn into_bytes(self) -> Vec<u8> {
                    Encode!(&self).expect(concat!("encode ", stringify!($ty)))
                }

                fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
                    Decode!(bytes.as_ref(), Self).expect(concat!("decode ", stringify!($ty)))
                }
            }
        )+
    };
}

candid_storable!(
    ArtifactId,
    ArtifactMetadata,
    ArtifactUpload,
    ArtifactUploadState,
    ReleaseId,
    ReleaseManifest,
    ReleaseActivateResult,
    ReleaseError,
    ArtifactAuditEntry,
    ArtifactAuditAction,
    ArtifactAuditOutcome,
    ReleaseInstallResult,
    InstallError,
);
