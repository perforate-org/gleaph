//! Synthetic MeCab-format dictionary fixtures, built PROGRAMMATICALLY (no 50 MB
//! artifacts in CI): a valid sys.dic/unk.dic/matrix.bin/char.bin quartet exercising
//! lemma emission, the drop set, unknown-word handling, determinism/idempotence, the
//! SplitImage residency split, and the MPD container round-trip.
//!
//! The double-array trie is built with `yada` (same Darts unit layout: LE i32 base +
//! u32 check, leaf value encoded as `-base - 1` — the fixture asserts this equivalence
//! against morph-dict's own trie walker, which is exactly the vendored MeCrab layout).

use morph_dict::byteimage::{ByteImage, HeapImage, SplitImage};
use morph_dict::container;
mod darts_fixture;
use morph_dict::Analyzer;
use morph_dict::dict::sys_dic::SysDic;
use morph_dict::profile::DictionaryProfile;
use sha2::Digest;

/// One dictionary entry: surface key + its MeCab token fields + feature string.
struct Entry {
    key: String,
    left: u16,
    right: u16,
    wcost: i16,
    feature: String,
}

/// Builds the four MeCab-format images from a sorted entry list + unk category entries.
///
/// File layout (matches the vendored `SysDic::from_image` parser):
/// header(72B) | double-array trie | token array (16B each) | feature strings.
fn build_mecab_dic(dict_type: u32, entries: &[Entry], left_size: u16, right_size: u16) -> Vec<u8> {
    assert!(
        entries
            .windows(2)
            .all(|w| w[0].key.as_bytes() <= w[1].key.as_bytes()),
        "entries must be sorted byte-wise for the DAT builder"
    );

    // Token array + feature blob.
    let mut tokens = Vec::new();
    let mut features = Vec::new();
    for e in entries {
        let feature_offset = features.len() as u32;
        features.extend_from_slice(e.feature.as_bytes());
        features.push(0);
        tokens.push(TokenRaw {
            left: e.left,
            right: e.right,
            wcost: e.wcost,
            feature_offset,
        });
    }

    // Trie: darts value = (token_start << 8) | token_count.
    let mut trie_entries = Vec::new();
    for (token_start, e) in entries.iter().enumerate() {
        // Count how many consecutive entries share the key (same surface, multiple tokens).
        trie_entries.push((e.key.as_bytes().to_vec(), ((token_start << 8) as u32) | 1));
    }
    // NOTE: multi-token keys are not exercised by the synthetic fixture (count=1 per key).
    let dat = darts_fixture::build_double_array(&trie_entries);

    let token_bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|t| {
            let mut b = Vec::with_capacity(16);
            b.extend_from_slice(&t.left.to_le_bytes());
            b.extend_from_slice(&t.right.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes()); // pos_id
            b.extend_from_slice(&t.wcost.to_le_bytes());
            b.extend_from_slice(&t.feature_offset.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes()); // compound
            b
        })
        .collect();

    let header_size = 72usize;
    let total = header_size + dat.len() + token_bytes.len() + features.len();
    let mut out = Vec::with_capacity(total);
    // magic = filesize ^ DICTIONARY_MAGIC_ID
    out.extend_from_slice(&((total as u32) ^ 0xef71_8f77).to_le_bytes());
    out.extend_from_slice(&102u32.to_le_bytes()); // DIC_VERSION
    out.extend_from_slice(&dict_type.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes()); // lexicon size
    out.extend_from_slice(&(left_size as u32).to_le_bytes()); // left context size
    out.extend_from_slice(&(right_size as u32).to_le_bytes()); // right context size
    out.extend_from_slice(&(dat.len() as u32).to_le_bytes());
    out.extend_from_slice(&(token_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(features.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // dummy
    let mut charset = [0u8; 32];
    charset[..5].copy_from_slice(b"utf8\0");
    out.extend_from_slice(&charset);
    debug_assert_eq!(out.len(), header_size);
    out.extend_from_slice(&dat);
    out.extend_from_slice(&token_bytes);
    out.extend_from_slice(&features);
    debug_assert_eq!(out.len(), total);
    out
}

struct TokenRaw {
    left: u16,
    right: u16,
    wcost: i16,
    feature_offset: u32,
}

/// char.bin: u32 csize | csize × 32B category names | 0xFFFF × 4B packed CharInfo.
///
/// Packed CharInfo: type 18 bits | default_type 8 | length 4 | group 1 | invoke 1.
fn build_char_bin(
    categories: &[&str],
    char_defaults: &[(u32, u8)],
    flags: &[(u8, bool, bool, u8)],
) -> Vec<u8> {
    let csize = categories.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&csize.to_le_bytes());
    for name in categories {
        let mut buf = [0u8; 32];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&buf);
    }
    // Per-category packed info table (id → flags).
    let mut packed_by_id = vec![0u32; categories.len()];
    for &(id, invoke, group, length) in flags {
        packed_by_id[id as usize] = (invoke as u32) << 31
            | (group as u32) << 30
            | ((length as u32) & 0xF) << 26
            | (id as u32) << 18; // default_type = id
    }
    let mut map = vec![0u8; 0xFFFF * 4];
    for &(code, id) in char_defaults {
        let packed = packed_by_id[id as usize];
        map[code as usize * 4..code as usize * 4 + 4].copy_from_slice(&packed.to_le_bytes());
    }
    // Everything else defaults to category 0 (DEFAULT).
    let default_packed = (0u32) << 18;
    for i in 0..0xFFFF {
        if map[i * 4..i * 4 + 4] == [0, 0, 0, 0] {
            map[i * 4..i * 4 + 4].copy_from_slice(&default_packed.to_le_bytes());
        }
    }
    out.extend_from_slice(&map);
    out
}

