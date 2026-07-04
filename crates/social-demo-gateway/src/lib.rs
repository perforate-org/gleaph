//! Application-owned public read gateway for the social demo.
//!
//! Exposes exactly five fixed read-only scenarios. Each scenario maps internally to a
//! registered Router prepared query and is delegated to the configured Router canister as a
//! bounded-wait composite query. The Gateway principal is graph-visible so the Router can resolve
//! the prepared plan; no arbitrary GQL, query name, graph name, or client parameters are accepted.
//!
//! Semantic scenarios carry an internally generated, fixed query-vector parameter blob encoded
//! with the `gleaph-gql-ic` parameter wire format. The vector is a scenario input, not a second
//! copy of stored Post embeddings, and is never exposed to callers.

use candid::{CandidType, Deserialize, Principal};
use gleaph_cdk::call_prepared_query;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use ic_cdk_macros::{init, post_upgrade, query};
use std::cell::RefCell;

/// Fixed public scenarios. The Gateway maps each variant to a single prepared-query name
/// registered by the deploy administrator; callers cannot supply GQL, names, graph names,
/// raw vectors, or parameters.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SocialDemoScenario {
    PublicTimeline,
    AliceHomeFeed,
    TopicPath,
    SemanticDiscovery,
    AliceSemanticFeed,
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GatewayInitArgs {
    pub router_canister: Principal,
}

/// Errors that originate in the Gateway boundary, distinct from errors returned by Router.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SocialDemoGatewayError {
    /// Router rejected the prepared query.
    Router(RouterError),
    /// Inter-canister call failed (rejected, timed out, or could not be decoded).
    CallFailed(String),
    /// Gateway has not been initialized with a Router principal.
    NotConfigured,
}

