//! PocketIC smoke test for the `verus-stable64-canister` fixture.
//!
//! Requires `POCKET_IC_BIN` to point at a PocketIC server binary and the canister
//! wasm to be built first:
//!   cargo build --target wasm32-unknown-unknown --release -p verus-stable64-canister

use candid::{Decode, Encode, Principal};
use pocket_ic::PocketIcBuilder;
use std::path::PathBuf;

fn wasm_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/verus_stable64_canister.wasm")
}

#[test]
fn upgrade_preserves_stable_memory() {
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
            "build the fixture wasm first ({}): {e}",
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
    let out = pic
        .update_call(
            canister,
            caller,
            "grow_to_pages",
            Encode!(&2u64).expect("encode target"),
        )
        .expect("grow_to_pages update");
    assert_eq!(Decode!(&out, u64).expect("decode pages"), 2);

    pic.update_call(
        canister,
        caller,
        "write_marker",
        Encode!(&vec![1u8, 2, 3, 4]).expect("encode marker"),
    )
    .expect("write_marker update");

    let read_marker = |pic: &pocket_ic::PocketIc| -> Vec<u8> {
        let out = pic
            .query_call(
                canister,
                caller,
                "read_marker",
                Encode!(&()).expect("encode read"),
            )
            .expect("read_marker query");
        Decode!(&out, Vec<u8>).expect("decode marker")
    };
    assert_eq!(read_marker(&pic), vec![1, 2, 3, 4, 0, 0, 0, 0]);

    pic.upgrade_canister(canister, wasm, Encode!(&()).expect("encode upgrade"), None)
        .expect("upgrade canister");
    assert_eq!(read_marker(&pic), vec![1, 2, 3, 4, 0, 0, 0, 0]);

    // The upgraded canister is already at two pages: an absolute request below
    // the current size must not grow, and must report the current size.
    let out = pic
        .update_call(
            canister,
            caller,
            "grow_to_pages",
            Encode!(&1u64).expect("encode target"),
        )
        .expect("grow_to_pages after upgrade");
    assert_eq!(Decode!(&out, u64).expect("decode pages"), 2);

    let out = pic
        .query_call(
            canister,
            caller,
            "pages",
            Encode!(&()).expect("encode pages"),
        )
        .expect("pages query");
    assert_eq!(Decode!(&out, u64).expect("decode pages"), 2);
}
