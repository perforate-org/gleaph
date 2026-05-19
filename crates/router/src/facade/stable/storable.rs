//! [`Storable`] codecs for router stable maps.

use candid::{Decode, Encode};
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredGraphRegistryEntry(pub GraphRegistryEntry);

impl Storable for StoredGraphRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&self.0).expect("encode GraphRegistryEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self.0).expect("encode GraphRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(Decode!(bytes.as_ref(), GraphRegistryEntry).expect("decode GraphRegistryEntry"))
    }

    const BOUND: Bound = Bound::Unbounded;
}
