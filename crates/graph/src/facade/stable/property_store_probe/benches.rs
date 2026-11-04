//! Managed full-store fixtures. Generation/oracles/audits/result destruction are outside timing.
use super::{
    Arena, Candidate, Direct, EdgePropertyKey, Fixture, FixtureKey, Kind, LARGE, ProbeStore, Value,
    VertexPropertyKey, chunks, encoded, scalar, text,
};
use canbench_rs::{BenchResult, bench, bench_fn};
use ic_stable_structures::Memory;
use std::hint::black_box;

impl<K: FixtureKey, S: ProbeStore<K>> Fixture<K, S> {
    fn seed(&mut self, key: K, value: Value) {
        let bytes = encoded(value.clone());
        let previous = self.store.set(key, value).unwrap().map(encoded);
        assert_eq!(previous, self.expected.insert(key, bytes));
    }
    fn erase(&mut self, key: K) {
        assert_eq!(
            self.store.remove(key).unwrap().map(encoded),
            self.expected.remove(&key)
        );
    }
    fn local_reopen(self) -> Self {
        let before = self.pages();
        let store = self.store.reopen();
        let result = Self { store, ..self };
        assert_eq!(result.pages(), before);
        result.audit();
        result
    }
    fn pages(&self) -> (u64, Vec<u64>) {
        (
            ic_cdk::api::stable_size(),
            self.store.memories().iter().map(Memory::size).collect(),
        )
    }
    fn before(&self) -> (u64, Vec<u64>) {
        self.audit();
        self.pages()
    }
    fn finish(&self, case: &str, before: (u64, Vec<u64>), result: BenchResult) -> BenchResult {
        let after = self.pages();
        let stats = self.audit();
        assert_eq!(self.pages(), after, "audit changed backing");
        assert_eq!(result.total.stable_memory_increase, after.0 - before.0);
        for pages in [&before, &after] {
            assert_eq!(pages.1.len(), S::KIND.regions());
            assert_eq!(
                pages.0,
                2 + pages.1.iter().map(|p| p.div_ceil(4) * 4).sum::<u64>()
            );
        }
        let regions = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(":");
        ic_cdk::println!(
            "PROPERTY_IC,{},{},{},{case},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            K::BOUND.max_size(),
            S::KIND.tag(),
            S::KIND.cap(),
            before.0,
            after.0,
            regions(&before.1),
            regions(&after.1),
            self.expected.len(),
            self.expected.values().map(Vec::len).sum::<usize>(),
            stats.map_entry_bytes,
            stats.entries,
            stats.body_key_bytes,
            stats.end,
            stats.live,
            stats.free,
            stats.holes,
            stats.external_bodies,
            stats.external_payload_bytes,
            result.total.instructions,
            result.total.heap_increase,
            result.total.stable_memory_increase
        );
        result
    }
    fn measured_set(&mut self, case: &str, key: K, value: Value) -> BenchResult {
        let bytes = encoded(value.clone());
        let previous = self.expected.get(&key).cloned();
        let before = self.before();
        let mut actual = None;
        let result = bench_fn(|| {
            actual = black_box(self.store.set(black_box(key), black_box(value)).unwrap())
        });
        assert_eq!(actual.map(encoded), previous);
        self.expected.insert(key, bytes);
        self.finish(case, before, result)
    }
}

