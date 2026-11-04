// Shared native/managed-IC allocator; caller descriptors are not a persistent property directory.
use super::{
    BUDGET, BodyMemory, LARGE, Memory, Prefix, StableBTreeMap, fixture_prefix as id, payload,
};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use {
    super::{CountedMemory, Io},
    std::rc::Rc,
};

const HEADER: u64 = 64;
const PAGE: u64 = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BodyAddress {
    pub(crate) offset: u64,
    pub(crate) len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Free,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    offset: u64,
    total: u64,
    state: State,
}

impl Span {
    fn end(self) -> u64 {
        self.offset + self.total
    }
    fn key(self) -> [u8; 16] {
        free_key(self.total, self.offset)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Pair,
    Header,
    Length,
    Budget,
    Span,
    FreeIndex,
    Overflow,
    Allocation,
}
type Result<T> = std::result::Result<T, Error>;

fn free_key(total: u64, offset: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&total.to_be_bytes());
    key[8..].copy_from_slice(&offset.to_be_bytes());
    key
}

fn admit(old: usize, new: usize, budget: usize) -> Result<()> {
    for len in [old, new] {
        len.checked_add(1)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(Error::Length)?;
    }
    if old.checked_add(new).ok_or(Error::Overflow)? > budget {
        return Err(Error::Budget);
    }
    Ok(())
}

fn need(len: usize) -> u64 {
    ((len as u64 + 23) & !7).max(32)
}

pub(crate) struct Arena {
    pub(crate) memory: BodyMemory,
    index: BodyMemory,
    free: StableBTreeMap<[u8; 16], (), BodyMemory>,
}

pub(crate) struct Prepared<'a> {
    arena: &'a mut Arena,
    plan: SpanPlan,
    live: Option<Span>,
    bytes: &'a [u8],
    previous: Vec<u8>,
}

impl Prepared<'_> {
    pub(crate) fn previous(&self) -> &[u8] {
        &self.previous
    }
    pub(crate) fn commit(self, publish: impl FnOnce(Option<BodyAddress>)) -> Vec<u8> {
        self.arena.commit(self.plan, self.live, publish, self.bytes);
        self.previous
    }
}

// Bounded physical-span edits, not an arbitrary memory overlay or a second persistent index.
struct SpanPlan {
    base_end: u64,
    end: u64,
    before: Vec<Span>,
    after: Vec<Span>,
}

impl SpanPlan {
    fn new(end: u64) -> Self {
        Self {
            base_end: end,
            end,
            before: Vec::with_capacity(4),
            after: Vec::with_capacity(3),
        }
    }

    fn at(&self, arena: &Arena, offset: u64) -> Result<Span> {
        if let Some(span) = self.after.iter().find(|s| s.offset == offset) {
            return Ok(*span);
        }
        if self
            .before
            .iter()
            .any(|s| (s.offset..s.end()).contains(&offset))
        {
            return Err(Error::Span);
        }
        arena.span(offset, self.base_end)
    }

    fn left(&self, arena: &Arena, offset: u64) -> Result<Option<Span>> {
        if offset == HEADER {
            return Ok(None);
        }
        if let Some(span) = self.after.iter().find(|s| s.end() == offset) {
            return Ok(Some(*span));
        }
        if offset < HEADER + 32
            || offset > self.end
            || self
                .before
                .iter()
                .any(|s| (s.offset..s.end()).contains(&(offset - 8)))
        {
            return Err(Error::Span);
        }
        let total = arena.word(offset - 8);
        let at = offset.checked_sub(total).ok_or(Error::Span)?;
        let span = self.at(arena, at)?;
        if span.end() != offset {
            return Err(Error::Span);
        }
        Ok(Some(span))
    }

    fn right(&self, arena: &Arena, offset: u64) -> Result<Option<Span>> {
        if offset == self.end {
            Ok(None)
        } else {
            self.at(arena, offset).map(Some)
        }
    }

    fn consume(&mut self, arena: &Arena, span: Span) -> Result<()> {
        if let Some(i) = self.after.iter().position(|s| *s == span) {
            self.after.remove(i);
        } else {
            if self.before.iter().any(|s| s.offset == span.offset) || self.before.len() == 4 {
                return Err(Error::Span);
            }
            if arena.free.contains_key(&span.key()) != (span.state == State::Free) {
                return Err(Error::FreeIndex);
            }
            self.before.push(span);
        }
        Ok(())
    }

    fn split(&mut self, offset: u64, total: u64, required: u64) -> Span {
        assert!(total >= required);
        let used = if total - required >= 32 {
            required
        } else {
            total
        };
        let live = Span {
            offset,
            total: used,
            state: State::Live,
        };
        self.after.push(live);
        if used != total {
            self.after.push(Span {
                offset: offset + used,
                total: total - used,
                state: State::Free,
            });
        }
        live
    }

    fn allocate(&mut self, arena: &Arena, required: u64) -> Result<Span> {
        let fit = arena
            .free
            .range(free_key(required, 0)..)
            .next()
            .map(|e| *e.key());
        if let Some(key) = fit {
            let offset = u64::from_be_bytes(key[8..].try_into().unwrap());
            let span = self.at(arena, offset)?;
            if span.state != State::Free || span.key() != key || span.total < required {
                return Err(Error::FreeIndex);
            }
            self.consume(arena, span)?;
            return Ok(self.split(span.offset, span.total, required));
        }
        let last = self.left(arena, self.end)?;
        let offset = if let Some(span) = last.filter(|s| s.state == State::Free) {
            // A fitting trailing span missing from the index is corruption, not an append opportunity.
            if span.total >= required {
                return Err(Error::FreeIndex);
            }
            self.consume(arena, span)?;
            span.offset
        } else {
            self.end
        };
        self.end = offset.checked_add(required).ok_or(Error::Overflow)?;
        Ok(self.split(offset, required, required))
    }

