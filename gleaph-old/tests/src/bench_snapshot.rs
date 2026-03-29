//! PocketIC-based stable memory snapshot generators for canbench benchmarks.
//!
//! Each `gen_*_stable_snapshot` test deploys a graph canister, calls its setup
//! endpoints to populate data, then persists state via dedicated bench endpoints
//! (overlay + edge metadata) and extracts stable memory to the corresponding
//! `bench/<name>/stable_memory.bin` file.
//!
//! Run with:
//!   cargo test -p gleaph-tests -- --ignored gen_ecom_stable_snapshot

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use candid::{Principal, decode_one, encode_args};
use pocket_ic::{PocketIc, common::rest::BlobCompression};

// ---------------------------------------------------------------------------
// Progress reporter — background thread redraws the current line every second
// ---------------------------------------------------------------------------

struct Progress {
    inner: Arc<Mutex<ProgressInner>>,
    handle: Option<thread::JoinHandle<()>>,
}

struct ProgressInner {
    /// Current progress text (without elapsed suffix).
    line: String,
    /// Phase start time — reset by `finish()`.
    t0: Instant,
    /// Signal for the ticker thread to exit.
    stop: bool,
}

impl Progress {
    fn new() -> Self {
        let inner = Arc::new(Mutex::new(ProgressInner {
            line: String::new(),
            t0: Instant::now(),
            stop: false,
        }));
        let inner2 = inner.clone();
        let handle = thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                let s = inner2.lock().unwrap();
                if s.stop {
                    break;
                }
                if !s.line.is_empty() {
                    let elapsed = s.t0.elapsed().as_secs_f64();
                    eprint!("\r\x1b[K{} [{:.0}s]", s.line, elapsed);
                    let _ = std::io::stderr().flush();
                }
            }
        });
        Self {
            inner,
            handle: Some(handle),
        }
    }

    /// Update the live progress line.  Printed immediately and then refreshed
    /// every second by the ticker thread with an updated `[Ns]` suffix.
    fn set(&self, msg: impl Into<String>) {
        let mut s = self.inner.lock().unwrap();
        s.line = msg.into();
        let elapsed = s.t0.elapsed().as_secs_f64();
        eprint!("\r\x1b[K{} [{:.0}s]", s.line, elapsed);
        let _ = std::io::stderr().flush();
    }

    /// Seconds since the last `finish()` (or `new()`).
    fn elapsed(&self) -> f64 {
        self.inner.lock().unwrap().t0.elapsed().as_secs_f64()
    }

    /// Clear the live line, print a final message, and reset the phase timer.
    fn finish(&self, msg: &str) {
        let mut s = self.inner.lock().unwrap();
        s.line.clear();
        s.t0 = Instant::now();
        eprint!("\r\x1b[K{msg}\n");
        let _ = std::io::stderr().flush();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.inner.lock().unwrap().stop = true;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bench_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bench")
}

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
    let path = wasm_path(crate_stem);
    fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read wasm at {}: {e}\n\
             Build it first with the appropriate --features flag",
            path.display(),
        )
    })
}

fn update_call(
    pic: &PocketIc,
    canister_id: Principal,
    sender: Principal,
    method: &str,
    args: Vec<u8>,
) {
    pic.update_call(canister_id, sender, method, args)
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"));
}

fn update_call_decode<R>(pic: &PocketIc, canister_id: Principal, sender: Principal, method: &str, args: Vec<u8>) -> R
where
    R: candid::CandidType + for<'de> candid::Deserialize<'de>,
{
    let bytes = pic
        .update_call(canister_id, sender, method, args)
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"));
    decode_one(&bytes).unwrap_or_else(|e| panic!("{method} decode failed: {e:?}"))
}

/// Runs a batch loop with live progress.
#[allow(clippy::too_many_arguments)]
fn batch_loop(
    pic: &PocketIc,
    canister_id: Principal,
    sender: Principal,
    progress: &Progress,
    label: &str,
    method: &str,
    total: u32,
    batch_size: u32,
) {
    let num_calls = total.div_ceil(batch_size);
    for (i, start) in (0..total).step_by(batch_size as usize).enumerate() {
        let end = (start + batch_size).min(total);
        let pct = (i + 1) * 100 / num_calls as usize;
        progress.set(format!("  {label}: {end}/{total} ({pct}%)"));
        update_call(
            pic,
            canister_id,
            sender,
            method,
            encode_args((start, end)).expect("encode"),
        );
    }
    let elapsed = progress.elapsed();
    progress.finish(&format!(
        "  {label}: {total}/{total} (100%) [{elapsed:.1}s]"
    ));
}

