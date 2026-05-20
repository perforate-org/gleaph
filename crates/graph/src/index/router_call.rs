//! Blocking graph → router inter-canister calls on Wasm (`ic-cdk` async `Call` API).

use candid::Principal;

fn map_call_err(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

pub(crate) fn call_router0<R: candid::CandidType + for<'de> candid::Deserialize<'de>>(
    router: Principal,
    method: &'static str,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    pollster::block_on(async move {
        Call::unbounded_wait(router, method)
            .await
            .map_err(map_call_err)?
            .candid()
            .map_err(|e| format!("candid decode: {e}"))
    })
}

pub(crate) fn call_router1<
    T: candid::CandidType,
    R: candid::CandidType + for<'de> candid::Deserialize<'de>,
>(
    router: Principal,
    method: &'static str,
    arg: T,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    pollster::block_on(async move {
        Call::unbounded_wait(router, method)
            .with_arg(&arg)
            .await
            .map_err(map_call_err)?
            .candid()
            .map_err(|e| format!("candid decode: {e}"))
    })
}

pub(crate) fn call_router2<
    T0: candid::CandidType,
    T1: candid::CandidType,
    R: candid::CandidType + for<'de> candid::Deserialize<'de>,
>(
    router: Principal,
    method: &'static str,
    arg0: T0,
    arg1: T1,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    pollster::block_on(async move {
        Call::unbounded_wait(router, method)
            .with_args(&(arg0, arg1))
            .await
            .map_err(map_call_err)?
            .candid()
            .map_err(|e| format!("candid decode: {e}"))
    })
}
