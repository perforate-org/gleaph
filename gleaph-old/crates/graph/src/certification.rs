use std::cell::RefCell;
use std::collections::BTreeMap;

use candid::{CandidType, encode_one};
use gleaph_types::{CertifiedResponse, GraphStats, PageRankResult, SsspResult};
use serde::Deserialize;
use serde::Serialize;

thread_local! {
    static CERT_STATS: RefCell<Option<GraphStats>> = const { RefCell::new(None) };
    static CERT_PAGERANK_CACHE: RefCell<BTreeMap<Vec<u8>, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    static CERT_SSSP_CACHE: RefCell<BTreeMap<Vec<u8>, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    static CERT_ALGO_BYTES: RefCell<BTreeMap<Vec<u8>, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    static CERT_LEAF_BYTES: RefCell<BTreeMap<Vec<u8>, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
}

#[cfg(target_arch = "wasm32")]
use ic_certified_map::{AsHashTree, HashTree, RbTree};

#[cfg(target_arch = "wasm32")]
thread_local! {
    // Phase 3 migration scaffold:
    // This tree will replace the current placeholder witness bytes once proof generation is wired.
    static CERT_TREE: RefCell<RbTree<Vec<u8>, Vec<u8>>> = RefCell::new(RbTree::new());
}

const CACHE_BLOB_VERSION: u8 = 1;
const CACHE_CODEC_RKYV: u8 = 1;
const CACHE_HEADER_LEN: usize = 8;
const CERT_KEY_PREFIX: &str = "gleaph/v1";
const LEAF_CANON_VERSION: u8 = 1;
const LEAF_FORMAT_CANDID: u8 = 1;

#[derive(Clone, Debug, Default, CandidType, Serialize, Deserialize, PartialEq)]
pub struct CertificationCacheSnapshot {
    pub pagerank_cache: Vec<(Vec<u8>, Vec<u8>)>,
    pub sssp_cache: Vec<(Vec<u8>, Vec<u8>)>,
    pub leaf_bytes: Vec<(Vec<u8>, Vec<u8>)>,
}

pub fn init_certification() {
    CERT_STATS.with(|s| *s.borrow_mut() = None);
    CERT_PAGERANK_CACHE.with(|m| m.borrow_mut().clear());
    CERT_SSSP_CACHE.with(|m| m.borrow_mut().clear());
    CERT_ALGO_BYTES.with(|m| m.borrow_mut().clear());
    CERT_LEAF_BYTES.with(|m| m.borrow_mut().clear());
    reset_cert_tree();
}

/// Evicts all algorithm result caches (PageRank, SSSP) and removes their certification leaves.
/// Must be called after any graph mutation so that stale certified algorithm results are never
/// served to callers.
pub fn invalidate_algo_caches() {
    // Collect the config-hash keys currently cached for PageRank so we can remove their leaves.
    let pr_config_hashes: Vec<Vec<u8>> =
        CERT_PAGERANK_CACHE.with(|m| m.borrow().keys().cloned().collect());

    CERT_PAGERANK_CACHE.with(|m| m.borrow_mut().clear());
    CERT_SSSP_CACHE.with(|m| m.borrow_mut().clear());
    CERT_ALGO_BYTES.with(|m| m.borrow_mut().clear());

    // Drop the per-config certification leaves that were inserted by certify_pagerank.
    // SSSP results are not inserted into CERT_LEAF_BYTES (they only go into CERT_SSSP_CACHE
    // and CERT_ALGO_BYTES), so no additional leaf removal is required for SSSP.
    CERT_LEAF_BYTES.with(|m| {
        let mut leaves = m.borrow_mut();
        for config_hash in &pr_config_hashes {
            leaves.remove(&cert_key_pagerank(config_hash));
        }
    });
    rebuild_cert_tree_from_leaves();
}

