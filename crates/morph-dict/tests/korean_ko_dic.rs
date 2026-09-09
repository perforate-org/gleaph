//! Plan 0341 todo 1 + todo 2 (text side): mecab-ko-dic artifact gates + profile contract.
//!
//! Artifact: pinned mecab-ko-dic 2.1.1-20180720 (eunjeon, Apache-2.0), compiled with
//! mecab-dict-index (mecab 0.996, `-f utf-8 -t utf-8`), images cached gitignored under
//! `crates/pocket-ic-tests/resources/mecab-ko-dic/` (fetch-once, same pattern as the
//! ipadic `mecrab/` cache). Artifact-gated tests SKIP (loudly) when the images are
//! absent so the suite stays runnable without the ~100 MB build.

use std::path::PathBuf;
use std::sync::Arc;

use morph_dict::Analyzer;
use morph_dict::byteimage::HeapImage;
use morph_dict::profile::DictionaryProfile;

fn ko_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../pocket-ic-tests/resources/mecab-ko-dic")
}

fn ko_images_if_present() -> Option<Vec<(String, Vec<u8>)>> {
    let dir = ko_dir();
    let mut images = Vec::new();
    for n in ["sys.dic", "unk.dic", "matrix.bin", "char.bin"] {
        images.push((n.to_string(), std::fs::read(dir.join(n)).ok()?));
    }
    Some(images)
}

fn ko_container_if_present() -> Option<Vec<u8>> {
    Some(morph_dict::container::build(ko_images_if_present()?))
}

// -- todo 2: profile drop matrix (no artifact needed) -------------------------------

#[test]
fn ko_profile_drop_matrix_prefix_semantics() {
    let ko = DictionaryProfile::korean_mecab_ko_dic();
    assert_eq!(ko.lemma_column, Some(3));
    // Kept content tags (incl. compound first-subtag that is content-bearing).
    for keep in [
        "NNG", "NNP", "NNB", "NNBC", "NR", "NP", "VV+EC", "VA", "VX", "VCP", "VCN", "MAG", "MAJ",
        "MM", "IC", "XR", "SL", "SH", "NNG+JX",
    ] {
        assert!(!ko.drops(keep), "ko profile must keep {keep}");
    }
    // Dropped: particle/ending/affix/symbol/unknown families incl. compounds.
    for drop in [
        "JKB",
        "JX",
        "J",
        "EC",
        "ETM",
        "ETM+NNG+JC",
        "XSN",
        "XSV",
        "XSA",
        "SF",
        "SE",
        "SC",
        "SSC",
        "SSO",
        "SP",
        "SY",
        "SW",
        "UNA",
        "NA",
    ] {
        assert!(ko.drops(drop), "ko profile must drop {drop}");
    }
}

#[test]
fn ipadic_profile_zero_behavior_change() {
    // Additive field: ipadic keeps empty prefixes and its exact-match set is untouched.
    let ip = DictionaryProfile::japanese_ipadic();
    assert!(ip.drop_prefixes.is_empty());
    assert!(ip.drops("助詞"));
    assert!(!ip.drops("名詞"));
    // A compound-looking tag must NOT be dropped under ipadic (no prefix semantics).
    assert!(!ip.drops("助詞+EC"));
}

// -- todo 1 gate (a): container-basis zstd-19 size ----------------------------------

#[test]
fn ko_container_basis_zstd19_size_gate() {
    let Some(container) = ko_container_if_present() else {
        eprintln!("SKIPPED (ko-dic images not built; see pocket-ic-tests/resources/mecab-ko-dic)");
        return;
    };
    // zstd-19 via the CLI (no new cargo dep for one gate measurement).
    let path = std::env::temp_dir().join("ko_dic_container_basis.mpd");
    std::fs::write(&path, &container).expect("write basis container");
    let out = std::process::Command::new("zstd")
        .args(["-19", "-c"])
        .arg(&path)
        .output()
        .expect("spawn zstd -19");
    assert!(out.status.success(), "zstd -19 failed: {:?}", out.status);
    let _ = std::fs::remove_file(&path);
    println!(
        "plan-0341 gate (a): ko-dic container {} B -> zstd-19 {} B ({:.1}%)",
        container.len(),
        out.stdout.len(),
        100.0 * out.stdout.len() as f64 / container.len() as f64
    );
    assert!(
        out.stdout.len() < 21 * 1024 * 1024,
        "container-basis zstd-19 {} B exceeds the ~21 MB surface gate",
        out.stdout.len()
    );
}

// -- todo 1 gates (b)+(c): recall smoke + resident set --------------------------------

#[test]
fn ko_analyzer_open_recalls_headline_and_resident_set() {
    let Some(container) = ko_container_if_present() else {
        eprintln!("SKIPPED (ko-dic images not built; see pocket-ic-tests/resources/mecab-ko-dic)");
        return;
    };
    let analyzer = Analyzer::open(
        Arc::new(HeapImage::from_vec(container)),
        DictionaryProfile::korean_mecab_ko_dic(),
    )
    .expect("ko-dic container opens");
    // Headline smoke: 학교 from 학교에서 (the plan-0341 gate).
    let units = analyzer.analyze("학교에서");
    println!("plan-0341 gate (b): 학교에서 -> {units:?}");
    assert!(
        units.iter().any(|u| u == "학교"),
        "headline recall: 학교에서 must emit 학교, got {units:?}"
    );
    // Resident-set measurement (plan-0341 gate (c)): the open path materializes
    // matrix.bin + char.bin + unk.dic fully plus sys.dic's resident prefix (header +
    // trie + word params; feature region stays lazy). Recompute the same split here
    // from the image sizes so the recorded number matches production residency.
    let dir = ko_dir();
    let sys_len = std::fs::metadata(dir.join("sys.dic"))
        .expect("stat sys.dic")
        .len();
    let matrix_len = std::fs::metadata(dir.join("matrix.bin"))
        .expect("stat matrix.bin")
        .len();
    let char_len = std::fs::metadata(dir.join("char.bin"))
        .expect("stat char.bin")
        .len();
    let unk_len = std::fs::metadata(dir.join("unk.dic"))
        .expect("stat unk.dic")
        .len();
    let sys_image = morph_dict::byteimage::HeapImage::from_vec(
        std::fs::read(dir.join("sys.dic")).expect("read sys.dic"),
    );
    let feature_offset = morph_dict::dict::sys_dic::SysDic::header_feature_offset(&sys_image)
        .expect("sys.dic header");
    let resident = feature_offset + matrix_len + char_len + unk_len;
    println!(
        "plan-0341 gate (c): ko-dic Analyzer resident set ~{resident} B ({:.1} MB; sys prefix {feature_offset} + matrix {matrix_len} + char {char_len} + unk {unk_len}; sys.dic total {sys_len})",
        resident as f64 / (1024.0 * 1024.0)
    );
    let _ = analyzer.dictionary().size();
}
