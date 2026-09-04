//! Native measurement harness for plan 0330: heap footprint after dictionary load
//! (counting global allocator) and analysis throughput over a ~1 MB Japanese corpus.
//! wasm32 size deltas are recorded separately by `build_wasm.sh` (wasm files carry no
//! runtime allocator to count here).
//!
//! Run one candidate per invocation: `cargo test -p text-analyzer-spike --features X
//! --test measure -- --nocapture`.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { std::alloc::System.alloc(layout) };
        if !ptr.is_null() {
            let cur = ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(cur, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

use text_analyzer_spike::harness::candidates;

/// ~1 MB Japanese sample corpus (repeated real paragraph; the repeat is intentional —
/// throughput is a unit-time measurement, not a corpus-variety one).
fn corpus() -> String {
    let paragraph = "昨日、附属病院の前の公園を全力で走った。データは整理して送付する。\
                     本とカレーの街神保町へようこそ。機械学習のモデルを更新しました。";
    let mut text = String::new();
    while text.chars().count() < 1_000_000 {
        text.push_str(paragraph);
    }
    text
}

#[test]
fn measure_heap_and_throughput() {
    let text = corpus();
    let chars = text.chars().count();

    for (name, analyze) in candidates() {
        // Warm-up + load: the first call triggers the (counted) dictionary load.
        // Drop all outputs before reading the steady state so the unit stream is not
        // counted; the corpus string's own bytes are subtracted explicitly.
        let corpus_bytes = text.len();
        PEAK.store(0, Ordering::Relaxed);
        {
            let _units = (analyze)(text.as_str());
        }
        let heap_after_load = ALLOCATED.load(Ordering::Relaxed) - corpus_bytes;
        let heap_peak = PEAK.load(Ordering::Relaxed) - corpus_bytes;

        // Throughput: chars/s over the corpus (warm).
        let start = std::time::Instant::now();
        let mut total_units = 0usize;
        for _ in 0..3 {
            total_units += (analyze)(text.as_str()).len();
        }
        let units = total_units / 3;
        let elapsed = start.elapsed();
        let chars_per_s = (chars as f64 * 3.0) / elapsed.as_secs_f64();
        println!(
            "measure[{name}]: heap_after_load={:.1}MB heap_peak_during_load={:.1}MB \
             throughput={:.0} chars/s elapsed={:.3}s units_per_run={}",
            mb(heap_after_load),
            mb(heap_peak),
            chars_per_s,
            elapsed.as_secs_f64(),
            units
        );
        let _ = units;
    }
}