    fn resize(&mut self, arena: &Arena, old: Span, required: u64) -> Result<Option<Span>> {
        let right = self
            .right(arena, old.end())?
            .filter(|s| s.state == State::Free);
        let use_right = required > old.total || old.total - required >= 32;
        let right = right.filter(|_| use_right);
        let total = old
            .total
            .checked_add(right.map_or(0, |s| s.total))
            .ok_or(Error::Overflow)?;
        let end = old.offset.checked_add(total).ok_or(Error::Overflow)?;
        if total < required && end != self.end {
            return Ok(None);
        }
        self.consume(arena, old)?;
        if let Some(span) = right {
            self.consume(arena, span)?;
        }
        let total = if total < required {
            self.end = old.offset.checked_add(required).ok_or(Error::Overflow)?;
            required
        } else {
            total
        };
        Ok(Some(self.split(old.offset, total, required)))
    }

    fn release(&mut self, arena: &Arena, old: Span) -> Result<()> {
        // These neighbors are resolved AFTER allocation, including an allocated old-left span.
        let left = self
            .left(arena, old.offset)?
            .filter(|s| s.state == State::Free);
        let right = self
            .right(arena, old.end())?
            .filter(|s| s.state == State::Free);
        self.consume(arena, old)?;
        let mut total = old.total;
        for span in [left, right].into_iter().flatten() {
            self.consume(arena, span)?;
            total = total.checked_add(span.total).ok_or(Error::Overflow)?;
        }
        self.after.push(Span {
            offset: left.map_or(old.offset, |s| s.offset),
            total,
            state: State::Free,
        });
        Ok(())
    }

    fn validate(&self, arena: &Arena) -> Result<()> {
        if self.end < self.base_end
            || self.end.checked_add(PAGE - 1).is_none()
            || self.before.len() > 4
            || self.after.len() > 3
            || self
                .before
                .iter()
                .filter(|s| s.state == State::Free)
                .count()
                > 3
            || self.after.iter().filter(|s| s.state == State::Free).count() > 2
        {
            return Err(Error::Span);
        }
        let coverage = |mut ranges: Vec<(u64, u64)>| -> Result<Vec<(u64, u64)>> {
            ranges.sort_unstable();
            let mut result: Vec<(u64, u64)> = Vec::new();
            for (start, end) in ranges {
                if start < HEADER
                    || !start.is_multiple_of(8)
                    || !end.is_multiple_of(8)
                    || end <= start
                {
                    return Err(Error::Span);
                }
                if let Some(last) = result.last_mut() {
                    if start < last.1 {
                        return Err(Error::Span);
                    }
                    if start == last.1 {
                        last.1 = end;
                        continue;
                    }
                }
                result.push((start, end));
            }
            Ok(result)
        };
        let mut before: Vec<_> = self.before.iter().map(|s| (s.offset, s.end())).collect();
        if self.end > self.base_end {
            before.push((self.base_end, self.end));
        }
        let after = self.after.iter().map(|s| (s.offset, s.end())).collect();
        if coverage(before)? != coverage(after)? {
            return Err(Error::Span);
        }
        for span in self.after.iter().filter(|s| s.state == State::Free) {
            if !self.before.contains(span) && arena.free.contains_key(&span.key()) {
                return Err(Error::FreeIndex);
            }
        }
        Ok(())
    }
}

impl Arena {
    pub(crate) fn open(memory: BodyMemory, index: BodyMemory) -> Result<Self> {
        #[cfg(test)]
        if Rc::ptr_eq(&memory.inner, &index.inner) {
            return Err(Error::Pair);
        }
        // Managed fixture construction owns distinct region IDs before entering this pair boundary.
        if (memory.size() == 0) != (index.size() == 0) {
            return Err(Error::Pair);
        }
        if memory.size() == 0 {
            assert!(memory.grow(1) >= 0, "arena creation growth failed");
            let mut header = [0; HEADER as usize];
            header[..4].copy_from_slice(b"PVE\x01");
            header[8..16].copy_from_slice(&HEADER.to_le_bytes());
            memory.write(0, &header);
            Ok(Self {
                memory,
                free: StableBTreeMap::new(index.clone()),
                index,
            })
        } else {
            Self::header(&memory)?;
            Ok(Self {
                memory,
                free: StableBTreeMap::load(index.clone()),
                index,
            })
        }
    }

    pub(crate) fn header(memory: &BodyMemory) -> Result<u64> {
        let mut bytes = [0; HEADER as usize];
        if memory.size() == 0 {
            return Err(Error::Header);
        }
        memory.read(0, &mut bytes);
        let end = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        if &bytes[..4] != b"PVE\x01"
            || bytes[4..8].iter().chain(&bytes[16..]).any(|b| *b != 0)
            || end < HEADER
            || (end != HEADER && end < HEADER + 32)
            || !end.is_multiple_of(8)
            || end > memory.size().checked_mul(PAGE).ok_or(Error::Header)?
        {
            return Err(Error::Header);
        }
        Ok(end)
    }

