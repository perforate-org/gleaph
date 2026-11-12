//! Prototype canister wrapper around `glev-infer`.
//!
//! The weight pack is installed in chunks by a controller, held in the canister heap (328MB for
//! the 14-layer model), and inference runs on update calls because one forward is ~18.5B
//! instructions (the 40B update budget), far above the 5B query budget.
//!
//! The tokenizer is installed into the same canister (ADR 0113): it is a model-side asset with
//! the pack, and callers may send question text instead of token ids. The token-id ingress stays
//! for tests and operators.

use std::borrow::Cow;
use std::cell::RefCell;

use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{
    BTreeMap as StableMap, Cell as StableCell, DefaultMemoryImpl, Storable,
};

use candid::CandidType;
use glev_infer::model::Model;
use glev_infer::pack::Pack;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use gleaph_routing_wire::MAX_QUESTION_TOKENS;

pub use gleaph_routing_wire::{
    RouteAndExtract, SlotSpan, TextRouteAndExtract, TextSlotSpan, TokenizerFingerprint,
};

/// Deterministic getrandom backend for wasm32-unknown-unknown (ADR 0113). The tokenizer's hash
/// maps need a successful fill, not unpredictable bytes.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom hands us a valid writable buffer of `len` bytes.
    unsafe { core::ptr::write_bytes(dest, 0, len) };
    Ok(())
}

#[derive(CandidType, Deserialize)]
struct InstallChunk {
    offset: u64,
    bytes: Vec<u8>,
}

/// Maximum cached decisions; the cache is cleared when it fills (bounded stable memory use).
const CACHE_LIMIT: usize = 4096;
/// Stable memory id used by the decision cache (memory id 0 is reserved for the manager).
const CACHE_MEMORY_ID: MemoryId = MemoryId::new(1);
/// Tokenizer JSON bytes (ADR 0113).
const TOKENIZER_MEMORY_ID: MemoryId = MemoryId::new(2);
/// Tokenizer fingerprint: 32-byte SHA-256 followed by the u64 byte length.
const TOKENIZER_FINGERPRINT_MEMORY_ID: MemoryId = MemoryId::new(3);
/// Upper bound for one cached probability vector (count + up to 255 f32 values).
const CACHED_PROBS_BOUND: u32 = 4 + 255 * 4;

/// Probabilities cached in stable memory; encoded as `u32` length + little-endian f32 values.
#[derive(Clone, PartialEq)]
struct CachedProbs(Vec<f32>);

impl Storable for CachedProbs {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = Vec::with_capacity(4 + self.0.len() * 4);
        bytes.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        for value in &self.0 {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Cow::Owned(bytes)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        if bytes.len() < 4 {
            return Self(Vec::new());
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let available = (bytes.len() - 4) / 4;
        let count = count.min(available);
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let start = 4 + index * 4;
            values.push(f32::from_le_bytes(
                bytes[start..start + 4].try_into().unwrap(),
            ));
        }
        Self(values)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Bounded {
            max_size: CACHED_PROBS_BOUND,
            is_fixed_size: false,
        };
}

/// Stable-memory decision cache. Writes happen only in update calls (query calls cannot mutate
/// state), and the entries survive canister upgrades.
struct StableCache {
    _manager: MemoryManager<DefaultMemoryImpl>,
    entries: StableMap<u64, CachedProbs, VirtualMemory<DefaultMemoryImpl>>,
    tokenizer_bytes: StableCell<Vec<u8>, VirtualMemory<DefaultMemoryImpl>>,
    tokenizer_fingerprint: StableCell<Vec<u8>, VirtualMemory<DefaultMemoryImpl>>,
}

impl StableCache {
    fn new() -> Self {
        let manager = MemoryManager::init(DefaultMemoryImpl::default());
        let entries = StableMap::init(manager.get(CACHE_MEMORY_ID));
        let tokenizer_bytes = StableCell::init(manager.get(TOKENIZER_MEMORY_ID), Vec::new());
        let tokenizer_fingerprint =
            StableCell::init(manager.get(TOKENIZER_FINGERPRINT_MEMORY_ID), Vec::new());
        Self {
            _manager: manager,
            entries,
            tokenizer_bytes,
            tokenizer_fingerprint,
        }
    }
}

thread_local! {
    static STABLE_CACHE: RefCell<StableCache> = RefCell::new(StableCache::new());
}

#[derive(Default)]
struct State {
    pack: Vec<u8>,
    model: Option<Model>,
    tokenizer: Option<Tokenizer>,
    tokenizer_staging: Vec<u8>,
    tokenizer_fingerprint: Option<TokenizerFingerprint>,
    cache_hits: u64,
    cache_misses: u64,
}

/// Parse the tokenizer JSON and clear the asset's embedded 48-token truncation.
fn parse_tokenizer(bytes: &[u8]) -> Result<Tokenizer, String> {
    let mut tokenizer = Tokenizer::from_bytes(bytes).map_err(|error| error.to_string())?;
    tokenizer
        .with_truncation(None)
        .map_err(|error| error.to_string())?;
    Ok(tokenizer)
}

/// The SDK's abstain policy measure: non-whitespace CJK fraction of the decoded question.
fn cjk_fraction(tokenizer: &Tokenizer, ids: &[u32]) -> f32 {
    let Ok(text) = tokenizer.decode(ids, true) else {
        return 0.0;
    };
    let mut total = 0usize;
    let mut cjk = 0usize;
    for character in text.chars() {
        if character.is_whitespace() {
            continue;
        }
        total += 1;
        let code = character as u32;
        let is_cjk = (0x3040..=0x30ff).contains(&code)
            || (0x3400..=0x4dbf).contains(&code)
            || (0x4e00..=0x9fff).contains(&code)
            || (0xf900..=0xfaff).contains(&code)
            || (0xff66..=0xff9f).contains(&code);
        cjk += usize::from(is_cjk);
    }
    if total == 0 {
        0.0
    } else {
        cjk as f32 / total as f32
    }
}

fn fingerprint_bytes(fingerprint: &TokenizerFingerprint) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(&fingerprint.sha256);
    bytes.extend_from_slice(&fingerprint.bytes.to_le_bytes());
    bytes
}