#[derive(Clone, Copy, Debug)]
enum Scalars {
    Insert,
    Get,
    Replace,
    Remove,
    Enumerate,
}
fn scalar_keys<K: FixtureKey>() -> Vec<K> {
    let scopes = [0, 1, 2, 3, u32::MAX];
    let keys = scopes
        .into_iter()
        .enumerate()
        .flat_map(|(i, scope)| {
            let count = if i == 4 { 52 } else { 51 };
            (1..=count).map(move |p| K::fixture(scope, if p == count { u32::MAX } else { p }))
        })
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 256);
    keys
}
fn scalar_case<K: FixtureKey, S: ProbeStore<K>>(case: Scalars) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    let keys = scalar_keys::<K>();
    if !matches!(case, Scalars::Insert) {
        for (n, key) in keys.iter().enumerate() {
            f.seed(*key, scalar(n));
        }
    }
    let before = f.before();
    let result = match case {
        Scalars::Insert | Scalars::Replace => {
            let values = (0..256)
                .map(|n| {
                    scalar(
                        n + if matches!(case, Scalars::Replace) {
                            17
                        } else {
                            0
                        },
                    )
                })
                .collect::<Vec<_>>();
            let bytes = values.iter().cloned().map(encoded).collect::<Vec<_>>();
            let wanted = keys
                .iter()
                .map(|k| f.expected.get(k).cloned())
                .collect::<Vec<_>>();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = keys
                    .iter()
                    .zip(values)
                    .map(|(k, v)| f.store.set(black_box(*k), black_box(v)).unwrap())
                    .collect();
            });
            assert_eq!(
                actual
                    .into_iter()
                    .map(|v| v.map(encoded))
                    .collect::<Vec<_>>(),
                wanted
            );
            f.expected.extend(keys.iter().copied().zip(bytes));
            result
        }
        Scalars::Get => {
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = keys
                    .iter()
                    .map(|k| f.store.get(black_box(*k)).unwrap())
                    .collect()
            });
            for (key, value) in keys.iter().zip(actual) {
                assert_eq!(value.map(encoded).as_ref(), f.expected.get(key));
            }
            assert_eq!(f.pages(), before);
            result
        }
        Scalars::Remove => {
            let selected = keys.iter().step_by(2).copied().collect::<Vec<_>>();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = selected
                    .iter()
                    .map(|k| f.store.remove(black_box(*k)).unwrap())
                    .collect()
            });
            for (key, value) in selected.into_iter().zip(actual) {
                assert_eq!(value.map(encoded), f.expected.remove(&key));
            }
            result
        }
        Scalars::Enumerate => {
            let scope = K::fixture(0, 0);
            let wanted = f
                .expected
                .iter()
                .filter(|(k, _)| k.scope() == scope)
                .map(|(k, b)| (k.property(), b.clone()))
                .collect::<Vec<_>>();
            assert_eq!(wanted.len(), 51);
            let mut actual = Vec::new();
            let result = bench_fn(|| actual = f.store.enumerate(black_box(scope)).unwrap());
            assert_eq!(
                actual
                    .into_iter()
                    .map(|(p, v)| (p, encoded(v)))
                    .collect::<Vec<_>>(),
                wanted
            );
            assert_eq!(f.pages(), before);
            result
        }
    };
    f.finish(&format!("scalar_{case:?}"), before, result)
}