    fn word(&self, offset: u64) -> u64 {
        let mut bytes = [0; 8];
        self.memory.read(offset, &mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn span(&self, offset: u64, end: u64) -> Result<Span> {
        if offset < HEADER
            || !offset.is_multiple_of(8)
            || offset.checked_add(32).is_none_or(|n| n > end)
        {
            return Err(Error::Span);
        }
        let tag = self.word(offset);
        let state = match tag & 7 {
            0 => State::Free,
            1 => State::Live,
            _ => return Err(Error::Span),
        };
        let total = tag & !7;
        let limit = offset.checked_add(total).ok_or(Error::Span)?;
        if total < 32 || limit > end || self.word(limit - 8) != total {
            return Err(Error::Span);
        }
        Ok(Span {
            offset,
            total,
            state,
        })
    }

    fn load(&self, reference: BodyAddress, end: u64) -> Result<(Span, Vec<u8>)> {
        let span = self.span(reference.offset, end)?;
        if span.state != State::Live || reference.len as u64 > span.total - 16 {
            return Err(Error::Span);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(reference.len as usize)
            .map_err(|_| Error::Allocation)?;
        bytes.resize(reference.len as usize, 0);
        #[cfg(test)]
        {
            self.memory.io.borrow_mut().body_buffer_bytes += bytes.len();
        }
        self.memory.read(reference.offset + 8, &mut bytes);
        Ok((span, bytes))
    }

    pub(crate) fn read(&self, reference: BodyAddress, budget: usize) -> Result<Vec<u8>> {
        if reference.len == 0 {
            return Err(Error::Length);
        }
        admit(reference.len as usize, 0, budget)?;
        self.load(reference, Self::header(&self.memory)?)
            .map(|(_, bytes)| bytes)
    }

    fn change(
        &mut self,
        reference: &mut Option<BodyAddress>,
        bytes: &[u8],
        budget: usize,
    ) -> Result<Vec<u8>> {
        Ok(self
            .prepare(*reference, bytes, budget)?
            .commit(|next| *reference = next))
    }

    pub(crate) fn prepare<'a>(
        &'a mut self,
        reference: Option<BodyAddress>,
        bytes: &'a [u8],
        budget: usize,
    ) -> Result<Prepared<'a>> {
        if reference.is_none() && bytes.is_empty() || reference.is_some_and(|r| r.len == 0) {
            return Err(Error::Length);
        }
        admit(reference.map_or(0, |r| r.len as usize), bytes.len(), budget)?;
        let end = Self::header(&self.memory)?;
        let (old, previous) = match reference {
            Some(r) => {
                let (span, value) = self.load(r, end)?;
                (Some(span), value)
            }
            None => (None, Vec::new()),
        };
        let mut plan = SpanPlan::new(end);
        let live = if bytes.is_empty() {
            plan.release(self, old.unwrap())?;
            None
        } else if let Some(old) = old {
            match plan.resize(self, old, need(bytes.len()))? {
                Some(span) => Some(span),
                None => {
                    let span = plan.allocate(self, need(bytes.len()))?;
                    plan.release(self, old)?;
                    Some(span)
                }
            }
        } else {
            Some(plan.allocate(self, need(bytes.len()))?)
        };
        plan.validate(self)?;
        if !plan
            .after
            .iter()
            .filter(|s| s.state == State::Live)
            .copied()
            .eq(live)
        {
            return Err(Error::Span);
        }
        Ok(Prepared {
            arena: self,
            plan,
            live,
            bytes,
            previous,
        })
    }

    fn write_span(&self, span: Span) {
        let tag = span.total | u64::from(span.state == State::Live);
        self.memory.write(span.offset, &tag.to_le_bytes());
        self.memory.write(span.end() - 8, &span.total.to_le_bytes());
    }

    fn commit(
        &mut self,
        plan: SpanPlan,
        live: Option<Span>,
        publish: impl FnOnce(Option<BodyAddress>),
        bytes: &[u8],
    ) {
        // First grow, index or arena write starts commit. Remaining failures panic, not Err.
        let pages = plan.end.div_ceil(PAGE);
        if pages > self.memory.size() {
            assert!(
                self.memory.grow(pages - self.memory.size()) >= 0,
                "arena commit growth failed"
            );
        }
        for span in plan.before.iter().filter(|s| s.state == State::Free) {
            assert!(self.free.remove(&span.key()).is_some());
        }
        if let Some(span) = live {
            self.write_span(span);
            self.memory.write(span.offset + 8, bytes);
        }
        publish(live.map(|s| BodyAddress {
            offset: s.offset,
            len: bytes.len() as u32,
        }));
        // The caller publishes its descriptor/directory before old storage is retired.
        for span in plan.after.iter().filter(|s| s.state == State::Free) {
            self.write_span(*span);
            assert!(self.free.insert(span.key(), ()).is_none());
        }
        if plan.end != plan.base_end {
            self.memory.write(8, &plan.end.to_le_bytes());
        }
    }

    fn reopen(self) -> Self {
        Self::open(self.memory, self.free.into_memory()).unwrap()
    }

    // Full walk is a fixture oracle, never allocator lookup or reopen behavior.
    pub(crate) fn audit<'a>(
        &self,
        owners: impl IntoIterator<Item = (BodyAddress, &'a [u8])>,
    ) -> (u64, u64, usize) {
        let end = Arena::header(&self.memory).unwrap();
        let mut at = HEADER;
        let mut free_keys = BTreeSet::new();
        let mut live_spans = BTreeMap::new();
        let (mut live_bytes, mut free_bytes) = (0, 0);
        let mut previous_free = false;
        while at < end {
            let span = self.span(at, end).unwrap();
            match span.state {
                State::Free => {
                    assert!(!previous_free, "adjacent free spans");
                    assert!(free_keys.insert(span.key()));
                    free_bytes += span.total;
                }
                State::Live => {
                    live_spans.insert(at, span);
                    live_bytes += span.total;
                }
            }
            previous_free = span.state == State::Free;
            at = span.end();
        }
        assert_eq!(at, end);
        assert_eq!(live_bytes + free_bytes, end - HEADER);
        assert_eq!(free_keys, self.free.iter().map(|e| *e.key()).collect());
        let mut owned = BTreeSet::new();
        for (reference, bytes) in owners {
            assert!(owned.insert(reference.offset), "duplicate body ownership");
            assert!(live_spans.contains_key(&reference.offset));
            assert_eq!(reference.len as usize, bytes.len());
            assert_eq!(self.read(reference, BUDGET).unwrap(), bytes);
        }
        assert_eq!(live_spans.len(), owned.len());
        (live_bytes, free_bytes, free_keys.len())
    }
}

struct Scenario {
    arena: Arena,
    owners: BTreeMap<Prefix, (BodyAddress, Vec<u8>)>,
}

impl Scenario {
    fn new() -> Self {
        #[cfg(test)]
        let arena = Arena::open(CountedMemory::default(), CountedMemory::default()).unwrap();
        #[cfg(not(test))]
        let arena = {
            use ic_stable_structures::memory_manager::MemoryId;
            let manager = super::managed_memories();
            Arena::open(manager.get(MemoryId::new(0)), manager.get(MemoryId::new(1))).unwrap()
        };
        Self {
            arena,
            owners: BTreeMap::new(),
        }
    }

