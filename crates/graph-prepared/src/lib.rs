//! Stable storage for **prepared GQL**: register time parses source into [`GqlProgram`];
//! execution loads the rkyv payload from stable memory without running the parser again.

use gleaph_gql::ast::GqlProgram;
use gleaph_gql::parser;
use ic_stable_structures::{
    Memory, StableBTreeMap,
    storable::{Bound, Storable},
};
use std::borrow::Cow;

/// Failure parsing a prepared program or storing it.
#[derive(Debug, thiserror::Error)]
pub enum PreparedQueryError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("prepared GQL must be a transaction with a statement body")]
    MissingStatementBlock,
}

/// Parse `source` and ensure it has the shape expected for prepared execution (non-empty transaction body).
pub fn compile_prepared_source(source: &str) -> Result<GqlProgram, PreparedQueryError> {
    let program = parser::parse(source).map_err(|e| PreparedQueryError::Parse(e.to_string()))?;
    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or(PreparedQueryError::MissingStatementBlock)?;
    if tx.body.is_none() {
        return Err(PreparedQueryError::MissingStatementBlock);
    }
    Ok(program)
}

fn rkyv_from_bytes_aligned_program(bytes: &[u8]) -> Result<GqlProgram, rkyv::rancor::Error> {
    let mut aligned = rkyv::util::AlignedVec::<16>::new();
    aligned.extend_from_slice(bytes);
    rkyv::from_bytes::<GqlProgram, rkyv::rancor::Error>(&aligned)
}

/// rkyv-encoded [`GqlProgram`] for [`StableBTreeMap`] (spans omitted from archive).
#[derive(Clone, Debug, PartialEq, derive_more::From, derive_more::Into)]
pub struct StorablePreparedGqlProgram(GqlProgram);

impl Storable for StorablePreparedGqlProgram {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&self.0)
            .expect("prepared GQL program rkyv encode should not fail");
        Cow::Owned(bytes.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(&self.0)
            .expect("prepared GQL program rkyv encode should not fail")
            .to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(
            rkyv_from_bytes_aligned_program(&bytes[..])
                .expect("prepared GQL program rkyv decode should not fail"),
        )
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Keyed store of prepared programs (parsed AST), using stable memory region `M`.
pub struct PreparedQueryCatalog<M: Memory> {
    map: StableBTreeMap<String, StorablePreparedGqlProgram, M>,
}

impl<M: Memory> std::fmt::Debug for PreparedQueryCatalog<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedQueryCatalog")
            .field("prepared_count", &self.map.len())
            .finish()
    }
}

impl<M: Memory> PreparedQueryCatalog<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    /// Parse `source`, validate shape, and insert or replace under `name`.
    pub fn register(&mut self, name: String, source: &str) -> Result<(), PreparedQueryError> {
        let program = compile_prepared_source(source)?;
        self.map.insert(name, program.into());
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.map.remove(&name.into());
    }

    pub fn get(&self, name: &str) -> Option<GqlProgram> {
        self.map.get(&name.into()).map(Into::into)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.map.contains_key(&name.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn compile_rejects_program_without_body() {
        let err = compile_prepared_source("").expect_err("empty");
        assert!(matches!(
            err,
            PreparedQueryError::MissingStatementBlock | PreparedQueryError::Parse(_)
        ));
    }

    #[test]
    fn roundtrip_register_and_get() {
        let mut c = PreparedQueryCatalog::init(VectorMemory::default());
        let gql = "MATCH (n:PrepRoundtrip) RETURN n NEXT INSERT (m:PrepRoundtrip {k: 1})";
        c.register("q".into(), gql).expect("register");
        let got = c.get("q").expect("get");
        assert_eq!(
            got.transaction_activity
                .as_ref()
                .and_then(|t| t.body.as_ref())
                .map(|b| b.iter_statements().count()),
            Some(2)
        );
    }
}
