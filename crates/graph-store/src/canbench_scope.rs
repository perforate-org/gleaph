//! Optional [`canbench_rs::bench_scope`] for instruction-level splits in PMA flush paths.

#[cfg(feature = "canbench-rs")]
pub(crate) fn scope(label: &'static str) -> canbench_rs::BenchScope {
    canbench_rs::bench_scope(label)
}

#[cfg(not(feature = "canbench-rs"))]
pub(crate) fn scope(_label: &'static str) {}
