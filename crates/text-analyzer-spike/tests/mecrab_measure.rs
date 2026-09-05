//! Plan 0333 — measurement harness (todo 4): dictionary load time, ByteImage access
//! statistics across LRU budgets, native throughput vs vibrato, upgrade-rebind
//! projection inputs. Prints tables to stdout; numbers are recorded in the plan/report.
//!
//! Run: `cargo test --features "vibrato,mecrab-dict" --test mecrab_measure -- --nocapture --release`

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use text_analyzer_spike::mecrab_vendor::byteimage::{
    ByteImage, HeapImage, SimulatedStableImage,
};
use text_analyzer_spike::mecrab_vendor::MecrabAnalyzer;

const RESOURCES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/resources");

fn read_file(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(RESOURCES_DIR).join("mecrab").join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn image_sizes() -> [(&'static str, u64); 4] {
    let names = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
    names.map(|n| (n, read_file(n).len() as u64))
}

/// Load the four images as HeapImage, run the analyzer over `text`, return (load_ms, units).
fn run_heap(text: &str) -> (f64, usize) {
    let t = Instant::now();
    let sys = Arc::new(HeapImage::from_vec(read_file("sys.dic"))) as Arc<dyn ByteImage>;
    let unk = Arc::new(HeapImage::from_vec(read_file("unk.dic"))) as Arc<dyn ByteImage>;
    let matrix = Arc::new(HeapImage::from_vec(read_file("matrix.bin"))) as Arc<dyn ByteImage>;
    let char_bin = Arc::new(HeapImage::from_vec(read_file("char.bin"))) as Arc<dyn ByteImage>;
    let a = MecrabAnalyzer::from_images(
        std::sync::Arc::clone(&sys),
        unk,
        matrix,
        char_bin,
    )
    .expect("load");
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let units = a.analyze(text).len();
    (load_ms, units)
}

/// Same images through SimulatedStableImage with a cache budget; returns
/// (load_ms, units, AccessStats aggregated over the four images).
fn run_simulated(text: &str, budget: usize) -> (f64, usize, Vec<(String, text_analyzer_spike::mecrab_vendor::byteimage::AccessStats)>) {
    let t = Instant::now();
    let names = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
    let mut images: Vec<Arc<SimulatedStableImage>> = Vec::new();
    for n in names {
        images.push(Arc::new(SimulatedStableImage::new(read_file(n), budget)));
    }
    let a = MecrabAnalyzer::from_images(
        images[0].clone(),
        images[1].clone(),
        images[2].clone(),
        images[3].clone(),
    )
    .expect("simulated image dict load");
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let units = a.analyze(text).len();
    let stats = images
        .iter()
        .zip(names)
        .map(|(img, n)| (n.to_string(), img.stats()))
        .collect();
    (load_ms, units, stats)
}

/// ~1 MB newline-separated Japanese corpus (same generator as the parity test).
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
    println!("corpus bytes = {}", corpus.len());

    // ── (1) load time: HeapImage (3 runs) ──
    let mut heap_loads = Vec::new();
    for _ in 0..3 {
        let (ms, units) = run_heap(&corpus);
        heap_loads.push(ms);
        println!("heap: load_ms={ms:.1} units={units}");
    }
    println!("HEAP load_ms (mean of 3, incl. file read + validation) = {:.1}", mean(&heap_loads));

    // warm heap load excluding fs::read: simulate the upgrade shape where bytes are
    // already in memory (from stable memory read) — time from_image only.
    let bytes: Vec<Vec<u8>> = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"]
        .iter()
        .map(|n| read_file(n))
        .collect();
    let t = Instant::now();
    let _ = MecrabAnalyzer::from_images(
        Arc::new(HeapImage::from_vec(bytes[0].clone())),
        Arc::new(HeapImage::from_vec(bytes[1].clone())),
        Arc::new(HeapImage::from_vec(bytes[2].clone())),
        Arc::new(HeapImage::from_vec(bytes[3].clone())),
    )
    .unwrap();
    println!(
        "HEAP from_images only (bytes pre-resident): {:.3} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    // ── (2) SimulatedStableImage budgets ──
    for budget in [256 * 1024usize, 1 << 20, 4 << 20, 16 << 20] {
        let (load_ms, units, stats) = run_simulated(&corpus, budget);
        println!("SIMULATED budget={}KiB load_ms={:.1} units={}", budget / 1024, load_ms, units);
        let mut tot_calls = 0u64;
        let mut tot_bytes = 0u64;
        let mut tot_unique = 0u64;
        let mut tot_loads = 0u64;
        for (name, s) in &stats {
            println!(
                "    {name}: read_calls={} bytes_read={} unique_frames={} hot_set_KiB={} frame_loads={} hit_rate={:.3}",
                s.read_calls,
                s.bytes_read,
                s.unique_frames,
                s.hot_set_bytes() / 1024,
                s.frame_loads,
                s.hit_rate()
            );
            tot_calls += s.read_calls;
            tot_bytes += s.bytes_read;
            tot_unique += s.hot_set_bytes();
            let _ = tot_loads;
            tot_loads += s.frame_loads;
        }
        println!(
            "    TOTAL: read_calls={tot_calls} bytes_read={tot_bytes} unique_bytes={tot_unique} frame_loads={tot_loads}"
        );
    }

    // ── (3) throughput tokens/s ──
    // vibrato baseline
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

    let (m_units, m_elapsed) = {
        let a = MecrabAnalyzer::from_dir(Path::new(RESOURCES_DIR).join("mecrab").as_path())
            .expect("mecrab load");
        let t = Instant::now();
        let u = a.analyze(&corpus);
        (u.len(), t.elapsed().as_secs_f64())
    };
    println!(
        "THROUGHPUT mecrab (HeapImage): {m_units} units in {m_elapsed:.3}s = {:.0} units/s (ratio vs vibrato {:.2}x)",
        m_units as f64 / m_elapsed,
        (m_units as f64 / m_elapsed) / (v_units.len() as f64 / v_elapsed)
    );

    // throughput under SimulatedStableImage 1 MiB budget (steady-state cache)
    {
        let budget = 4 << 20;
        let images: Vec<Arc<SimulatedStableImage>> = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"]
            .iter()
            .map(|n| Arc::new(SimulatedStableImage::new(read_file(n), budget)))
            .collect();
        let a = MecrabAnalyzer::from_images(
            images[0].clone(), images[1].clone(), images[2].clone(), images[3].clone(),
        )
        .unwrap();
        let _ = a.analyze(&corpus); // warm
        let t = Instant::now();
        let u = a.analyze(&corpus);
        let el = t.elapsed().as_secs_f64();
        let s0 = images[0].stats();
        println!(
            "THROUGHPUT mecrab (SimulatedStableImage {budget}KiB budget): {} units in {el:.3}s = {:.0} units/s; sys.dic hit_rate={:.3}",
            u.len(),
            u.len() as f64 / el,
            s0.hit_rate()
        );
    }

    // ── image sizes ──
    for (n, sz) in image_sizes() {
        println!("IMAGE {n}: {sz} bytes");
    }
    let total: u64 = image_sizes().iter().map(|(_, s)| s).sum();
    println!("IMAGE total: {total} bytes ({:.1} MiB)", total as f64 / 1048576.0);
}