    fn set(&mut self, key: Prefix, bytes: &[u8]) -> Option<BodyAddress> {
        let mut reference = self.owners.get(&key).map(|(r, _)| *r);
        let previous = self.arena.change(&mut reference, bytes, BUDGET).unwrap();
        assert_eq!(
            previous.as_slice(),
            self.owners.get(&key).map_or(&[][..], |(_, b)| b.as_slice())
        );
        if let Some(r) = reference {
            self.owners.insert(key, (r, bytes.to_vec()));
        } else {
            self.owners.remove(&key);
        }
        reference
    }

    fn audit(&self) -> (u64, u64, usize) {
        self.arena
            .audit(self.owners.values().map(|(r, b)| (*r, b.as_slice())))
    }

    fn reopen(self) -> Self {
        #[cfg(test)]
        {
            *self.arena.memory.io.borrow_mut() = Io::default();
            *self.arena.index.io.borrow_mut() = Io::default();
        }
        let reopened = Self {
            arena: self.arena.reopen(),
            ..self
        };
        #[cfg(test)]
        for memory in [&reopened.arena.memory, &reopened.arena.index] {
            assert_eq!(memory.io.borrow().writes, 0);
            assert_eq!(memory.io.borrow().grows, 0);
        }
        reopened.audit();
        reopened
    }

    #[cfg(test)]
    fn measure(&mut self, case: &str, input: usize, operation: impl FnOnce(&mut Self)) -> (Io, Io) {
        self.audit();
        let before_arena = self.arena.memory.size() * PAGE;
        let before_index = self.arena.index.size() * PAGE;
        *self.arena.memory.io.borrow_mut() = Io::default();
        *self.arena.index.io.borrow_mut() = Io::default();
        operation(self);
        let arena = *self.arena.memory.io.borrow();
        let index = *self.arena.index.io.borrow();
        let end = Arena::header(&self.arena.memory).unwrap();
        let (live, free, count) = self.audit();
        println!(
            "EXTENT,{case},{input},{},{},{},{},{},{},{},{},{},{},{before_arena},{},{before_index},{},{end},{live},{free},{count},{}",
            arena.reads,
            arena.read_bytes,
            arena.writes,
            arena.write_bytes,
            arena.grows,
            index.reads,
            index.read_bytes,
            index.writes,
            index.write_bytes,
            index.grows,
            self.arena.memory.size() * PAGE,
            self.arena.index.size() * PAGE,
            arena.body_buffer_bytes
        );
        (arena, index)
    }
}

#[cfg(test)]
fn reject(
    arena: &mut Arena,
    reference: &mut Option<BodyAddress>,
    expected: Error,
    action: impl FnOnce(&mut Arena, &mut Option<BodyAddress>) -> Result<Vec<u8>>,
) {
    let before = arena.memory.inner.borrow().clone();
    let index = arena.index.inner.borrow().clone();
    let old = *reference;
    *arena.memory.io.borrow_mut() = Io::default();
    *arena.index.io.borrow_mut() = Io::default();
    assert_eq!(action(arena, reference), Err(expected));
    assert_eq!(*reference, old);
    assert_eq!(*arena.memory.inner.borrow(), before);
    assert_eq!(*arena.index.inner.borrow(), index);
    for memory in [&arena.memory, &arena.index] {
        assert_eq!(memory.io.borrow().writes, 0);
        assert_eq!(memory.io.borrow().grows, 0);
    }
}

#[test]
fn extent_lifecycle_preserves_partition_and_reuse() {
    let mut s = Scenario::new();
    let a = s.set(id(1), &payload(65, 11)).unwrap();
    let b = s.set(id(2), &payload(65, 37)).unwrap();
    let c = s.set(id(3), &payload(65, 77)).unwrap();
    assert_eq!((a.offset, b.offset, c.offset), (64, 152, 240));
    let bytes = payload(65, 193);
    let prepared = s.arena.prepare(Some(a), &bytes, 130).unwrap();
    assert_eq!(prepared.previous(), payload(65, 11));
    let mut published = None;
    assert_eq!(prepared.commit(|r| published = r), payload(65, 11));
    assert_eq!(published.unwrap().offset, a.offset);
    s.owners.insert(id(1), (published.unwrap(), bytes));
    assert_eq!(s.set(id(1), &payload(33, 9)).unwrap().offset, a.offset);
    assert_eq!(s.audit(), (232, 32, 1));
    assert_eq!(s.set(id(1), &payload(43, 10)).unwrap().offset, a.offset); // absorb24 B remainder
    assert_eq!(s.audit(), (264, 0, 0));
    s.set(id(2), &[]);
    assert_eq!(s.set(id(1), &payload(100, 12)).unwrap().offset, a.offset);
    assert_eq!(s.audit(), (208, 56, 1));
    assert_eq!(s.set(id(3), &payload(73, 14)).unwrap().offset, c.offset);
    assert_eq!(Arena::header(&s.arena.memory).unwrap(), 336); // eight-byte frontier extension
    assert_eq!(s.set(id(3), &payload(129, 15)).unwrap().offset, c.offset); // frontier shortfall
    assert_eq!(Arena::header(&s.arena.memory).unwrap(), 392);
    s.set(id(1), &[]);
    s.set(id(3), &[]);
    assert_eq!(s.audit(), (0, 328, 1));
    s = s.reopen();
    assert_eq!(s.set(id(900), &payload(500, 21)).unwrap().offset, 64);
    assert_eq!(Arena::header(&s.arena.memory).unwrap(), 584); // extend trailing Free, not append
    s.set(id(900), &payload(65, 22));
    assert_eq!(s.set(id(900), &payload(600, 23)).unwrap().offset, 64);
    assert_eq!(Arena::header(&s.arena.memory).unwrap(), 680); // old + insufficient trailing Free
    s.set(id(900), &[]);
    // Deterministic mixed operations; every step compares the complete partition/index/owner set.
    let mut seed = 19u32;
    for i in 0..256 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let key = id(seed % 16);
        let mut size = [0, 17, 33, 65, 73, 129, 4096][(seed >> 8) as usize % 7];
        if size == 0 && !s.owners.contains_key(&key) {
            size = 17;
        }
        s.set(key, &payload(size, i as u8));
        s.audit();
    }
    for key in s.owners.keys().copied().collect::<Vec<_>>() {
        s.set(key, &[]);
    }
    for i in 100..104 {
        s.set(id(i), &payload(65, i as u8));
    }
    s.set(id(100), &[]);
    s.set(id(102), &[]);
    assert_eq!(s.set(id(104), &payload(65, 17)).unwrap().offset, 64); // exact-fit lowest-offset tie
    s.audit();
}

