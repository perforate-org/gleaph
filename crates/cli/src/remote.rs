//! Shared IC-agent transport for Router subcommands.
//!
//! Owns network/identity setup and the candid `Result` decode only. Subcommand logic stays in
//! the pure modules (`migration`, `load`); this module is the single place that touches the
//! agent, so the same conventions apply to every Router-facing command.

use candid::{CandidType, Decode, Encode};
use ic_agent::Agent;
use std::path::Path;

pub const DEFAULT_IC_URL: &str = "https://icp-api.io";
pub const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

/// One connected Router endpoint.
pub struct RemoteTransport {
    agent: Agent,
    canister: candid::Principal,
    runtime: tokio::runtime::Runtime,
}

impl RemoteTransport {
    /// Build a transport using the same network and identity conventions as `gleaph migration`.
    pub fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, String> {
        let (url, should_fetch_root_key) = resolve_network(network, fetch_root_key)?;
        let canister = candid::Principal::from_text(canister)
            .map_err(|error| format!("invalid canister principal: {error}"))?;
        let agent = if let Some(identity) = identity {
            let identity = ic_agent::identity::Secp256k1Identity::from_pem_file(identity)
                .map_err(|error| format!("read identity {}: {error}", identity.display()))?;
            Agent::builder()
                .with_url(url)
                .with_identity(identity)
                .build()
                .map_err(|error| format!("create IC agent: {error}"))?
        } else {
            Agent::builder()
                .with_url(url)
                .build()
                .map_err(|error| format!("create IC agent: {error}"))?
        };
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|error| format!("create async runtime: {error}"))?;
        if should_fetch_root_key {
            runtime
                .block_on(agent.fetch_root_key())
                .map_err(|error| format!("fetch IC root key: {error}"))?;
        }
        Ok(Self {
            agent,
            canister,
            runtime,
        })
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    /// Query one Router method whose arguments are a single Candid value, and decode the
    /// candid `Result<T, E>` envelope.
    pub fn query<T, E>(&self, method: &str, args: &impl CandidType) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded = Encode!(args).map_err(|error| format!("encode {method} args: {error}"))?;
        self.query_raw(method, encoded)
    }

    /// Query one Router method whose arguments are multiple separate Candid values (one per
    /// tuple element), e.g. `bulk_load_status(graph, key, cursor, max_receipts)`. A tuple
    /// passed as a single `&impl CandidType` value would encode as one record argument, so
    /// multi-argument methods must use this variant.
    pub fn query_args<T, E>(
        &self,
        method: &str,
        args: impl candid::utils::ArgumentEncoder,
    ) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded =
            candid::encode_args(args).map_err(|error| format!("encode {method} args: {error}"))?;
        self.query_raw(method, encoded)
    }

    fn query_raw<T, E>(&self, method: &str, encoded: Vec<u8>) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let response = self
            .block_on(
                self.agent
                    .query(&self.canister, method)
                    .with_arg(encoded)
                    .call(),
            )
            .map_err(|error| format!("query {method}: {error}"))?;
        decode_result(&response, method)
    }

    /// Update one Router method whose arguments are a single Candid value, and decode the
    /// candid `Result<T, E>` envelope.
    pub fn update<T, E>(&self, method: &str, args: &impl CandidType) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded = Encode!(args).map_err(|error| format!("encode {method} args: {error}"))?;
        self.update_raw(method, encoded)
    }

    /// Update one Router method whose arguments are multiple separate Candid values (one per
    /// tuple element), e.g. `ensure_properties(graph, names)`. See [`Self::query_args`].
    pub fn update_args<T, E>(
        &self,
        method: &str,
        args: impl candid::utils::ArgumentEncoder,
    ) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded =
            candid::encode_args(args).map_err(|error| format!("encode {method} args: {error}"))?;
        self.update_raw(method, encoded)
    }

    fn update_raw<T, E>(&self, method: &str, encoded: Vec<u8>) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let response = self
            .block_on(
                self.agent
                    .update(&self.canister, method)
                    .with_arg(encoded)
                    .call_and_wait(),
            )
            .map_err(|error| format!("update {method}: {error}"))?;
        decode_result(&response, method)
    }
}

fn resolve_network(network: &str, fetch_root_key: bool) -> Result<(&str, bool), String> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL, false)),
        "local" => Ok((DEFAULT_LOCAL_URL, true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            if !fetch_root_key {
                return Err("a custom network URL requires --fetch-root-key".to_string());
            }
            Ok((url, true))
        }
        other => Err(format!(
            "unknown network {other:?}; expected \"ic\", \"local\", or an http(s) URL"
        )),
    }
}

