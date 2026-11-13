//! Optional text-embedding provider boundary.
//!
//! The core SDK never bundles, downloads or initialises a model. Applications that need semantic
//! search implement [`EmbeddingProvider`] (an HTTP sidecar, a browser ONNX runtime, a native
//! ONNX/fastembed backend, or a hosted embedding API) and pass the resulting bytes to a prepared
//! operation. See ADR 0120.

#![warn(missing_docs)]

/// Version of the public embedding-contract shape.
pub const EMBEDDING_CONTRACT_VERSION: u32 = 1;

/// Identity and preprocessing of one embedding space.
///
/// Vectors from different contracts are not comparable, even when their dimensions match. The
/// contract is therefore mandatory for every provider and must travel with any vector that is
/// written to or queried against a vector index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingContract {
    /// Contract shape version.
    pub contract_version: u32,
    /// Model identifier, for example `intfloat/multilingual-e5-small`.
    pub model: String,
    /// Optional pinned model revision.
    pub model_revision: Option<String>,
    /// Output dimensions.
    pub dimensions: u32,
    /// Prefix applied to query text, possibly empty.
    pub query_prefix: String,
    /// Prefix applied to passage/document text, possibly empty.
    pub passage_prefix: String,
    /// Pooling strategy, for example `mean` or `cls`.
    pub pooling: String,
    /// Whether vectors are L2-normalised.
    pub normalized: bool,
    /// Similarity function, for example `cosine`.
    pub similarity: String,
    /// Optional model input limit in tokens.
    pub max_input_tokens: Option<u32>,
}

impl EmbeddingContract {
    /// Validate the contract's structural invariants.
    pub fn validate(&self) -> Result<(), EmbeddingError> {
        if self.contract_version != EMBEDDING_CONTRACT_VERSION {
            return Err(EmbeddingError::InvalidContract(format!(
                "unsupported embedding contract version {}",
                self.contract_version
            )));
        }
        if self.model.trim().is_empty() {
            return Err(EmbeddingError::InvalidContract(
                "model must not be empty".to_owned(),
            ));
        }
        if self.dimensions == 0 {
            return Err(EmbeddingError::InvalidContract(
                "dimensions must be positive".to_owned(),
            ));
        }
        if self.pooling.trim().is_empty() {
            return Err(EmbeddingError::InvalidContract(
                "pooling must not be empty".to_owned(),
            ));
        }
        if self.similarity.trim().is_empty() {
            return Err(EmbeddingError::InvalidContract(
                "similarity must not be empty".to_owned(),
            ));
        }
        if self.max_input_tokens == Some(0) {
            return Err(EmbeddingError::InvalidContract(
                "max_input_tokens must be positive".to_owned(),
            ));
        }
        Ok(())
    }

    /// Validate that `values` fits this contract.
    pub fn validate_vector(&self, values: &[f32]) -> Result<(), EmbeddingError> {
        self.validate()?;
        let actual = u32::try_from(values.len()).unwrap_or(u32::MAX);
        if actual != self.dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.dimensions,
                actual,
            });
        }
        Ok(())
    }

    /// Encode one vector as little-endian `f32` bytes, the Router's `Value::Bytes` form.
    pub fn encode_vector(&self, values: &[f32]) -> Result<Vec<u8>, EmbeddingError> {
        self.validate_vector(values)?;
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Ok(bytes)
    }
}

/// One optional text-embedding backend.
///
/// Loading is deliberately not part of this trait: an implementation may download a model on its
/// own explicit `load()` method, but the SDK never triggers that implicitly.
pub trait EmbeddingProvider {
    /// Contract of the vectors this provider returns.
    fn contract(&self) -> &EmbeddingContract;

    /// Embed one query string.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed one passage/document string.
    fn embed_passage(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;
}

/// Failure while validating an embedding contract or producing a vector.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddingError {
    /// The contract is structurally invalid or unsupported.
    #[error("invalid embedding contract: {0}")]
    InvalidContract(String),
    /// A provider returned a vector with the wrong dimension.
    #[error("embedding dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch {
        /// Contract dimension.
        expected: u32,
        /// Provider dimension.
        actual: u32,
    },
    /// The provider failed to produce an embedding.
    #[error("embedding provider failed: {0}")]
    Provider(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(dimensions: u32) -> EmbeddingContract {
        EmbeddingContract {
            contract_version: EMBEDDING_CONTRACT_VERSION,
            model: "test/model".to_owned(),
            model_revision: Some("rev-1".to_owned()),
            dimensions,
            query_prefix: "query: ".to_owned(),
            passage_prefix: "passage: ".to_owned(),
            pooling: "mean".to_owned(),
            normalized: true,
            similarity: "cosine".to_owned(),
            max_input_tokens: Some(512),
        }
    }

    #[test]
    fn encodes_little_endian_f32() {
        let bytes = contract(2).encode_vector(&[1.0, -0.5]).expect("encode");
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[4..], &(-0.5f32).to_le_bytes());
    }

    #[test]
    fn rejects_dimension_mismatch() {
        let error = contract(3).encode_vector(&[1.0]).expect_err("mismatch");
        assert_eq!(
            error,
            EmbeddingError::DimensionMismatch {
                expected: 3,
                actual: 1
            }
        );
    }

    #[test]
    fn rejects_unsupported_contract_version() {
        let mut invalid = contract(384);
        invalid.contract_version = 999;
        assert!(invalid.validate().is_err());
    }
}
