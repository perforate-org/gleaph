//! PocketIC cost benchmark for the additive general route (ADR 0114).
//!
//! The canister is built with `--features bench`; `bench_route_general` returns the route result and
//! the measured `performance_counter(0)` delta. Grid: 6/10/20/50 operations x 32/96/256-token
//! questions. Missing wasm/server skips; a present cell over the hard cap fails.

use std::path::{Path, PathBuf};

use candid::{Decode, Encode, Principal};
use gleaph_routing_canister::GeneralRouteResult;
use gleaph_routing_wire::{
    GeneralEdge, GeneralGraphSchema, GeneralLabel, GeneralOperation, GeneralParameter,
    GeneralProperty, GeneralRouteRequest, GeneralRoutingContext, general_context_fingerprint,
};
use pocket_ic::PocketIcBuilder;

fn artifact(relative: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    path.exists().then_some(path)
}

fn wasm() -> Option<Vec<u8>> {
    artifact("target/wasm32-unknown-unknown/release/gleaph_routing_canister.wasm")
        .map(|path| std::fs::read(path).expect("read canister wasm"))
}

fn context(operations: usize) -> GeneralRoutingContext {
    let mut cards = vec![
        operation(
            "films-by-year",
            "List films whose year is in a range and whose genre matches.",
            &[("from", "int"), ("to", "int"), ("genre", "string")],
        ),
        operation(
            "synopsis-search",
            "Search film synopses for a text query.",
            &[("query", "string")],
        ),
        operation(
            "similar-films",
            "Find films similar to an anchor film.",
            &[("anchor", "string")],
        ),
        operation(
            "common-cast",
            "Find cast members shared by two films.",
            &[("left", "string"), ("right", "string")],
        ),
        operation(
            "sequel-chain",
            "List the sequel chain reachable from a film.",
            &[("start", "string"), ("hops", "int")],
        ),
        operation(
            "team-readable-films",
            "List films a team can read.",
            &[("team", "string"), ("topic", "string")],
        ),
    ];
    while cards.len() < operations {
        let index = cards.len();
        cards.push(operation(
            &format!("synopsis-search-alt{index}"),
            "Search film synopses for a text query in an alternative catalog entry.",
            &[("query", "string")],
        ));
    }
    let mut context = GeneralRoutingContext {
        schema_version: 1,
        graph: GeneralGraphSchema {
            labels: vec![GeneralLabel {
                name: "Film".into(),
                properties: vec![GeneralProperty {
                    name: "title".into(),
                    property_type: "string".into(),
                }],
            }],
            edges: vec![GeneralEdge {
                name: "SIMILAR_TO".into(),
                from_label: "Film".into(),
                to_label: "Film".into(),
            }],
        },
        operations: cards,
        abstain_option: "none-of-these".into(),
        abstain_threshold: 0.2,
        policy_version: 1,
        context_fingerprint: vec![],
    };
    context.context_fingerprint = general_context_fingerprint(&context);
    context
}

fn operation(id: &str, description: &str, parameters: &[(&str, &str)]) -> GeneralOperation {
    GeneralOperation {
        id: id.into(),
        description: description.into(),
        parameters: parameters
            .iter()
            .map(|(name, kind)| GeneralParameter {
                name: (*name).into(),
                parameter_type: (*kind).into(),
                required: true,
                domain: Vec::new(),
            })
            .collect(),
        result: vec!["title".into()],
    }
}

fn question(tokens: usize) -> String {
    let words = [
        "find",
        "films",
        "about",
        "artificial",
        "intelligence",
        "from",
        "the",
        "1990s",
        "similar",
        "to",
        "arrival",
        "starring",
        "actors",
        "who",
        "appear",
        "in",
        "both",
    ];
    let mut text = String::new();
    while text.split_whitespace().count() < tokens {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(words[text.split_whitespace().count() % words.len()]);
    }
    text
}

#[test]
fn general_route_instruction_grid() {
    let (Some(wasm), Some(server)) = (
        wasm(),
        std::env::var_os("POCKET_IC_BIN")
            .map(PathBuf::from)
            .filter(|path| path.exists())
            .or_else(|| artifact("crates/pocket-ic-tests/.pocket-ic/pocket-ic-launcher")),
    ) else {
        eprintln!("skipping: bench wasm or PocketIC server not present");
        return;
    };
    let pic = PocketIcBuilder::new()
        .with_server_binary(server)
        .with_application_subnet()
        .build();
    let controller = Principal::from_slice(&[0x51; 29]);
    let canister = pic.create_canister();
    pic.add_cycles(canister, 200_000_000_000_000);
    pic.set_controllers(canister, None, vec![controller])
        .expect("controllers");
    pic.install_canister(
        canister,
        wasm.clone(),
        Encode!(&()).unwrap(),
        Some(controller),
    );
    let memory_before = pic
        .canister_status(canister, Some(controller))
        .expect("status")
        .memory_size
        .clone();

    println!(
        "wasm_bytes={} sizes=6,10,20,50 tokens=32,96,256",
        wasm.len()
    );
    println!("ops tokens instructions context_bytes max_probability operation abstain");
    let mut cells = Vec::new();
    for operations in [6usize, 10, 20, 50] {
        for tokens in [32usize, 96, 256] {
            let request = GeneralRouteRequest {
                text: question(tokens),
                context: context(operations),
            };
            let reply = pic
                .query_call(
                    canister,
                    controller,
                    "bench_route_general",
                    Encode!(&request).expect("encode request"),
                )
                .unwrap_or_else(|error| {
                    panic!("bench call ops={operations} tokens={tokens}: {error}")
                });
            let (result, instructions): (GeneralRouteResult, u64) =
                Decode!(&reply, Result<(GeneralRouteResult, u64), String>)
                    .expect("decode bench result")
                    .unwrap_or_else(|error| panic!("bench result: {error}"));
            let context_bytes = Encode!(&request.context).expect("encode context").len();
            let maximum = result.probabilities.iter().copied().fold(0.0f32, f32::max);
            println!(
                "{operations} {tokens} {instructions} {context_bytes} {maximum:.6} {:?} {}",
                result.operation, result.abstain
            );
            assert!(
                instructions <= 5_000_000_000,
                "query budget exceeded at {operations}/{tokens}: {instructions}"
            );
            cells.push((operations, tokens, instructions));
        }
    }
    let memory_after = pic
        .canister_status(canister, Some(controller))
        .expect("status")
        .memory_size
        .clone();
    println!("memory_before={memory_before} memory_after={memory_after}");
    let base = cells
        .iter()
        .find(|(ops, tokens, _)| *ops == 6 && *tokens == 32)
        .unwrap()
        .2;
    let largest = cells
        .iter()
        .find(|(ops, tokens, _)| *ops == 50 && *tokens == 256)
        .unwrap()
        .2;
    println!("ratio_50_256_over_6_32={:.2}", largest as f64 / base as f64);
    assert!(
        largest <= base * 10,
        "50-card/256-token cost is more than 10x the 6-card/32-token cost"
    );
}
