//! PocketIC E2E for ADR 0115: graph tenants read/write their routing profile; non-tenants fail
//! closed. Prepared-catalog tenant registration is covered by the Router unit test because the
//! default federation graph name is dotted and cannot be referenced by `USE`.

use std::collections::BTreeSet;

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{GRAPH_NAME, install_single_shard_federation_with_graph_admins};

fn decode<T: candid::CandidType + for<'de> candid::Deserialize<'de>>(
    bytes: &[u8],
    method: &str,
) -> Result<T, RouterError> {
    let result: Result<T, RouterError> = Decode!(bytes, Result<T, RouterError>)
        .unwrap_or_else(|error| panic!("decode {method}: {error}"));
    result
}

#[test]
fn graph_tenant_manages_its_profile() {
    let owner = Principal::from_slice(&[0xC1; 29]);
    let non_tenant = Principal::from_slice(&[0xC2; 29]);
    let env = install_single_shard_federation_with_graph_admins(BTreeSet::from([owner]));
    let pic = &env.pic;
    pic.set_controllers(env.router, None, vec![env.admin])
        .expect("router controller");

    // Owner (a graph admin, not a Router controller) can set and read the profile.
    let reply = pic
        .update_call(
            env.router,
            owner,
            "set_ask_config",
            Encode!(&GRAPH_NAME.to_owned(), &2_500u32, &2u32).expect("encode profile"),
        )
        .expect("set profile");
    decode::<()>(&reply, "set profile").expect("owner sets profile");
    let reply = pic
        .query_call(
            env.router,
            owner,
            "get_ask_config",
            Encode!(&GRAPH_NAME.to_owned()).expect("encode read"),
        )
        .expect("read profile");
    let profile: (u32, u32) = Decode!(&reply, Result<(u32, u32), RouterError>)
        .expect("decode read")
        .expect("profile");
    assert_eq!(profile, (2_500, 2));

    // The raw calibration diagnostic is equally scoped: a non-tenant is rejected before any
    // inference-canister lookup.
    let reply = pic
        .query_call(
            env.router,
            non_tenant,
            "debug_interpret",
            Encode!(&GRAPH_NAME.to_owned(), &"probe".to_owned()).expect("encode raw"),
        )
        .expect("raw call");
    decode::<gleaph_router_wire::DebugInterpretation>(&reply, "debug_interpret")
        .expect_err("non-tenant raw route must fail");
    // The raw batch diagnostic is scoped the same way.
    let reply = pic
        .query_call(
            env.router,
            non_tenant,
            "debug_interpret_batch",
            Encode!(&GRAPH_NAME.to_owned(), &vec!["probe".to_owned()]).expect("encode raw batch"),
        )
        .expect("raw batch call");
    decode::<Vec<gleaph_router_wire::DebugInterpretation>>(&reply, "debug_interpret_batch")
        .expect_err("non-tenant raw batch must fail");

    // A non-tenant cannot read or write the profile.
    for (method, args) in [
        (
            "get_ask_config",
            Encode!(&GRAPH_NAME.to_owned()).expect("read args"),
        ),
        (
            "set_ask_config",
            Encode!(&GRAPH_NAME.to_owned(), &3_000u32, &2u32).expect("write args"),
        ),
    ] {
        let reply = if method == "get_ask_config" {
            pic.query_call(env.router, non_tenant, method, args)
        } else {
            pic.update_call(env.router, non_tenant, method, args)
        }
        .expect("non-tenant call");
        let result: Result<(), RouterError> = Decode!(&reply, Result<(), RouterError>)
            .unwrap_or_else(|_| {
                // Query profile returns a tuple; any rejection decodes as the RouterError branch.
                Decode!(&reply, Result<(u32, u32), RouterError>)
                    .map(|decoded| decoded.map(|_| ()))
                    .expect("decode rejection")
            });
        assert!(matches!(result, Err(RouterError::Forbidden)));
    }
}