#[test]
fn extent_relocation_uses_post_allocation_neighbors() {
    for size in [160, 320] {
        let mut s = Scenario::new();
        s.set(id(1), &payload(320, 11));
        let old = s.set(id(2), &payload(65, 37)).unwrap();
        s.set(id(3), &payload(33, 51));
        let guard = s.set(id(4), &payload(65, 77)).unwrap();
        assert_eq!((old.offset, guard.offset), (400, 544));
        s.set(id(1), &[]);
        s.set(id(3), &[]);
        let end = Arena::header(&s.arena.memory).unwrap();
        let new = s.set(id(2), &payload(size, 193)).unwrap();
        assert_eq!(new.offset, 64);
        assert_eq!(Arena::header(&s.arena.memory).unwrap(), end);
        let start = 64 + need(size);
        assert_eq!(
            s.arena.free.iter().map(|e| *e.key()).collect::<Vec<_>>(),
            vec![free_key(544 - start, start)]
        );
        assert_eq!(s.arena.read(guard, 65).unwrap(), payload(65, 77));
        s.audit();
        let large = s.set(id(2), &payload(65536, 194)).unwrap();
        // Larger than the available holes, so this guard must append after the selected body.
        let blocker = s.set(id(5), &payload(1024, 91)).unwrap();
        assert_eq!(blocker.offset, large.offset + need(65536));
        let before = Arena::header(&s.arena.memory).unwrap();
        assert_eq!(s.set(id(2), &payload(LARGE, 195)).unwrap().offset, before);
        assert_eq!(
            Arena::header(&s.arena.memory).unwrap(),
            before + need(LARGE)
        );
        assert_eq!(s.arena.read(guard, 65).unwrap(), payload(65, 77));
        s = s.reopen();
        s.audit();
    }
}

#[test]
fn extent_rejects_before_effects_and_reopens_without_rebuild() {
    let mut s = Scenario::new();
    let original = s.set(id(1), &payload(65, 11)).unwrap();
    let guard = s.set(id(2), &payload(65, 77)).unwrap();
    let mut reference = Some(original);
    let index_before = s.arena.index.inner.borrow().clone();
    *s.arena.index.io.borrow_mut() = Io::default();
    reject(&mut s.arena, &mut reference, Error::Budget, |a, r| {
        a.read(r.unwrap(), 64)
    });
    assert_eq!(*s.arena.memory.io.borrow(), Io::default());
    assert_eq!(*s.arena.index.io.borrow(), Io::default());
    reject(&mut s.arena, &mut reference, Error::Budget, |a, r| {
        a.change(r, &[], 64)
    });
    assert_eq!(*s.arena.memory.io.borrow(), Io::default());
    reject(&mut s.arena, &mut reference, Error::Budget, |a, r| {
        a.change(r, &payload(65, 193), 129)
    });
    assert_eq!(*s.arena.memory.io.borrow(), Io::default());
    assert_eq!(s.arena.read(original, 65).unwrap(), payload(65, 11));
    let replacement = payload(65, 193);
    assert_eq!(
        s.arena.change(&mut reference, &replacement, 130).unwrap(),
        payload(65, 11)
    );
    s.owners.get_mut(&id(1)).unwrap().1 = replacement;
    let saved = reference;
    reference = Some(BodyAddress {
        len: u32::MAX,
        ..original
    });
    reject(&mut s.arena, &mut reference, Error::Length, |a, r| {
        a.read(r.unwrap(), usize::MAX)
    });
    for (bad, error) in [
        (BodyAddress { len: 0, ..original }, Error::Length),
        (
            BodyAddress {
                len: 73,
                ..original
            },
            Error::Span,
        ),
        (
            BodyAddress {
                offset: 0,
                ..original
            },
            Error::Span,
        ),
        (
            BodyAddress {
                offset: 65,
                ..original
            },
            Error::Span,
        ),
        (
            BodyAddress {
                offset: u64::MAX,
                ..original
            },
            Error::Span,
        ),
    ] {
        reference = Some(bad);
        reject(&mut s.arena, &mut reference, error, |a, r| {
            a.read(r.unwrap(), BUDGET)
        });
    }
    reference = saved;
    let header = s.arena.memory.inner.borrow()[..64].to_vec();
    for (offset, bad) in [
        (3, vec![2]),
        (4, vec![1]),
        (16, vec![1]),
        (8, 65u64.to_le_bytes().to_vec()),
        (8, 72u64.to_le_bytes().to_vec()),
        (8, u64::MAX.to_le_bytes().to_vec()),
    ] {
        s.arena.memory.write(offset, &bad);
        reject(&mut s.arena, &mut reference, Error::Header, |a, r| {
            a.change(r, &[1], BUDGET)
        });
        s.arena.memory.write(0, &header);
    }
    s.arena.memory.write(0, b"BAD");
    reject(&mut s.arena, &mut reference, Error::Budget, |a, r| {
        a.read(r.unwrap(), 64)
    });
    reject(&mut s.arena, &mut reference, Error::Header, |a, r| {
        a.change(r, &[1], BUDGET)
    });
    assert!(matches!(
        Arena::open(s.arena.memory.clone(), s.arena.index.clone()),
        Err(Error::Header)
    ));
    s.arena.memory.write(0, b"PVE");
    let span = s
        .arena
        .span(original.offset, Arena::header(&s.arena.memory).unwrap())
        .unwrap();
    for tag in [span.total | 2, 16 | 1, !7u64 | 1] {
        s.arena.memory.write(span.offset, &tag.to_le_bytes());
        reject(&mut s.arena, &mut reference, Error::Span, |a, r| {
            a.change(r, &[1], BUDGET)
        });
    }
    s.arena.write_span(span);
    s.arena
        .memory
        .write(span.end() - 8, &(span.total + 8).to_le_bytes());
    reject(&mut s.arena, &mut reference, Error::Span, |a, r| {
        a.change(r, &[1], BUDGET)
    });
    s.arena.write_span(span);
    assert_eq!(*s.arena.index.inner.borrow(), index_before);
    s = s.reopen();
    s.set(id(1), &[]);
    reject(&mut s.arena, &mut reference, Error::Span, |a, r| {
        a.change(r, &[], BUDGET)
    }); // double release
    let free = Span {
        state: State::Free,
        ..span
    };
    s.arena.free.remove(&free.key());
    let index_before = s.arena.index.inner.borrow().clone();
    let mut guard_reference = Some(guard);
    reject(
        &mut s.arena,
        &mut guard_reference,
        Error::FreeIndex,
        |a, r| {
            // Releasing the guard must consume the missing left-free entry before any effects.
            a.change(r, &[], BUDGET)
        },
    );
    assert_eq!(*s.arena.index.inner.borrow(), index_before);
    s.arena.free.insert(free.key(), ());
    s.arena.free.insert(free_key(4096, 64), ()); // false fitting size, canonical tags disagree
    let index_before = s.arena.index.inner.borrow().clone();
    let mut absent = None;
    reject(&mut s.arena, &mut absent, Error::Length, |a, r| {
        a.change(r, &[], BUDGET)
    });
    reject(&mut s.arena, &mut absent, Error::Budget, |a, r| {
        a.change(r, &payload(1024, 9), 1023)
    });
    assert_eq!(*s.arena.memory.io.borrow(), Io::default());
    assert_eq!(*s.arena.index.io.borrow(), Io::default());
    reject(&mut s.arena, &mut absent, Error::FreeIndex, |a, r| {
        a.change(r, &payload(1024, 9), BUDGET)
    });
    assert_eq!(*s.arena.index.inner.borrow(), index_before);
    s.arena.free.remove(&free_key(4096, 64));
    for (arena, index) in [
        (s.arena.memory.clone(), CountedMemory::default()),
        (CountedMemory::default(), s.arena.index.clone()),
        (s.arena.memory.clone(), s.arena.memory.clone()),
    ] {
        let before_a = arena.inner.borrow().clone();
        let before_i = index.inner.borrow().clone();
        *arena.io.borrow_mut() = Io::default();
        *index.io.borrow_mut() = Io::default();
        assert!(matches!(
            Arena::open(arena.clone(), index.clone()),
            Err(Error::Pair)
        ));
        assert_eq!(*arena.inner.borrow(), before_a);
        assert_eq!(*index.inner.borrow(), before_i);
        assert_eq!(*arena.io.borrow(), Io::default());
        assert_eq!(*index.io.borrow(), Io::default());
    }
    s.audit();
}

