//! Stable storage for **prepared GQL**: register time parses source into [`GqlProgram`];
//! execution loads the rkyv payload from stable memory without running the parser again.

use gleaph_gql::ast::GqlProgram;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
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

/// Parsed prepared program plus whether it requires the update (write) canister path.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedQueryRecord {
    pub program: GqlProgram,
    pub requires_write_path: bool,
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

/// Stable prepared program blob: `requires_write` byte + rkyv [`GqlProgram`] bytes.
pub fn encode_program_wire(record: &PreparedQueryRecord) -> Vec<u8> {
    let prog_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&record.program)
        .expect("prepared GQL program rkyv encode should not fail")
        .into_vec();
    let mut v = Vec::with_capacity(1 + prog_bytes.len());
    v.push(u8::from(record.requires_write_path));
    v.extend_from_slice(&prog_bytes);
    v
}

/// Decode [`encode_program_wire`] payload.
pub fn decode_program_wire(bytes: &[u8]) -> Result<PreparedQueryRecord, PreparedQueryError> {
    if bytes.is_empty() {
        return Err(PreparedQueryError::Parse("empty program blob".into()));
    }
    let requires_write_path = bytes[0] != 0;
    let program = rkyv_from_bytes_aligned_program(&bytes[1..])
        .map_err(|e| PreparedQueryError::Parse(format!("program rkyv decode: {e}")))?;
    Ok(PreparedQueryRecord {
        program,
        requires_write_path,
    })
}

impl Storable for PreparedQueryRecord {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let prog_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&self.program)
            .expect("prepared GQL program rkyv encode should not fail")
            .into_vec();
        let mut v = Vec::with_capacity(1 + prog_bytes.len());
        v.push(u8::from(self.requires_write_path));
        v.extend_from_slice(&prog_bytes);
        Cow::Owned(v)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        let requires_write_path = b[0] != 0;
        let program = rkyv_from_bytes_aligned_program(&b[1..])
            .expect("prepared GQL program rkyv decode should not fail");
        Self {
            requires_write_path,
            program,
        }
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Keyed store of prepared programs (parsed AST), using stable memory region `M`.
pub struct PreparedQueryCatalog<M: Memory> {
    map: StableBTreeMap<String, PreparedQueryRecord, M>,
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

    /// Parse `source`, classify write requirements, validate shape, and insert or replace under `name`.
    pub fn register(&mut self, name: String, source: &str) -> Result<(), PreparedQueryError> {
        let program = compile_prepared_source(source)?;
        let requires_write_path = classify_program(&program).requires_write_path();
        self.map.insert(
            name,
            PreparedQueryRecord {
                program,
                requires_write_path,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.map.remove(&name.into());
    }

    pub fn get(&self, name: &str) -> Option<PreparedQueryRecord> {
        self.map.get(&name.into())
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.map.contains_key(&name.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut c = PreparedQueryCatalog::init(ic_stable_structures::VectorMemory::default());
        let gql = "MATCH (n:PrepRoundtrip) RETURN n NEXT INSERT (m:PrepRoundtrip {k: 1})";
        c.register("q".into(), gql).expect("register");
        let got = c.get("q").expect("get");
        assert!(got.requires_write_path);
        assert_eq!(
            got.program
                .transaction_activity
                .as_ref()
                .and_then(|t| t.body.as_ref())
                .map(|b| b.iter_statements().count()),
            Some(2)
        );
    }

    #[test]
    fn read_only_prepared_has_false_requires_write() {
        let mut c = PreparedQueryCatalog::init(ic_stable_structures::VectorMemory::default());
        c.register("ro".into(), "MATCH (n:Roprep) RETURN n")
            .expect("register");
        let got = c.get("ro").expect("get");
        assert!(!got.requires_write_path);
    }
}