pub fn snapshot_caches() -> CertificationCacheSnapshot {
    CertificationCacheSnapshot {
        pagerank_cache: CERT_PAGERANK_CACHE.with(|m| {
            m.borrow()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
        sssp_cache: CERT_SSSP_CACHE.with(|m| {
            m.borrow()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
        leaf_bytes: CERT_LEAF_BYTES.with(|m| {
            m.borrow()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
    }
}

pub fn restore_caches(snapshot: CertificationCacheSnapshot) {
    CERT_PAGERANK_CACHE.with(|m| {
        *m.borrow_mut() = snapshot.pagerank_cache.into_iter().collect();
    });
    CERT_SSSP_CACHE.with(|m| {
        *m.borrow_mut() = snapshot.sssp_cache.into_iter().collect();
    });
    CERT_LEAF_BYTES.with(|m| {
        *m.borrow_mut() = snapshot.leaf_bytes.into_iter().collect();
    });
    rebuild_cert_tree_from_leaves();
}

pub fn certify_stats(stats: GraphStats) {
    CERT_STATS.with(|s| *s.borrow_mut() = Some(stats.clone()));
    if let Ok(bytes) = encode_one(&stats) {
        upsert_leaf(cert_key_stats(), canonical_leaf_bytes("stats", &bytes));
    }
}

pub fn certify_algo_result<T: CandidType + Serialize>(key: Vec<u8>, data: &T) {
    if let Ok(bytes) = encode_one(data) {
        let key_for_store = key.clone();
        CERT_ALGO_BYTES.with(|m| {
            m.borrow_mut().insert(key_for_store, bytes);
        });
    }
}

pub fn certify_pagerank(config_hash: Vec<u8>, result: PageRankResult) {
    cache_pagerank_rkyv(config_hash.clone(), &result);
    if let Ok(candid_bytes) = encode_one(&result) {
        upsert_leaf(
            cert_key_pagerank(&config_hash),
            canonical_leaf_bytes("algo/pagerank", &candid_bytes),
        );
    }
    certify_algo_result(config_hash, &result);
}

pub fn cache_sssp_result(cache_key: Vec<u8>, result: &SsspResult) {
    if let Some(blob) = encode_sssp_cache_blob(result) {
        CERT_SSSP_CACHE.with(|m| {
            m.borrow_mut().insert(cache_key, blob);
        });
    }
}

pub fn get_stats_certified() -> CertifiedResponse<GraphStats> {
    let data = CERT_STATS.with(|s| s.borrow().clone()).unwrap_or_default();
    CertifiedResponse {
        data,
        certificate: current_certificate(),
        witness: witness_for_key(&cert_key_stats()),
    }
}

pub fn get_pagerank_certified(config_hash: Vec<u8>) -> Option<CertifiedResponse<PageRankResult>> {
    let data = CERT_PAGERANK_CACHE.with(|m| {
        m.borrow()
            .get(&config_hash)
            .and_then(|blob| decode_pagerank_cache_blob(blob))
    })?;
    Some(CertifiedResponse {
        data,
        certificate: current_certificate(),
        witness: witness_for_key(&cert_key_pagerank(&config_hash)),
    })
}

fn current_certificate() -> Vec<u8> {
    #[cfg(target_arch = "wasm32")]
    {
        ic_cdk::api::data_certificate().unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Vec::new()
    }
}

fn cert_key_stats() -> Vec<u8> {
    format!("{CERT_KEY_PREFIX}/stats").into_bytes()
}

fn cert_key_pagerank(config_hash: &[u8]) -> Vec<u8> {
    format!("{CERT_KEY_PREFIX}/algo/pagerank/{}", hex_bytes(config_hash)).into_bytes()
}

fn canonical_leaf_bytes(kind: &str, payload: &[u8]) -> Vec<u8> {
    let kind_bytes = kind.as_bytes();
    let mut out = Vec::with_capacity(16 + kind_bytes.len() + payload.len());
    out.extend_from_slice(b"GLEAPH_CERT");
    out.push(LEAF_CANON_VERSION);
    out.push(LEAF_FORMAT_CANDID);
    out.extend_from_slice(&(kind_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(kind_bytes);
    out.extend_from_slice(payload);
    out
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn upsert_leaf(key: Vec<u8>, bytes: Vec<u8>) {
    CERT_LEAF_BYTES.with(|m| {
        m.borrow_mut().insert(key.clone(), bytes.clone());
    });
    cert_tree_insert(key, bytes);
    sync_certified_root();
}

fn rebuild_cert_tree_from_leaves() {
    reset_cert_tree();
    CERT_LEAF_BYTES.with(|m| {
        for (k, v) in m.borrow().iter() {
            cert_tree_insert(k.clone(), v.clone());
        }
    });
    sync_certified_root();
}

#[cfg(target_arch = "wasm32")]
fn cert_tree_insert(key: Vec<u8>, bytes: Vec<u8>) {
    CERT_TREE.with(|t| {
        t.borrow_mut().insert(key, bytes);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn cert_tree_insert(_key: Vec<u8>, _bytes: Vec<u8>) {}

#[cfg(target_arch = "wasm32")]
fn reset_cert_tree() {
    CERT_TREE.with(|t| *t.borrow_mut() = RbTree::new());
}

#[cfg(not(target_arch = "wasm32"))]
fn reset_cert_tree() {}

#[cfg(target_arch = "wasm32")]
fn sync_certified_root() {
    CERT_TREE.with(|t| {
        let root = t.borrow().root_hash();
        ic_cdk::api::certified_data_set(&root);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn sync_certified_root() {}

#[cfg(target_arch = "wasm32")]
fn witness_for_key(key: &[u8]) -> Vec<u8> {
    CERT_TREE.with(|t| {
        let binding = t.borrow();
        let witness: HashTree<'_> = binding.witness(key);
        serde_cbor::to_vec(&witness).unwrap_or_default()
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn witness_for_key(key: &[u8]) -> Vec<u8> {
    key.to_vec()
}

fn cache_pagerank_rkyv(config_hash: Vec<u8>, result: &PageRankResult) {
    if let Some(blob) = encode_pagerank_cache_blob(result) {
        CERT_PAGERANK_CACHE.with(|m| {
            m.borrow_mut().insert(config_hash, blob);
        });
    }
}

fn encode_pagerank_cache_blob(value: &PageRankResult) -> Option<Vec<u8>> {
    let mut out = vec![0u8; CACHE_HEADER_LEN];
    out[0] = CACHE_BLOB_VERSION;
    out[1] = CACHE_CODEC_RKYV;
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(value).ok()?;
    out.extend_from_slice(&bytes);
    Some(out)
}

fn encode_sssp_cache_blob(value: &SsspResult) -> Option<Vec<u8>> {
    let mut out = vec![0u8; CACHE_HEADER_LEN];
    out[0] = CACHE_BLOB_VERSION;
    out[1] = CACHE_CODEC_RKYV;
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(value).ok()?;
    out.extend_from_slice(&bytes);
    Some(out)
}

fn decode_pagerank_cache_blob(blob: &[u8]) -> Option<PageRankResult> {
    if blob.len() < CACHE_HEADER_LEN || blob[0] != CACHE_BLOB_VERSION || blob[1] != CACHE_CODEC_RKYV
    {
        return None;
    }
    rkyv::from_bytes::<PageRankResult, rkyv::rancor::Error>(&blob[CACHE_HEADER_LEN..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagerank_rkyv_cache_round_trip() {
        init_certification();
        let key = b"cfg".to_vec();
        let value = PageRankResult {
            scores: vec![(1, 0.7), (2, 0.3)],
            iterations: 4,
            converged: true,
        };
        certify_pagerank(key.clone(), value.clone());
        let got = get_pagerank_certified(key).expect("cached pagerank");
        assert_eq!(got.data, value);
    }

    #[test]
    fn invalid_cache_version_is_ignored() {
        CERT_PAGERANK_CACHE.with(|m| {
            m.borrow_mut()
                .insert(b"bad".to_vec(), vec![99, CACHE_CODEC_RKYV, 0]);
        });
        assert!(get_pagerank_certified(b"bad".to_vec()).is_none());
    }

    #[test]
    fn canonical_cert_keys_are_ascii_and_versioned() {
        let stats_key = String::from_utf8(cert_key_stats()).unwrap();
        assert_eq!(stats_key, "gleaph/v1/stats");
        let pagerank_key = String::from_utf8(cert_key_pagerank(&[0xde, 0xad, 0xbe, 0xef])).unwrap();
        assert_eq!(pagerank_key, "gleaph/v1/algo/pagerank/deadbeef");
    }

    #[test]
    fn canonical_leaf_bytes_are_domain_separated() {
        let a = canonical_leaf_bytes("stats", b"\x01");
        let b = canonical_leaf_bytes("algo/pagerank", b"\x01");
        assert_ne!(a, b);
        assert!(a.starts_with(b"GLEAPH_CERT"));
    }
}
