//! Application-owned public read gateway for the social demo.
//!
//! Exposes exactly three fixed read-only scenarios. Each scenario maps internally to a
//! registered Router prepared query and is delegated to the configured Router canister as a
//! bounded-wait composite query. The Gateway principal is graph-visible so the Router can resolve
//! the prepared plan; no arbitrary GQL, query name, graph name, or client parameters are accepted.

use candid::{CandidType, Deserialize, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use ic_cdk_macros::{init, post_upgrade, query};
use std::cell::RefCell;

/// Fixed public scenarios. The Gateway maps each variant to a single prepared-query name
/// registered by the deploy administrator; callers cannot supply GQL, names, or parameters.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SocialDemoScenario {
    PublicTimeline,
    AliceHomeFeed,
    TopicPath,
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

fn scenario_to_name(scenario: SocialDemoScenario) -> &'static str {
    match scenario {
        SocialDemoScenario::PublicTimeline => "public_timeline",
        SocialDemoScenario::AliceHomeFeed => "alice_home_feed",
        SocialDemoScenario::TopicPath => "topic_path_explanation",
    }
}

/// Public composite query: execute one fixed social-demo scenario through the configured Router.
#[query(composite = true)]
async fn execute_social_demo_scenario(
    scenario: SocialDemoScenario,
) -> Result<GqlQueryResult, SocialDemoGatewayError> {
    let router = router_canister()?;
    let name = scenario_to_name(scenario);
    let params: Vec<u8> = Vec::new();

    let encoded_args =
        candid::utils::encode_args((name.to_string(), params)).expect("encode prepared args");
    let call_result = ic_cdk::call::Call::bounded_wait(router, "prepared_execute_query")
        .with_raw_args(&encoded_args)
        .await;

    match call_result {
        Ok(response) => {
            let decoded: Result<GqlQueryResult, RouterError> = response.candid().map_err(|e| {
                SocialDemoGatewayError::CallFailed(format!("decode router response: {e}"))
            })?;
            decoded.map_err(SocialDemoGatewayError::Router)
        }
        Err(err) => Err(SocialDemoGatewayError::CallFailed(format!(
            "router call failed: {err}"
        ))),
    }
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
            scenario_to_name(SocialDemoScenario::PublicTimeline),
            "public_timeline"
        );
        assert_eq!(
            scenario_to_name(SocialDemoScenario::AliceHomeFeed),
            "alice_home_feed"
        );
        assert_eq!(
            scenario_to_name(SocialDemoScenario::TopicPath),
            "topic_path_explanation"
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