// Old-value availability microbenchmark, not Graph index planning/outbox execution.
fn observe(previous: Option<&Value>) -> Option<bool> {
    black_box(previous).map(|value| matches!(black_box(value), Value::Null))
}
fn read_then_set<K: FixtureKey, S: ProbeStore<K>>(
    store: &mut S,
    key: K,
    value: Value,
) -> (Option<bool>, Option<Value>) {
    let old = store.get(key).unwrap();
    let seen = observe(old.as_ref());
    (seen, store.set(key, value).unwrap())
}
fn shared_set<K: FixtureKey>(
    store: &mut Candidate<K, Arena>,
    key: K,
    value: Value,
) -> (Option<bool>, Option<Value>) {
    store
        .with_prepared(key, Some(value), chunks::BUDGET, |p| {
            (observe(p.previous()), p.commit())
        })
        .unwrap()
}
#[derive(Clone, Copy, Debug)]
enum ReuseInput {
    Mixed256,
    Large,
}
fn reuse_case<K: FixtureKey, S: ProbeStore<K>>(
    input: ReuseInput,
    mut update: impl FnMut(&mut S, K, Value) -> (Option<bool>, Option<Value>),
) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    let keys = match input {
        ReuseInput::Mixed256 => scalar_keys::<K>(),
        ReuseInput::Large => vec![K::fixture(0, 1)],
    };
    let mut wanted = Vec::new();
    let mut seen = Vec::new();
    let mut values = Vec::new();
    for (n, key) in keys.iter().enumerate() {
        let (old, next) = match input {
            ReuseInput::Mixed256 => (scalar(n), scalar(n + 17)),
            ReuseInput::Large => (text(LARGE, 1), text(LARGE, 2)),
        };
        wanted.push(Some(encoded(old.clone())));
        seen.push(observe(Some(&old)));
        f.seed(*key, old);
        values.push(next);
    }
    let bytes = values.iter().cloned().map(encoded).collect::<Vec<_>>();
    let before = f.before();
    let mut actual = Vec::new();
    let result = bench_fn(|| {
        actual = keys
            .iter()
            .zip(values)
            .map(|(key, value)| update(&mut f.store, black_box(*key), black_box(value)))
            .collect();
    });
    assert_eq!(actual.iter().map(|v| v.0).collect::<Vec<_>>(), seen);
    assert_eq!(
        actual
            .into_iter()
            .map(|v| v.1.map(encoded))
            .collect::<Vec<_>>(),
        wanted
    );
    f.expected.extend(keys.into_iter().zip(bytes));
    f.finish(&format!("previous_{input:?}"), before, result)
}
macro_rules! reuse_benches {
    ($key:ty, $store:ty, $update:ident; $small:ident, $large:ident) => {
        #[bench(raw)]
        fn $small() -> BenchResult {
            reuse_case::<$key, $store>(ReuseInput::Mixed256, $update)
        }
        #[bench(raw)]
        fn $large() -> BenchResult {
            reuse_case::<$key, $store>(ReuseInput::Large, $update)
        }
    };
}
reuse_benches!(VertexPropertyKey, Direct<VertexPropertyKey>, read_then_set; ps_v_d_prev256, ps_v_d_prev1m);
reuse_benches!(VertexPropertyKey, Candidate<VertexPropertyKey, Arena>, read_then_set; ps_v_x_prev256, ps_v_x_prev1m);
reuse_benches!(VertexPropertyKey, Candidate<VertexPropertyKey, Arena>, shared_set; ps_v_x_shared256, ps_v_x_shared1m);
reuse_benches!(EdgePropertyKey, Direct<EdgePropertyKey>, read_then_set; ps_e_d_prev256, ps_e_d_prev1m);
reuse_benches!(EdgePropertyKey, Candidate<EdgePropertyKey, Arena>, read_then_set; ps_e_x_prev256, ps_e_x_prev1m);
reuse_benches!(EdgePropertyKey, Candidate<EdgePropertyKey, Arena>, shared_set; ps_e_x_shared256, ps_e_x_shared1m);

#[derive(Clone, Copy, Debug)]
enum Point {
    Read1m,
    Replace1m,
    Shrink1m,
    Read65,
    Replace65,
    Read66,
    Replace66,
}
fn point_case<K: FixtureKey, S: ProbeStore<K>>(case: Point) -> BenchResult {
    let (len, neighbor, replacement) = match case {
        Point::Read1m => (LARGE, 65, None),
        Point::Replace1m => (LARGE, 65, Some(LARGE)),
        Point::Shrink1m => (LARGE, 65, Some(65)),
        Point::Read65 => (65, LARGE, None),
        Point::Replace65 => (65, LARGE, Some(65)),
        Point::Read66 => (66, LARGE, None),
        Point::Replace66 => (66, LARGE, Some(66)),
    };
    let mut f = Fixture::<K, S>::new();
    let key = K::fixture(0, 2);
    f.seed(K::fixture(0, 1), text(neighbor, 7));
    f.seed(key, text(len, 3));
    let name = format!("point_{case:?}");
    if let Some(len) = replacement {
        return f.measured_set(&name, key, text(len, 13));
    }
    let before = f.before();
    let mut actual = None;
    let result = bench_fn(|| actual = black_box(f.store.get(black_box(key)).unwrap()));
    assert_eq!(actual.map(encoded).as_ref(), f.expected.get(&key));
    assert_eq!(f.pages(), before);
    f.finish(&name, before, result)
}

