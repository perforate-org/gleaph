use std::fs;
use std::path::{Path, PathBuf};

use candid::{Principal, decode_one, encode_args, encode_one};
use gleaph_algo::{bfs::BfsConfig, pagerank::PageRankConfig, recommend::RecommendConfig};
use gleaph_types::{
    BfsResult, CertifiedResponse, EdgeData, GraphStats, PageRankResult, Recommendation,
};
use ic_certification::{Certificate as IcCertificate, hash_tree::LookupResult as IcLookupResult};
use ic_certified_map::HashTree as WitnessHashTree;
use ic_verify_bls_signature::verify_bls_signature;
use pocket_ic::PocketIc;

fn wasm_path(crate_stem: &str) -> PathBuf {
    let release = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(format!("{crate_stem}.wasm"));
    if release.exists() {
        return release;
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("debug")
        .join(format!("{crate_stem}.wasm"))
}

fn load_wasm(crate_stem: &str) -> Vec<u8> {
    fs::read(wasm_path(crate_stem)).unwrap_or_else(|e| {
        panic!(
            "failed to read wasm for {crate_stem}; build it first with `cargo build -p {} --target wasm32-unknown-unknown --release` (preferred) or `cargo build -p {} --target wasm32-unknown-unknown`: {e}",
            crate_stem.replace('_', "-"),
            crate_stem.replace('_', "-")
        )
    })
}

struct CertifiedHarness {
    pic: PocketIc,
    graph_id: Principal,
    sender: Principal,
}

impl CertifiedHarness {
    fn new() -> Self {
        let pic = PocketIc::new();
        let sender = Principal::anonymous();
        let graph_id = pic.create_canister();
        pic.add_cycles(graph_id, 2_000_000_000_000);
        pic.install_canister(
            graph_id,
            load_wasm("gleaph_graph"),
            encode_args((Some(64u32), Some(0u64))).expect("graph init arg"),
            Some(sender),
        );
        Self {
            pic,
            graph_id,
            sender,
        }
    }

    fn graph_update<
        T: candid::CandidType,
        R: candid::CandidType + for<'de> candid::Deserialize<'de>,
    >(
        &self,
        method: &str,
        args: T,
    ) -> R {
        let bytes = self
            .pic
            .update_call(
                self.graph_id,
                self.sender,
                method,
                encode_args((args,)).expect("encode update args"),
            )
            .unwrap_or_else(|e| panic!("update_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode update response")
    }

    fn graph_query<
        T: candid::CandidType,
        R: candid::CandidType + for<'de> candid::Deserialize<'de>,
    >(
        &self,
        method: &str,
        args: T,
    ) -> R {
        let bytes = self
            .pic
            .query_call(
                self.graph_id,
                self.sender,
                method,
                encode_args((args,)).expect("encode query args"),
            )
            .unwrap_or_else(|e| panic!("query_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode query response")
    }

    fn graph_query0<R: candid::CandidType + for<'de> candid::Deserialize<'de>>(
        &self,
        method: &str,
    ) -> R {
        let bytes = self
            .pic
            .query_call(
                self.graph_id,
                self.sender,
                method,
                encode_args(()).expect("encode empty query args"),
            )
            .unwrap_or_else(|e| panic!("query_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode query response")
    }

    fn graph_query_args<R: candid::CandidType + for<'de> candid::Deserialize<'de>>(
        &self,
        method: &str,
        encoded_args: Vec<u8>,
    ) -> R {
        let bytes = self
            .pic
            .query_call(self.graph_id, self.sender, method, encoded_args)
            .unwrap_or_else(|e| panic!("query_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode query response")
    }
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn get_stats_certified_returns_certificate_and_cbor_witness() {
    let h = CertifiedHarness::new();
    let _: Result<u64, String> = h.graph_update(
        "add_edge",
        EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 10,
        },
    );

    let r1: CertifiedResponse<GraphStats> = h.graph_query0("get_stats_certified");
    let r2: CertifiedResponse<GraphStats> = h.graph_query0("get_stats_certified");

    assert!(
        !r1.certificate.is_empty(),
        "data certificate should be present"
    );
    assert!(!r1.witness.is_empty(), "witness should be present");
    assert_eq!(r1.data.num_edges, 1);
    assert_eq!(
        r1.witness, r2.witness,
        "witness should be stable for unchanged state"
    );

    let witness_tree: WitnessHashTree<'_> =
        serde_cbor::from_slice(&r1.witness).expect("witness must be valid CBOR HashTree");
    assert!(
        witness_contains_label_prefix(&witness_tree, b"gleaph/v1/"),
        "witness should contain Gleaph key namespace labels"
    );
    assert!(
        witness_contains_leaf_prefix(&witness_tree, b"GLEAPH_CERT"),
        "witness should contain canonical leaf payloads"
    );
    maybe_assert_certificate_signature_valid(h.pic.root_key(), h.graph_id, &r1.certificate);
    assert_witness_matches_canister_certified_data(h.graph_id, &r1.certificate, &r1.witness);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn bfs_query_returns_expected_path() {
    let h = CertifiedHarness::new();
    let edges = vec![
        EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        },
        EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        },
        EdgeData {
            src: 2,
            dst: 3,
            weight: 1.0,
            timestamp: 3,
        },
    ];
    let _: Result<u64, String> = h.graph_update("bulk_insert_edges", edges);

    let res: Result<BfsResult, gleaph_types::GleaphError> = h.graph_query_args(
        "bfs",
        encode_args((
            0u32,
            BfsConfig {
                target: Some(3),
                max_depth: Some(4),
                ..Default::default()
            },
        ))
        .expect("encode bfs args"),
    );
    let res = res.expect("bfs query should succeed");
    assert_eq!(res.path, Some(vec![0, 1, 2, 3]));
    assert!(res.visited.contains(&3));
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn recommend_query_returns_collaborative_candidates() {
    let h = CertifiedHarness::new();
    let edges = vec![
        EdgeData {
            src: 10,
            dst: 100,
            weight: 1.0,
            timestamp: 1,
        },
        EdgeData {
            src: 20,
            dst: 100,
            weight: 1.0,
            timestamp: 2,
        },
        EdgeData {
            src: 20,
            dst: 200,
            weight: 1.0,
            timestamp: 3,
        },
        EdgeData {
            src: 30,
            dst: 200,
            weight: 1.0,
            timestamp: 4,
        },
        EdgeData {
            src: 30,
            dst: 300,
            weight: 1.0,
            timestamp: 50,
        },
    ];
    let _: Result<u64, String> = h.graph_update("bulk_insert_edges", edges);

    let recs: Result<Vec<Recommendation>, gleaph_types::GleaphError> = h.graph_query_args(
        "recommend",
        encode_args((
            10u32,
            RecommendConfig {
                edge_label: "".into(),
                max_hops: 4,
                limit: 10,
                ts_range: Some(gleaph_types::TimestampRange {
                    start: Some(0),
                    end: Some(10),
                }),
                exclude_known: true,
            },
        ))
        .expect("encode recommend args"),
    );
    let recs = recs.expect("recommend query should succeed");
    assert!(
        recs.iter().any(|r| r.vertex_id == 200),
        "expected collaborative recommendation"
    );
    assert!(
        recs.iter().all(|r| r.vertex_id != 100),
        "exclude_known should not return already-owned item"
    );
    assert!(
        recs.iter().all(|r| r.vertex_id != 300),
        "temporal window should exclude late interaction"
    );
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn pagerank_certified_witness_uses_stable_key_namespace_and_survives_upgrade() {
    let h = CertifiedHarness::new();
    let edges = vec![
        EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        },
        EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        },
        EdgeData {
            src: 2,
            dst: 0,
            weight: 1.0,
            timestamp: 3,
        },
    ];
    let _: Result<u64, String> = h.graph_update("bulk_insert_edges", edges);

    let config = PageRankConfig {
        damping: 0.85,
        max_iterations: 5,
        convergence_threshold: 1e-6,
        ts_range: None,
    };

    let compute: Result<PageRankResult, gleaph_types::GleaphError> =
        h.graph_update("compute_pagerank", config.clone());
    let compute = compute.expect("compute_pagerank");
    assert!(!compute.scores.is_empty());

    let config_hash = encode_one(&config).expect("config hash bytes");
    let certified1: Result<CertifiedResponse<PageRankResult>, gleaph_types::GleaphError> =
        h.graph_query("get_pagerank_certified", config_hash.clone());
    let certified1 = certified1.expect("get_pagerank_certified");
    assert!(!certified1.certificate.is_empty());
    assert!(!certified1.witness.is_empty());
    assert_eq!(certified1.data, compute);

    let witness1: WitnessHashTree<'_> =
        serde_cbor::from_slice(&certified1.witness).expect("witness must be valid CBOR HashTree");
    assert!(
        witness_contains_label_prefix(&witness1, b"gleaph/v1/algo/pagerank/"),
        "witness should contain pagerank key namespace label"
    );
    assert!(witness_contains_leaf_prefix(&witness1, b"GLEAPH_CERT"));
    maybe_assert_certificate_signature_valid(h.pic.root_key(), h.graph_id, &certified1.certificate);
    assert_witness_matches_canister_certified_data(
        h.graph_id,
        &certified1.certificate,
        &certified1.witness,
    );

    h.pic
        .upgrade_canister(
            h.graph_id,
            load_wasm("gleaph_graph"),
            encode_args((Option::<u32>::None, Option::<u64>::None)).expect("upgrade arg"),
            Some(h.sender),
        )
        .expect("upgrade graph canister");

    let certified2: Result<CertifiedResponse<PageRankResult>, gleaph_types::GleaphError> =
        h.graph_query("get_pagerank_certified", config_hash);
    let resp = certified2.expect("pagerank certified cache should persist across upgrade");
    let witness2: WitnessHashTree<'_> =
        serde_cbor::from_slice(&resp.witness).expect("CBOR witness");
    assert!(witness_contains_label_prefix(
        &witness2,
        b"gleaph/v1/algo/pagerank/"
    ));
    maybe_assert_certificate_signature_valid(h.pic.root_key(), h.graph_id, &resp.certificate);
    assert_witness_matches_canister_certified_data(h.graph_id, &resp.certificate, &resp.witness);
    assert_eq!(
        resp.data, compute,
        "cached pagerank result should persist across upgrade"
    );
}

fn assert_witness_matches_canister_certified_data(
    canister_id: Principal,
    certificate_bytes: &[u8],
    witness_bytes: &[u8],
) {
    let cert: IcCertificate =
        serde_cbor::from_slice(certificate_bytes).expect("valid IC certificate CBOR");
    let certified_data = match cert.tree.lookup_path([
        b"canister".as_slice(),
        canister_id.as_slice(),
        b"certified_data".as_slice(),
    ]) {
        IcLookupResult::Found(bytes) => bytes,
        other => panic!("certified_data not found in certificate tree: {other:?}"),
    };

    let witness: ic_certified_map::HashTree<'_> =
        serde_cbor::from_slice(witness_bytes).expect("valid witness hash tree CBOR");
    let witness_root = witness.reconstruct();
    assert_eq!(
        certified_data,
        &witness_root[..],
        "witness root digest must match certificate certified_data"
    );
}

fn witness_contains_label_prefix(tree: &WitnessHashTree<'_>, prefix: &[u8]) -> bool {
    match tree {
        WitnessHashTree::Labeled(label, child) => {
            label.starts_with(prefix) || witness_contains_label_prefix(child, prefix)
        }
        WitnessHashTree::Fork(children) => {
            witness_contains_label_prefix(&children.0, prefix)
                || witness_contains_label_prefix(&children.1, prefix)
        }
        _ => false,
    }
}

fn witness_contains_leaf_prefix(tree: &WitnessHashTree<'_>, prefix: &[u8]) -> bool {
    match tree {
        WitnessHashTree::Leaf(bytes) => bytes.as_ref().starts_with(prefix),
        WitnessHashTree::Labeled(_, child) => witness_contains_leaf_prefix(child, prefix),
        WitnessHashTree::Fork(children) => {
            witness_contains_leaf_prefix(&children.0, prefix)
                || witness_contains_leaf_prefix(&children.1, prefix)
        }
        _ => false,
    }
}

fn assert_certificate_signature_valid(
    root_key_der: &[u8],
    effective_canister_id: Principal,
    certificate_bytes: &[u8],
) {
    let cert: IcCertificate =
        serde_cbor::from_slice(certificate_bytes).expect("valid certificate cbor");
    let root_key_raw = extract_bls_der_key(root_key_der).expect("valid DER root key");
    verify_cert_with_root_key(&root_key_raw, effective_canister_id, &cert);
}

fn maybe_assert_certificate_signature_valid(
    root_key_der: Option<Vec<u8>>,
    effective_canister_id: Principal,
    certificate_bytes: &[u8],
) {
    match root_key_der {
        Some(root_key) => {
            assert_certificate_signature_valid(&root_key, effective_canister_id, certificate_bytes)
        }
        None => {
            eprintln!(
                "PocketIC root key is unavailable in this environment; skipping certificate signature verification and validating witness/root-hash consistency only"
            );
        }
    }
}

fn verify_cert_with_root_key(
    root_key_raw: &[u8],
    effective_canister_id: Principal,
    cert: &IcCertificate,
) {
    let signing_key = if let Some(delegation) = &cert.delegation {
        let delegated_cert: IcCertificate =
            serde_cbor::from_slice(&delegation.certificate).expect("valid delegated certificate");
        verify_cert_with_root_key(root_key_raw, effective_canister_id, &delegated_cert);
        assert_canister_in_delegation_ranges(
            &delegated_cert,
            delegation.subnet_id.as_ref(),
            effective_canister_id,
        );
        let pk = lookup_path_bytes(
            &delegated_cert,
            &[
                b"subnet".as_slice(),
                delegation.subnet_id.as_ref(),
                b"public_key".as_slice(),
            ],
        )
        .expect("delegated subnet public_key");
        extract_bls_der_key(pk).expect("delegated subnet DER public key")
    } else {
        root_key_raw.to_vec()
    };

    let mut msg = Vec::with_capacity(14 + 32);
    msg.extend_from_slice(b"\x0Dic-state-root");
    msg.extend_from_slice(&cert.tree.digest());
    verify_bls_signature(&cert.signature, &msg, &signing_key)
        .expect("valid BLS certificate signature");
}

fn lookup_path_bytes<'a>(cert: &'a IcCertificate, path: &[&[u8]]) -> Option<&'a [u8]> {
    match cert.tree.lookup_path(path.iter().copied()) {
        IcLookupResult::Found(bytes) => Some(bytes),
        _ => None,
    }
}

fn extract_bls_der_key(der: &[u8]) -> Result<Vec<u8>, String> {
    // Copied from ic-agent's internal `extract_der` logic to avoid depending on a private module.
    const DER_PREFIX: &[u8; 37] = b"\x30\x81\x82\x30\x1d\x06\x0d\x2b\x06\x01\x04\x01\x82\xdc\x7c\x05\x03\x01\x02\x01\x06\x0c\x2b\x06\x01\x04\x01\x82\xdc\x7c\x05\x03\x02\x01\x03\x61\x00";
    const KEY_LENGTH: usize = 96;
    let expected_length = DER_PREFIX.len() + KEY_LENGTH;
    if der.len() != expected_length {
        return Err(format!(
            "DER key length mismatch: expected {expected_length}, got {}",
            der.len()
        ));
    }
    if &der[..DER_PREFIX.len()] != DER_PREFIX {
        return Err("DER prefix mismatch".into());
    }
    Ok(der[DER_PREFIX.len()..].to_vec())
}

fn assert_canister_in_delegation_ranges(
    delegated_cert: &IcCertificate,
    subnet_id: &[u8],
    canister_id: Principal,
) {
    let raw = lookup_path_bytes(
        delegated_cert,
        &[
            b"subnet".as_slice(),
            subnet_id,
            b"canister_ranges".as_slice(),
        ],
    )
    .expect("delegated certificate must contain subnet/<id>/canister_ranges");

    let ranges: Vec<(Principal, Principal)> =
        serde_cbor::from_slice(raw).expect("valid CBOR canister_ranges");
    assert!(
        principal_in_ranges(canister_id, &ranges),
        "effective canister {} not covered by delegated subnet canister_ranges {:?}",
        canister_id,
        ranges
    );
}

fn principal_in_ranges(target: Principal, ranges: &[(Principal, Principal)]) -> bool {
    let t = target.as_slice();
    ranges.iter().any(|(start, end)| {
        let s = start.as_slice();
        let e = end.as_slice();
        s <= t && t <= e
    })
}