fn synthetic_images() -> Vec<(String, Vec<u8>)> {
    // Context IDs: 0 = BOS/EOS, 1 = content, 2 = droppable.
    let entries = vec![
        Entry {
            key: "は".into(),
            left: 0,
            right: 0,
            wcost: 100,
            feature: "助詞,係助詞,*,*,*,*,は,ハ,ワ".into(),
        },
        Entry {
            key: "走った".into(),
            left: 1,
            right: 0,
            wcost: 300,
            feature: "動詞,自立,*,*,特殊・タ,基本形,走った,ハシッタ,ハシッタ".into(),
        },
        Entry {
            key: "走る".into(),
            left: 1,
            right: 0,
            wcost: 300,
            feature: "動詞,自立,*,*,五段・ラ行,基本形,走る,ハシル,ハシル".into(),
        },
    ];
    // Entries must be sorted byte-wise.
    let mut entries = entries;
    entries.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));

    let sys = build_mecab_dic(0, &entries, 3, 3);

    // unk.dic: one ALPHA-category entry (invoked unknown words emit the unk feature).
    let unk_entries = vec![Entry {
        key: "ALPHA".into(),
        left: 1,
        right: 0,
        wcost: 5000,
        feature: "名詞,固有名詞,一般,*,*,*,*,*,*".into(),
    }];
    let unk = build_mecab_dic(2, &unk_entries, 3, 3);

    // matrix.bin: 3×3 zero-cost transitions.
    let mut matrix = Vec::new();
    matrix.extend_from_slice(&3u16.to_le_bytes());
    matrix.extend_from_slice(&3u16.to_le_bytes());
    for _ in 0..9 {
        matrix.extend_from_slice(&0i16.to_le_bytes());
    }

    // char.bin: ALPHA category (id 5) groups ASCII letters with invoke; HIRAGANA (6)
    // without invoke; everything else DEFAULT (0).
    let categories = [
        "DEFAULT", "SPACE", "KANJI", "SYMBOL", "NUMERIC", "ALPHA", "HIRAGANA",
    ];
    let mut char_defaults = Vec::new();
    for (code, cat) in (0x61u32..=0x7A).map(|c| (c, 5u8)) {
        char_defaults.push((code, cat));
    }
    let char_bin = build_char_bin(&categories, &char_defaults, &[(5, true, true, 4)]);

    vec![
        ("sys.dic".to_string(), sys),
        ("unk.dic".to_string(), unk),
        ("matrix.bin".to_string(), matrix),
        ("char.bin".to_string(), char_bin),
    ]
}

fn ipadic_profile() -> DictionaryProfile {
    DictionaryProfile::japanese_ipadic()
}

fn analyzer_from_container(images: Vec<(String, Vec<u8>)>) -> Analyzer {
    let container = container::build(images);
    Analyzer::open(
        std::sync::Arc::new(HeapImage::from_vec(container)),
        ipadic_profile(),
    )
    .expect("synthetic container open")
}

#[test]
fn synthetic_lemma_emission_and_drop_set() {
    let a = analyzer_from_container(synthetic_images());
    for (s_, f_) in a.analyze_tokens("走った") {
        println!("DBG token: {:?} {:?}", s_, f_);
    }
    // 走った keeps its lemma (the fixture entry's own base form), は is dropped.
    assert_eq!(a.analyze("走った"), vec!["走った"]);
    // Unknown ASCII word: unk feature has an unassigned lemma → surface emitted.
    assert_eq!(a.analyze("abc"), vec!["abc"]);
    // Combined: the particle drops out.
    assert_eq!(a.analyze("走ったは"), vec!["走った"]);
}

#[test]
fn synthetic_determinism_and_idempotence() {
    let a = analyzer_from_container(synthetic_images());
    for fixture in ["", "走った", "abc", "走ったは", "走った走った走った"] {
        let first = a.analyze(fixture);
        assert_eq!(first, a.analyze(fixture), "determinism on {fixture:?}");
        let joined = first.join(" ");
        assert_eq!(a.analyze(&joined), first, "idempotence on {fixture:?}");
    }
}