#[test]
fn extent_native_comparison_and_fragmentation() {
    println!(
        "EXTENT_HEADER,case,case_input,arena_reads,arena_read_bytes,arena_writes,arena_write_bytes,arena_grows,index_reads,index_read_bytes,index_writes,index_write_bytes,index_grows,before_arena,after_arena,before_index,after_index,logical_end,live_span_bytes,free_span_bytes,free_spans,body_buffer_bytes"
    );
    {
        let mut s = Scenario::new();
        let max = [u8::MAX; 14];
        let sanity = s.set(max, &payload(65, 9)).unwrap();
        assert_eq!(s.arena.read(sanity, 65).unwrap(), payload(65, 9));
        s.set(max, &[]);
        let initial: BTreeMap<_, _> = (0..256).map(|i| (id(i), payload(65, i as u8))).collect();
        s.measure("density_insert_256", 65, |s| {
            for (k, v) in &initial {
                s.set(*k, v);
            }
        });
        let (a, i) = s.measure("density_get_256", 65, |s| {
            for (reference, value) in s.owners.values() {
                assert_eq!(s.arena.read(*reference, 65).unwrap(), *value);
            }
        });
        super::readonly(a);
        super::readonly(i);
        let replacements: BTreeMap<_, _> = initial
            .keys()
            .enumerate()
            .map(|(i, k)| (*k, payload(65, (i as u8).wrapping_add(31))))
            .collect();
        s.measure("density_rewrite_256", 65, |s| {
            for (k, v) in &replacements {
                s.set(*k, v);
            }
        });
        let removed: Vec<_> = initial.keys().step_by(2).copied().collect();
        s.measure("density_remove_128", 65, |s| {
            for k in &removed {
                s.set(*k, &[]);
            }
        });
        s = s.reopen();
        let refill = payload(65, 203);
        s.measure("density_refill_128", 65, |s| {
            for k in &removed {
                s.set(*k, &refill);
            }
        });
    }
    {
        let mut s = Scenario::new();
        let small = [1; 14];
        let large = [2; 14];
        let mut small_bytes = payload(65, 11);
        let mut large_bytes = payload(65, 37);
        s.set(small, &small_bytes);
        s.set(large, &large_bytes);
        for n in [65, 4096, 65536, LARGE] {
            if n != large_bytes.len() {
                large_bytes = payload(n, 37);
                s.set(large, &large_bytes);
            }
            let (a, i) = s.measure("neighbor_small_get", n, |s| {
                assert_eq!(s.arena.read(s.owners[&small].0, 65).unwrap(), small_bytes);
            });
            super::readonly(a);
            super::readonly(i);
            small_bytes = payload(65, small_bytes[0].wrapping_add(1));
            s.measure("neighbor_small_rewrite", n, |s| {
                s.set(small, &small_bytes);
            });
            let (a, i) = s.measure("neighbor_large_get", n, |s| {
                assert_eq!(s.arena.read(s.owners[&large].0, n).unwrap(), large_bytes);
            });
            super::readonly(a);
            super::readonly(i);
            large_bytes = payload(n, large_bytes[0].wrapping_add(1));
            s.measure("neighbor_large_rewrite", n, |s| {
                s.set(large, &large_bytes);
            });
        }
        s.reopen().audit();
    }
    {
        let mut s = Scenario::new();
        let key = [u8::MAX; 14];
        let mut current = payload(65, 7);
        s.set(key, &current);
        for (case, n) in [("hot_warm_large", LARGE), ("hot_warm_small", 65)] {
            current = payload(n, current[0].wrapping_add(1));
            s.measure(case, n, |s| {
                s.set(key, &current);
            });
        }
        s = s.reopen();
        let arena_pages = s.arena.memory.size();
        let index_pages = s.arena.index.size();
        for i in 0..8 {
            let n = if i % 2 == 0 { LARGE } else { 65 };
            current = payload(n, current[0].wrapping_add(1));
            let (a, i) = s.measure("hot_after_reopen", n, |s| {
                s.set(key, &current);
            });
            assert_eq!((a.grows, i.grows), (0, 0));
            assert_eq!(
                (s.arena.memory.size(), s.arena.index.size()),
                (arena_pages, index_pages)
            );
        }
        s.measure("hot_remove", 65, |s| {
            s.set(key, &[]);
        });
        assert!(s.owners.is_empty());
        assert_eq!(s.audit().2, 1);
    }
    for holes in [8, 128] {
        let mut s = Scenario::new();
        for i in 0..holes {
            s.set(id(2 * i), &payload(33, i as u8));
            s.set(id(2 * i + 1), &payload(65, (i as u8).wrapping_add(77)));
        }
        for i in 0..holes {
            s.set(id(2 * i), &[]);
        }
        s = s.reopen();
        let (_, free, count) = s.audit();
        assert_eq!((free, count), (56 * holes as u64, holes as usize));
        let end = Arena::header(&s.arena.memory).unwrap();
        // Aggregate free space is ample but every isolated hole is too small.
        let replacement = payload(65, 193);
        s.measure("fragment_allocate_no_fit", holes as usize, |s| {
            s.set(id(1000), &replacement);
        });
        assert_eq!(s.owners[&id(1000)].0.offset, end);
        assert_eq!(Arena::header(&s.arena.memory).unwrap(), end + 88);
        s.set(id(1), &[]); // joins first two holes through the intervening live span
        let replacement = payload(160, 201);
        s.measure("fragment_allocate_coalesced", holes as usize, |s| {
            s.set(id(1001), &replacement);
        });
        assert_eq!(s.owners[&id(1001)].0.offset, 64);
        assert_eq!(Arena::header(&s.arena.memory).unwrap(), end + 88);
        s.audit();
    }
}