thread_local! {
    static ROUTER_CANISTER: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

/// Fixed dimension of the deterministic Post embedding vectors used by the semantic scenarios.
pub const SEMANTIC_EMBEDDING_DIMS: u16 = 8;

/// Fixed embedding name shared by all canonical Post embeddings and the vector index.
pub const SEMANTIC_EMBEDDING_NAME: &str = "post_vec";

#[init]
fn init(args: GatewayInitArgs) {
    do_init(args);
}

#[post_upgrade]
fn post_upgrade(args: GatewayInitArgs) {
    do_init(args);
}

fn do_init(args: GatewayInitArgs) {
    validate_router_principal(args.router_canister).unwrap_or_else(|e| ic_cdk::trap(&e));
    ROUTER_CANISTER.with(|rc| *rc.borrow_mut() = Some(args.router_canister));
}

fn validate_router_principal(router_canister: Principal) -> Result<(), String> {
    if router_canister == Principal::anonymous() {
        return Err("router_canister must not be anonymous".to_string());
    }
    Ok(())
}

fn router_canister() -> Result<Principal, SocialDemoGatewayError> {
    ROUTER_CANISTER.with(|rc| rc.borrow().ok_or(SocialDemoGatewayError::NotConfigured))
}

/// Fixed query vector for the semantic scenarios. The L2-squared distance ordering is designed
/// so that `post-dave-1` (an author Alice does not follow) is the globally nearest public Post,
/// while the followed-author Posts are deliberately ordered `post-bob-2`, `post-carol-1`,
/// `post-bob-1`.
fn semantic_query_vector() -> Vec<f32> {
    vec![8.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
}

/// Encode the fixed semantic query vector as a compact-binary GQL parameter blob.
fn encode_semantic_query_params() -> Vec<u8> {
    let bytes: Vec<u8> = semantic_query_vector()
        .into_iter()
        .flat_map(|v| v.to_le_bytes().to_vec())
        .collect();
    gleaph_gql_ic::encode_gql_params_blob(vec![(
        "query".to_string(),
        gleaph_gql::Value::Bytes(bytes),
    )])
    .expect("fixed semantic query vector encodes")
}

/// Internal mapping from scenario to prepared-query name and parameter blob.
fn scenario_to_request(scenario: SocialDemoScenario) -> (&'static str, Vec<u8>) {
    match scenario {
        SocialDemoScenario::PublicTimeline => ("public_timeline", Vec::new()),
        SocialDemoScenario::AliceHomeFeed => ("alice_home_feed", Vec::new()),
        SocialDemoScenario::TopicPath => ("topic_path_explanation", Vec::new()),
        SocialDemoScenario::SemanticDiscovery => {
            ("semantic_discovery", encode_semantic_query_params())
        }
        SocialDemoScenario::AliceSemanticFeed => {
            ("alice_semantic_feed", encode_semantic_query_params())
        }
    }
}

/// Public composite query: execute one fixed social-demo scenario through the configured Router.
#[query(composite = true)]
async fn execute_social_demo_scenario(
    scenario: SocialDemoScenario,
) -> Result<GqlQueryResult, SocialDemoGatewayError> {
    let router = router_canister()?;
    let (name, params) = scenario_to_request(scenario);

    let router_result: Result<GqlQueryResult, RouterError> =
        call_prepared_query(router, name, params)
            .await
            .map_err(|e| SocialDemoGatewayError::CallFailed(e.to_string()))?;
    router_result.map_err(SocialDemoGatewayError::Router)
}

ic_cdk::export_candid!();

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_config() {
        ROUTER_CANISTER.with(|rc| *rc.borrow_mut() = None);
    }

    #[test]
    fn scenario_names_match_expected_prepared_queries() {
        assert_eq!(
            scenario_to_request(SocialDemoScenario::PublicTimeline).0,
            "public_timeline"
        );
        assert_eq!(
            scenario_to_request(SocialDemoScenario::AliceHomeFeed).0,
            "alice_home_feed"
        );
        assert_eq!(
            scenario_to_request(SocialDemoScenario::TopicPath).0,
            "topic_path_explanation"
        );
        assert_eq!(
            scenario_to_request(SocialDemoScenario::SemanticDiscovery).0,
            "semantic_discovery"
        );
        assert_eq!(
            scenario_to_request(SocialDemoScenario::AliceSemanticFeed).0,
            "alice_semantic_feed"
        );
    }

    #[test]
    fn semantic_query_params_decode_to_fixed_vector() {
        let (_, params) = scenario_to_request(SocialDemoScenario::SemanticDiscovery);
        let decoded = gleaph_gql_ic::decode_gql_params_blob(&params).expect("decode params");
        assert_eq!(decoded.len(), 1);
        let query = match decoded.get("query").expect("query parameter") {
            gleaph_gql::Value::Bytes(b) => b.clone(),
            other => panic!("expected Bytes query parameter, got {other:?}"),
        };
        assert_eq!(query.len(), SEMANTIC_EMBEDDING_DIMS as usize * 4);
        let values: Vec<f32> = query
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("4 bytes")))
            .collect();
        assert_eq!(values, semantic_query_vector());

        let (_, alice_params) = scenario_to_request(SocialDemoScenario::AliceSemanticFeed);
        assert_eq!(
            alice_params, params,
            "both semantic scenarios share the fixed query vector"
        );
    }

    #[test]
    fn validate_router_principal_rejects_anonymous() {
        assert_eq!(
            validate_router_principal(Principal::anonymous()),
            Err("router_canister must not be anonymous".to_string())
        );
    }

    #[test]
    fn validate_router_principal_accepts_non_anonymous() {
        let principal = Principal::from_slice(&[0xAB; 29]);
        assert_eq!(validate_router_principal(principal), Ok(()));
    }

    #[test]
    fn router_canister_fails_when_uninitialized() {
        reset_config();
        assert_eq!(
            router_canister(),
            Err(SocialDemoGatewayError::NotConfigured)
        );
    }

    #[test]
    fn init_stores_router_canister() {
        reset_config();
        let principal = Principal::from_slice(&[0xCD; 29]);
        init(GatewayInitArgs {
            router_canister: principal,
        });
        assert_eq!(router_canister(), Ok(principal));
    }
}