/// Decode one candid `Result<T, E>` payload, keeping the `Err` variant typed so callers can
/// distinguish domain errors (e.g. `NotFound`) from transport failures.
fn decode_result<T, E>(response: &[u8], method: &str) -> Result<Result<T, E>, String>
where
    T: CandidType + for<'de> serde::Deserialize<'de>,
    E: CandidType + for<'de> serde::Deserialize<'de>,
{
    // Decode the `Result` envelope directly. Round-tripping through `IDLArgs`/`IDLValue` is
    // lossy: it re-encodes `None` options as `opt empty` and collapses variants to their single
    // observed member, so `Decode!` can no longer match record types that contain options or
    // multi-member variants (e.g. `list_schema_migrations`).
    Decode!(response, Result<T, E>).map_err(|error| format!("decode {method} response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_migration_api::{
        ListSchemaMigrationsResult, ListSchemaMigrationsResultV1, ResolvedSchemaMigrationGraph,
        SchemaMigrationChecksum, SchemaMigrationChecksumAlgorithm, SchemaMigrationGraphSelector,
        SchemaMigrationRecord, SchemaMigrationRecordState, SchemaMigrationRecordV1,
        SchemaMigrationStatementProfile,
    };

    fn sample_result() -> Result<ListSchemaMigrationsResult, RouterError> {
        Ok(ListSchemaMigrationsResult::V1(
            ListSchemaMigrationsResultV1 {
                migrations: vec![
                    SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                        id: "000001_init".into(),
                        parent: None,
                        graph_selector: SchemaMigrationGraphSelector::Default,
                        resolved_graph: None,
                        checksum: SchemaMigrationChecksum {
                            algorithm: SchemaMigrationChecksumAlgorithm::Sha256,
                            digest: vec![7; 32],
                        },
                        actor: candid::Principal::anonymous(),
                        recorded_at: 1,
                        statement: "CREATE GRAPH TYPE Social {}".into(),
                        profile: SchemaMigrationStatementProfile::CreateGraphType,
                        state: SchemaMigrationRecordState::Applied { applied_at: 2 },
                    }),
                    SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                        id: "000002_graph".into(),
                        parent: Some("000001_init".into()),
                        graph_selector: SchemaMigrationGraphSelector::Named("social".into()),
                        resolved_graph: Some(ResolvedSchemaMigrationGraph {
                            graph_id: GraphId::from_raw(1),
                            graph_name: "social".into(),
                        }),
                        checksum: SchemaMigrationChecksum {
                            algorithm: SchemaMigrationChecksumAlgorithm::Sha256,
                            digest: vec![8; 32],
                        },
                        actor: candid::Principal::anonymous(),
                        recorded_at: 3,
                        statement: "CREATE GRAPH social TYPED Social".into(),
                        profile: SchemaMigrationStatementProfile::CreateTypedGraph,
                        state: SchemaMigrationRecordState::Applied { applied_at: 4 },
                    }),
                ],
                next_start_after: None,
            },
        ))
    }

    #[test]
    fn decode_result_preserves_records_with_options_and_variants() {
        // Regression: the previous IDLArgs/IDLValue round-trip re-encoded `None` options as
        // `opt empty` and collapsed multi-member variants to their observed member, so any
        // record containing options or variants (e.g. `list_schema_migrations`) failed to
        // decode even when the payload was produced by the same candid version.
        let bytes = Encode!(&sample_result()).expect("encode result");
        let decoded = decode_result::<ListSchemaMigrationsResult, RouterError>(&bytes, "probe")
            .expect("decode");
        match decoded {
            Ok(value) => assert_eq!(value, sample_result().expect("sample")),
            Err(error) => panic!("decoded Err variant: {error:?}"),
        }
    }

    #[test]
    fn decode_result_preserves_typed_err_variant() {
        let bytes = Encode!(&Err::<ListSchemaMigrationsResult, RouterError>(
            RouterError::NotFound("social".into())
        ))
        .expect("encode err");
        let decoded = decode_result::<ListSchemaMigrationsResult, RouterError>(&bytes, "probe")
            .expect("decode");
        assert!(matches!(
            decoded,
            Err(RouterError::NotFound(name)) if name == "social"
        ));
    }

    #[test]
    fn multi_arg_encoding_emits_one_argument_per_element() {
        // Regression: `Encode!(&tuple)` encodes ONE record argument (a Rust tuple is a Candid
        // record), so multi-argument Router methods (ensure_properties, bulk_load_status) must
        // be called through `query_args`/`update_args`, which encode each tuple element as a
        // separate Candid argument.
        let tuple_bytes =
            Encode!(&("social".to_string(), vec!["user_id".to_string()])).expect("encode tuple");
        assert!(
            candid::decode_args::<(String, Vec<String>)>(&tuple_bytes).is_err(),
            "a tuple passed as one CandidType value must not decode as two arguments"
        );

        let args_bytes = candid::encode_args((&"social".to_string(), &vec!["user_id".to_string()]))
            .expect("encode args");
        let decoded = candid::decode_args::<(String, Vec<String>)>(&args_bytes).expect("decode");
        assert_eq!(decoded, ("social".to_string(), vec!["user_id".to_string()]));
    }
}