#[cfg(not(test))]
mod benches {
    use super::super::{FragmentPhase, HotPhase};
    use super::{Arena, BUDGET, LARGE, Memory, Scenario, id, need, payload};
    use canbench_rs::{BenchResult, bench, bench_fn};
    use std::hint::black_box;

    fn new_scenario() -> Scenario {
        let mut s = Scenario::new();
        let key = [0; 14];
        let first = payload(65, 7);
        let second = payload(65, 9);
        s.set(key, &first);
        assert_eq!(s.arena.read(s.owners[&key].0, 65).unwrap(), first);
        s.set(key, &second);
        assert_eq!(s.arena.read(s.owners[&key].0, 65).unwrap(), second);
        s.set(key, &first[..32]);
        assert_eq!(s.arena.read(s.owners[&key].0, 32).unwrap(), first[..32]);
        s.set(key, &[]);
        assert_eq!(s.audit().0, 0);
        s
    }

    fn fixture(size: usize, neighbor: usize) -> Scenario {
        let mut s = new_scenario();
        s.set([1; 14], &payload(neighbor, 77));
        s.set([2; 14], &payload(size, 31));
        s.audit();
        s
    }

    fn read_case(size: usize, neighbor: usize) -> BenchResult {
        let s = fixture(size, neighbor);
        let (reference, original) = &s.owners[&[2; 14]];
        let before = ic_cdk::api::stable_size();
        let regions = (s.arena.memory.size(), s.arena.index.size());
        let mut actual = Vec::new();
        let result = bench_fn(|| {
            actual = black_box(
                s.arena
                    .read(black_box(*reference), black_box(BUDGET))
                    .unwrap(),
            );
        });
        assert_eq!(actual, *original);
        s.audit();
        assert_eq!((s.arena.memory.size(), s.arena.index.size()), regions);
        assert_eq!(ic_cdk::api::stable_size(), before);
        assert_eq!(result.total.stable_memory_increase, 0);
        ic_cdk::println!(
            "BODY_CASE,p=extent,op=read,old={size},new={size},neighbor={neighbor},before={before},after={},region0={},region1={}",
            ic_cdk::api::stable_size(),
            regions.0,
            regions.1
        );
        result
    }

    fn rewrite_case(size: usize, new_size: usize, neighbor: usize) -> BenchResult {
        let mut s = fixture(size, neighbor);
        let mut reference = Some(s.owners[&[2; 14]].0);
        let replacement = payload(new_size, 193);
        let before = ic_cdk::api::stable_size();
        let mut actual = Vec::new();
        let result = bench_fn(|| {
            actual = black_box(
                s.arena
                    .change(
                        black_box(&mut reference),
                        black_box(&replacement),
                        black_box(BUDGET),
                    )
                    .unwrap(),
            );
        });
        assert_eq!(actual, s.owners[&[2; 14]].1);
        s.owners.insert([2; 14], (reference.unwrap(), replacement));
        let (live, free, count) = s.audit();
        assert_eq!(
            result.total.stable_memory_increase,
            ic_cdk::api::stable_size() - before
        );
        ic_cdk::println!(
            "BODY_CASE,p=extent,op=rewrite,old={size},new={new_size},neighbor={neighbor},before={before},after={},region0={},region1={},end={},live={live},free={free},free_spans={count}",
            ic_cdk::api::stable_size(),
            s.arena.memory.size(),
            s.arena.index.size(),
            Arena::header(&s.arena.memory).unwrap()
        );
        result
    }