// ---------------------------------------------------------------------------
// Snapshot generator
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires wasm built with --features bench-ecom and PocketIC runtime"]
fn gen_ecom_stable_snapshot() {
    let t_total = Instant::now();
    let progress = Progress::new();

    eprintln!("=== gen_ecom_stable_snapshot ===");

    // [1/7] Install canister.
    progress.set("[1/7] Installing canister...");
    let pic = PocketIc::new();
    let sender = Principal::anonymous();
    let canister_id = pic.create_canister();
    pic.add_cycles(canister_id, 10_000_000_000_000);
    let wasm = load_wasm("gleaph_graph");
    pic.install_canister(
        canister_id,
        wasm,
        // 524_288 = 2^19 (covers 300K vertices: 50K users + 50K products + 200K orders).
        // 2.8M edge capacity = 700K edges × 4 gap factor (avoids PMA rebalancing).
        encode_args((Some(524_288u32), Some(2_800_000u64))).expect("init arg"),
        Some(sender),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[1/7] Canister installed [{e:.1}s]"));

    // [2/7] Create indexes.
    progress.set("[2/7] Creating indexes...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_setup_ecom_indexes",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[2/7] Indexes created [{e:.1}s]"));

    // [3/7] Create vertices.
    let num_users = 50_000u32;
    let num_products = 50_000u32;
    let num_orders = num_users * 4; // 200K
    eprintln!("[3/7] Creating vertices (50K users + 50K products + 200K orders)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Users",
        "bench_setup_ecom_users",
        num_users,
        50_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Products",
        "bench_setup_ecom_products",
        num_products,
        50_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Orders",
        "bench_setup_ecom_orders",
        num_orders,
        50_000,
    );

    // [4/7] Create edges.
    eprintln!("[4/7] Creating ~700K edges (10K users/call)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Edges",
        "bench_setup_ecom_edges",
        num_users,
        10_000,
    );

    // [5/7] Persist overlay.
    progress.set("[5/7] Persisting overlay...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_persist_overlay",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[5/7] Overlay persisted [{e:.1}s]"));

    // [6/7] Extract stable memory.
    progress.set("[6/7] Extracting stable memory...");
    let stable_memory = pic.get_stable_memory(canister_id);
    let mem_mb = stable_memory.len() as f64 / 1_048_576.0;
    let e = progress.elapsed();
    progress.finish(&format!("[6/7] Extracted {mem_mb:.1} MB [{e:.1}s]"));

    // [7/7] Write to disk.
    progress.set("[7/7] Writing to disk...");
    let out_path = bench_dir().join("ecom").join("stable_memory.bin");
    fs::create_dir_all(out_path.parent().unwrap()).expect("create bench/ecom dir");
    fs::write(&out_path, &stable_memory).expect("write stable_memory.bin");
    let file_mb = stable_memory.len() as f64 / 1_048_576.0;
    let e = progress.elapsed();
    progress.finish(&format!(
        "[7/7] Wrote {file_mb:.1} MB to {} [{e:.1}s]",
        out_path.display(),
    ));

    eprintln!("=== Done in {:.0}s ===", t_total.elapsed().as_secs_f64());
}

// ---------------------------------------------------------------------------
// Social media benchmark snapshot
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires wasm built with --features bench-social and PocketIC runtime"]
fn gen_social_stable_snapshot() {
    let t_total = Instant::now();
    let progress = Progress::new();

    eprintln!("=== gen_social_stable_snapshot ===");

    // [1/7] Install canister.
    progress.set("[1/7] Installing canister...");
    let pic = PocketIc::new();
    let sender = Principal::anonymous();
    let canister_id = pic.create_canister();
    pic.add_cycles(canister_id, 10_000_000_000_000);
    let wasm = load_wasm("gleaph_graph");
    pic.install_canister(
        canister_id,
        wasm,
        // 524_288 = 2^19 (covers 255K vertices: 50K users + 100K posts + 100K comments + 5K hashtags).
        // 4_200_000 edge capacity = ~1.05M edges × 4 gap factor.
        encode_args((Some(524_288u32), Some(4_200_000u64))).expect("init arg"),
        Some(sender),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[1/7] Canister installed [{e:.1}s]"));

    // [2/7] Create indexes.
    progress.set("[2/7] Creating indexes...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_setup_social_indexes",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[2/7] Indexes created [{e:.1}s]"));

    // [3/7] Create vertices.
    let num_users = 50_000u32;
    let num_posts = 100_000u32;
    let num_comments = 100_000u32;
    let num_hashtags = 5_000u32;
    eprintln!("[3/7] Creating vertices (50K users + 100K posts + 100K comments + 5K hashtags)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Users",
        "bench_setup_social_users",
        num_users,
        50_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Posts",
        "bench_setup_social_posts",
        num_posts,
        50_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Comments",
        "bench_setup_social_comments",
        num_comments,
        50_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Hashtags",
        "bench_setup_social_hashtags",
        num_hashtags,
        5_000,
    );

    // [4/7] Create edges.
    eprintln!("[4/7] Creating ~1.05M edges (10K users/call)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Follow edges",
        "bench_setup_social_follow_edges",
        num_users,
        10_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Content edges",
        "bench_setup_social_content_edges",
        num_users,
        10_000,
    );

    // [5/7] Persist overlay.
    progress.set("[5/7] Persisting overlay...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_persist_overlay",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[5/7] Overlay persisted [{e:.1}s]"));

    // [6/7] Extract stable memory.
    progress.set("[6/7] Extracting stable memory...");
    let stable_memory = pic.get_stable_memory(canister_id);
    let mem_mb = stable_memory.len() as f64 / 1_048_576.0;
    let e = progress.elapsed();
    progress.finish(&format!("[6/7] Extracted {mem_mb:.1} MB [{e:.1}s]"));

    // [7/7] Write to disk.
    progress.set("[7/7] Writing to disk...");
    let out_path = bench_dir().join("social").join("stable_memory.bin");
    fs::create_dir_all(out_path.parent().unwrap()).expect("create bench/social dir");
    fs::write(&out_path, &stable_memory).expect("write stable_memory.bin");
    let file_mb = stable_memory.len() as f64 / 1_048_576.0;
    let e = progress.elapsed();
    progress.finish(&format!(
        "[7/7] Wrote {file_mb:.1} MB to {} [{e:.1}s]",
        out_path.display(),
    ));

    eprintln!("=== Done in {:.0}s ===", t_total.elapsed().as_secs_f64());
}

#[test]
#[ignore = "requires social stable snapshot, wasm built with --features bench-social, and PocketIC runtime"]
fn probe_social_profiled_queries_from_stable_snapshot() {
    let pic = PocketIc::new();
    let sender = Principal::anonymous();
    let canister_id = pic.create_canister();
    pic.add_cycles(canister_id, 10_000_000_000_000);
    pic.install_canister(
        canister_id,
        load_wasm("gleaph_graph"),
        encode_args((Some(524_288u32), Some(4_200_000u64))).expect("init arg"),
        Some(sender),
    );

    let snapshot_path = bench_dir().join("social").join("stable_memory.bin");
    let stable_memory = fs::read(&snapshot_path).unwrap_or_else(|e| {
        panic!(
            "failed to read social stable snapshot at {}: {e}",
            snapshot_path.display()
        )
    });
    pic.set_stable_memory(canister_id, stable_memory, BlobCompression::NoCompression);

    for name in [
        "content_virality",
        "feed",
        "engagement_rate",
        "cross_engagement",
        "fof_recommend",
        "thread_depth",
    ] {
        let result: Result<Vec<String>, gleaph_types::GleaphError> = update_call_decode(
            &pic,
            canister_id,
            sender,
            "bench_social_probe_query_profiled",
            encode_args((name.to_string(), None::<u32>, None::<u64>, true)).expect("encode"),
        );
        let lines = result.unwrap_or_else(|e| panic!("{name} probe failed: {e:?}"));
        eprintln!("=== social profile: {name} ===");
        for line in lines {
            eprintln!("{line}");
        }
    }
}

#[test]
#[ignore = "requires social stable snapshot, wasm built with --features bench-social, and PocketIC runtime"]
fn probe_social_thread_depth_from_stable_snapshot() {
    let pic = PocketIc::new();
    let sender = Principal::anonymous();
    let canister_id = pic.create_canister();
    pic.add_cycles(canister_id, 10_000_000_000_000);
    pic.install_canister(
        canister_id,
        load_wasm("gleaph_graph"),
        encode_args((Some(524_288u32), Some(4_200_000u64))).expect("init arg"),
        Some(sender),
    );

    let snapshot_path = bench_dir().join("social").join("stable_memory.bin");
    let stable_memory = fs::read(&snapshot_path).unwrap_or_else(|e| {
        panic!(
            "failed to read social stable snapshot at {}: {e}",
            snapshot_path.display()
        )
    });
    pic.set_stable_memory(canister_id, stable_memory, BlobCompression::NoCompression);

    let gql = "MATCH (u:User {verified: 1}) \
               WITH u LIMIT 50 \
               MATCH (u)-[:Posted]->(p:Post)<-[:ReplyTo]-(c:Comment)<-[:ReplyToComment*1..3]-(deep:Comment) \
               RETURN p.id, COUNT(DISTINCT deep) AS chain_length \
               ORDER BY chain_length DESC LIMIT 10";
    let result: Result<Vec<String>, gleaph_types::GleaphError> = update_call_decode(
        &pic,
        canister_id,
        sender,
        "bench_social_probe_gql_profiled",
        encode_args((gql.to_string(), None::<u32>, None::<u64>, true)).expect("encode"),
    );
    let lines = result.unwrap_or_else(|e| panic!("thread_depth probe failed: {e:?}"));
    eprintln!("=== social profile: thread_depth_scaled ===");
    for line in lines {
        eprintln!("{line}");
    }
}

// ---------------------------------------------------------------------------
// Timeline benchmark snapshot
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires wasm built with --features bench-timeline and PocketIC runtime"]
fn gen_timeline_stable_snapshot() {
    let t_total = Instant::now();
    let progress = Progress::new();

    eprintln!("=== gen_timeline_stable_snapshot ===");

    // [1/8] Install canister.
    progress.set("[1/8] Installing canister...");
    let pic = PocketIc::new();
    let sender = Principal::anonymous();
    let canister_id = pic.create_canister();
    pic.add_cycles(canister_id, 10_000_000_000_000);
    let wasm = load_wasm("gleaph_graph");
    pic.install_canister(
        canister_id,
        wasm,
        // 131_072 = 2^17 (covers 60K vertices: 10K users + 50K posts).
        // 6_000_000 edge capacity = ~1.5M edges × 4 gap factor (Timeline fan-out dominates).
        encode_args((Some(131_072u32), Some(6_000_000u64))).expect("init arg"),
        Some(sender),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[1/8] Canister installed [{e:.1}s]"));

    // [2/8] Create indexes.
    progress.set("[2/8] Creating indexes...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_setup_timeline_indexes",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[2/8] Indexes created [{e:.1}s]"));

    // [3/8] Create vertices.
    let num_users = 10_000u32;
    let num_posts = 50_000u32;
    eprintln!("[3/8] Creating vertices (10K users + 50K posts)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Users",
        "bench_setup_timeline_users",
        num_users,
        1_000,
    );
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Posts",
        "bench_setup_timeline_posts",
        num_posts,
        10_000,
    );

    // [4/8] Create Follow edges.
    eprintln!("[4/8] Creating ~100K Follow edges...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Follow edges",
        "bench_setup_timeline_follows",
        num_users,
        2_000,
    );

    // [5/8] Create Posted edges.
    eprintln!("[5/8] Creating 50K Posted edges...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Posted edges",
        "bench_setup_timeline_posted",
        num_users,
        2_000,
    );

    // [6/8] Create Timeline edges (fan-out for non-celebrity posts).
    eprintln!("[6/8] Creating Timeline edges (fan-out, may take a while)...");
    batch_loop(
        &pic,
        canister_id,
        sender,
        &progress,
        "Timeline edges",
        "bench_setup_timeline_fanout",
        num_users,
        500,
    );

    // [7/8] Persist overlay.
    progress.set("[7/8] Persisting overlay...");
    update_call(
        &pic,
        canister_id,
        sender,
        "bench_persist_overlay",
        encode_args(()).expect("encode empty"),
    );
    let e = progress.elapsed();
    progress.finish(&format!("[7/8] Overlay persisted [{e:.1}s]"));

    // [8/8] Extract stable memory and write to disk.
    progress.set("[8/8] Extracting stable memory...");
    let stable_memory = pic.get_stable_memory(canister_id);
    let mem_mb = stable_memory.len() as f64 / 1_048_576.0;
    let e = progress.elapsed();
    progress.finish(&format!("[8/8] Extracted {mem_mb:.1} MB [{e:.1}s]"));

    let out_path = bench_dir().join("timeline").join("stable_memory.bin");
    fs::create_dir_all(out_path.parent().unwrap()).expect("create bench/timeline dir");
    fs::write(&out_path, &stable_memory).expect("write stable_memory.bin");
    let file_mb = stable_memory.len() as f64 / 1_048_576.0;
    progress.finish(&format!("Wrote {file_mb:.1} MB to {}", out_path.display(),));

    eprintln!("=== Done in {:.0}s ===", t_total.elapsed().as_secs_f64());
}
