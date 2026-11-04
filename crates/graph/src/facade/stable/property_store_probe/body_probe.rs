//! Native I/O and managed IC body comparison for exact ic-stable-structures 0.7.2.
//! No directory, Graph Value codec or IC rollback simulation. Extents reuse the same backend.
//! See ../2026-09-13-property-body-btree-comparison.md for scope and reproduction.

#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) mod probe {
    use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
    use std::{borrow::Cow, collections::BTreeMap};
    #[cfg(test)]
    use {
        ic_stable_structures::VectorMemory,
        std::{cell::RefCell, rc::Rc},
    };

    #[cfg(test)]
    pub(crate) type BodyMemory = CountedMemory;
    #[cfg(not(test))]
    pub(crate) type BodyMemory =
        ic_stable_variable_memory_manager::VirtualMemory<ic_stable_structures::Ic0StableMemory>;

    #[cfg(not(test))]
    pub(crate) fn managed_memories()
    -> ic_stable_variable_memory_manager::MemoryManager<ic_stable_structures::Ic0StableMemory> {
        let raw = ic_stable_structures::Ic0StableMemory;
        assert_eq!(raw.size(), 0, "one fresh manager per benchmark");
        let manager =
            ic_stable_variable_memory_manager::MemoryManager::init_with_default_bucket_size(raw, 4);
        ic_cdk::println!("BODY_MANAGER,base_pages={},bucket_pages=4", raw.size());
        manager
    }

    #[cfg(not(test))]
    #[derive(Clone, Copy, Debug)]
    enum HotPhase {
        InitialGrow,
        ReuseGrow,
        ReuseShrink,
    }

    #[cfg(not(test))]
    #[derive(Clone, Copy, Debug)]
    enum FragmentPhase {
        NoFit,
        Coalesced,
    }

    pub(crate) const LARGE: usize = 1024 * 1024;
    pub(crate) const BUDGET: usize = 2 * LARGE;
    type Prefix = [u8; 14];

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Io {
        pub(crate) reads: u64,
        pub(crate) read_bytes: u64,
        pub(crate) writes: u64,
        pub(crate) write_bytes: u64,
        pub(crate) grows: u64,
        pub(crate) body_buffer_bytes: usize,
    }

    #[cfg(test)]
    #[derive(Clone, Default)]
    pub(crate) struct CountedMemory {
        pub(crate) inner: VectorMemory,
        pub(crate) io: Rc<RefCell<Io>>,
    }

    #[cfg(test)]
    impl Memory for CountedMemory {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn grow(&self, pages: u64) -> i64 {
            self.io.borrow_mut().grows += 1;
            self.inner.grow(pages)
        }
        fn read(&self, offset: u64, dst: &mut [u8]) {
            let mut io = self.io.borrow_mut();
            io.reads += 1;
            io.read_bytes += dst.len() as u64;
            drop(io);
            self.inner.read(offset, dst);
        }
        fn write(&self, offset: u64, src: &[u8]) {
            let mut io = self.io.borrow_mut();
            io.writes += 1;
            io.write_bytes += src.len() as u64;
            drop(io);
            self.inner.write(offset, src);
        }
    }

    // P=0 is the uncapped whole-body control. D=0 selects upstream default geometry;
    // D=P declares the cap to Storable. Both chunk geometries enforce the same P.
    pub(crate) struct Chunk<const P: usize, const D: u32>(pub(crate) Vec<u8>);

    impl<const P: usize, const D: u32> Chunk<P, D> {
        fn check_width(len: usize) {
            assert!(P == 0 || len <= P);
        }
    }

    impl<const P: usize, const D: u32> Storable for Chunk<P, D> {
        const BOUND: Bound = if D == 0 {
            Bound::Unbounded
        } else {
            Bound::Bounded {
                max_size: D,
                is_fixed_size: false,
            }
        };

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Self::check_width(self.0.len());
            Cow::Borrowed(&self.0)
        }
        fn into_bytes(self) -> Vec<u8> {
            Self::check_width(self.0.len());
            self.0
        }
        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Self::check_width(bytes.len());
            Self(bytes.into_owned())
        }
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Body<K = Prefix> {
        pub(crate) prefix: K,
        pub(crate) len: usize,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Error {
        Length,
        Budget,
        Sequence,
        Width,
        Missing,
        Extra,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct BodyKey<K> {
        prefix: K,
        ordinal: u32,
    }

    impl<K: Storable> Storable for BodyKey<K> {
        const BOUND: Bound = Bound::Bounded {
            max_size: K::BOUND.max_size() + 4,
            is_fixed_size: true,
        };
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut bytes = self.prefix.to_bytes().into_owned();
            bytes.extend_from_slice(&self.ordinal.to_be_bytes());
            Cow::Owned(bytes)
        }
        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }
        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            let n = K::BOUND.max_size() as usize;
            assert_eq!(bytes.len(), n + 4);
            Self {
                prefix: K::from_bytes(Cow::Borrowed(&bytes[..n])),
                ordinal: u32::from_be_bytes(bytes[n..].try_into().unwrap()),
            }
        }
    }

    pub(crate) fn key<K>(prefix: K, ordinal: u32) -> BodyKey<K> {
        BodyKey { prefix, ordinal }
    }

    fn fixture_prefix(n: u32) -> Prefix {
        let mut prefix = [0; 14];
        prefix[10..].copy_from_slice(&n.to_be_bytes());
        prefix
    }

    fn payload(len: usize, salt: u8) -> Vec<u8> {
        (0..len)
            .map(|i| ((i / 4) as u32).to_le_bytes()[i % 4].wrapping_add(salt))
            .collect()
    }

    #[cfg(test)]
    fn readonly(io: Io) {
        assert_eq!(io.writes, 0);
        assert_eq!(io.grows, 0);
    }

    pub(crate) struct Store<const P: usize, const D: u32, K: Storable + Ord + Clone = Prefix> {
        pub(crate) map: StableBTreeMap<BodyKey<K>, Chunk<P, D>, BodyMemory>,
        memory: BodyMemory,
    }

    // Owns the exclusive store borrow and the exact prepared bytes until publication/retirement.
    pub(crate) struct Prepared<'a, const P: usize, const D: u32, K: Storable + Ord + Copy> {
        store: &'a mut Store<P, D, K>,
        prefix: K,
        bytes: Cow<'a, [u8]>,
        previous: Vec<u8>,
        source: Option<K>,
    }

    impl<const P: usize, const D: u32, K: Storable + Ord + Copy> Prepared<'_, P, D, K> {
        pub(crate) fn previous(&self) -> &[u8] {
            &self.previous
        }
        pub(crate) fn bytes(&self) -> &[u8] {
            &self.bytes
        }
        pub(crate) fn commit(self, publish: impl FnOnce()) -> Vec<u8> {
            for (i, part) in self.bytes.chunks(Store::<P, D, K>::width()).enumerate() {
                self.store
                    .map
                    .insert(key(self.prefix, i as u32), Chunk(part.to_vec()));
            }
            publish();
            for i in Store::<P, D, K>::count(self.bytes.len())
                ..Store::<P, D, K>::count(self.previous.len())
            {
                assert!(self.store.map.remove(&key(self.prefix, i as u32)).is_some());
            }
            if let Some(source) = self.source {
                for i in 0..Store::<P, D, K>::count(self.bytes.len()) {
                    assert!(self.store.map.remove(&key(source, i as u32)).is_some());
                }
            }
            self.previous
        }
    }

    impl<const P: usize, const D: u32, K: Storable + Ord + Copy> Store<P, D, K> {
        fn new() -> Self {
            assert!(D == 0 || D as usize == P);
            #[cfg(test)]
            let memory = CountedMemory::default();
            #[cfg(not(test))]
            let memory =
                managed_memories().get(ic_stable_structures::memory_manager::MemoryId::new(0));
            Self::open(memory)
        }

        pub(crate) fn open(memory: BodyMemory) -> Self {
            assert!(D == 0 || D as usize == P);
            assert!(K::BOUND.is_fixed_size());
            let map = if memory.size() == 0 {
                StableBTreeMap::new(memory.clone())
            } else {
                StableBTreeMap::load(memory.clone())
            };
            Self { map, memory }
        }

        fn reopen(self) -> Self {
            Self {
                map: StableBTreeMap::load(self.map.into_memory()),
                memory: self.memory,
            }
        }

        fn width() -> usize {
            if P == 0 { usize::MAX } else { P }
        }

        fn count(len: usize) -> usize {
            len.div_ceil(Self::width())
        }

        fn representable(len: usize) -> Result<(), Error> {
            len.checked_add(1)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(Error::Length)?;
            Ok(())
        }

        pub(crate) fn read(&self, body: Body<K>, budget: usize) -> Result<Vec<u8>, Error> {
            Self::representable(body.len)?;
            if body.len == 0 {
                return Err(Error::Length);
            }
            if body.len > budget {
                return Err(Error::Budget);
            }
            #[cfg(test)]
            {
                self.memory.io.borrow_mut().body_buffer_bytes += body.len;
            }
            let mut result = Vec::with_capacity(body.len);
            let expected = Self::count(body.len);
            let mut seen = 0;
            // u32::MAX is the actual ordinal-domain maximum, not a guessed body-key sentinel.
            for entry in self
                .map
                .range(key(body.prefix, 0)..=key(body.prefix, u32::MAX))
            {
                if seen == expected {
                    return Err(Error::Extra);
                }
                if *entry.key() != key(body.prefix, seen as u32) {
                    return Err(Error::Sequence);
                }
                let value = entry.value();
                if value.0.len() != (body.len - result.len()).min(Self::width()) {
                    return Err(Error::Width);
                }
                result.extend_from_slice(&value.0);
                seen += 1;
            }
            if seen != expected {
                return Err(Error::Missing);
            }
            Ok(result)
        }

        // Fresh fixture insertion, not a production create-or-replace API.
        fn seed(&mut self, prefix: K, bytes: &[u8]) {
            assert!(!bytes.is_empty());
            Self::representable(bytes.len()).unwrap();
            for (i, part) in bytes.chunks(Self::width()).enumerate() {
                self.map.insert(key(prefix, i as u32), Chunk(part.to_vec()));
            }
        }

        // Empty replacement removes the body; stored empty bodies are outside this probe.
        fn rewrite(&mut self, old: Body<K>, bytes: &[u8], budget: usize) -> Result<Vec<u8>, Error> {
            Ok(self
                .prepare(old.prefix, Some(old.len), bytes, budget)?
                .commit(|| {}))
        }

        fn previous(&self, prefix: K, len: Option<usize>, budget: usize) -> Result<Vec<u8>, Error> {
            match len {
                Some(len) => self.read(Body { prefix, len }, budget),
                None => {
                    if self
                        .map
                        .range(key(prefix, 0)..=key(prefix, u32::MAX))
                        .next()
                        .is_some()
                    {
                        return Err(Error::Extra);
                    }
                    Ok(Vec::new())
                }
            }
        }

        pub(crate) fn prepare<'a>(
            &'a mut self,
            prefix: K,
            old: Option<usize>,
            bytes: &'a [u8],
            budget: usize,
        ) -> Result<Prepared<'a, P, D, K>, Error> {
            Self::representable(bytes.len())?;
            if old.is_none() && bytes.is_empty() {
                return Err(Error::Length);
            }
            let total = old
                .unwrap_or(0)
                .checked_add(bytes.len())
                .ok_or(Error::Length)?;
            if total > budget {
                return Err(Error::Budget);
            }
            let previous = self.previous(prefix, old, budget)?;
            Ok(Prepared {
                store: self,
                prefix,
                bytes: Cow::Borrowed(bytes),
                previous,
                source: None,
            })
        }

        pub(crate) fn prepare_move(
            &mut self,
            source: Body<K>,
            target: K,
            old: Option<usize>,
            budget: usize,
        ) -> Result<Prepared<'_, P, D, K>, Error> {
            if source.prefix == target {
                return Err(Error::Length);
            }
            if source
                .len
                .checked_add(old.unwrap_or(0))
                .ok_or(Error::Length)?
                > budget
            {
                return Err(Error::Budget);
            }
            let bytes = self.read(source, budget)?;
            let previous = self.previous(target, old, budget)?;
            Ok(Prepared {
                store: self,
                prefix: target,
                bytes: Cow::Owned(bytes),
                previous,
                source: Some(source.prefix),
            })
        }

        pub(crate) fn audit(&self, expected: &BTreeMap<K, Vec<u8>>) {
            let mut count = 0;
            for (prefix, bytes) in expected {
                assert_eq!(
                    self.read(
                        Body {
                            prefix: *prefix,
                            len: bytes.len()
                        },
                        BUDGET
                    )
                    .unwrap(),
                    *bytes
                );
                count += Self::count(bytes.len());
            }
            assert_eq!(self.map.len(), count as u64);
        }

        #[cfg(test)]
        fn measure<T>(
            &mut self,
            case: &str,
            input: usize,
            operation: impl FnOnce(&mut Self) -> T,
        ) -> (T, Io) {
            let before = self.memory.size() * 65536;
            *self.memory.io.borrow_mut() = Io::default();
            let result = operation(self);
            let io = *self.memory.io.borrow();
            println!(
                "MEASURE,{P},{D},{case},{input},{},{},{},{},{},{before},{},{},{}",
                io.reads,
                io.read_bytes,
                io.writes,
                io.write_bytes,
                io.grows,
                self.memory.size() * 65536,
                self.map.len(),
                io.body_buffer_bytes
            );
            (result, io)
        }

        #[cfg(test)]
        fn reject(
            &mut self,
            expected: Error,
            operation: impl FnOnce(&mut Self) -> Result<Vec<u8>, Error>,
        ) {
            let before = self.memory.inner.borrow().clone();
            let entries = self.map.len();
            *self.memory.io.borrow_mut() = Io::default();
            assert_eq!(operation(self), Err(expected));
            assert_eq!(*self.memory.inner.borrow(), before);
            assert_eq!(self.map.len(), entries);
            readonly(*self.memory.io.borrow());
        }
    }

    #[cfg(test)]
    fn density<const P: usize, const D: u32>() {
        let mut store = Store::<P, D>::new();
        let sanity = payload(65, 9);
        let max = [u8::MAX; 14];
        store.seed(max, &sanity);
        assert_eq!(
            store
                .read(
                    Body {
                        prefix: max,
                        len: 65
                    },
                    65
                )
                .unwrap(),
            sanity
        );
        assert_eq!(
            store
                .rewrite(
                    Body {
                        prefix: max,
                        len: 65
                    },
                    &[],
                    65
                )
                .unwrap(),
            sanity
        );
        let mut expected = BTreeMap::new();
        for i in 0..256u32 {
            let mut prefix = [0; 14];
            prefix[10..].copy_from_slice(&i.to_be_bytes());
            expected.insert(prefix, payload(65, i as u8));
        }
        store.measure("density_insert_256", 65, |s| {
            for (k, v) in &expected {
                s.seed(*k, v);
            }
        });
        store.audit(&expected);
        let (values, io) = store.measure("density_get_256", 65, |s| {
            expected
                .keys()
                .map(|k| {
                    s.read(
                        Body {
                            prefix: *k,
                            len: 65,
                        },
                        65,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(values, expected.values().cloned().collect::<Vec<_>>());
        readonly(io);
        let replacements: BTreeMap<_, _> = expected
            .keys()
            .enumerate()
            .map(|(i, k)| (*k, payload(65, (i as u8).wrapping_add(31))))
            .collect();
        let (previous, _) = store.measure("density_rewrite_256", 65, |s| {
            replacements
                .iter()
                .map(|(k, v)| {
                    s.rewrite(
                        Body {
                            prefix: *k,
                            len: 65,
                        },
                        v,
                        130,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(previous, expected.values().cloned().collect::<Vec<_>>());
        expected = replacements;
        store.audit(&expected);
        let removed: Vec<_> = expected.keys().step_by(2).copied().collect();
        let (previous, _) = store.measure("density_remove_128", 65, |s| {
            removed
                .iter()
                .map(|k| {
                    s.rewrite(
                        Body {
                            prefix: *k,
                            len: 65,
                        },
                        &[],
                        65,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        });
        for (k, bytes) in removed.iter().zip(previous) {
            assert_eq!(expected.remove(k).unwrap(), bytes);
        }
        store = store.reopen();
        store.audit(&expected);
        let refill = payload(65, 203);
        store.measure("density_refill_128", 65, |s| {
            for k in &removed {
                s.seed(*k, &refill);
            }
        });
        for k in removed {
            expected.insert(k, refill.clone());
        }
        store.audit(&expected);
    }

    #[cfg(test)]
    fn neighbors<const P: usize, const D: u32>() {
        let mut store = Store::<P, D>::new();
        let small = Body {
            prefix: [1; 14],
            len: 65,
        };
        let mut large = Body {
            prefix: [2; 14],
            len: 65,
        };
        let mut small_bytes = payload(65, 11);
        let mut large_bytes = payload(65, 37);
        store.seed(small.prefix, &small_bytes);
        store.seed(large.prefix, &large_bytes);
        store.audit(&BTreeMap::from([
            (small.prefix, small_bytes.clone()),
            (large.prefix, large_bytes.clone()),
        ]));
        for n in [65, 4096, 65536, LARGE] {
            if n != large.len {
                let new = payload(n, 37);
                assert_eq!(store.rewrite(large, &new, BUDGET).unwrap(), large_bytes);
                large.len = n;
                large_bytes = new;
            }
            let (bytes, io) =
                store.measure("neighbor_small_get", n, |s| s.read(small, 65).unwrap());
            assert_eq!(bytes, small_bytes);
            readonly(io);
            if P == 0 {
                let (bytes, io) = store.measure("neighbor_small_point_get", n, |s| {
                    s.map.get(&key(small.prefix, 0)).unwrap().0
                });
                assert_eq!(bytes, small_bytes);
                readonly(io);
            }
            let (missing, io) = store.measure("neighbor_missing_prefix", n, |s| {
                s.map
                    .range(key([0; 14], 0)..=key([0; 14], u32::MAX))
                    .next()
                    .is_none()
            });
            assert!(missing);
            readonly(io);
            let replacement = payload(65, small_bytes[0].wrapping_add(1));
            let (previous, _) = store.measure("neighbor_small_rewrite", n, |s| {
                s.rewrite(small, &replacement, 130).unwrap()
            });
            assert_eq!(previous, small_bytes);
            small_bytes = replacement;
            let (bytes, io) = store.measure("neighbor_large_get", n, |s| s.read(large, n).unwrap());
            assert_eq!(bytes, large_bytes);
            readonly(io);
            if P == 0 {
                let (bytes, io) = store.measure("neighbor_large_point_get", n, |s| {
                    s.map.get(&key(large.prefix, 0)).unwrap().0
                });
                assert_eq!(bytes, large_bytes);
                readonly(io);
            }
            let replacement = payload(n, large_bytes[0].wrapping_add(1));
            let (previous, _) = store.measure("neighbor_large_rewrite", n, |s| {
                s.rewrite(large, &replacement, BUDGET).unwrap()
            });
            assert_eq!(previous, large_bytes);
            large_bytes = replacement;
            store.audit(&BTreeMap::from([
                (small.prefix, small_bytes.clone()),
                (large.prefix, large_bytes.clone()),
            ]));
        }
        store = store.reopen();
        store.audit(&BTreeMap::from([
            (small.prefix, small_bytes),
            (large.prefix, large_bytes),
        ]));
    }

    #[cfg(test)]
    fn hot<const P: usize, const D: u32>() {
        let mut store = Store::<P, D>::new();
        let mut body = Body {
            prefix: [u8::MAX; 14],
            len: 65,
        };
        let mut current = payload(65, 7);
        store.seed(body.prefix, &current);
        assert_eq!(store.read(body, 65).unwrap(), current);
        for (case, n) in [("hot_warm_large", LARGE), ("hot_warm_small", 65)] {
            let new = payload(n, current[0].wrapping_add(1));
            let (previous, _) = store.measure(case, n, |s| s.rewrite(body, &new, BUDGET).unwrap());
            assert_eq!(previous, current);
            body.len = n;
            current = new;
            store.audit(&BTreeMap::from([(body.prefix, current.clone())]));
        }
        store = store.reopen();
        store.audit(&BTreeMap::from([(body.prefix, current.clone())]));
        let high_water = store.memory.size();
        for i in 0..8 {
            let n = if i % 2 == 0 { LARGE } else { 65 };
            let new = payload(n, current[0].wrapping_add(1));
            let (previous, io) = store.measure("hot_after_reopen", n, |s| {
                s.rewrite(body, &new, BUDGET).unwrap()
            });
            assert_eq!(previous, current);
            body.len = n;
            current = new;
            store.audit(&BTreeMap::from([(body.prefix, current.clone())]));
            assert_eq!(io.grows, 0);
            assert_eq!(store.memory.size(), high_water);
        }
        let (previous, _) = store.measure("hot_remove", body.len, |s| {
            s.rewrite(body, &[], body.len).unwrap()
        });
        assert_eq!(previous, current);
        assert!(store.map.is_empty());
        assert_eq!(store.memory.size(), high_water);
    }

    #[test]
    fn body_btree_matrix() {
        // A swapped chunk must not be invisible to exact-byte fixture assertions.
        {
            let bytes = payload(LARGE, 9);
            for width in [256, 4096, 16384, 65536] {
                let distinct: std::collections::BTreeSet<_> = bytes.chunks(width).collect();
                assert_eq!(distinct.len(), LARGE / width);
            }
        }
        println!(
            "MEASURE_HEADER,cap,declared,case,input_bytes,reads,read_bytes,writes,write_bytes,grows,before_memory,after_memory,entries,body_buffer_bytes"
        );
        density::<0, 0>();
        neighbors::<0, 0>();
        hot::<0, 0>();
        density::<256, 256>();
        neighbors::<256, 256>();
        hot::<256, 256>();
        density::<256, 0>();
        neighbors::<256, 0>();
        hot::<256, 0>();
        density::<4096, 4096>();
        neighbors::<4096, 4096>();
        hot::<4096, 4096>();
        density::<4096, 0>();
        neighbors::<4096, 0>();
        hot::<4096, 0>();
        density::<16384, 0>();
        neighbors::<16384, 0>();
        hot::<16384, 0>();
        density::<65536, 0>();
        neighbors::<65536, 0>();
        hot::<65536, 0>();
    }

    #[cfg(test)]
    fn negative<const P: usize, const D: u32>() {
        let mut store = Store::<P, D>::new();
        let body = Body {
            prefix: [1; 14],
            len: 3 * P + 1,
        };
        let bytes = payload(body.len, 31);
        let other = payload(65, 77);
        store.seed(body.prefix, &bytes);
        store.seed([u8::MAX; 14], &other);
        assert_eq!(store.read(body, body.len).unwrap(), bytes);
        store.reject(Error::Budget, |s| s.read(body, body.len - 1));
        assert_eq!(*store.memory.io.borrow(), Io::default());
        let replacement = payload(P + 1, 193);
        store.reject(Error::Budget, |s| {
            s.rewrite(body, &replacement, body.len + replacement.len() - 1)
        });
        assert_eq!(*store.memory.io.borrow(), Io::default());
        store.reject(Error::Length, |s| {
            s.read(
                Body {
                    len: usize::MAX,
                    ..body
                },
                BUDGET,
            )
        });
        assert_eq!(*store.memory.io.borrow(), Io::default());
        store.reject(Error::Budget, |s| {
            s.read(
                Body {
                    len: 4 * LARGE,
                    ..body
                },
                BUDGET,
            )
        });
        assert_eq!(*store.memory.io.borrow(), Io::default());

        let saved = store.map.remove(&key(body.prefix, 1)).unwrap();
        store.reject(Error::Sequence, |s| s.rewrite(body, &replacement, BUDGET));
        store.map.insert(key(body.prefix, 1), saved);
        let saved = store.map.remove(&key(body.prefix, 3)).unwrap();
        store.reject(Error::Missing, |s| s.rewrite(body, &replacement, BUDGET));
        store.map.insert(key(body.prefix, 3), saved);
        store.map.insert(key(body.prefix, u32::MAX), Chunk(vec![9]));
        store.reject(Error::Extra, |s| s.rewrite(body, &replacement, BUDGET));
        store.map.remove(&key(body.prefix, u32::MAX));
        let saved = store
            .map
            .insert(key(body.prefix, 1), Chunk(vec![8; P - 1]))
            .unwrap();
        store.reject(Error::Budget, |s| s.read(body, body.len - 1));
        assert_eq!(*store.memory.io.borrow(), Io::default());
        store.reject(Error::Width, |s| s.rewrite(body, &replacement, BUDGET));
        store.map.insert(key(body.prefix, 1), saved);
        store.audit(&BTreeMap::from([
            (body.prefix, bytes.clone()),
            ([u8::MAX; 14], other.clone()),
        ]));
        assert_eq!(
            store
                .rewrite(body, &replacement, body.len + replacement.len())
                .unwrap(),
            bytes
        );
        let short = Body {
            len: replacement.len(),
            ..body
        };
        store.audit(&BTreeMap::from([
            (body.prefix, replacement.clone()),
            ([u8::MAX; 14], other.clone()),
        ]));
        store.reject(Error::Budget, |s| s.rewrite(short, &[], short.len - 1));
        assert_eq!(store.rewrite(short, &[], short.len).unwrap(), replacement);
        store.reject(Error::Missing, |s| s.rewrite(short, &[], BUDGET));
        store.audit(&BTreeMap::from([([u8::MAX; 14], other.clone())]));
        let moved = store
            .prepare_move(
                Body {
                    prefix: [u8::MAX; 14],
                    len: other.len(),
                },
                body.prefix,
                None,
                BUDGET,
            )
            .unwrap();
        assert_eq!(moved.bytes(), other);
        assert!(moved.previous().is_empty());
        moved.commit(|| {});
        store.audit(&BTreeMap::from([(body.prefix, other)]));
    }

    #[test]
    fn body_btree_rejects_incomplete_or_over_budget() {
        negative::<256, 256>();
        negative::<256, 0>();
        negative::<4096, 4096>();
        negative::<4096, 0>();
        negative::<16384, 0>();
        negative::<65536, 0>();
    }

    pub(crate) mod extents {
        include!("body_extent_probe.rs");
    }

    #[cfg(not(test))]
    mod benches {
        use super::*;
        use canbench_rs::{BenchResult, bench, bench_fn};
        use std::hint::black_box;

        fn new_store<const P: usize>() -> Store<P, 0> {
            assert_eq!(ic_stable_structures::Ic0StableMemory.size(), 0);
            let mut store = Store::<P, 0>::new();
            let sanity = Body {
                prefix: [0; 14],
                len: 65,
            };
            let first = payload(65, 7);
            let second = payload(65, 9);
            store.seed(sanity.prefix, &first);
            assert_eq!(store.read(sanity, 65).unwrap(), first);
            assert_eq!(store.rewrite(sanity, &second, 130).unwrap(), first);
            assert_eq!(store.read(sanity, 65).unwrap(), second);
            assert_eq!(store.rewrite(sanity, &first[..32], 97).unwrap(), second);
            let short = Body { len: 32, ..sanity };
            assert_eq!(store.read(short, 32).unwrap(), first[..32]);
            assert_eq!(store.rewrite(short, &[], 32).unwrap(), first[..32]);
            assert!(store.map.is_empty());
            store
        }

        fn fixture<const P: usize>(
            size: usize,
            neighbor: usize,
        ) -> (Store<P, 0>, Body, Vec<u8>, Vec<u8>) {
            let mut store = new_store::<P>();
            let body = Body {
                prefix: [2; 14],
                len: size,
            };
            let original = payload(size, 31);
            let guard = payload(neighbor, 77);
            store.seed([1; 14], &guard);
            store.seed(body.prefix, &original);
            store.audit(&BTreeMap::from([
                (body.prefix, original.clone()),
                ([1; 14], guard.clone()),
            ]));
            assert!(ic_stable_structures::Ic0StableMemory.size() > 0);
            assert!(store.memory.size() > 0);
            assert!(store.memory.size() <= ic_cdk::api::stable_size());
            (store, body, original, guard)
        }

        fn read_case<const P: usize>(size: usize, neighbor: usize) -> BenchResult {
            let (store, body, original, guard) = fixture::<P>(size, neighbor);
            let before = ic_cdk::api::stable_size();
            let virtual_before = store.memory.size();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = black_box(store.read(black_box(body), black_box(BUDGET)).unwrap());
            });
            assert_eq!(actual, original);
            store.audit(&BTreeMap::from([(body.prefix, original), ([1; 14], guard)]));
            assert_eq!(store.memory.size(), virtual_before);
            assert_eq!(ic_cdk::api::stable_size(), before);
            assert_eq!(result.total.stable_memory_increase, 0);
            ic_cdk::println!(
                "BODY_CASE,p={P},op=read,old={size},new={size},neighbor={neighbor},before={before},after={},region0={},region1=0",
                ic_cdk::api::stable_size(),
                store.memory.size()
            );
            result
        }

        fn rewrite_case<const P: usize>(
            size: usize,
            new_size: usize,
            neighbor: usize,
        ) -> BenchResult {
            let (mut store, body, original, guard) = fixture::<P>(size, neighbor);
            let replacement = payload(new_size, 193);
            let before = ic_cdk::api::stable_size();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = black_box(
                    store
                        .rewrite(black_box(body), black_box(&replacement), black_box(BUDGET))
                        .unwrap(),
                );
            });
            assert_eq!(actual, original);
            store.audit(&BTreeMap::from([
                (body.prefix, replacement),
                ([1; 14], guard),
            ]));
            assert_eq!(
                result.total.stable_memory_increase,
                ic_cdk::api::stable_size() - before
            );
            ic_cdk::println!(
                "BODY_CASE,p={P},op=rewrite,old={size},new={new_size},neighbor={neighbor},before={before},after={},region0={},region1=0",
                ic_cdk::api::stable_size(),
                store.memory.size()
            );
            result
        }

        fn point_case(size: usize) -> BenchResult {
            let (store, body, original, guard) = fixture::<0>(size, 65);
            let before = ic_cdk::api::stable_size();
            let virtual_before = store.memory.size();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = black_box(store.map.get(&black_box(key(body.prefix, 0))).unwrap().0);
            });
            assert_eq!(actual, original);
            store.audit(&BTreeMap::from([(body.prefix, original), ([1; 14], guard)]));
            assert_eq!(store.memory.size(), virtual_before);
            assert_eq!(ic_cdk::api::stable_size(), before);
            assert_eq!(result.total.stable_memory_increase, 0);
            ic_cdk::println!(
                "BODY_CASE,p=0,op=point,old={size},new={size},neighbor=65,before={before},after={},region0={},region1=0",
                ic_cdk::api::stable_size(),
                store.memory.size()
            );
            result
        }

        fn hot_case<const P: usize>(phase: HotPhase) -> BenchResult {
            let mut store = new_store::<P>();
            let mut body = Body {
                prefix: [2; 14],
                len: 65,
            };
            let mut original = payload(65, 31);
            store.seed(body.prefix, &original);
            match phase {
                HotPhase::InitialGrow => {}
                HotPhase::ReuseGrow | HotPhase::ReuseShrink => {
                    for (len, salt) in [(LARGE, 40), (65, 42)] {
                        let bytes = payload(len, salt);
                        assert_eq!(store.rewrite(body, &bytes, BUDGET).unwrap(), original);
                        body.len = len;
                        original = bytes;
                    }
                    store = store.reopen();
                    store.audit(&BTreeMap::from([(body.prefix, original.clone())]));
                    if matches!(phase, HotPhase::ReuseShrink) {
                        let bytes = payload(LARGE, 43);
                        assert_eq!(store.rewrite(body, &bytes, BUDGET).unwrap(), original);
                        body.len = LARGE;
                        original = bytes;
                    }
                }
            }
            store.audit(&BTreeMap::from([(body.prefix, original.clone())]));
            let new_len = if matches!(phase, HotPhase::ReuseShrink) {
                65
            } else {
                LARGE
            };
            let replacement = payload(new_len, 193);
            let before = ic_cdk::api::stable_size();
            let virtual_before = store.memory.size();
            let mut actual = Vec::new();
            let result = bench_fn(|| {
                actual = black_box(
                    store
                        .rewrite(black_box(body), black_box(&replacement), black_box(BUDGET))
                        .unwrap(),
                );
            });
            assert_eq!(actual, original);
            store.audit(&BTreeMap::from([(body.prefix, replacement)]));
            if !matches!(phase, HotPhase::InitialGrow) {
                assert_eq!(store.memory.size(), virtual_before);
                assert_eq!(ic_cdk::api::stable_size(), before);
            }
            assert_eq!(
                result.total.stable_memory_increase,
                ic_cdk::api::stable_size() - before
            );
            ic_cdk::println!(
                "BODY_HOT,p={P},phase={phase:?},old={},new={new_len},before={before},after={},region0={},region1=0",
                body.len,
                ic_cdk::api::stable_size(),
                store.memory.size()
            );
            result
        }

        fn fragment_case<const P: usize>(holes: u32, phase: FragmentPhase) -> BenchResult {
            let mut store = new_store::<P>();
            let mut expected = BTreeMap::new();
            for i in 0..holes {
                for (n, len, salt) in [
                    (2 * i, 33, i as u8),
                    (2 * i + 1, 65, (i as u8).wrapping_add(77)),
                ] {
                    let bytes = payload(len, salt);
                    let prefix = fixture_prefix(n);
                    store.seed(prefix, &bytes);
                    expected.insert(prefix, bytes);
                }
            }
            for i in 0..holes {
                let prefix = fixture_prefix(2 * i);
                assert_eq!(
                    store
                        .rewrite(Body { prefix, len: 33 }, &[], BUDGET)
                        .unwrap(),
                    expected.remove(&prefix).unwrap()
                );
            }
            if matches!(phase, FragmentPhase::Coalesced) {
                let prefix = fixture_prefix(1);
                assert_eq!(
                    store
                        .rewrite(Body { prefix, len: 65 }, &[], BUDGET)
                        .unwrap(),
                    expected.remove(&prefix).unwrap()
                );
            }
            store.audit(&expected);
            let len = match phase {
                FragmentPhase::NoFit => 65,
                FragmentPhase::Coalesced => 160,
            };
            let prefix = fixture_prefix(1000);
            let replacement = payload(len, 193);
            let before = ic_cdk::api::stable_size();
            let result = bench_fn(|| store.seed(black_box(prefix), black_box(&replacement)));
            assert!(expected.insert(prefix, replacement).is_none());
            store.audit(&expected);
            assert_eq!(
                result.total.stable_memory_increase,
                ic_cdk::api::stable_size() - before
            );
            ic_cdk::println!(
                "BODY_FRAGMENT,p={P},phase={phase:?},holes={holes},new={len},before={before},after={},region0={},region1=0",
                ic_cdk::api::stable_size(),
                store.memory.size()
            );
            result
        }

        macro_rules! cases {
            ($($name:ident => $run:expr),+ $(,)?) => {
                $(#[bench(raw)] fn $name() -> BenchResult { $run })+
            };
        }

        cases! {
            body_whole_read_65b => read_case::<0>(65, 65),
            body_whole_read_4k => read_case::<0>(4096, 65),
            body_whole_read_64k => read_case::<0>(65536, 65),
            body_whole_read_1m => read_case::<0>(LARGE, 65),
            body_chunk4k_read_65b => read_case::<4096>(65, 65),
            body_chunk4k_read_4k => read_case::<4096>(4096, 65),
            body_chunk4k_read_64k => read_case::<4096>(65536, 65),
            body_chunk4k_read_1m => read_case::<4096>(LARGE, 65),
            body_whole_rewrite_65b => rewrite_case::<0>(65, 65, 65),
            body_whole_rewrite_4k => rewrite_case::<0>(4096, 4096, 65),
            body_whole_rewrite_64k => rewrite_case::<0>(65536, 65536, 65),
            body_whole_rewrite_1m => rewrite_case::<0>(LARGE, LARGE, 65),
            body_chunk4k_rewrite_65b => rewrite_case::<4096>(65, 65, 65),
            body_chunk4k_rewrite_4k => rewrite_case::<4096>(4096, 4096, 65),
            body_chunk4k_rewrite_64k => rewrite_case::<4096>(65536, 65536, 65),
            body_chunk4k_rewrite_1m => rewrite_case::<4096>(LARGE, LARGE, 65),
            body_whole_shrink_65b => rewrite_case::<0>(65, 32, 65),
            body_whole_shrink_4k => rewrite_case::<0>(4096, 65, 65),
            body_whole_shrink_64k => rewrite_case::<0>(65536, 65, 65),
            body_whole_shrink_1m => rewrite_case::<0>(LARGE, 65, 65),
            body_chunk4k_shrink_65b => rewrite_case::<4096>(65, 32, 65),
            body_chunk4k_shrink_4k => rewrite_case::<4096>(4096, 65, 65),
            body_chunk4k_shrink_64k => rewrite_case::<4096>(65536, 65, 65),
            body_chunk4k_shrink_1m => rewrite_case::<4096>(LARGE, 65, 65),
            body_whole_point_65b => point_case(65),
            body_whole_point_4k => point_case(4096),
            body_whole_point_64k => point_case(65536),
            body_whole_point_1m => point_case(LARGE),
            body_whole_neighbor_read => read_case::<0>(65, LARGE),
            body_chunk4k_neighbor_read => read_case::<4096>(65, LARGE),
            body_whole_neighbor_rewrite => rewrite_case::<0>(65, 65, LARGE),
            body_chunk4k_neighbor_rewrite => rewrite_case::<4096>(65, 65, LARGE),
            body_chunk16k_read_64k => read_case::<16384>(65536, 65),
            body_chunk16k_read_1m => read_case::<16384>(LARGE, 65),
            body_chunk16k_rewrite_64k => rewrite_case::<16384>(65536, 65536, 65),
            body_chunk16k_rewrite_1m => rewrite_case::<16384>(LARGE, LARGE, 65),
            body_chunk16k_shrink_64k => rewrite_case::<16384>(65536, 65, 65),
            body_chunk16k_shrink_1m => rewrite_case::<16384>(LARGE, 65, 65),
            body_chunk16k_neighbor_read => read_case::<16384>(65, LARGE),
            body_chunk16k_neighbor_rewrite => rewrite_case::<16384>(65, 65, LARGE),
            body_chunk64k_read_64k => read_case::<65536>(65536, 65),
            body_chunk64k_read_1m => read_case::<65536>(LARGE, 65),
            body_chunk64k_rewrite_64k => rewrite_case::<65536>(65536, 65536, 65),
            body_chunk64k_rewrite_1m => rewrite_case::<65536>(LARGE, LARGE, 65),
            body_chunk64k_shrink_64k => rewrite_case::<65536>(65536, 65, 65),
            body_chunk64k_shrink_1m => rewrite_case::<65536>(LARGE, 65, 65),
            body_chunk64k_neighbor_read => read_case::<65536>(65, LARGE),
            body_chunk64k_neighbor_rewrite => rewrite_case::<65536>(65, 65, LARGE),
            body_whole_initial_grow => hot_case::<0>(HotPhase::InitialGrow),
            body_whole_reuse_grow => hot_case::<0>(HotPhase::ReuseGrow),
            body_whole_reuse_shrink => hot_case::<0>(HotPhase::ReuseShrink),
            body_chunk4k_initial_grow => hot_case::<4096>(HotPhase::InitialGrow),
            body_chunk4k_reuse_grow => hot_case::<4096>(HotPhase::ReuseGrow),
            body_chunk4k_reuse_shrink => hot_case::<4096>(HotPhase::ReuseShrink),
            body_chunk16k_initial_grow => hot_case::<16384>(HotPhase::InitialGrow),
            body_chunk16k_reuse_grow => hot_case::<16384>(HotPhase::ReuseGrow),
            body_chunk16k_reuse_shrink => hot_case::<16384>(HotPhase::ReuseShrink),
            body_chunk64k_initial_grow => hot_case::<65536>(HotPhase::InitialGrow),
            body_chunk64k_reuse_grow => hot_case::<65536>(HotPhase::ReuseGrow),
            body_chunk64k_reuse_shrink => hot_case::<65536>(HotPhase::ReuseShrink),
            body_whole_fragment_nofit_8 => fragment_case::<0>(8,FragmentPhase::NoFit),
            body_whole_fragment_nofit_128 => fragment_case::<0>(128,FragmentPhase::NoFit),
            body_whole_fragment_coalesced_8 => fragment_case::<0>(8,FragmentPhase::Coalesced),
            body_whole_fragment_coalesced_128 => fragment_case::<0>(128,FragmentPhase::Coalesced),
            body_chunk4k_fragment_nofit_8 => fragment_case::<4096>(8,FragmentPhase::NoFit),
            body_chunk4k_fragment_nofit_128 => fragment_case::<4096>(128,FragmentPhase::NoFit),
            body_chunk4k_fragment_coalesced_8 => fragment_case::<4096>(8,FragmentPhase::Coalesced),
            body_chunk4k_fragment_coalesced_128 => fragment_case::<4096>(128,FragmentPhase::Coalesced),
            body_chunk16k_fragment_nofit_8 => fragment_case::<16384>(8,FragmentPhase::NoFit),
            body_chunk16k_fragment_nofit_128 => fragment_case::<16384>(128,FragmentPhase::NoFit),
            body_chunk16k_fragment_coalesced_8 => fragment_case::<16384>(8,FragmentPhase::Coalesced),
            body_chunk16k_fragment_coalesced_128 => fragment_case::<16384>(128,FragmentPhase::Coalesced),
            body_chunk64k_fragment_nofit_8 => fragment_case::<65536>(8,FragmentPhase::NoFit),
            body_chunk64k_fragment_nofit_128 => fragment_case::<65536>(128,FragmentPhase::NoFit),
            body_chunk64k_fragment_coalesced_8 => fragment_case::<65536>(8,FragmentPhase::Coalesced),
            body_chunk64k_fragment_coalesced_128 => fragment_case::<65536>(128,FragmentPhase::Coalesced),
        }
    }
}