fn fingerprint_from_bytes(bytes: &[u8]) -> Option<TokenizerFingerprint> {
    if bytes.len() != 40 {
        return None;
    }
    Some(TokenizerFingerprint {
        sha256: bytes[..32].to_vec(),
        bytes: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
    })
}

fn input_key(token_ids: &[u32]) -> u64 {
    // FNV-1a 64 over the question token ids.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for value in token_ids {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[ic_cdk::update]
fn admin_install_chunk(chunk: InstallChunk) -> Result<u64, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("install rejected: caller is not a controller".into());
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let end = chunk.offset as usize + chunk.bytes.len();
        if state.pack.len() < end {
            state.pack.resize(end, 0);
        }
        state.pack[chunk.offset as usize..end].copy_from_slice(&chunk.bytes);
        Ok(state.pack.len() as u64)
    })
}

#[ic_cdk::update]
fn admin_install_finish(sha256_hex: String) -> Result<u64, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("install rejected: caller is not a controller".into());
    }
    if sha256_hex.len() != 64 {
        return Err("expected a 64-char sha256 hex digest".into());
    }
    let bytes = STATE.with(|state| state.borrow().pack.clone());
    if bytes.is_empty() {
        return Err("no pack bytes installed".into());
    }
    let pack = Pack::parse(&bytes)?;
    let model = Model::from_pack(&pack)?;
    let len = bytes.len() as u64;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.model = Some(model);
        state.pack.clear();
    });
    Ok(len)
}

#[ic_cdk::update]
fn admin_install_tokenizer_chunk(chunk: InstallChunk) -> Result<u64, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("tokenizer install rejected: caller is not a controller".into());
    }
    let end = usize::try_from(chunk.offset)
        .ok()
        .and_then(|offset| offset.checked_add(chunk.bytes.len()))
        .ok_or_else(|| "tokenizer offset overflow".to_owned())?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.tokenizer_staging.len() < end {
            state.tokenizer_staging.resize(end, 0);
        }
        let offset = chunk.offset as usize;
        state.tokenizer_staging[offset..end].copy_from_slice(&chunk.bytes);
        Ok(state.tokenizer_staging.len() as u64)
    })
}

#[ic_cdk::update]
fn admin_install_tokenizer_finish() -> Result<TokenizerFingerprint, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("tokenizer install rejected: caller is not a controller".into());
    }
    let bytes = STATE.with(|state| state.borrow().tokenizer_staging.clone());
    if bytes.is_empty() {
        return Err("no tokenizer bytes installed".into());
    }
    let tokenizer = parse_tokenizer(&bytes)?;
    let digest = Sha256::digest(&bytes);
    let fingerprint = TokenizerFingerprint {
        sha256: digest.to_vec(),
        bytes: bytes.len() as u64,
    };
    STABLE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.tokenizer_bytes.set(bytes.clone());
        cache
            .tokenizer_fingerprint
            .set(fingerprint_bytes(&fingerprint));
    });
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.tokenizer = Some(tokenizer);
        state.tokenizer_staging.clear();
        state.tokenizer_fingerprint = Some(fingerprint.clone());
    });
    Ok(fingerprint)
}