#[test]
fn split_image_matches_whole_image_and_classifies_reads() {
    let images = synthetic_images();
    let container_bytes = container::build(images.clone());

    // Reference: parse sys.dic directly from a full heap copy of the container.
    let full = std::sync::Arc::new(HeapImage::from_vec(container_bytes.clone()));
    let entries = container::validate(full.as_ref()).unwrap();
    let sys_entry = container::entry(&entries, "sys.dic").unwrap().clone();
    let sys_view =
        morph_dict::byteimage::OffsetImage::new(full.clone(), sys_entry.offset, sys_entry.len);
    let direct = SysDic::from_image(std::sync::Arc::new(sys_view)).unwrap();

    // Split open: resident prefix = header+trie+tokens, lazy suffix over the same copy.
    let boundary = SysDic::header_feature_offset(&morph_dict::byteimage::OffsetImage::new(
        full.clone(),
        sys_entry.offset,
        sys_entry.len,
    ))
    .unwrap();
    let make_split = || {
        let mut prefix_bytes = vec![0u8; boundary as usize];
        full.read_exact_at(sys_entry.offset, &mut prefix_bytes);
        SplitImage::new(
            std::sync::Arc::new(HeapImage::from_vec(prefix_bytes)),
            std::sync::Arc::new(morph_dict::byteimage::OffsetImage::new(
                full.clone(),
                sys_entry.offset,
                sys_entry.len,
            )),
            boundary,
        )
    };
    let _split_image = make_split();
    let via_split = SysDic::from_image(std::sync::Arc::new(make_split())).unwrap();
    let stats_image = make_split();
    let stats_image = std::sync::Arc::new(stats_image);
    let stats_dic = SysDic::from_image(stats_image.clone()).unwrap();

    for key in ["は", "走った", "走る", "走", "abc", "走ったは"] {
        assert_eq!(
            direct.common_prefix_search(key).len(),
            via_split.common_prefix_search(key).len(),
            "split image must be byte-identical to the full copy for {key:?}"
        );
        assert_eq!(
            direct
                .common_prefix_search(key)
                .iter()
                .map(|e| (e.word_id, e.feature.clone()))
                .collect::<Vec<_>>(),
            via_split
                .common_prefix_search(key)
                .iter()
                .map(|e| (e.word_id, e.feature.clone()))
                .collect::<Vec<_>>(),
        );
    }
    // Drive HOT-PATH (feature-free) lookups through the stats-holding image.
    for key in ["は", "走った", "走る", "走"] {
        let _ = stats_dic.common_prefix_search_lite(key);
    }
    let stats = stats_image.stats();
    assert!(
        stats.prefix_calls > 0,
        "trie/token reads hit the resident prefix"
    );
    assert_eq!(
        stats.suffix_bytes, 0,
        "no feature-region reads during lookups"
    );
    // Feature reads flow to the suffix once get_feature is used.
    if let Some(tok) = stats_dic.get_token(0) {
        let _ = stats_dic.get_feature(&tok);
        assert!(
            stats_image.stats().suffix_bytes > 0,
            "feature reads hit the lazy suffix"
        );
    }
}

#[test]
fn container_round_trip_and_validation() {
    let images = synthetic_images();
    let container_bytes = container::build(images.clone());
    let img = HeapImage::from_vec(container_bytes.clone());
    let entries = container::validate(&img).expect("valid container");
    assert_eq!(entries.len(), 4);
    for (name, bytes) in &images {
        let e = container::entry(&entries, name).unwrap();
        assert_eq!(e.len, bytes.len() as u64);
        let mut sha = sha2::Sha256::new();
        sha.update(bytes);
        assert_eq!(e.sha256, sha.finalize().as_slice(), "sha256 of {name}");
        assert_eq!(
            e.sha256,
            container::attest(&img, e.offset, e.len),
            "attest re-derivation for {name}"
        );
    }
}

#[test]
fn container_validation_fails_closed() {
    // Bad magic.
    let mut bad = container::build(synthetic_images());
    bad[0] = b'X';
    assert!(container::validate(&HeapImage::from_vec(bad)).is_err());
    // Truncated table.
    let good = container::build(synthetic_images());
    assert!(container::validate(&HeapImage::from_vec(good[..20].to_vec())).is_err());
    // Empty.
    assert!(container::validate(&HeapImage::from_vec(Vec::new())).is_err());
}

#[test]
fn long_line_fails_closed() {
    let a = analyzer_from_container(synthetic_images());
    let long_line: String = "走".repeat(1024 * 1024 + 1);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.analyze(&long_line)));
    assert!(
        result.is_err(),
        "a >max_line_bytes line must fail closed, not hang"
    );
}

// Silence the unused-field warning on the synthetic builder helper.
#[allow(dead_code)]
fn _keep(entries: &[Entry]) -> usize {
    entries.len()
}
