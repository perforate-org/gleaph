//! PocketIC test for a `StableBTreeMap` over the verus-ic-backed `Memory` adapter.
//!
//! Requires `POCKET_IC_BIN` and a prebuilt map-canister wasm:
//!   cargo build --target wasm32-unknown-unknown --release -p verus-stable64-map-canister

use candid::{Decode, Encode, Principal};
use pocket_ic::PocketIcBuilder;
use std::path::PathBuf;

fn wasm_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/verus_stable64_map_canister.wasm")
}

#[test]
fn map_survives_upgrade_over_verus_memory() {
    let server = std::env::var("POCKET_IC_BIN")
        .expect("set POCKET_IC_BIN to a PocketIC server binary before running this test");
    let pic = PocketIcBuilder::new()
        .with_server_binary(server.into())
        .with_application_subnet()
        .build();
    let canister = pic.create_canister();
    pic.add_cycles(canister, 10_000_000_000_000);

    let wasm = std::fs::read(wasm_path()).unwrap_or_else(|e| {
        panic!(
            "build the map fixture wasm first ({}): {e}",
            wasm_path().display()
        )
    });
    pic.install_canister(
        canister,
        wasm.clone(),
        Encode!(&()).expect("encode init"),
        None,
    );

    let caller = Principal::anonymous();
    pic.update_call(
        canister,
        caller,
        "put",
        Encode!(&7u64, &42u64).expect("encode put"),
    )
    .expect("put update");

    let get = |pic: &pocket_ic::PocketIc| -> Option<u64> {
        let out = pic
            .query_call(canister, caller, "get", Encode!(&7u64).expect("encode get"))
            .expect("get query");
        Decode!(&out, Option<u64>).expect("decode get")
    };
    assert_eq!(get(&pic), Some(42));

    let len = |pic: &pocket_ic::PocketIc| -> u64 {
        let out = pic
            .query_call(canister, caller, "len", Encode!(&()).expect("encode len"))
            .expect("len query");
        Decode!(&out, u64).expect("decode len")
    };
    assert_eq!(len(&pic), 1);

    pic.upgrade_canister(canister, wasm, Encode!(&()).expect("encode upgrade"), None)
        .expect("upgrade canister");

    assert_eq!(get(&pic), Some(42));
    assert_eq!(len(&pic), 1);
}