/// SHA-256 and length of the installed tokenizer asset, if any.
#[ic_cdk::query]
fn tokenizer_fingerprint() -> Option<TokenizerFingerprint> {
    STATE.with(|state| state.borrow().tokenizer_fingerprint.clone())
}

/// Token count and CJK fraction for the Router's exact quota charge (no model forward).
#[ic_cdk::update]
fn tokenizer_token_count(text: String) -> Result<(u32, f32), String> {
    STATE.with(|state| {
        let state = state.borrow();
        let tokenizer = state
            .tokenizer
            .as_ref()
            .ok_or("tokenizer is not installed")?;
        let encoding = tokenizer
            .encode(text, false)
            .map_err(|error| error.to_string())?;
        let ids = encoding.get_ids();
        let count = u32::try_from(ids.len()).map_err(|_| "token count overflow".to_owned())?;
        Ok((count, cjk_fraction(tokenizer, ids)))
    })
}

/// Tokenise a question, run the forward pass, and return byte-offset spans for clients without a
/// tokenizer (ADR 0113).
#[ic_cdk::update]
fn route_and_extract_text(text: String) -> Result<TextRouteAndExtract, String> {
    STATE.with(|state| {
        let state = state.borrow();
        let tokenizer = state
            .tokenizer
            .as_ref()
            .ok_or("tokenizer is not installed")?;
        let model = state.model.as_ref().ok_or("weights are not installed")?;
        let encoding = tokenizer
            .encode(text, false)
            .map_err(|error| error.to_string())?;
        let mut ids = encoding.get_ids().to_vec();
        let mut offsets = encoding.get_offsets().to_vec();
        if ids.len() > MAX_QUESTION_TOKENS {
            ids.truncate(MAX_QUESTION_TOKENS);
            offsets.truncate(MAX_QUESTION_TOKENS);
        }
        let (probabilities, tags, scores) = model.forward_question(&ids);
        let tags = tags.ok_or("pack has no slot head")?;
        let scores = scores.unwrap_or_default();
        let token_spans = spans_from_tags(&tags, &scores, model.slot_tag_names());
        let spans = token_spans
            .into_iter()
            .filter_map(|span| {
                let start = offsets.get(span.start as usize)?.0;
                let end = offsets.get(span.end as usize - 1)?.1;
                Some(TextSlotSpan {
                    tag: span.tag,
                    start: u32::try_from(start).ok()?,
                    end: u32::try_from(end).ok()?,
                    score: span.score,
                })
            })
            .collect();
        Ok(TextRouteAndExtract {
            probabilities,
            spans,
            token_count: ids.len() as u32,
            cjk_fraction: cjk_fraction(tokenizer, &ids),
        })
    })
}

/// Reload the persisted tokenizer after an upgrade (ADR 0113). The model pack is not persisted;
/// operators reinstall it after an upgrade.
#[ic_cdk::post_upgrade]
fn post_upgrade() {
    let bytes = STABLE_CACHE.with(|cache| cache.borrow().tokenizer_bytes.get().clone());
    if bytes.is_empty() {
        return;
    }
    let Ok(tokenizer) = parse_tokenizer(&bytes) else {
        return;
    };
    let fingerprint = STABLE_CACHE
        .with(|cache| fingerprint_from_bytes(cache.borrow().tokenizer_fingerprint.get()));
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.tokenizer = Some(tokenizer);
        state.tokenizer_fingerprint = fingerprint;
    });
}

#[ic_cdk::update]
fn infer(token_ids: Vec<u32>) -> Result<Vec<f32>, String> {
    let key = input_key(&token_ids);
    let cached = STABLE_CACHE.with(|cache| cache.borrow().entries.get(&key));
    if let Some(cached) = cached {
        STATE.with(|state| state.borrow_mut().cache_hits += 1);
        return Ok(cached.0);
    }
    let probabilities = STATE.with(|state| {
        let state = state.borrow();
        let model = state.model.as_ref().ok_or("weights are not installed")?;
        Ok::<Vec<f32>, String>(model.forward_question(&token_ids).0)
    })?;
    STATE.with(|state| state.borrow_mut().cache_misses += 1);
    STABLE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.entries.len() >= CACHE_LIMIT as u64 {
            cache.entries.clear_new();
        }
        cache
            .entries
            .insert(key, CachedProbs(probabilities.clone()));
    });
    Ok(probabilities)
}