#[derive(Clone, Copy, Debug)]
enum Cycle {
    ColdGrow,
    WarmGrow,
    WarmShrink,
}
fn cycle_case<K: FixtureKey, S: ProbeStore<K>>(phase: Cycle) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    let key = K::fixture(0, 1);
    f.seed(key, text(65, 1));
    if !matches!(phase, Cycle::ColdGrow) {
        f.seed(key, text(LARGE, 2));
        f.seed(key, text(65, 3));
        f = f.local_reopen();
        if matches!(phase, Cycle::WarmShrink) {
            f.seed(key, text(LARGE, 4));
        }
    }
    let before = f.pages();
    let len = if matches!(phase, Cycle::WarmShrink) {
        65
    } else {
        LARGE
    };
    let result = f.measured_set(&format!("cycle_{phase:?}"), key, text(len, 9));
    if S::KIND == Kind::Extent && !matches!(phase, Cycle::ColdGrow) {
        assert_eq!(f.pages(), before);
    }
    result
}

#[derive(Clone, Copy, Debug)]
enum Fragment {
    NoFit,
    Coalesced,
}
fn fragment_case<K: FixtureKey, S: ProbeStore<K>>(phase: Fragment) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    for i in 0..128 {
        f.seed(K::fixture(0, 2 * i + 1), text(66, i as u8));
        f.seed(
            K::fixture(0, 2 * i + 2),
            text(97, (i as u8).wrapping_add(7)),
        );
    }
    for i in 0..128 {
        f.erase(K::fixture(0, 2 * i + 1));
    }
    let stats = f.audit();
    if S::KIND == Kind::Extent {
        assert_eq!(
            (stats.end, stats.live, stats.free, stats.holes),
            (26688, 15360, 11264, 128)
        );
    }
    let (key, len) = match phase {
        Fragment::NoFit => (K::fixture(0, 1000), 97),
        Fragment::Coalesced => {
            f.seed(K::fixture(0, 1000), text(97, 2));
            f.erase(K::fixture(0, 2));
            (K::fixture(0, 1001), 257)
        }
    };
    let end = f.audit().end;
    let result = f.measured_set(&format!("fragment_{phase:?}"), key, text(len, 11));
    if S::KIND == Kind::Extent {
        let stats = f.audit();
        match phase {
            Fragment::NoFit => {
                assert_eq!(f.address(key).unwrap().offset, end);
                assert_eq!(stats.end, end + 120);
            }
            Fragment::Coalesced => {
                assert_eq!(f.address(key).unwrap().offset, 64);
                assert_eq!((stats.end, stats.free, stats.holes), (end, 11088, 126));
            }
        }
    }
    result
}

#[derive(Clone, Copy, Debug)]
enum Move {
    Small,
    Large,
}
fn move_case<K: FixtureKey, S: ProbeStore<K>>(case: Move) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    let source = K::fixture(0, 1);
    let target = K::fixture(1, 1);
    let (from, to) = match case {
        Move::Small => (65, 97),
        Move::Large => (LARGE, LARGE),
    };
    f.seed(source, text(from, 1));
    f.seed(target, text(to, 7));
    f.seed(K::fixture(0, 2), text(65, 5));
    let address = (S::KIND == Kind::Extent)
        .then(|| f.address(source))
        .flatten();
    let bytes = f.expected[&source].clone();
    let before = f.before();
    let mut actual = None;
    let result = bench_fn(|| {
        actual = black_box(f.store.rekey(black_box(source), black_box(target)).unwrap())
    });
    assert_eq!(actual.map(encoded), Some(bytes.clone()));
    f.expected.remove(&source);
    f.expected.insert(target, bytes);
    if let Some(address) = address {
        assert_eq!(f.address(target), Some(address));
    }
    f.finish(&format!("move_{case:?}"), before, result)
}

#[derive(Clone, Copy, Debug)]
enum Relocation {
    Grow,
    Remove,
}
fn relocation_case<K: FixtureKey, S: ProbeStore<K>>(phase: Relocation) -> BenchResult {
    let mut f = Fixture::<K, S>::new();
    let source = K::fixture(0, 1);
    f.seed(source, text(66, 1));
    f.seed(K::fixture(0, 2), text(97, 7));
    f.seed(K::fixture(0, 3), text(LARGE, 5));
    let end = f.audit().end;
    let old = (S::KIND == Kind::Extent)
        .then(|| f.address(source))
        .flatten();
    let check = |f: &Fixture<K, S>| {
        if let Some(old) = old {
            assert_eq!(end, 1048864);
            assert_eq!(f.address(source).unwrap().offset, end);
            assert_ne!(f.address(source), Some(old));
            assert_eq!(f.audit().end, end + 1048592);
        }
    };
    match phase {
        Relocation::Grow => {
            let result = f.measured_set("relocation_Grow", source, text(LARGE, 13));
            check(&f);
            result
        }
        Relocation::Remove => {
            f.seed(source, text(LARGE, 13));
            check(&f);
            let before = f.before();
            let mut actual = None;
            let result =
                bench_fn(|| actual = black_box(f.store.remove(black_box(source)).unwrap()));
            assert_eq!(actual.map(encoded), f.expected.remove(&source));
            if S::KIND == Kind::Extent {
                assert_eq!(f.pages(), before);
            }
            f.finish("relocation_Remove", before, result)
        }
    }
}

