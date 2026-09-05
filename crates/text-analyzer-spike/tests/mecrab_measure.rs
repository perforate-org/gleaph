//! Plan 0334 re-measure against the landed `morph-dict` engine: open cost (container
//! validation + resident-set materialization), per-structure access classification
//! (sys.dic trie / word-params / feature-region), and native throughput vs vibrato.
//! The 0333 LRU-budget sweep moved to the `ic-morph-dict` adapter tests (native
//! `VectorMemory`).
//!
//! Run: `cargo test --features "vibrato,mecrab-dict" --test mecrab_measure -- --nocapture --release`

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use morph_dict::byteimage::{ByteImage, CountingImage, HeapImage, SplitImage};
use morph_dict::container;
use morph_dict::dict::Dictionary;
use morph_dict::DictionaryProfile;
use text_analyzer_spike::mecrab_vendor::Analyzer;

const RESOURCES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/resources");

fn container_bytes() -> Vec<u8> {
    let names = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
    let images: Vec<(String, Vec<u8>)> = names
        .iter()
        .map(|n| {
            (
                n.to_string(),
                std::fs::read(Path::new(RESOURCES_DIR).join("mecrab").join(n))
                    .unwrap_or_else(|e| panic!("{n}: {e}")),
            )
        })
        .collect();
    container::build(images)
}

/// ~1 MB newline-separated Japanese corpus (0330/0333 method).
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

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

#[test]
fn measure_tables() {
    let corpus = corpus();
    let container = container_bytes();
    println!("container bytes = {}", container.len());

    // ── open cost: container validation + resident-set materialization ──
    let mut opens = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        let a = Analyzer::open(
            Arc::new(HeapImage::from_vec(container.clone())),
            DictionaryProfile::japanese_ipadic(),
        )
        .expect("open");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        opens.push(ms);
        let _ = a;
    }
    println!("OPEN (validate + resident memcpy, mean of 3) = {:.1} ms", mean(&opens));

    // ── per-structure access classification over the corpus run ──
    // Wrap each container entry: sys.dic through SplitImage (trie/params resident,
    // feature region lazy) with CountingImage accounting per side.
    {
        let img = Arc::new(HeapImage::from_vec(container.clone()));
        let entries = container::validate(img.as_ref()).expect("validate");
        let sys = container::entry(&entries, "sys.dic").unwrap().clone();
        let sys_view =
            morph_dict::byteimage::OffsetImage::new(img.clone(), sys.offset, sys.len);
        let feature_offset =
            morph_dict::dict::sys_dic::SysDic::header_feature_offset(&sys_view).unwrap();
        let mut prefix = vec![0u8; feature_offset as usize];
        sys_view.read_exact_at(0, &mut prefix);

        // Counting wrappers over the same images.
        let counted_sys_prefix = Arc::new(CountingImage::new(Arc::new(HeapImage::from_vec(
            prefix.clone(),
        ))));
        let counted_sys_lazy =
            Arc::new(CountingImage::new(Arc::new(morph_dict::byteimage::OffsetImage::new(
                img.clone(),
                sys.offset,
                sys.len,
            ))));
        let split = Arc::new(SplitImage::new(
            counted_sys_prefix.clone(),
            counted_sys_lazy.clone(),
            feature_offset,
        ));
        let matrix_entry = container::entry(&entries, "matrix.bin").unwrap().clone();
        let char_entry = container::entry(&entries, "char.bin").unwrap().clone();
        let unk_entry = container::entry(&entries, "unk.dic").unwrap().clone();
        let read_heap = |e: &morph_dict::container::ContainerEntry| {
            let mut b = vec![0u8; e.len as usize];
            img.read_exact_at(e.offset, &mut b);
            Arc::new(HeapImage::from_vec(b)) as Arc<dyn ByteImage>
        };
        let dict = Dictionary::from_images(
            split,
            read_heap(&unk_entry),
            read_heap(&matrix_entry),
            read_heap(&char_entry),
        )
        .expect("dict");
        let a = Analyzer::from_dir(Path::new(RESOURCES_DIR).join("mecrab").as_path(), DictionaryProfile::japanese_ipadic()).expect("dir analyzer");
        let _ = a.analyze(&corpus); // warm

        // Analyze via the instrumented dictionary (bypass Analyzer; same rules).
        let units = {
            let mut out = Vec::new();
            for line in corpus.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let lat = morph_dict::lattice::Lattice::build(
                    line,
                    &dict,
                    &DictionaryProfile::japanese_ipadic(),
                )
                .unwrap();
                let solver = morph_dict::viterbi::ViterbiSolver::new(&dict);
                for node in solver.solve(&lat).unwrap() {
                    let columns: Vec<&str> = node.feature.split(',').collect();
                    let pos = columns.first().copied().unwrap_or("*");
                    if matches!(pos, "助詞" | "助動詞" | "記号" | "接頭辞" | "接尾辞" | "フィラー") {
                        continue;
                    }
                    let base = columns.get(6).copied().unwrap_or("*");
                    if base == "*" || base.is_empty() {
                        out.push(node.surface.to_string());
                    } else {
                        out.push(base.to_string());
                    }
                }
            }
            out
        };
        let sys_stats = counted_sys_prefix.stats();
        let lazy_stats = counted_sys_lazy.stats();
        // Classify sys.dic reads: trie region = [72, feature_offset), header < 72.
        println!("RESIDENT sys.dic prefix (header+trie+word-params, {} bytes):", feature_offset);
        println!("    read_calls={} bytes_read={} unique_pages={}", sys_stats.read_calls, sys_stats.bytes_read, sys_stats.unique_pages);
        println!(
            "LAZY feature region: read_calls={} bytes_read={} unique_pages={} ({} KiB)",
            lazy_stats.read_calls,
            lazy_stats.bytes_read,
            lazy_stats.unique_pages,
            lazy_stats.unique_pages * 4
        );
        println!("units = {}", units.len());
    }

    // ── throughput vs vibrato ──
    let v = text_analyzer_spike::vibrato_candidate::Analyzer::from_zstd_file(
        Path::new(RESOURCES_DIR).join("vibrato/system.dic.zst").as_path(),
    )
    .expect("vibrato load");
    let t = Instant::now();
    let v_units = v.analyze(&corpus);
    let v_elapsed = t.elapsed().as_secs_f64();
    println!(
        "THROUGHPUT vibrato: {} units in {:.3}s = {:.0} units/s",
        v_units.len(),
        v_elapsed,
        v_units.len() as f64 / v_elapsed
    );

    let a = Analyzer::open(
        Arc::new(HeapImage::from_vec(container.clone())),
        DictionaryProfile::japanese_ipadic(),
    )
    .expect("open");
    let t = Instant::now();
    let m_units = a.analyze(&corpus);
    let m_elapsed = t.elapsed().as_secs_f64();
    let ratio = (m_units.len() as f64 / m_elapsed) / (v_units.len() as f64 / v_elapsed);
    println!(
        "THROUGHPUT morph-dict (resident-split open): {} units in {:.3}s = {:.0} units/s (ratio vs vibrato {:.2}x)",
        m_units.len(),
        m_elapsed,
        m_units.len() as f64 / m_elapsed,
        ratio
    );
    assert_eq!(v_units, m_units, "throughput run must keep 100% parity");
}
