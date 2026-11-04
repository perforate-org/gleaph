//! Shared Graph-owned native/managed-IC experiment. No production layout or global store.
//! See `design/investigations/2026-09-13-property-store-comparison-design.md` for the design of the
//! comparison and the recorded results.

mod body_probe;
use super::{
    edge_properties::{EdgePropertyKey, EdgePropertyStore},
    vertex_properties::{
        StoredPropertyValue, VertexPropertyKey, VertexPropertyStore, VertexPropertyStoreError,
    },
};
use body_probe::probe::{self as chunks, BUDGET, Body, BodyMemory, LARGE};
use chunks::extents::{self, Arena, BodyAddress};
#[cfg(test)]
use chunks::{Chunk, Io};
use gleaph_gql::Value;
#[cfg(test)]
use gleaph_gql::ValueBinaryError;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
#[cfg(test)]
use std::rc::Rc;
use std::{borrow::Cow, collections::BTreeMap, fmt::Debug};

const SMALL: usize = 65;
#[cfg(test)]
const PAGE: u64 = 65536;

fn new_memories(count: usize) -> Vec<BodyMemory> {
    #[cfg(test)]
    {
        (0..count).map(|_| BodyMemory::default()).collect()
    }
    #[cfg(not(test))]
    {
        use ic_stable_structures::memory_manager::MemoryId;
        let manager = chunks::managed_memories();
        // Only this constructor supplies Wasm regions; each ID belongs to one collection/arena.
        (0..count)
            .map(|n| manager.get(MemoryId::new(n as u8)))
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Property(VertexPropertyStoreError),
    Limit,
    Layout,
    Chunks(chunks::Error),
    Extent(extents::Error),
}
type Result<T> = std::result::Result<T, Error>;
impl From<chunks::Error> for Error {
    fn from(e: chunks::Error) -> Self {
        Self::Chunks(e)
    }
}
impl From<extents::Error> for Error {
    fn from(e: extents::Error) -> Self {
        Self::Extent(e)
    }
}
impl From<VertexPropertyStoreError> for Error {
    fn from(e: VertexPropertyStoreError) -> Self {
        Self::Property(e)
    }
}

fn validate(key: impl FixtureKey, value: &Value) -> Result<()> {
    crate::property::ensure_property_id(key.property())
        .map_err(VertexPropertyStoreError::ReservedPropertyId)?;
    crate::property::ensure_persistable(value).map_err(VertexPropertyStoreError::InvalidValue)?;
    Ok(())
}
fn encoded(value: Value) -> Vec<u8> {
    StoredPropertyValue::V1(value).into_bytes()
}
fn decoded(bytes: &[u8]) -> Value {
    StoredPropertyValue::from_bytes(Cow::Borrowed(bytes)).into_value()
}
fn previous(bytes: &[u8]) -> Option<Value> {
    (!bytes.is_empty()).then(|| decoded(bytes))
}
fn admit(lengths: impl IntoIterator<Item = usize>, budget: usize) -> Result<()> {
    let total = lengths.into_iter().try_fold(0usize, |sum, n| {
        if n > LARGE {
            return Err(Error::Limit);
        }
        sum.checked_add(n).ok_or(Error::Limit)
    })?;
    if total > budget {
        return Err(Error::Limit);
    }
    Ok(())
}

// Only fixture construction and forwarding to the two real private Graph stores differ.
trait FixtureKey: Storable + Ord + Copy + Debug {
    type Direct;
    fn fixture(scope: u32, property: u32) -> Self;
    // Concrete rejected destinations for fixture source (scope0, property1).
    #[cfg(test)]
    fn invalid_move_targets() -> Vec<Self>;
    fn property(self) -> PropertyId;
    fn scope(self) -> Self;
    fn same_move_scope(self, other: Self) -> bool;
    fn direct_open(memory: BodyMemory) -> Self::Direct;
    fn direct_get(store: &Self::Direct, key: Self) -> Option<Value>;
    fn direct_set(store: &mut Self::Direct, key: Self, value: Value) -> Result<Option<Value>>;
    fn direct_remove(store: &mut Self::Direct, key: Self) -> Option<Value>;
    fn direct_scope(store: &Self::Direct, key: Self) -> Vec<(PropertyId, Value)>;
    fn direct_all(store: &Self::Direct) -> Vec<(Self, Value)>;
}
impl FixtureKey for VertexPropertyKey {
    type Direct = VertexPropertyStore<BodyMemory>;
    fn fixture(scope: u32, property: u32) -> Self {
        Self::new(VertexId::from(scope), PropertyId::from_raw(property))
    }
    #[cfg(test)]
    fn invalid_move_targets() -> Vec<Self> {
        vec![Self::fixture(1, 2)]
    }
    fn property(self) -> PropertyId {
        self.property_id()
    }
    fn scope(self) -> Self {
        Self::new(self.vertex_id(), PropertyId::default())
    }
    fn same_move_scope(self, other: Self) -> bool {
        self.property_id() == other.property_id()
    }
    fn direct_open(memory: BodyMemory) -> Self::Direct {
        VertexPropertyStore::init(memory)
    }
    fn direct_get(s: &Self::Direct, k: Self) -> Option<Value> {
        s.get(k.vertex_id(), k.property_id())
    }
    fn direct_set(s: &mut Self::Direct, k: Self, v: Value) -> Result<Option<Value>> {
        Ok(s.set(k.vertex_id(), k.property_id(), v)?)
    }
    fn direct_remove(s: &mut Self::Direct, k: Self) -> Option<Value> {
        s.remove(k.vertex_id(), k.property_id())
    }
    fn direct_scope(s: &Self::Direct, k: Self) -> Vec<(PropertyId, Value)> {
        s.properties_for(k.vertex_id())
    }
    fn direct_all(s: &Self::Direct) -> Vec<(Self, Value)> {
        s.scan_properties_batch(None, 257)
    }
}
impl FixtureKey for EdgePropertyKey {
    type Direct = EdgePropertyStore<BodyMemory>;
    fn fixture(scope: u32, property: u32) -> Self {
        let (owner, label, slot) = match scope {
            2 => (0, 1, 0),
            3 => (1, 0, 0),
            u32::MAX => (u32::MAX, u16::MAX, u32::MAX),
            n => (0, 0, n),
        };
        Self::new(
            VertexId::from(owner),
            label,
            slot,
            PropertyId::from_raw(property),
        )
    }
    #[cfg(test)]
    fn invalid_move_targets() -> Vec<Self> {
        vec![
            Self::fixture(1, 2),
            Self::fixture(2, 1),
            Self::fixture(3, 1),
        ]
    }
    fn property(self) -> PropertyId {
        self.property_id()
    }
    fn scope(self) -> Self {
        Self::new(
            self.owner_vertex_id(),
            self.label_id(),
            self.slot_index(),
            PropertyId::default(),
        )
    }
    fn same_move_scope(self, other: Self) -> bool {
        self.property_id() == other.property_id()
            && self.owner_vertex_id() == other.owner_vertex_id()
            && self.label_id() == other.label_id()
    }
    fn direct_open(memory: BodyMemory) -> Self::Direct {
        EdgePropertyStore::init(memory)
    }
    fn direct_get(s: &Self::Direct, k: Self) -> Option<Value> {
        s.get(
            k.owner_vertex_id(),
            k.label_id(),
            k.slot_index(),
            k.property_id(),
        )
    }
    fn direct_set(s: &mut Self::Direct, k: Self, v: Value) -> Result<Option<Value>> {
        Ok(s.set(
            k.owner_vertex_id(),
            k.label_id(),
            k.slot_index(),
            k.property_id(),
            v,
        )?)
    }
    fn direct_remove(s: &mut Self::Direct, k: Self) -> Option<Value> {
        s.remove(
            k.owner_vertex_id(),
            k.label_id(),
            k.slot_index(),
            k.property_id(),
        )
    }
    fn direct_scope(s: &Self::Direct, k: Self) -> Vec<(PropertyId, Value)> {
        s.properties_for_edge(k.owner_vertex_id(), k.label_id(), k.slot_index())
    }
    fn direct_all(s: &Self::Direct) -> Vec<(Self, Value)> {
        s.scan_properties_batch(None, 257)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Direct,
    Chunks(usize),
    Extent,
}
impl Kind {
    fn cap(self) -> usize {
        match self {
            Self::Chunks(p) => p,
            Self::Direct | Self::Extent => 0,
        }
    }
    fn tag(self) -> u8 {
        match self {
            Self::Direct => 0,
            Self::Chunks(_) => 1,
            Self::Extent => 2,
        }
    }
    fn regions(self) -> usize {
        match self {
            Self::Direct => 1,
            Self::Chunks(_) => 2,
            Self::Extent => 3,
        }
    }
    fn metadata<K: FixtureKey>(self) -> RawCell {
        assert_ne!(self, Self::Direct);
        let mut bytes = vec![0x30, K::BOUND.max_size() as u8, self.tag()];
        bytes.extend_from_slice(&(SMALL as u16).to_le_bytes());
        bytes.extend_from_slice(&(self.cap() as u32).to_le_bytes());
        RawCell(bytes)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reference {
    Chunks(u32),
    Extent(BodyAddress),
}
impl Reference {
    fn len(self) -> usize {
        match self {
            Self::Chunks(n) => n as usize,
            Self::Extent(r) => r.len as usize,
        }
    }
    fn chunks(self) -> Result<usize> {
        match self {
            Self::Chunks(n) => Ok(n as usize),
            Self::Extent(_) => Err(Error::Layout),
        }
    }
    fn extent(self) -> Result<BodyAddress> {
        match self {
            Self::Extent(r) => Ok(r),
            Self::Chunks(_) => Err(Error::Layout),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    Small(Vec<u8>),
    External(Reference),
}
impl Cell {
    fn len(&self) -> usize {
        match self {
            Self::Small(b) => b.len(),
            Self::External(r) => r.len(),
        }
    }
    fn external(&self) -> Option<Reference> {
        match self {
            Self::Small(_) => None,
            Self::External(r) => Some(*r),
        }
    }
    fn encode(self) -> RawCell {
        let mut bytes = Vec::new();
        match self {
            Self::Small(b) => {
                assert!((2..=SMALL).contains(&b.len()));
                bytes.push(0x31);
                bytes.extend(b);
            }
            Self::External(Reference::Chunks(n)) => {
                bytes.push(0x32);
                bytes.extend(n.to_le_bytes());
            }
            Self::External(Reference::Extent(r)) => {
                bytes.push(0x33);
                bytes.extend(r.offset.to_le_bytes());
                bytes.extend(r.len.to_le_bytes());
            }
        }
        RawCell(bytes)
    }
    fn decode(raw: RawCell, kind: Kind) -> Result<Self> {
        match raw.0.as_slice() {
            [0x31, b @ ..] if (2..=SMALL).contains(&b.len()) => Ok(Self::Small(b.to_vec())),
            [0x32, b @ ..] if b.len() == 4 && matches!(kind, Kind::Chunks(_)) => {
                let n = u32::from_le_bytes(b.try_into().unwrap());
                if n as usize <= SMALL || n as usize > LARGE {
                    return Err(Error::Layout);
                }
                Ok(Self::External(Reference::Chunks(n)))
            }
            [0x33, b @ ..] if b.len() == 12 && kind == Kind::Extent => {
                let r = BodyAddress {
                    offset: u64::from_le_bytes(b[..8].try_into().unwrap()),
                    len: u32::from_le_bytes(b[8..].try_into().unwrap()),
                };
                if r.len as usize <= SMALL || r.len as usize > LARGE {
                    return Err(Error::Layout);
                }
                Ok(Self::External(Reference::Extent(r)))
            }
            _ => Err(Error::Layout),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct RawCell(Vec<u8>);
impl Storable for RawCell {
    const BOUND: Bound = Bound::Bounded {
        max_size: (SMALL + 1) as u32,
        is_fixed_size: false,
    };
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }
    fn into_bytes(self) -> Vec<u8> {
        self.0
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct BodyStats {
    end: u64,
    live: u64,
    free: u64,
    holes: usize,
    entries: u64,
    map_entry_bytes: usize,
    body_key_bytes: u64,
    external_bodies: usize,
    external_payload_bytes: usize,
}

// Private adapters preserve the existing owners' exclusive preparation and publication fences.
trait PreparedBody {
    fn previous(&self) -> &[u8];
    fn commit(self, publish: impl FnOnce(Option<Reference>));
}
impl<K: FixtureKey, const P: usize> PreparedBody for chunks::Prepared<'_, P, 0, K> {
    fn previous(&self) -> &[u8] {
        self.previous()
    }
    fn commit(self, publish: impl FnOnce(Option<Reference>)) {
        let next =
            (!self.bytes().is_empty()).then_some(Reference::Chunks(self.bytes().len() as u32));
        self.commit(|| publish(next));
    }
}
impl PreparedBody for extents::Prepared<'_> {
    fn previous(&self) -> &[u8] {
        self.previous()
    }
    fn commit(self, publish: impl FnOnce(Option<Reference>)) {
        self.commit(|r| publish(r.map(Reference::Extent)));
    }
}
trait BodyLane<K: FixtureKey>: Sized {
    const KIND: Kind;
    type Prepared<'a>: PreparedBody
    where
        Self: 'a;
    fn prepare<'a>(
        &'a mut self,
        key: K,
        old: Option<Reference>,
        bytes: &'a [u8],
    ) -> Result<Self::Prepared<'a>>;
    fn open(memories: &[BodyMemory]) -> Result<Self>;
    fn read(&self, key: K, reference: Reference) -> Result<Vec<u8>>;
    fn replace(
        &mut self,
        key: K,
        old: Option<Reference>,
        bytes: &[u8],
        publish: impl FnOnce(Option<Reference>),
    ) -> Result<Option<Value>> {
        let prepared = self.prepare(key, old, bytes)?;
        let value = previous(prepared.previous());
        prepared.commit(publish);
        Ok(value)
    }
    fn relocate(
        &mut self,
        source: (K, Reference),
        target: (K, Option<Reference>),
        publish: impl FnOnce(Reference),
    ) -> Result<Value>;
    fn audit(&self, owners: &BTreeMap<K, (Reference, Vec<u8>)>) -> BodyStats;
}
impl<K: FixtureKey, const P: usize> BodyLane<K> for chunks::Store<P, 0, K> {
    const KIND: Kind = Kind::Chunks(P);
    fn open(m: &[BodyMemory]) -> Result<Self> {
        Ok(Self::open(m[0].clone()))
    }
    fn read(&self, key: K, r: Reference) -> Result<Vec<u8>> {
        let len = r.chunks()?;
        Ok(self.read(Body { prefix: key, len }, len)?)
    }
    type Prepared<'a>
        = chunks::Prepared<'a, P, 0, K>
    where
        Self: 'a;
    fn prepare<'a>(
        &'a mut self,
        key: K,
        old: Option<Reference>,
        bytes: &'a [u8],
    ) -> Result<Self::Prepared<'a>> {
        Ok(self.prepare(key, old.map(Reference::chunks).transpose()?, bytes, BUDGET)?)
    }
    fn relocate(
        &mut self,
        source: (K, Reference),
        target: (K, Option<Reference>),
        publish: impl FnOnce(Reference),
    ) -> Result<Value> {
        let len = source.1.chunks()?;
        let prepared = self.prepare_move(
            Body {
                prefix: source.0,
                len,
            },
            target.0,
            target.1.map(Reference::chunks).transpose()?,
            BUDGET,
        )?;
        let value = decoded(prepared.bytes());
        previous(prepared.previous()); // Validate displaced Value before the first effect.
        prepared.commit(|| publish(Reference::Chunks(len as u32)));
        Ok(value)
    }
    fn audit(&self, owners: &BTreeMap<K, (Reference, Vec<u8>)>) -> BodyStats {
        let bytes = owners
            .iter()
            .map(|(k, (r, b))| {
                assert_eq!(r.chunks().unwrap(), b.len());
                (*k, b.clone())
            })
            .collect();
        self.audit(&bytes);
        BodyStats {
            entries: self.map.len(),
            body_key_bytes: self.map.len() * (K::BOUND.max_size() as u64 + 4),
            ..BodyStats::default()
        }
    }
}
impl<K: FixtureKey> BodyLane<K> for Arena {
    const KIND: Kind = Kind::Extent;
    fn open(m: &[BodyMemory]) -> Result<Self> {
        Ok(Self::open(m[0].clone(), m[1].clone())?)
    }
    fn read(&self, _: K, r: Reference) -> Result<Vec<u8>> {
        Ok(self.read(r.extent()?, r.len())?)
    }
    type Prepared<'a> = extents::Prepared<'a>;
    fn prepare<'a>(
        &'a mut self,
        _: K,
        old: Option<Reference>,
        bytes: &'a [u8],
    ) -> Result<Self::Prepared<'a>> {
        Ok(self.prepare(old.map(Reference::extent).transpose()?, bytes, BUDGET)?)
    }
    fn relocate(
        &mut self,
        source: (K, Reference),
        target: (K, Option<Reference>),
        publish: impl FnOnce(Reference),
    ) -> Result<Value> {
        let address = source.1.extent()?;
        let target = target.1.map(Reference::extent).transpose()?;
        if target.is_some_and(|r| r.offset == address.offset) {
            return Err(Error::Layout);
        }
        let value = decoded(&self.read(address, source.1.len())?);
        if let Some(target) = target {
            let prepared = self.prepare(Some(target), &[], BUDGET)?;
            previous(prepared.previous());
            prepared.commit(|_| publish(source.1));
        } else {
            publish(source.1);
        }
        Ok(value)
    }
    fn audit(&self, owners: &BTreeMap<K, (Reference, Vec<u8>)>) -> BodyStats {
        let (live, free, holes) = self.audit(
            owners
                .values()
                .map(|(r, b)| (r.extent().unwrap(), b.as_slice())),
        );
        BodyStats {
            end: Arena::header(&self.memory).unwrap(),
            live,
            free,
            holes,
            entries: holes as u64,
            body_key_bytes: holes as u64 * 16,
            ..BodyStats::default()
        }
    }
}

// Only with_prepared can construct this capability. No key/input override and no reusable snapshot.
struct PreparedChange<'a, K: FixtureKey, P: PreparedBody> {
    directory: &'a mut StableBTreeMap<K, RawCell, BodyMemory>,
    key: K,
    small: Option<RawCell>,
    previous: Option<Value>,
    body: Option<P>,
}
impl<K: FixtureKey, P: PreparedBody> PreparedChange<'_, K, P> {
    fn previous(&self) -> Option<&Value> {
        self.previous.as_ref()
    }
    fn commit(self) -> Option<Value> {
        if self.body.is_none() && self.previous.is_none() && self.small.is_none() {
            return None;
        }
        let publish = |reference: Option<Reference>| {
            let cell = reference.map(|r| Cell::External(r).encode()).or(self.small);
            match cell {
                Some(c) => {
                    self.directory.insert(self.key, c);
                }
                None => {
                    self.directory.remove(&self.key);
                }
            }
        };
        if let Some(body) = self.body {
            body.commit(publish);
        } else {
            publish(None);
        }
        self.previous
    }
}

struct Candidate<K: FixtureKey, L: BodyLane<K>> {
    directory: StableBTreeMap<K, RawCell, BodyMemory>,
    lane: L,
    memories: Vec<BodyMemory>,
}
impl<K: FixtureKey, L: BodyLane<K>> Candidate<K, L> {
    fn open(memories: Vec<BodyMemory>) -> Result<Self> {
        if !matches!(L::KIND, Kind::Chunks(4096 | 16384 | 65536) | Kind::Extent)
            || memories.len() != L::KIND.regions()
        {
            return Err(Error::Layout);
        }
        let empty = memories[0].size() == 0;
        if memories.iter().any(|m| (m.size() == 0) != empty) {
            return Err(Error::Layout);
        }
        #[cfg(test)]
        for (i, memory) in memories.iter().enumerate() {
            if memories[..i]
                .iter()
                .any(|m| Rc::ptr_eq(&m.inner, &memory.inner))
            {
                return Err(Error::Layout);
            }
        }
        let metadata = L::KIND.metadata::<K>();
        let mut directory = if empty {
            StableBTreeMap::new(memories[0].clone())
        } else {
            let directory = StableBTreeMap::load(memories[0].clone());
            println!(
                "PROPERTY_OPEN,{},{},{},load,1,0",
                K::BOUND.max_size(),
                L::KIND.tag(),
                L::KIND.cap()
            );
            if directory.get(&K::fixture(0, 0)) != Some(metadata.clone()) {
                return Err(Error::Layout);
            }
            directory
        };
        // Once creation writes begin, an unexpected child failure must not become an ordinary Err.
        let lane = if empty {
            L::open(&memories[1..]).expect("body creation after directory fence")
        } else {
            L::open(&memories[1..])?
        };
        if empty {
            directory.insert(K::fixture(0, 0), metadata);
        }
        println!(
            "PROPERTY_OPEN,{},{},{},{},{},{}",
            K::BOUND.max_size(),
            L::KIND.tag(),
            L::KIND.cap(),
            if empty { "new" } else { "load" },
            if empty { 2 } else { 1 },
            usize::from(empty && L::KIND == Kind::Extent)
        );
        Ok(Self {
            directory,
            lane,
            memories,
        })
    }
    fn cell(&self, key: K) -> Result<Option<Cell>> {
        if key.property().raw() == 0 {
            return Ok(None);
        }
        self.directory
            .get(&key)
            .map(|r| Cell::decode(r, L::KIND))
            .transpose()
    }
    fn bytes(&self, key: K, cell: &Cell) -> Result<Vec<u8>> {
        match cell {
            Cell::Small(b) => Ok(b.clone()),
            Cell::External(r) => self.lane.read(key, *r),
        }
    }
    fn get_bounded(&self, key: K, budget: usize) -> Result<Option<Value>> {
        let Some(cell) = self.cell(key)? else {
            return Ok(None);
        };
        admit([cell.len()], budget)?;
        Ok(Some(decoded(&self.bytes(key, &cell)?)))
    }
    fn change(&mut self, key: K, value: Option<Value>, budget: usize) -> Result<Option<Value>> {
        self.with_prepared(key, value, budget, |prepared| prepared.commit())
    }
    // The callback cannot return a capability borrowing this stack's encoded input. A caller may
    // reject/drop before effects or commit its own intent and then consume the infallible capability.
    fn with_prepared<T>(
        &mut self,
        key: K,
        value: Option<Value>,
        budget: usize,
        run: impl FnOnce(PreparedChange<'_, K, L::Prepared<'_>>) -> T,
    ) -> Result<T> {
        let bytes = match value {
            Some(value) => {
                validate(key, &value)?;
                encoded(value)
            }
            None => Vec::new(),
        };
        admit([bytes.len()], budget)?;
        let old = self.cell(key)?;
        admit([old.as_ref().map_or(0, Cell::len), bytes.len()], budget)?;
        let old_reference = old.as_ref().and_then(Cell::external);
        let small_previous = match &old {
            Some(Cell::Small(b)) => Some(decoded(b)),
            _ => None,
        };
        let small = if bytes.is_empty() {
            None
        } else if bytes.len() <= SMALL {
            Some(Cell::Small(bytes.clone()).encode())
        } else {
            None
        };
        let body = if bytes.len() > SMALL || old_reference.is_some() {
            let external_bytes = if bytes.len() > SMALL {
                bytes.as_slice()
            } else {
                &[]
            };
            Some(self.lane.prepare(key, old_reference, external_bytes)?)
        } else {
            None
        };
        let previous = body
            .as_ref()
            .and_then(|p| previous(p.previous()))
            .or(small_previous);
        Ok(run(PreparedChange {
            directory: &mut self.directory,
            key,
            small,
            previous,
            body,
        }))
    }
    fn rekey_bounded(&mut self, source: K, target: K, budget: usize) -> Result<Option<Value>> {
        if !source.same_move_scope(target) {
            return Err(Error::Layout);
        }
        if source == target {
            return Ok(None);
        }
        let Some(cell) = self.cell(source)? else {
            return Ok(None);
        };
        let old = self.cell(target)?;
        admit([cell.len(), old.as_ref().map_or(0, Cell::len)], budget)?;
        if let Some(Cell::Small(bytes)) = &old {
            decoded(bytes);
        }
        let old_reference = old.as_ref().and_then(Cell::external);
        let moved = match &cell {
            Cell::Small(b) => Some(decoded(b)),
            Cell::External(_) => None,
        };
        let source_reference = cell.external();
        let publish = |reference: Option<Reference>| {
            let cell = reference.map(Cell::External).unwrap_or(cell);
            self.directory.insert(target, cell.encode());
            assert!(self.directory.remove(&source).is_some());
        };
        if let Some(reference) = source_reference {
            Ok(Some(self.lane.relocate(
                (source, reference),
                (target, old_reference),
                |r| publish(Some(r)),
            )?))
        } else if old_reference.is_some() {
            self.lane.replace(target, old_reference, &[], publish)?;
            Ok(moved)
        } else {
            publish(None);
            Ok(moved)
        }
    }
    fn enumerate_bounded(
        &self,
        scope: K,
        budget: usize,
        max: usize,
    ) -> Result<Vec<(PropertyId, Value)>> {
        let scope = scope.scope();
        let mut cells = Vec::new();
        let mut total = 0usize;
        for entry in self
            .directory
            .range(scope..)
            .take_while(|e| e.key().scope() == scope)
        {
            let key = *entry.key();
            if key == K::fixture(0, 0) {
                continue;
            }
            if key.property().raw() == 0 {
                return Err(Error::Layout);
            }
            if cells.len() == max {
                return Err(Error::Limit);
            }
            let cell = Cell::decode(entry.value(), L::KIND)?;
            total = total.checked_add(cell.len()).ok_or(Error::Limit)?;
            if total > budget {
                return Err(Error::Limit);
            }
            cells.push((key, cell));
        }
        cells
            .into_iter()
            .map(|(k, c)| Ok((k.property(), decoded(&self.bytes(k, &c)?))))
            .collect()
    }
    fn entries(&self) -> BTreeMap<K, Vec<u8>> {
        self.directory
            .iter()
            .filter_map(|e| {
                let k = *e.key();
                if k == K::fixture(0, 0) {
                    assert_eq!(e.value(), L::KIND.metadata::<K>());
                    return None;
                }
                assert_ne!(k.property().raw(), 0);
                let cell = Cell::decode(e.value(), L::KIND).unwrap();
                Some((k, self.bytes(k, &cell).unwrap()))
            })
            .collect()
    }
}

// Normalized successful fixture operations. Only candidates expose additional bounded methods.
trait ProbeStore<K: FixtureKey>: Sized {
    const KIND: Kind;
    fn new() -> Self;
    fn get(&self, key: K) -> Result<Option<Value>>;
    fn set(&mut self, key: K, value: Value) -> Result<Option<Value>>;
    fn remove(&mut self, key: K) -> Result<Option<Value>>;
    fn rekey(&mut self, source: K, target: K) -> Result<Option<Value>>;
    fn enumerate(&self, scope: K) -> Result<Vec<(PropertyId, Value)>>;
    fn memories(&self) -> &[BodyMemory];
    fn raw_properties(&self) -> Vec<(K, Vec<u8>)>;
    fn reopen(self) -> Self;
    fn audit(&self, expected: &BTreeMap<K, Vec<u8>>) -> BodyStats;
}
impl<K: FixtureKey, L: BodyLane<K>> ProbeStore<K> for Candidate<K, L> {
    const KIND: Kind = L::KIND;
    fn new() -> Self {
        Self::open(new_memories(L::KIND.regions())).unwrap()
    }
    fn get(&self, key: K) -> Result<Option<Value>> {
        self.get_bounded(key, BUDGET)
    }
    fn set(&mut self, key: K, value: Value) -> Result<Option<Value>> {
        self.change(key, Some(value), BUDGET)
    }
    fn remove(&mut self, key: K) -> Result<Option<Value>> {
        self.change(key, None, BUDGET)
    }
    fn rekey(&mut self, source: K, target: K) -> Result<Option<Value>> {
        self.rekey_bounded(source, target, BUDGET)
    }
    fn enumerate(&self, scope: K) -> Result<Vec<(PropertyId, Value)>> {
        self.enumerate_bounded(scope, BUDGET, 256)
    }
    fn memories(&self) -> &[BodyMemory] {
        &self.memories
    }
    fn raw_properties(&self) -> Vec<(K, Vec<u8>)> {
        let rows: Vec<_> = self
            .directory
            .iter()
            .map(|e| (*e.key(), e.value().0))
            .collect();
        assert_eq!(rows.len() as u64, self.directory.len());
        rows
    }
    fn reopen(self) -> Self {
        Self::open(self.memories).unwrap()
    }
    fn audit(&self, expected: &BTreeMap<K, Vec<u8>>) -> BodyStats {
        assert_eq!(&self.entries(), expected);
        assert_eq!(self.directory.len(), expected.len() as u64 + 1);
        let owners = self
            .directory
            .iter()
            .filter_map(|e| {
                let key = *e.key();
                if key == K::fixture(0, 0) {
                    return None;
                }
                let cell = Cell::decode(e.value(), L::KIND).unwrap();
                cell.external().map(|r| (key, (r, expected[&key].clone())))
            })
            .collect();
        let mut stats = self.lane.audit(&owners);
        stats.external_bodies = owners.len();
        stats.external_payload_bytes = owners.values().map(|(_, bytes)| bytes.len()).sum();
        stats.map_entry_bytes = self
            .directory
            .iter()
            .map(|e| K::BOUND.max_size() as usize + e.value().0.len())
            .sum();
        stats
    }
}
struct Direct<K: FixtureKey> {
    store: K::Direct,
    memories: [BodyMemory; 1],
}
impl<K: FixtureKey> ProbeStore<K> for Direct<K> {
    const KIND: Kind = Kind::Direct;
    fn new() -> Self {
        let memory = new_memories(1).pop().unwrap();
        let store = K::direct_open(memory.clone());
        println!("PROPERTY_OPEN,{},0,0,new,1,0", K::BOUND.max_size());
        Self {
            store,
            memories: [memory],
        }
    }
    fn get(&self, k: K) -> Result<Option<Value>> {
        Ok(K::direct_get(&self.store, k))
    }
    fn set(&mut self, k: K, v: Value) -> Result<Option<Value>> {
        K::direct_set(&mut self.store, k, v)
    }
    fn remove(&mut self, k: K) -> Result<Option<Value>> {
        Ok(K::direct_remove(&mut self.store, k))
    }
    fn rekey(&mut self, source: K, target: K) -> Result<Option<Value>> {
        if !source.same_move_scope(target) {
            return Err(Error::Layout);
        }
        if source == target {
            return Ok(None);
        }
        let Some(value) = self.get(source)? else {
            return Ok(None);
        };
        validate(target, &value)?;
        K::direct_remove(&mut self.store, source);
        K::direct_set(&mut self.store, target, value.clone())
            .expect("validated rekey after removal fence");
        Ok(Some(value))
    }
    fn enumerate(&self, scope: K) -> Result<Vec<(PropertyId, Value)>> {
        Ok(K::direct_scope(&self.store, scope))
    }
    fn memories(&self) -> &[BodyMemory] {
        &self.memories
    }
    fn raw_properties(&self) -> Vec<(K, Vec<u8>)> {
        K::direct_all(&self.store)
            .into_iter()
            .map(|(k, v)| (k, encoded(v)))
            .collect()
    }
    fn reopen(self) -> Self {
        let store = K::direct_open(self.memories[0].clone());
        println!("PROPERTY_OPEN,{},0,0,load,1,0", K::BOUND.max_size());
        Self { store, ..self }
    }
    fn audit(&self, expected: &BTreeMap<K, Vec<u8>>) -> BodyStats {
        let actual: BTreeMap<_, _> = K::direct_all(&self.store)
            .into_iter()
            .map(|(k, v)| (k, encoded(v)))
            .collect();
        assert_eq!(&actual, expected);
        BodyStats {
            entries: expected.len() as u64,
            map_entry_bytes: expected
                .values()
                .map(|v| K::BOUND.max_size() as usize + v.len())
                .sum(),
            ..BodyStats::default()
        }
    }
}

fn text(size: usize, salt: u8) -> Value {
    let overhead = encoded(Value::Text(String::new())).len();
    assert!(size >= overhead);
    let bytes = (0..size - overhead)
        .map(|i| b'a' + ((((i / 8) >> (4 * (i % 8))) as u8 & 15).wrapping_add(salt) & 15))
        .collect();
    let value = Value::Text(String::from_utf8(bytes).unwrap());
    assert_eq!(encoded(value.clone()).len(), size);
    value
}
fn scalar(n: usize) -> Value {
    match n % 8 {
        0 => Value::Null,
        1 => Value::Bool(n % 16 == 1),
        2 => Value::Int64(n as i64),
        3 => Value::Text("日本語🌿".into()),
        4 => Value::Text(String::new()),
        5 => Value::Record(vec![(
            "a".into(),
            Value::List(vec![Value::Null, Value::Int64(n as i64)]),
        )]),
        6 => Value::Float64(f64::NAN),
        _ => gleaph_gql_ic::PrincipalValue(candid::Principal::management_canister()).into(),
    }
}

struct Fixture<K: FixtureKey, S: ProbeStore<K>> {
    store: S,
    expected: BTreeMap<K, Vec<u8>>,
}
impl<K: FixtureKey, S: ProbeStore<K>> Fixture<K, S> {
    fn new() -> Self {
        let mut store = S::new();
        // Small-only sanity leaves every external body lane unprimed for cold large/fragment cases.
        let key = K::fixture(99, 42);
        assert_eq!(store.get(key).unwrap(), None);
        assert_eq!(store.set(key, Value::Null).unwrap(), None);
        assert_eq!(store.get(key).unwrap(), Some(Value::Null));
        assert_eq!(
            store.set(key, Value::Bool(true)).unwrap(),
            Some(Value::Null)
        );
        assert_eq!(store.remove(key).unwrap(), Some(Value::Bool(true)));
        let result = Self {
            store,
            expected: BTreeMap::new(),
        };
        result.audit();
        result
    }
    fn audit(&self) -> BodyStats {
        assert!(self.expected.len() <= 256);
        assert!(self.expected.values().all(|b| b.len() <= LARGE));
        assert!(self.expected.values().map(Vec::len).sum::<usize>() <= 8 * LARGE);
        self.store.audit(&self.expected)
    }
    fn address(&self, key: K) -> Option<BodyAddress> {
        assert_eq!(S::KIND, Kind::Extent);
        self.store
            .raw_properties()
            .into_iter()
            .find_map(|(k, bytes)| {
                (k == key)
                    .then(|| {
                        Cell::decode(RawCell(bytes), Kind::Extent)
                            .unwrap()
                            .external()
                    })
                    .flatten()
            })
            .map(|r| r.extent().unwrap())
    }
}

#[cfg(not(test))]
mod benches;

#[cfg(test)]
mod native {
    use super::*;
    use chunks::CountedMemory;

    struct Sample<T> {
        result: T,
        before: Vec<u64>,
        io: Vec<Io>,
    }
    impl<K: FixtureKey, S: ProbeStore<K>> Fixture<K, S> {
        fn measure<T>(&mut self, operation: impl FnOnce(&mut S) -> T) -> Sample<T> {
            let memories = self.store.memories();
            let before = memories.iter().map(|m| m.size() * PAGE).collect();
            for m in memories {
                *m.io.borrow_mut() = Io::default();
            }
            let result = operation(&mut self.store);
            let io = self
                .store
                .memories()
                .iter()
                .map(|m| *m.io.borrow())
                .collect();
            Sample { result, before, io }
        }
        fn report<T>(&self, case: &str, input: usize, sample: &Sample<T>) {
            let stats = self.audit();
            let stored: usize = self.expected.values().map(Vec::len).sum();
            let total: u64 = self.store.memories().iter().map(|m| m.size() * PAGE).sum();
            let projected = 2 + self
                .store
                .memories()
                .iter()
                .map(|m| 4 * m.size().div_ceil(4))
                .sum::<u64>();
            for (region, (m, io)) in self.store.memories().iter().zip(&sample.io).enumerate() {
                println!(
                    "PROPERTY,{},{},{},{case},{input},{},{stored},{},{},{},{},{},{},{region},{},{},{},{},{},{},{},{},{total},{projected},{},{}",
                    K::BOUND.max_size(),
                    S::KIND.tag(),
                    S::KIND.cap(),
                    self.expected.len(),
                    stats.map_entry_bytes,
                    stats.entries,
                    stats.body_key_bytes,
                    stats.end,
                    stats.live,
                    stats.free,
                    io.reads,
                    io.read_bytes,
                    io.writes,
                    io.write_bytes,
                    io.grows,
                    io.body_buffer_bytes,
                    sample.before[region],
                    m.size() * PAGE,
                    stats.external_bodies,
                    stats.external_payload_bytes
                );
            }
        }
        // Sequential independent property operations in one measurement, not a batch transaction.
        fn batch(&mut self, case: &str, writes: Vec<(K, Option<Value>)>) -> Vec<Option<Value>> {
            let new: Vec<_> = writes
                .iter()
                .map(|(k, v)| (*k, v.clone().map(encoded)))
                .collect();
            let small_only = new.iter().all(|(key, bytes)| {
                bytes.as_ref().is_none_or(|b| b.len() <= SMALL)
                    && self.expected.get(key).is_none_or(|b| b.len() <= SMALL)
            });
            let input = new
                .iter()
                .map(|(k, b)| b.as_ref().or(self.expected.get(k)).map_or(0, Vec::len))
                .sum();
            let sample = self.measure(|s| {
                writes
                    .into_iter()
                    .map(|(k, v)| match v {
                        Some(v) => s.set(k, v),
                        None => s.remove(k),
                    })
                    .collect::<Result<Vec<_>>>()
            });
            let got = sample.result.as_ref().unwrap();
            if small_only {
                for io in &sample.io[1..] {
                    assert_eq!(
                        *io,
                        Io::default(),
                        "small-only mutation touched body storage"
                    );
                }
            }
            for ((k, bytes), previous) in new.into_iter().zip(got) {
                let old = match bytes {
                    Some(b) => self.expected.insert(k, b),
                    None => self.expected.remove(&k),
                };
                assert_eq!(
                    previous.clone().map(encoded),
                    old,
                    "previous value at {k:?}"
                );
            }
            self.report(case, input, &sample);
            sample.result.unwrap()
        }
        fn set(&mut self, case: &str, key: K, value: Value) -> Option<Value> {
            self.batch(case, vec![(key, Some(value))]).pop().unwrap()
        }
        fn remove(&mut self, case: &str, key: K) -> Option<Value> {
            self.batch(case, vec![(key, None)]).pop().unwrap()
        }
        fn get(&mut self, case: &str, keys: &[K]) -> Vec<Option<Value>> {
            let sample =
                self.measure(|s| keys.iter().map(|k| s.get(*k)).collect::<Result<Vec<_>>>());
            let got = sample.result.as_ref().unwrap();
            for (key, value) in keys.iter().zip(got) {
                assert_eq!(value.clone().map(encoded).as_ref(), self.expected.get(key));
            }
            for io in &sample.io {
                assert_eq!((io.writes, io.grows), (0, 0));
            }
            self.report(
                case,
                keys.iter()
                    .filter_map(|k| self.expected.get(k))
                    .map(Vec::len)
                    .sum(),
                &sample,
            );
            if S::KIND != Kind::Direct
                && keys
                    .iter()
                    .all(|k| self.expected.get(k).is_none_or(|b| b.len() <= SMALL))
            {
                for io in &sample.io[1..] {
                    assert_eq!(
                        *io,
                        Io::default(),
                        "small/missing get touched a body region"
                    );
                }
            }
            sample.result.unwrap()
        }
        fn rekey(&mut self, case: &str, source: K, target: K) {
            let before_source = self.expected.get(&source).cloned();
            let no_op = source == target || before_source.is_none();
            let before = no_op.then(|| {
                self.store
                    .memories()
                    .iter()
                    .map(|m| m.inner.borrow().clone())
                    .collect::<Vec<_>>()
            });
            let address = (S::KIND == Kind::Extent)
                .then(|| self.address(source))
                .flatten();
            let sample = self.measure(|s| s.rekey(source, target));
            if let Some(before) = before {
                for ((m, bytes), io) in self.store.memories().iter().zip(before).zip(&sample.io) {
                    assert_eq!(*m.inner.borrow(), bytes);
                    assert_eq!((io.writes, io.grows), (0, 0));
                }
            }
            let wanted = if source == target {
                None
            } else {
                before_source.clone()
            };
            assert_eq!(sample.result.as_ref().unwrap().clone().map(encoded), wanted);
            if source != target
                && let Some(bytes) = before_source
            {
                self.expected.remove(&source);
                self.expected.insert(target, bytes);
            }
            if S::KIND == Kind::Extent {
                if !no_op && let Some(address) = address {
                    assert_eq!(self.address(target), Some(address));
                }
                // Reference transfer may retire the target, but never rewrites the moved payload.
                let max_payload_write = sample.io[1].write_bytes;
                assert!(
                    max_payload_write <= 48,
                    "extent move rewrote source payload: {max_payload_write}"
                );
            }
            self.report(case, wanted.as_ref().map_or(0, Vec::len), &sample);
        }
        fn reopen(self) -> Self {
            let memories = self.store.memories().to_vec();
            let before: Vec<_> = memories.iter().map(|m| m.inner.borrow().clone()).collect();
            for m in &memories {
                *m.io.borrow_mut() = Io::default();
            }
            let store = self.store.reopen();
            for (m, before) in memories.iter().zip(before) {
                assert_eq!(*m.inner.borrow(), before);
                assert_eq!((m.io.borrow().writes, m.io.borrow().grows), (0, 0));
            }
            println!(
                "PROPERTY_REOPEN,{},{},{}",
                K::BOUND.max_size(),
                S::KIND.tag(),
                S::KIND.cap()
            );
            let result = Self { store, ..self };
            result.audit();
            result
        }
        fn clear(&mut self, case: &str) {
            let keys = self.expected.keys().copied().collect::<Vec<_>>();
            self.batch(case, keys.into_iter().map(|k| (k, None)).collect());
        }
    }

    fn density<K: FixtureKey, S: ProbeStore<K>>() {
        let mut f = Fixture::<K, S>::new();
        // Independent slot-, label- and owner-only edge foils, plus the maximum key scope.
        let scopes = [0, 1, 2, 3, u32::MAX];
        let keys: Vec<_> = scopes
            .into_iter()
            .enumerate()
            .flat_map(|(i, s)| {
                let count = if i == 4 { 52 } else { 51 };
                (1..=count).map(move |p| K::fixture(s, if p == count { u32::MAX } else { p }))
            })
            .collect();
        for k in &keys {
            let bytes = k.into_bytes();
            assert_eq!(bytes.len(), K::BOUND.max_size() as usize);
            assert_eq!(K::from_bytes(Cow::Borrowed(&bytes)), *k);
        }
        let lexical: Vec<_> = keys.iter().map(|k| k.into_bytes()).collect();
        assert!(lexical.windows(2).all(|w| w[0] < w[1]));
        f.batch(
            "density_insert_256",
            keys.iter()
                .enumerate()
                .map(|(n, k)| (*k, Some(scalar(n))))
                .collect(),
        );
        f.get("density_get_256", &keys);
        for scope in scopes {
            let key = K::fixture(scope, 0);
            let expected: Vec<_> = f
                .expected
                .iter()
                .filter(|(k, _)| k.scope() == key.scope())
                .map(|(k, b)| (k.property(), b.clone()))
                .collect();
            let sample = f.measure(|s| s.enumerate(key));
            assert_eq!(
                sample
                    .result
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|(p, v)| (*p, encoded(v.clone())))
                    .collect::<Vec<_>>(),
                expected
            );
            f.report(
                "scope_enumerate",
                expected.iter().map(|(_, b)| b.len()).sum(),
                &sample,
            );
        }
        f.batch(
            "density_rewrite_256",
            keys.iter()
                .enumerate()
                .map(|(i, k)| (*k, Some(text(65, (i % 16) as u8))))
                .collect(),
        );
        f.batch(
            "density_remove_128",
            keys.iter().step_by(2).map(|k| (*k, None)).collect(),
        );
        f = f.reopen();
        f.batch(
            "density_refill_128",
            keys.iter()
                .step_by(2)
                .map(|k| (*k, Some(Value::Null)))
                .collect(),
        );
        for (i, n) in [65, 66, 97, 65, 64, 66, 65].into_iter().enumerate() {
            f.set("threshold", keys[0], text(n, 32 + i as u8));
        }
        f.get("reserved_read", &[K::fixture(0, 0)]);
        assert_eq!(f.store.remove(K::fixture(0, 0)).unwrap(), None);
        f.audit();
    }
    fn neighbors<K: FixtureKey, S: ProbeStore<K>>() {
        let mut f = Fixture::<K, S>::new();
        let hot = K::fixture(0, 1);
        let guard = K::fixture(0, 2);
        f.set("seed_small", hot, text(65, 1));
        f.set("seed_guard", guard, text(65, 2));
        f.set("cold_grow_1m", hot, text(LARGE, 3));
        f.get("read_1m", &[hot]);
        f.set("rewrite_1m", hot, text(LARGE, 4));
        f.get("small65_beside_1m", &[guard]);
        f.set("small65_rewrite_beside_1m", guard, text(65, 5));
        f.set("external66_create_beside_1m", guard, text(66, 6));
        f.get("external66_beside_1m", &[guard]);
        f.set("external66_rewrite_beside_1m", guard, text(66, 7));
        f.set("shrink_65", hot, text(65, 8));
        f.set("grow_64k", hot, text(65536, 9));
        f.get("read_64k", &[hot]);
        f.set("rewrite_64k", hot, text(65536, 10));
        f.set("shrink_64k_to65", hot, text(65, 11));
        f = f.reopen();
        // The external guard can force relocation. Establish reuse high-water, not an in-place promise.
        let mut backing = f
            .store
            .memories()
            .iter()
            .map(Memory::size)
            .collect::<Vec<_>>();
        for i in 0..4 {
            f.set("reuse_grow_1m", hot, text(LARGE, 2 * i + 12));
            f.set("reuse_shrink_65", hot, text(65, 2 * i + 13));
            let after: Vec<_> = f.store.memories().iter().map(Memory::size).collect();
            if i > 0 {
                assert_eq!(after, backing, "warm repeated growth leaked backing");
            }
            backing = after;
        }
        // Put a live blocker immediately after a short source in the reused large hole. A 1MiB
        // replacement must relocate; publishing the old address would otherwise pass in-place cases.
        f.set("relocation_short_source", hot, text(66, 1));
        f.set("relocation_blocker", K::fixture(0, 3), text(97, 2));
        let before = f.audit();
        let old_address = (S::KIND == Kind::Extent).then(|| f.address(hot).unwrap());
        f.set("forced_relocation_1m", hot, text(LARGE, 3));
        if let Some(old) = old_address {
            let new = f.address(hot).unwrap();
            assert_ne!(new.offset, old.offset);
            assert_eq!(new.offset, before.end);
            assert_eq!(f.audit().end, before.end + LARGE as u64 + 16);
        }
        backing = f.store.memories().iter().map(Memory::size).collect();
        f.clear("neighbor_clear_after_relocation");
        let after: Vec<_> = f.store.memories().iter().map(Memory::size).collect();
        assert!(after.iter().zip(&backing).all(|(a, b)| a >= b));
        if S::KIND == Kind::Extent {
            assert_eq!(after, backing);
        }
        // Library B-tree deletion may allocate while merging/saving nodes. Its observed growth is
        // reported, not misclassified as a leaked property; the full audit above requires no bodies.
    }
    fn moves<K: FixtureKey, S: ProbeStore<K>>() {
        let mut f = Fixture::<K, S>::new();
        let writes = (0..256)
            .map(|n| {
                (
                    K::fixture(0, n + 1),
                    Some(text(if n % 2 == 0 { 66 } else { 97 }, n as u8)),
                )
            })
            .collect();
        f.batch("fragment_seed_256", writes);
        f.batch(
            "fragment_remove_128",
            (0..256)
                .step_by(2)
                .map(|n| (K::fixture(0, n + 1), None))
                .collect(),
        );
        let holes = f.audit();
        if S::KIND == Kind::Extent {
            assert_eq!(
                (holes.end, holes.live, holes.free, holes.holes),
                (64 + 128 * 208, 128 * 120, 128 * 88, 128)
            );
        }
        f.set("fragment_no_fit_128", K::fixture(1, 1), text(97, 3));
        if S::KIND == Kind::Extent {
            assert_eq!(f.audit().end, holes.end + 120);
        }
        f.remove("fragment_release_guard", K::fixture(0, 2));
        f.set("fragment_coalesced_fit_128", K::fixture(1, 2), text(257, 4));
        if S::KIND == Kind::Extent {
            let stats = f.audit();
            assert_eq!(stats.end, holes.end + 120);
            assert_eq!(stats.holes, 126);
        }
        f.clear("fragment_clear");
        let source = K::fixture(0, 1);
        let target = K::fixture(1, 1);
        let unrelated = K::fixture(0, 7);
        f.set("move_guard", unrelated, text(66, 9));
        f.set("missing_source_target", target, text(97, 10));
        f.rekey("move_missing", source, target);
        for source_size in [65, 66, LARGE] {
            for target_size in [0, 65, 97, LARGE] {
                f.set("move_source_setup", source, text(source_size, 1));
                if target_size == 0 {
                    f.remove("move_target_absent", target);
                } else {
                    f.set("move_target_setup", target, text(target_size, 2));
                }
                f.rekey("move_self", source, source);
                f.rekey(
                    &format!("move_{source_size}_over_{target_size}"),
                    source,
                    target,
                );
                f.set("move_source_recreate", source, Value::Null);
                f.remove("move_target_delete", target);
            }
        }
        f = f.reopen();
        f.set("move_post_reopen_source", source, text(97, 14));
        f.rekey("move_post_reopen", source, target);
        f.clear("move_clear");
    }

    macro_rules! matrix {
        ($family:ident, $key:ty) => {
            $family::<$key, Direct<$key>>();
            $family::<$key, Candidate<$key, chunks::Store<4096, 0, $key>>>();
            $family::<$key, Candidate<$key, chunks::Store<16384, 0, $key>>>();
            $family::<$key, Candidate<$key, chunks::Store<65536, 0, $key>>>();
            $family::<$key, Candidate<$key, Arena>>();
        };
    }
    #[test]
    fn property_store_scalar_scope_and_directory_density() {
        println!(
            "PROPERTY_HEADER,key_width,kind,cap,case,input_bytes,properties,stored_bytes,map_entry_bytes,body_entries,body_key_bytes,logical_end,live_span_bytes,free_span_bytes,region,reads,read_bytes,writes,write_bytes,grows,body_buffer_bytes,before_bytes,after_bytes,total_backing_bytes,projected_manager_pages,external_bodies,external_payload_bytes"
        );
        matrix!(density, VertexPropertyKey);
        matrix!(density, EdgePropertyKey);
    }
    #[test]
    fn property_store_large_neighbors_and_reuse() {
        matrix!(neighbors, VertexPropertyKey);
        matrix!(neighbors, EdgePropertyKey);
    }
    #[test]
    fn property_store_fragmentation_and_rekey() {
        matrix!(moves, VertexPropertyKey);
        matrix!(moves, EdgePropertyKey);
    }

    #[derive(Debug, Clone)]
    struct Unencodable;
    impl std::fmt::Display for Unencodable {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unencodable fixture")
        }
    }
    impl gleaph_gql::ExtensionValue for Unencodable {
        fn type_name(&self) -> &str {
            "unencodable fixture"
        }
        fn clone_box(&self) -> Box<dyn gleaph_gql::ExtensionValue> {
            Box::new(self.clone())
        }
        fn eq_ext(&self, other: &dyn gleaph_gql::ExtensionValue) -> bool {
            other.as_any().is::<Self>()
        }
        fn cmp_ext(&self, other: &dyn gleaph_gql::ExtensionValue) -> Option<std::cmp::Ordering> {
            other
                .as_any()
                .is::<Self>()
                .then_some(std::cmp::Ordering::Equal)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    fn reject<K: FixtureKey, S: ProbeStore<K>, T>(
        store: &mut S,
        error: Error,
        operation: impl FnOnce(&mut S) -> Result<T>,
    ) {
        let rows = store.raw_properties();
        let memories = store.memories().to_vec();
        let before: Vec<_> = memories.iter().map(|m| m.inner.borrow().clone()).collect();
        for m in &memories {
            *m.io.borrow_mut() = Io::default();
        }
        match operation(store) {
            Err(e) => assert_eq!(e, error),
            Ok(_) => panic!("expected {error:?}"),
        }
        for (m, bytes) in memories.iter().zip(before) {
            assert_eq!(
                *m.inner.borrow(),
                bytes,
                "rejection changed persistent bytes"
            );
            assert_eq!((m.io.borrow().writes, m.io.borrow().grows), (0, 0));
        }
        assert_eq!(
            store.raw_properties(),
            rows,
            "rejection changed actual directory entries"
        );
    }
    fn common_rejections<K: FixtureKey, S: ProbeStore<K>>() -> Fixture<K, S> {
        let mut f = Fixture::<K, S>::new();
        f.set("reject_source_seed", K::fixture(0, 1), text(LARGE, 3));
        f.set("reject_target_seed", K::fixture(1, 1), text(97, 4));
        f.set("reject_guard_seed", K::fixture(0, 7), text(65, 5));
        reject(
            &mut f.store,
            Error::Property(VertexPropertyStoreError::ReservedPropertyId(
                PropertyId::default(),
            )),
            |s| s.set(K::fixture(0, 0), Value::Null),
        );
        reject(
            &mut f.store,
            Error::Property(VertexPropertyStoreError::InvalidValue(
                ValueBinaryError::InvalidExtensionType,
            )),
            |s| s.set(K::fixture(0, 1), Value::Extension(Box::new(Unencodable))),
        );
        for target in K::invalid_move_targets() {
            reject(&mut f.store, Error::Layout, |s| {
                s.rekey(K::fixture(0, 1), target)
            });
        }
        f.audit();
        f
    }
    fn candidate_rejections<K: FixtureKey, L: BodyLane<K>>(f: &mut Fixture<K, Candidate<K, L>>) {
        let key = K::fixture(0, 1);
        let target = K::fixture(1, 1);
        reject(&mut f.store, Error::Limit, |s| {
            s.get_bounded(key, LARGE - 1)
        });
        for m in &f.store.memories[1..] {
            assert_eq!(
                *m.io.borrow(),
                Io::default(),
                "over-budget get entered body storage"
            );
        }
        assert_eq!(
            encoded(f.store.get_bounded(key, LARGE).unwrap().unwrap()),
            f.expected[&key]
        );
        reject(&mut f.store, Error::Limit, |s| {
            s.change(key, Some(text(65, 10)), LARGE + 64)
        });
        reject(&mut f.store, Error::Limit, |s| {
            s.change(key, None, LARGE - 1)
        });
        reject(&mut f.store, Error::Limit, |s| {
            s.rekey_bounded(key, target, LARGE + 96)
        });
        reject(&mut f.store, Error::Limit, |s| {
            s.enumerate_bounded(key, LARGE + 64, 256)
        });
        reject(&mut f.store, Error::Limit, |s| {
            s.enumerate_bounded(key, BUDGET, 1)
        });
        assert_eq!(
            f.store.enumerate_bounded(key, LARGE + 65, 2).unwrap().len(),
            2
        );
        reject(&mut f.store, Error::Limit, |s| {
            s.set(key, text(LARGE + 1, 7))
        });
        let saved = f.store.directory.insert(key, RawCell(vec![0xFF])).unwrap();
        reject(&mut f.store, Error::Layout, |s| s.get(key));
        f.store.directory.insert(key, saved);

        // Validate all supplied regions, including a fresh rejected partner, not just owner memories.
        let empty = CountedMemory::default();
        let mut partial = f.store.memories.clone();
        partial[1] = empty.clone();
        reject(&mut f.store, Error::Layout, |_| {
            Candidate::<K, L>::open(partial)
        });
        assert!(empty.inner.borrow().is_empty());
        assert_eq!(*empty.io.borrow(), Io::default());
        let mut alias = f.store.memories.clone();
        alias[1] = alias[0].clone();
        reject(&mut f.store, Error::Layout, |_| {
            Candidate::<K, L>::open(alias)
        });
        let metadata_key = K::fixture(0, 0);
        let original = f.store.directory.get(&metadata_key).unwrap();
        for byte in [0, 1, 2, 3, 5] {
            let mut wrong = original.clone();
            wrong.0[byte] ^= 0x80;
            f.store.directory.insert(metadata_key, wrong);
            let memories = f.store.memories.clone();
            reject(&mut f.store, Error::Layout, |_| {
                Candidate::<K, L>::open(memories)
            });
            f.store.directory.insert(metadata_key, original.clone());
        }
        f.store.directory.remove(&metadata_key);
        let memories = f.store.memories.clone();
        reject(&mut f.store, Error::Layout, |_| {
            Candidate::<K, L>::open(memories)
        });
        f.store.directory.insert(metadata_key, original);
        f.audit();
    }
    fn exact_candidate_limits<K: FixtureKey, L: BodyLane<K>>(f: &mut Fixture<K, Candidate<K, L>>) {
        let key = K::fixture(0, 1);
        let target = K::fixture(1, 1);
        let old = f.store.change(key, Some(text(65, 9)), LARGE + 65).unwrap();
        assert_eq!(
            old.map(encoded),
            f.expected.insert(key, encoded(text(65, 9)))
        );
        let removed = f.store.change(key, None, 65).unwrap();
        assert_eq!(removed.map(encoded), f.expected.remove(&key));
        let value = text(66, 11);
        assert_eq!(f.store.change(key, Some(value.clone()), 66).unwrap(), None);
        f.expected.insert(key, encoded(value));
        let moved = f.store.rekey_bounded(key, target, 66 + 97).unwrap();
        let expected = f.expected.remove(&key).unwrap();
        assert_eq!(moved.map(encoded), Some(expected.clone()));
        f.expected.insert(target, expected);
        f.audit();
    }
    fn chunk_rejections<K: FixtureKey, const P: usize>() {
        let mut f = common_rejections::<K, Candidate<K, chunks::Store<P, 0, K>>>();
        candidate_rejections(&mut f);
        let key = K::fixture(0, 1);
        let missing = f.store.lane.map.remove(&chunks::key(key, 1)).unwrap();
        reject(&mut f.store, Error::Limit, |s| {
            s.get_bounded(key, LARGE - 1)
        });
        reject(&mut f.store, Error::Chunks(chunks::Error::Sequence), |s| {
            s.set(key, text(65, 7))
        });
        f.store.lane.map.insert(chunks::key(key, 1), missing);
        let last = (LARGE / P - 1) as u32;
        let missing = f.store.lane.map.remove(&chunks::key(key, last)).unwrap();
        reject(&mut f.store, Error::Chunks(chunks::Error::Missing), |s| {
            s.remove(key)
        });
        f.store.lane.map.insert(chunks::key(key, last), missing);
        f.store
            .lane
            .map
            .insert(chunks::key(key, u32::MAX), Chunk(vec![5]));
        reject(&mut f.store, Error::Chunks(chunks::Error::Extra), |s| {
            s.rekey(key, K::fixture(1, 1))
        });
        f.store.lane.map.remove(&chunks::key(key, u32::MAX));
        let old = f
            .store
            .lane
            .map
            .insert(chunks::key(key, 1), Chunk(vec![9; P - 1]))
            .unwrap();
        reject(&mut f.store, Error::Chunks(chunks::Error::Width), |s| {
            s.set(key, text(65, 7))
        });
        f.store.lane.map.insert(chunks::key(key, 1), old);
        // Length-admitted bytes use the actual Graph codec, which traps on an invalid envelope.
        let target = K::fixture(1, 1);
        for selected in [key, target] {
            let position = chunks::key(selected, 0);
            let mut first = f.store.lane.map.remove(&position).unwrap();
            let version = first.0[0];
            first.0[0] = 0xFF;
            f.store.lane.map.insert(position, first);
            assert_decoder_trap(&mut f.store, |s| {
                if selected == key {
                    s.set(key, text(65, 8))
                } else {
                    s.rekey(key, target)
                }
            });
            let mut first = f.store.lane.map.remove(&position).unwrap();
            first.0[0] = version;
            f.store.lane.map.insert(position, first);
        }
        exact_candidate_limits(&mut f);
        f = f.reopen();
        f.audit();
    }
    fn assert_decoder_trap<K: FixtureKey, S: ProbeStore<K>>(
        s: &mut S,
        operation: impl FnOnce(&mut S) -> Result<Option<Value>>,
    ) {
        let before: Vec<_> = s
            .memories()
            .iter()
            .map(|m| m.inner.borrow().clone())
            .collect();
        for m in s.memories() {
            *m.io.borrow_mut() = Io::default();
        }
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(s)))
            .expect_err("invalid stored envelope must trap");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(
            message.contains("unknown stored property value layout version"),
            "{message}"
        );
        for (m, bytes) in s.memories().iter().zip(before) {
            assert_eq!(*m.inner.borrow(), bytes);
            assert_eq!((m.io.borrow().writes, m.io.borrow().grows), (0, 0));
        }
    }
    fn extent_rejections<K: FixtureKey>() {
        let mut f = common_rejections::<K, Candidate<K, Arena>>();
        candidate_rejections(&mut f);
        let key = K::fixture(0, 1);
        let raw = f.store.directory.get(&key).unwrap();
        let r = f
            .store
            .cell(key)
            .unwrap()
            .unwrap()
            .external()
            .unwrap()
            .extent()
            .unwrap();
        for offset in [0, r.offset + 1, u64::MAX] {
            f.store.directory.insert(
                key,
                Cell::External(Reference::Extent(BodyAddress { offset, ..r })).encode(),
            );
            reject(&mut f.store, Error::Extent(extents::Error::Span), |s| {
                s.set(key, text(65, 7))
            });
        }
        f.store.directory.insert(key, raw.clone());
        let target = K::fixture(1, 1);
        let saved_target = f.store.directory.insert(target, raw).unwrap();
        reject(&mut f.store, Error::Layout, |s| s.rekey(key, target));
        f.store.directory.insert(target, saved_target);
        let memory = f.store.memories[1].clone();
        for selected in [key, target] {
            let address = f.address(selected).unwrap();
            let mut version = [0];
            memory.read(address.offset + 8, &mut version);
            memory.write(address.offset + 8, &[0xFF]);
            reject(&mut f.store, Error::Limit, |s| {
                s.get_bounded(key, LARGE - 1)
            });
            assert_decoder_trap(&mut f.store, |s| {
                if selected == key {
                    s.set(key, text(65, 7))
                } else {
                    s.rekey(key, target)
                }
            });
            memory.write(address.offset + 8, &version);
        }
        exact_candidate_limits(&mut f);
        f = f.reopen();
        f.audit();
    }
    fn rejection_matrix<K: FixtureKey>() {
        let direct = common_rejections::<K, Direct<K>>().reopen();
        direct.audit();
        chunk_rejections::<K, 4096>();
        chunk_rejections::<K, 16384>();
        chunk_rejections::<K, 65536>();
        extent_rejections::<K>();
    }
    fn prepared_changes<K: FixtureKey, L: BodyLane<K>>() {
        let mut f = Fixture::<K, Candidate<K, L>>::new();
        let key = K::fixture(0, 1);
        let guard = K::fixture(0, 2);
        let guard_value = text(97, 11);
        f.store.set(guard, guard_value.clone()).unwrap();
        f.expected.insert(guard, encoded(guard_value));
        let mut intents = Vec::new(); // Local caller model, not Graph's durable outbox.
        for value in [
            Some(Value::Null),
            Some(text(65, 1)),
            Some(text(66, 2)),
            Some(text(LARGE, 3)),
            Some(text(65, 4)),
            Some(Value::Null),
            None,
            None,
            Some(text(66, 5)),
            None,
        ] {
            let old = f.expected.get(&key).cloned();
            let next = value.clone().map(encoded);
            let budget = old.as_ref().map_or(0, Vec::len) + next.as_ref().map_or(0, Vec::len);
            if budget > 0 {
                reject(&mut f.store, Error::Limit, |s| {
                    s.with_prepared(key, value.clone(), budget - 1, |_| {
                        panic!("over-budget preparation reached caller")
                    })
                });
                for m in &f.store.memories[1..] {
                    assert_eq!(*m.io.borrow(), Io::default());
                }
            }
            let mut calls = 0;
            reject(&mut f.store, Error::Layout, |s| {
                s.with_prepared(key, value.clone(), budget, |p| {
                    calls += 1;
                    assert_eq!(p.previous().cloned().map(encoded), old);
                    // A pure caller rejects; dropping preparation must not publish or retire.
                    Err::<(), _>(Error::Layout)
                })?
            });
            assert_eq!(calls, 1);
            let read = f.measure(|s| s.get(key));
            assert_eq!(read.result.unwrap().map(encoded), old);
            let canceled = f.measure(|s| {
                s.with_prepared(key, value.clone(), budget, |p| {
                    assert_eq!(p.previous().cloned().map(encoded), old);
                    drop(p);
                })
                .unwrap()
            });
            for io in &canceled.io {
                assert_eq!((io.writes, io.grows), (0, 0));
            }
            assert_eq!(
                canceled.io[1].body_buffer_bytes,
                old.as_ref().filter(|b| b.len() > SMALL).map_or(0, Vec::len)
            );
            if old.as_ref().is_none_or(|b| b.len() <= SMALL)
                && next.as_ref().is_none_or(|b| b.len() <= SMALL)
            {
                assert_eq!(
                    canceled.io, read.io,
                    "preparation duplicated the directory acquisition"
                );
            }
            let memories = f.store.memories.clone();
            let before: Vec<_> = memories.iter().map(|m| m.inner.borrow().clone()).collect();
            let actual = f.measure(|s| {
                s.with_prepared(key, value, budget, |p| {
                    assert_eq!(p.previous().cloned().map(encoded), old);
                    // Read-only fixture aliases observe the fence; they never mutate prepared state.
                    for (m, bytes) in memories.iter().zip(&before) {
                        assert_eq!(&*m.inner.borrow(), bytes);
                        assert_eq!((m.io.borrow().writes, m.io.borrow().grows), (0, 0));
                    }
                    intents.push(old.clone());
                    p.commit()
                })
            });
            assert_eq!(actual.result.unwrap().map(encoded), old);
            assert_eq!(intents.last(), Some(&old));
            assert_eq!(
                actual.io[1].body_buffer_bytes,
                canceled.io[1].body_buffer_bytes
            );
            if next.is_none() && old.is_none() {
                assert!(actual.io.iter().all(|io| io.writes == 0 && io.grows == 0));
            }
            match next {
                Some(bytes) => {
                    f.expected.insert(key, bytes);
                }
                None => {
                    f.expected.remove(&key);
                }
            }
            f.audit();
        }
        assert_eq!(intents.len(), 10);
        assert_eq!(
            f.store
                .with_prepared(K::fixture(0, 0), None, 0, |p| p.commit())
                .unwrap(),
            None
        );
        f.store.directory.insert(key, RawCell(vec![0x31, 0xFF, 0]));
        assert_decoder_trap(&mut f.store, |s| {
            s.with_prepared(key, Some(Value::Null), BUDGET, |_| {
                panic!("malformed old value reached caller")
            })
        });
        f.store.directory.remove(&key);
        f.audit();
    }
    #[test]
    fn property_store_prepared_change_shares_acquisition_and_cancels_without_effects() {
        prepared_changes::<VertexPropertyKey, chunks::Store<4096, 0, VertexPropertyKey>>();
        prepared_changes::<VertexPropertyKey, chunks::Store<16384, 0, VertexPropertyKey>>();
        prepared_changes::<VertexPropertyKey, chunks::Store<65536, 0, VertexPropertyKey>>();
        prepared_changes::<VertexPropertyKey, Arena>();
        prepared_changes::<EdgePropertyKey, chunks::Store<4096, 0, EdgePropertyKey>>();
        prepared_changes::<EdgePropertyKey, chunks::Store<16384, 0, EdgePropertyKey>>();
        prepared_changes::<EdgePropertyKey, chunks::Store<65536, 0, EdgePropertyKey>>();
        prepared_changes::<EdgePropertyKey, Arena>();
    }

    #[test]
    fn property_store_rejections_and_reopen_metadata() {
        rejection_matrix::<VertexPropertyKey>();
        rejection_matrix::<EdgePropertyKey>();
    }
}