    fn hot_case(phase: HotPhase) -> BenchResult {
        let mut s = new_scenario();
        let key = [2; 14];
        s.set(key, &payload(65, 31));
        match phase {
            HotPhase::InitialGrow => {}
            HotPhase::ReuseGrow | HotPhase::ReuseShrink => {
                s.set(key, &payload(LARGE, 40));
                s.set(key, &payload(65, 42));
                s = s.reopen();
                if matches!(phase, HotPhase::ReuseShrink) {
                    s.set(key, &payload(LARGE, 43));
                }
            }
        }
        s.audit();
        let mut reference = Some(s.owners[&key].0);
        let old_len = reference.unwrap().len;
        let new_len = if matches!(phase, HotPhase::ReuseShrink) {
            65
        } else {
            LARGE
        };
        let replacement = payload(new_len, 193);
        let before = ic_cdk::api::stable_size();
        let regions = (s.arena.memory.size(), s.arena.index.size());
        let mut actual = Vec::new();
        let result = bench_fn(|| {
            actual = black_box(
                s.arena
                    .change(
                        black_box(&mut reference),
                        black_box(&replacement),
                        black_box(BUDGET),
                    )
                    .unwrap(),
            );
        });
        assert_eq!(actual, s.owners[&key].1);
        s.owners.insert(key, (reference.unwrap(), replacement));
        let (live, free, count) = s.audit();
        if !matches!(phase, HotPhase::InitialGrow) {
            assert_eq!((s.arena.memory.size(), s.arena.index.size()), regions);
            assert_eq!(ic_cdk::api::stable_size(), before);
        }
        assert_eq!(
            result.total.stable_memory_increase,
            ic_cdk::api::stable_size() - before
        );
        ic_cdk::println!(
            "BODY_HOT,p=extent,phase={phase:?},old={old_len},new={new_len},before={before},after={},region0={},region1={},end={},live={live},free={free},free_spans={count}",
            ic_cdk::api::stable_size(),
            s.arena.memory.size(),
            s.arena.index.size(),
            Arena::header(&s.arena.memory).unwrap()
        );
        result
    }

    fn fragment_case(holes: u32, phase: FragmentPhase) -> BenchResult {
        let mut s = new_scenario();
        for i in 0..holes {
            s.set(id(2 * i), &payload(33, i as u8));
            s.set(id(2 * i + 1), &payload(65, (i as u8).wrapping_add(77)));
        }
        for i in 0..holes {
            s.set(id(2 * i), &[]);
        }
        assert_eq!(s.audit().2, holes as usize);
        if matches!(phase, FragmentPhase::Coalesced) {
            s.set(id(1), &[]);
        }
        let (_, _, count) = s.audit();
        assert_eq!(
            count,
            match phase {
                FragmentPhase::NoFit => holes as usize,
                FragmentPhase::Coalesced => holes as usize - 1,
            }
        );
        let end = Arena::header(&s.arena.memory).unwrap();
        let len = match phase {
            FragmentPhase::NoFit => 65,
            FragmentPhase::Coalesced => 160,
        };
        let replacement = payload(len, 193);
        let before = ic_cdk::api::stable_size();
        let mut reference = None;
        let mut actual = Vec::new();
        let result = bench_fn(|| {
            actual = black_box(
                s.arena
                    .change(
                        black_box(&mut reference),
                        black_box(&replacement),
                        black_box(BUDGET),
                    )
                    .unwrap(),
            );
        });
        assert!(actual.is_empty());
        let reference = reference.unwrap();
        match phase {
            FragmentPhase::NoFit => {
                assert_eq!(reference.offset, end);
                assert_eq!(Arena::header(&s.arena.memory).unwrap(), end + need(len));
            }
            FragmentPhase::Coalesced => {
                assert_eq!(reference.offset, 64);
                assert_eq!(Arena::header(&s.arena.memory).unwrap(), end);
            }
        }
        assert!(
            s.owners
                .insert(id(1000), (reference, replacement))
                .is_none()
        );
        let (live, free, count) = s.audit();
        assert_eq!(
            result.total.stable_memory_increase,
            ic_cdk::api::stable_size() - before
        );
        ic_cdk::println!(
            "BODY_FRAGMENT,p=extent,phase={phase:?},holes={holes},new={len},before={before},after={},region0={},region1={},end={},live={live},free={free},free_spans={count}",
            ic_cdk::api::stable_size(),
            s.arena.memory.size(),
            s.arena.index.size(),
            Arena::header(&s.arena.memory).unwrap()
        );
        result
    }

    macro_rules! cases {
        ($($name:ident => $run:expr),+ $(,)?) => {
            $(#[bench(raw)] fn $name() -> BenchResult {$run})+
        };
    }
    cases! {
        body_extent_read_65b=>read_case(65,65),
        body_extent_read_1m=>read_case(LARGE,65),
        body_extent_rewrite_65b=>rewrite_case(65,65,65),
        body_extent_rewrite_1m=>rewrite_case(LARGE,LARGE,65),
        body_extent_shrink_65b=>rewrite_case(65,32,65),
        body_extent_shrink_1m=>rewrite_case(LARGE,65,65),
        body_extent_neighbor_read=>read_case(65,LARGE),
        body_extent_neighbor_rewrite=>rewrite_case(65,65,LARGE),
        body_extent_initial_grow=>hot_case(HotPhase::InitialGrow),
        body_extent_reuse_grow=>hot_case(HotPhase::ReuseGrow),
        body_extent_reuse_shrink=>hot_case(HotPhase::ReuseShrink),
        body_extent_fragment_nofit_8=>fragment_case(8,FragmentPhase::NoFit),
        body_extent_fragment_nofit_128=>fragment_case(128,FragmentPhase::NoFit),
        body_extent_fragment_coalesced_8=>fragment_case(8,FragmentPhase::Coalesced),
        body_extent_fragment_coalesced_128=>fragment_case(128,FragmentPhase::Coalesced),
    }
}