/// Group the per-token BIO tag ids into spans named by the pack's tag schema.
fn spans_from_tags(tags: &[u8], scores: &[f32], names: &[String]) -> Vec<SlotSpan> {
    fn name_of<'a>(tags: &[u8], names: &'a [String], index: usize) -> &'a str {
        tags.get(index)
            .and_then(|tag| names.get(*tag as usize))
            .map(String::as_str)
            .unwrap_or("O")
    }

    let score_of = |start: usize, end: usize| -> f32 {
        if start >= end || scores.is_empty() {
            return 0.0;
        }
        scores[start..end.min(scores.len())].iter().sum::<f32>()
            / (end.min(scores.len()) - start) as f32
    };
    let mut raw: Vec<SlotSpan> = Vec::new();
    let mut open: Option<(String, usize)> = None;
    for index in 0..tags.len() {
        let name = name_of(tags, names, index);
        if let Some(slot) = name.strip_prefix("B-") {
            if let Some((tag, start)) = open.take() {
                raw.push(SlotSpan {
                    tag,
                    start: start as u32,
                    end: index as u32,
                    score: score_of(start, index),
                });
            }
            open = Some((slot.to_owned(), index));
        } else if let Some(slot) = name.strip_prefix("I-") {
            match &open {
                Some((tag, _)) if tag == slot => {}
                _ => {
                    if let Some((tag, start)) = open.take() {
                        raw.push(SlotSpan {
                            tag,
                            start: start as u32,
                            end: index as u32,
                            score: score_of(start, index),
                        });
                    }
                    open = Some((slot.to_owned(), index));
                }
            }
        } else if let Some((tag, start)) = open.take() {
            raw.push(SlotSpan {
                tag,
                start: start as u32,
                end: index as u32,
                score: score_of(start, index),
            });
        }
    }
    if let Some((tag, start)) = open {
        raw.push(SlotSpan {
            tag,
            start: start as u32,
            end: tags.len() as u32,
            score: score_of(start, tags.len()),
        });
    }
    // Overlapping windows can split one span into adjacent pieces; merge them back.
    let mut merged: Vec<SlotSpan> = Vec::with_capacity(raw.len());
    for span in raw {
        match merged.last_mut() {
            Some(previous) if previous.tag == span.tag && previous.end >= span.start => {
                previous.end = previous.end.max(span.end);
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// One forward pass returning routing probabilities and slot spans recovered from the token tags.
/// The decision cache is not used here because the tags require the forward pass.
#[ic_cdk::update]
fn route_and_extract(token_ids: Vec<u32>) -> Result<RouteAndExtract, String> {
    STATE.with(|state| {
        let state = state.borrow();
        let model = state.model.as_ref().ok_or("weights are not installed")?;
        let (probabilities, tags, scores) = model.forward_question(&token_ids);
        let tags = tags.ok_or("pack has no slot head")?;
        let scores = scores.unwrap_or_default();
        Ok(RouteAndExtract {
            probabilities,
            spans: spans_from_tags(&tags, &scores, model.slot_tag_names()),
        })
    })
}

#[ic_cdk::query]
fn wire_version() -> u32 {
    gleaph_routing_wire::WIRE_VERSION
}

#[ic_cdk::query]
fn route_options() -> Result<Vec<String>, String> {
    STATE.with(|state| {
        let state = state.borrow();
        let model = state.model.as_ref().ok_or("weights are not installed")?;
        Ok(model.route_options().to_vec())
    })
}

#[ic_cdk::query]
fn cache_stats() -> (u64, u64, u64) {
    let entries = STABLE_CACHE.with(|cache| cache.borrow().entries.len());
    STATE.with(|state| {
        let state = state.borrow();
        (state.cache_hits, state.cache_misses, entries)
    })
}

#[ic_cdk::update]
fn admin_clear_cache() -> Result<(), String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("cache clear rejected: caller is not a controller".into());
    }
    STABLE_CACHE.with(|cache| cache.borrow_mut().entries.clear_new());
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.cache_hits = 0;
        state.cache_misses = 0;
    });
    Ok(())
}

#[ic_cdk::query]
fn weights_installed() -> bool {
    STATE.with(|state| state.borrow().model.is_some())
}
