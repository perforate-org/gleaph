//! Plan 0333 — MeCrab (vendored, ByteImage) vs the landed vibrato 0.5.2 pipeline:
//! parity gate, 走った→走る lemma-recall fixture, determinism, idempotence.
//!
//! Enable BOTH features: `--features "vibrato,mecrab-dict"`.

use std::path::Path;
use std::sync::OnceLock;

const RESOURCES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/resources");

/// ~1 MB Japanese sample corpus (0330 method: concatenated mixed-domain sentences,
/// deterministically generated so runs are comparable across candidates).
fn corpus() -> String {
    let sentences = [
        "昨日、公園を全力で走った。",
        "今日も附属病院の前を走ります。",
        "知識グラフデータベースは人工知能の応用である。",
        "本とカレーの街神保町へようこそ。",
        "機械学習モデルの推論コストは入力長に依存する。",
        "東京都千代田区神田神保町一丁目",
        "自然言語処理のための形態素解析エンジンである。",
        "安定メモリから辞書を直接読み取る設計である。",
        "アップグレードのたびに辞書を再構築するのは高価だ。",
        "走った走った走った、そして歩いた。",
        "メモリマップトファイルはオフセットアクセスが可能である。",
        "ダブルアレイトライは接尾辞検索に適したデータ構造である。",
        "ビタビアルゴリズムで最適な分かち書きを求める。",
        "文字カテゴリ定義に基づいて未知語を処理する。",
        "接続コスト行列が品詞ペアの遷移を制約する。",
        "字種辞書は文字コードからカテゴリを引く表である。",
        "キャッシュの予算を大きくするとヒット率が上がる。",
        "ページフォールトのたびにフレームが読み込まれる。",
        "この文は形態素境界の曖昧さを含んでいる。",
        "出力ユニット列は決定論的でなければならない。",
    ];
    // Deterministic pseudo-shuffle to vary adjacency; ~1 MB total.
    let mut out = String::with_capacity(1 << 20);
    for i in 0..2600 {
        for (j, _s) in sentences.iter().enumerate() {
            let k = (i * 7 + j * 3) % sentences.len();

            out.push_str(sentences[k]);
            out.push('\n');
            if out.len() >= (1 << 20) {
                return out;
            }
        }
    }
    out
}

fn mecrab() -> &'static text_analyzer_spike::mecrab_vendor::MecrabAnalyzer {
    static A: OnceLock<text_analyzer_spike::mecrab_vendor::MecrabAnalyzer> = OnceLock::new();
    A.get_or_init(|| {
        text_analyzer_spike::mecrab_vendor::MecrabAnalyzer::from_dir(Path::new(RESOURCES_DIR)
            .join("mecrab")
            .as_path())
        .expect("mecrab dictionary load from resources/mecrab")
    })
}

fn vibrato() -> &'static text_analyzer_spike::vibrato_candidate::Analyzer {
    static A: OnceLock<text_analyzer_spike::vibrato_candidate::Analyzer> = OnceLock::new();
    A.get_or_init(|| {
        text_analyzer_spike::vibrato_candidate::Analyzer::from_zstd_file(
            Path::new(RESOURCES_DIR).join("vibrato/system.dic.zst").as_path(),
        )
        .expect("vibrato dictionary load")
    })
}

#[test]
fn mecrab_lemma_recall_hashitta() {
    let units = mecrab().analyze("走った");
    println!("mecrab 走った -> {units:?}");
    assert!(units.iter().any(|u| u == "走る"), "mecrab: 走った must yield 走る lemma, got {units:?}");
    let units_doc = mecrab().analyze("昨日、公園を全力で走った。");
    assert!(
        units_doc.iter().any(|u| u == "走る"),
        "mecrab: doc 走った must share 走る, got {units_doc:?}"
    );
}

#[test]
fn mecrab_determinism() {
    let samples = [
        "昨日、公園を全力で走った。",
        "附属病院の付属図書館",
        "本とカレーの街神保町へようこそ。",
        "走った走った走った",
    ];
    for s in samples {
        assert_eq!(mecrab().analyze(s), mecrab().analyze(s), "not deterministic on {s:?}");
    }
}

#[test]
fn mecrab_idempotence() {
    let samples = [
        "昨日、公園を全力で走った。",
        "附属病院の付属図書館",
        "走った走った走った",
    ];
    for s in samples {
        let units = mecrab().analyze(s);
        let joined = units.join(" ");
        let re = mecrab().analyze(&joined);
        assert_eq!(re, units, "mecrab: re-analysis of joined units must be a fixed point; input={s:?} units={units:?} re={re:?}");
    }
}

#[test]
fn parity_table_over_corpus_and_fixtures() {
    // Fixture sentences (0330 corpus + the samples above).
    let mut sentences: Vec<String> = corpus()
        .split('。')
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("{s}。"))
        .collect();
    sentences.truncate(400); // per-sentence tabulation cap; the full corpus is covered below

    let mut total = 0usize;
    let mut exact = 0usize;
    let mut seg_diff = Vec::new();
    let mut lemma_diff = Vec::new();

    for s in &sentences {
        let v = vibrato().analyze(s);
        let m = mecrab().analyze(s);
        total += 1;
        if v == m {
            exact += 1;
        } else {
            // classification: same length + positions differ only by lemma text →
            // lemma/feature diff; otherwise segmentation difference.
            let same_len = v.len() == m.len();
            if same_len {
                lemma_diff.push((s.clone(), v.clone(), m.clone()));
            } else {
                seg_diff.push((s.clone(), v.clone(), m.clone()));
            }
        }
    }

    // Whole-corpus aggregate parity.
    let corpus = corpus();
    let cv = vibrato().analyze(&corpus);
    let cm = mecrab().analyze(&corpus);
    let corpus_units_equal = cv == cm;

    let pct = 100.0 * exact as f64 / total.max(1) as f64;
    println!("PARITY(per-sentence fixtures): {exact}/{total} exact = {pct:.2}%");
    println!("PARITY(corpus, whole-text unit sequence equal): {corpus_units_equal}");
    println!("  vibrato units={}", cv.len());
    println!("  mecrab   units={}", cm.len());
    println!("SEGMENTATION DIFFS: {}", seg_diff.len());
    for (s, v, m) in seg_diff.iter().take(20) {
        println!("  [seg] {s:?}\n        vibrato={v:?}\n        mecrab={m:?}");
    }
    println!("LEMMA/FEATURE DIFFS: {}", lemma_diff.len());
    for (s, v, m) in lemma_diff.iter().take(20) {
        println!("  [lemma] {s:?}\n          vibrato={v:?}\n          mecrab={m:?}");
    }

    // Gate: ≥99% exact parity.
    assert!(
        pct >= 99.0,
        "parity gate failed: {pct:.2}% < 99% (seg={} lemma={})",
        seg_diff.len(),
        lemma_diff.len()
    );
}