macro_rules! cases {
    ($key:ty, $store:ty; $($name:ident => $run:ident($case:expr)),+ $(,)?) => {
        $(#[bench(raw)] fn $name() -> BenchResult { $run::<$key,$store>($case) })+
    };
}

cases! { VertexPropertyKey, Direct<VertexPropertyKey>;
    ps_v_d_ins256 => scalar_case(Scalars::Insert),
    ps_v_d_get256 => scalar_case(Scalars::Get),
    ps_v_d_set256 => scalar_case(Scalars::Replace),
    ps_v_d_del128 => scalar_case(Scalars::Remove),
    ps_v_d_enum51 => scalar_case(Scalars::Enumerate),
    ps_v_d_r1m => point_case(Point::Read1m),
    ps_v_d_w1m => point_case(Point::Replace1m),
    ps_v_d_s1m => point_case(Point::Shrink1m),
    ps_v_d_r65 => point_case(Point::Read65),
    ps_v_d_w65 => point_case(Point::Replace65),
    ps_v_d_r66 => point_case(Point::Read66),
    ps_v_d_w66 => point_case(Point::Replace66),
    ps_v_d_cold => cycle_case(Cycle::ColdGrow),
    ps_v_d_warmg => cycle_case(Cycle::WarmGrow),
    ps_v_d_warms => cycle_case(Cycle::WarmShrink),
    ps_v_d_nofit => fragment_case(Fragment::NoFit),
    ps_v_d_coal => fragment_case(Fragment::Coalesced),
    ps_v_d_m65 => move_case(Move::Small),
    ps_v_d_m1m => move_case(Move::Large),
    ps_v_d_reloc => relocation_case(Relocation::Grow),
    ps_v_d_delrel => relocation_case(Relocation::Remove),
}

cases! { VertexPropertyKey, Candidate<VertexPropertyKey, chunks::Store<4096, 0, VertexPropertyKey>>;
    ps_v_c4_ins256 => scalar_case(Scalars::Insert),
    ps_v_c4_get256 => scalar_case(Scalars::Get),
    ps_v_c4_set256 => scalar_case(Scalars::Replace),
    ps_v_c4_del128 => scalar_case(Scalars::Remove),
    ps_v_c4_enum51 => scalar_case(Scalars::Enumerate),
    ps_v_c4_r1m => point_case(Point::Read1m),
    ps_v_c4_w1m => point_case(Point::Replace1m),
    ps_v_c4_s1m => point_case(Point::Shrink1m),
    ps_v_c4_r65 => point_case(Point::Read65),
    ps_v_c4_w65 => point_case(Point::Replace65),
    ps_v_c4_r66 => point_case(Point::Read66),
    ps_v_c4_w66 => point_case(Point::Replace66),
    ps_v_c4_cold => cycle_case(Cycle::ColdGrow),
    ps_v_c4_warmg => cycle_case(Cycle::WarmGrow),
    ps_v_c4_warms => cycle_case(Cycle::WarmShrink),
    ps_v_c4_nofit => fragment_case(Fragment::NoFit),
    ps_v_c4_coal => fragment_case(Fragment::Coalesced),
    ps_v_c4_m65 => move_case(Move::Small),
    ps_v_c4_m1m => move_case(Move::Large),
    ps_v_c4_reloc => relocation_case(Relocation::Grow),
    ps_v_c4_delrel => relocation_case(Relocation::Remove),
}

cases! { VertexPropertyKey, Candidate<VertexPropertyKey, chunks::Store<16384, 0, VertexPropertyKey>>;
    ps_v_c16_ins256 => scalar_case(Scalars::Insert),
    ps_v_c16_get256 => scalar_case(Scalars::Get),
    ps_v_c16_set256 => scalar_case(Scalars::Replace),
    ps_v_c16_del128 => scalar_case(Scalars::Remove),
    ps_v_c16_enum51 => scalar_case(Scalars::Enumerate),
    ps_v_c16_r1m => point_case(Point::Read1m),
    ps_v_c16_w1m => point_case(Point::Replace1m),
    ps_v_c16_s1m => point_case(Point::Shrink1m),
    ps_v_c16_r65 => point_case(Point::Read65),
    ps_v_c16_w65 => point_case(Point::Replace65),
    ps_v_c16_r66 => point_case(Point::Read66),
    ps_v_c16_w66 => point_case(Point::Replace66),
    ps_v_c16_cold => cycle_case(Cycle::ColdGrow),
    ps_v_c16_warmg => cycle_case(Cycle::WarmGrow),
    ps_v_c16_warms => cycle_case(Cycle::WarmShrink),
    ps_v_c16_nofit => fragment_case(Fragment::NoFit),
    ps_v_c16_coal => fragment_case(Fragment::Coalesced),
    ps_v_c16_m65 => move_case(Move::Small),
    ps_v_c16_m1m => move_case(Move::Large),
    ps_v_c16_reloc => relocation_case(Relocation::Grow),
    ps_v_c16_delrel => relocation_case(Relocation::Remove),
}

cases! { VertexPropertyKey, Candidate<VertexPropertyKey, chunks::Store<65536, 0, VertexPropertyKey>>;
    ps_v_c64_ins256 => scalar_case(Scalars::Insert),
    ps_v_c64_get256 => scalar_case(Scalars::Get),
    ps_v_c64_set256 => scalar_case(Scalars::Replace),
    ps_v_c64_del128 => scalar_case(Scalars::Remove),
    ps_v_c64_enum51 => scalar_case(Scalars::Enumerate),
    ps_v_c64_r1m => point_case(Point::Read1m),
    ps_v_c64_w1m => point_case(Point::Replace1m),
    ps_v_c64_s1m => point_case(Point::Shrink1m),
    ps_v_c64_r65 => point_case(Point::Read65),
    ps_v_c64_w65 => point_case(Point::Replace65),
    ps_v_c64_r66 => point_case(Point::Read66),
    ps_v_c64_w66 => point_case(Point::Replace66),
    ps_v_c64_cold => cycle_case(Cycle::ColdGrow),
    ps_v_c64_warmg => cycle_case(Cycle::WarmGrow),
    ps_v_c64_warms => cycle_case(Cycle::WarmShrink),
    ps_v_c64_nofit => fragment_case(Fragment::NoFit),
    ps_v_c64_coal => fragment_case(Fragment::Coalesced),
    ps_v_c64_m65 => move_case(Move::Small),
    ps_v_c64_m1m => move_case(Move::Large),
    ps_v_c64_reloc => relocation_case(Relocation::Grow),
    ps_v_c64_delrel => relocation_case(Relocation::Remove),
}

cases! { VertexPropertyKey, Candidate<VertexPropertyKey, Arena>;
    ps_v_x_ins256 => scalar_case(Scalars::Insert),
    ps_v_x_get256 => scalar_case(Scalars::Get),
    ps_v_x_set256 => scalar_case(Scalars::Replace),
    ps_v_x_del128 => scalar_case(Scalars::Remove),
    ps_v_x_enum51 => scalar_case(Scalars::Enumerate),
    ps_v_x_r1m => point_case(Point::Read1m),
    ps_v_x_w1m => point_case(Point::Replace1m),
    ps_v_x_s1m => point_case(Point::Shrink1m),
    ps_v_x_r65 => point_case(Point::Read65),
    ps_v_x_w65 => point_case(Point::Replace65),
    ps_v_x_r66 => point_case(Point::Read66),
    ps_v_x_w66 => point_case(Point::Replace66),
    ps_v_x_cold => cycle_case(Cycle::ColdGrow),
    ps_v_x_warmg => cycle_case(Cycle::WarmGrow),
    ps_v_x_warms => cycle_case(Cycle::WarmShrink),
    ps_v_x_nofit => fragment_case(Fragment::NoFit),
    ps_v_x_coal => fragment_case(Fragment::Coalesced),
    ps_v_x_m65 => move_case(Move::Small),
    ps_v_x_m1m => move_case(Move::Large),
    ps_v_x_reloc => relocation_case(Relocation::Grow),
    ps_v_x_delrel => relocation_case(Relocation::Remove),
}

cases! { EdgePropertyKey, Direct<EdgePropertyKey>;
    ps_e_d_ins256 => scalar_case(Scalars::Insert),
    ps_e_d_get256 => scalar_case(Scalars::Get),
    ps_e_d_set256 => scalar_case(Scalars::Replace),
    ps_e_d_del128 => scalar_case(Scalars::Remove),
    ps_e_d_enum51 => scalar_case(Scalars::Enumerate),
    ps_e_d_r1m => point_case(Point::Read1m),
    ps_e_d_w1m => point_case(Point::Replace1m),
    ps_e_d_s1m => point_case(Point::Shrink1m),
    ps_e_d_r65 => point_case(Point::Read65),
    ps_e_d_w65 => point_case(Point::Replace65),
    ps_e_d_r66 => point_case(Point::Read66),
    ps_e_d_w66 => point_case(Point::Replace66),
    ps_e_d_cold => cycle_case(Cycle::ColdGrow),
    ps_e_d_warmg => cycle_case(Cycle::WarmGrow),
    ps_e_d_warms => cycle_case(Cycle::WarmShrink),
    ps_e_d_nofit => fragment_case(Fragment::NoFit),
    ps_e_d_coal => fragment_case(Fragment::Coalesced),
    ps_e_d_m65 => move_case(Move::Small),
    ps_e_d_m1m => move_case(Move::Large),
    ps_e_d_reloc => relocation_case(Relocation::Grow),
    ps_e_d_delrel => relocation_case(Relocation::Remove),
}

cases! { EdgePropertyKey, Candidate<EdgePropertyKey, chunks::Store<4096, 0, EdgePropertyKey>>;
    ps_e_c4_ins256 => scalar_case(Scalars::Insert),
    ps_e_c4_get256 => scalar_case(Scalars::Get),
    ps_e_c4_set256 => scalar_case(Scalars::Replace),
    ps_e_c4_del128 => scalar_case(Scalars::Remove),
    ps_e_c4_enum51 => scalar_case(Scalars::Enumerate),
    ps_e_c4_r1m => point_case(Point::Read1m),
    ps_e_c4_w1m => point_case(Point::Replace1m),
    ps_e_c4_s1m => point_case(Point::Shrink1m),
    ps_e_c4_r65 => point_case(Point::Read65),
    ps_e_c4_w65 => point_case(Point::Replace65),
    ps_e_c4_r66 => point_case(Point::Read66),
    ps_e_c4_w66 => point_case(Point::Replace66),
    ps_e_c4_cold => cycle_case(Cycle::ColdGrow),
    ps_e_c4_warmg => cycle_case(Cycle::WarmGrow),
    ps_e_c4_warms => cycle_case(Cycle::WarmShrink),
    ps_e_c4_nofit => fragment_case(Fragment::NoFit),
    ps_e_c4_coal => fragment_case(Fragment::Coalesced),
    ps_e_c4_m65 => move_case(Move::Small),
    ps_e_c4_m1m => move_case(Move::Large),
    ps_e_c4_reloc => relocation_case(Relocation::Grow),
    ps_e_c4_delrel => relocation_case(Relocation::Remove),
}

cases! { EdgePropertyKey, Candidate<EdgePropertyKey, chunks::Store<16384, 0, EdgePropertyKey>>;
    ps_e_c16_ins256 => scalar_case(Scalars::Insert),
    ps_e_c16_get256 => scalar_case(Scalars::Get),
    ps_e_c16_set256 => scalar_case(Scalars::Replace),
    ps_e_c16_del128 => scalar_case(Scalars::Remove),
    ps_e_c16_enum51 => scalar_case(Scalars::Enumerate),
    ps_e_c16_r1m => point_case(Point::Read1m),
    ps_e_c16_w1m => point_case(Point::Replace1m),
    ps_e_c16_s1m => point_case(Point::Shrink1m),
    ps_e_c16_r65 => point_case(Point::Read65),
    ps_e_c16_w65 => point_case(Point::Replace65),
    ps_e_c16_r66 => point_case(Point::Read66),
    ps_e_c16_w66 => point_case(Point::Replace66),
    ps_e_c16_cold => cycle_case(Cycle::ColdGrow),
    ps_e_c16_warmg => cycle_case(Cycle::WarmGrow),
    ps_e_c16_warms => cycle_case(Cycle::WarmShrink),
    ps_e_c16_nofit => fragment_case(Fragment::NoFit),
    ps_e_c16_coal => fragment_case(Fragment::Coalesced),
    ps_e_c16_m65 => move_case(Move::Small),
    ps_e_c16_m1m => move_case(Move::Large),
    ps_e_c16_reloc => relocation_case(Relocation::Grow),
    ps_e_c16_delrel => relocation_case(Relocation::Remove),
}

cases! { EdgePropertyKey, Candidate<EdgePropertyKey, chunks::Store<65536, 0, EdgePropertyKey>>;
    ps_e_c64_ins256 => scalar_case(Scalars::Insert),
    ps_e_c64_get256 => scalar_case(Scalars::Get),
    ps_e_c64_set256 => scalar_case(Scalars::Replace),
    ps_e_c64_del128 => scalar_case(Scalars::Remove),
    ps_e_c64_enum51 => scalar_case(Scalars::Enumerate),
    ps_e_c64_r1m => point_case(Point::Read1m),
    ps_e_c64_w1m => point_case(Point::Replace1m),
    ps_e_c64_s1m => point_case(Point::Shrink1m),
    ps_e_c64_r65 => point_case(Point::Read65),
    ps_e_c64_w65 => point_case(Point::Replace65),
    ps_e_c64_r66 => point_case(Point::Read66),
    ps_e_c64_w66 => point_case(Point::Replace66),
    ps_e_c64_cold => cycle_case(Cycle::ColdGrow),
    ps_e_c64_warmg => cycle_case(Cycle::WarmGrow),
    ps_e_c64_warms => cycle_case(Cycle::WarmShrink),
    ps_e_c64_nofit => fragment_case(Fragment::NoFit),
    ps_e_c64_coal => fragment_case(Fragment::Coalesced),
    ps_e_c64_m65 => move_case(Move::Small),
    ps_e_c64_m1m => move_case(Move::Large),
    ps_e_c64_reloc => relocation_case(Relocation::Grow),
    ps_e_c64_delrel => relocation_case(Relocation::Remove),
}

cases! { EdgePropertyKey, Candidate<EdgePropertyKey, Arena>;
    ps_e_x_ins256 => scalar_case(Scalars::Insert),
    ps_e_x_get256 => scalar_case(Scalars::Get),
    ps_e_x_set256 => scalar_case(Scalars::Replace),
    ps_e_x_del128 => scalar_case(Scalars::Remove),
    ps_e_x_enum51 => scalar_case(Scalars::Enumerate),
    ps_e_x_r1m => point_case(Point::Read1m),
    ps_e_x_w1m => point_case(Point::Replace1m),
    ps_e_x_s1m => point_case(Point::Shrink1m),
    ps_e_x_r65 => point_case(Point::Read65),
    ps_e_x_w65 => point_case(Point::Replace65),
    ps_e_x_r66 => point_case(Point::Read66),
    ps_e_x_w66 => point_case(Point::Replace66),
    ps_e_x_cold => cycle_case(Cycle::ColdGrow),
    ps_e_x_warmg => cycle_case(Cycle::WarmGrow),
    ps_e_x_warms => cycle_case(Cycle::WarmShrink),
    ps_e_x_nofit => fragment_case(Fragment::NoFit),
    ps_e_x_coal => fragment_case(Fragment::Coalesced),
    ps_e_x_m65 => move_case(Move::Small),
    ps_e_x_m1m => move_case(Move::Large),
    ps_e_x_reloc => relocation_case(Relocation::Grow),
    ps_e_x_delrel => relocation_case(Relocation::Remove),
}
