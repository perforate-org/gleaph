use crate::memory::{Memory, MemoryError};

pub const ABP_PAGE_SIZE: u32 = 4096;
pub const ABP_STORE_MAGIC: [u8; 8] = *b"GLABP1\0\0";
pub const ABP_STORE_VERSION: u16 = 1;
pub const ABP_STORE_HEADER_LEN: u64 = 40;
pub const ABP_NODE_HEADER_LEN: u16 = 32;
const ABP_LEAF_REC_HEADER_LEN: usize = 8; // key_len:u16, val_len:u16, flags:u8, reserved[3]
const ABP_REC_FLAG_TOMBSTONE: u8 = 0x01;
const ABP_INTERNAL_PAYLOAD_HEADER_LEN: usize = 8; // leftmost_child:u32, entry_count:u16, reserved:u16
const ABP_INTERNAL_ENTRY_HEADER_LEN: usize = 8; // key_len:u16, reserved:u16, child:u32

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyStoreFormatTag {
    AppendLog = 1,
    AbPlusTree = 2,
}

impl PropertyStoreFormatTag {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::AppendLog),
            2 => Some(Self::AbPlusTree),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbpStoreHeader {
    pub version: u16,
    pub format: PropertyStoreFormatTag,
    pub page_size: u32,
    pub root_page_id: u32,
    pub next_page_id: u32,
    pub free_list_head_page_id: u32,
}

impl Default for AbpStoreHeader {
    fn default() -> Self {
        Self {
            version: ABP_STORE_VERSION,
            format: PropertyStoreFormatTag::AbPlusTree,
            page_size: ABP_PAGE_SIZE,
            root_page_id: 0,
            next_page_id: 1,
            free_list_head_page_id: 0,
        }
    }
}

impl AbpStoreHeader {
    pub fn read_from<M: Memory>(mem: &M, offset: u64) -> Option<Self> {
        if mem.size_bytes() < offset + ABP_STORE_HEADER_LEN {
            return None;
        }
        let mut buf = [0u8; ABP_STORE_HEADER_LEN as usize];
        mem.read(offset, &mut buf);
        if buf[0..8] != ABP_STORE_MAGIC {
            return None;
        }
        let version = u16::from_le_bytes(buf[8..10].try_into().ok()?);
        let format = PropertyStoreFormatTag::from_u8(buf[10])?;
        let page_size = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        let root_page_id = u32::from_le_bytes(buf[16..20].try_into().ok()?);
        let next_page_id = u32::from_le_bytes(buf[20..24].try_into().ok()?);
        let free_list_head_page_id = u32::from_le_bytes(buf[24..28].try_into().ok()?);
        Some(Self {
            version,
            format,
            page_size,
            root_page_id,
            next_page_id,
            free_list_head_page_id,
        })
    }

    pub fn write_to<M: Memory>(&self, mem: &mut M, offset: u64) -> Result<(), MemoryError> {
        ensure_size(mem, offset + ABP_STORE_HEADER_LEN)?;
        let mut buf = [0u8; ABP_STORE_HEADER_LEN as usize];
        buf[0..8].copy_from_slice(&ABP_STORE_MAGIC);
        buf[8..10].copy_from_slice(&self.version.to_le_bytes());
        buf[10] = self.format as u8;
        // buf[11] reserved
        buf[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        buf[16..20].copy_from_slice(&self.root_page_id.to_le_bytes());
        buf[20..24].copy_from_slice(&self.next_page_id.to_le_bytes());
        buf[24..28].copy_from_slice(&self.free_list_head_page_id.to_le_bytes());
        mem.write(offset, &buf);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AbpNodeKind {
    Internal = 1,
    Leaf = 2,
    Free = 3,
}

impl AbpNodeKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Internal),
            2 => Some(Self::Leaf),
            3 => Some(Self::Free),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbpNodeHeader {
    pub kind: AbpNodeKind,
    pub key_count: u16,
    pub used_bytes: u16,
    pub level: u16,        // 0 = leaf
    pub prev_page_id: u32, // for leaf chain / free list links
    pub next_page_id: u32, // for leaf chain / free list links
    pub reserved: u32,
}

impl AbpNodeHeader {
    pub fn new_leaf() -> Self {
        Self {
            kind: AbpNodeKind::Leaf,
            key_count: 0,
            used_bytes: ABP_NODE_HEADER_LEN,
            level: 0,
            prev_page_id: 0,
            next_page_id: 0,
            reserved: 0,
        }
    }

    pub fn new_internal(level: u16) -> Self {
        Self {
            kind: AbpNodeKind::Internal,
            key_count: 0,
            used_bytes: ABP_NODE_HEADER_LEN,
            level,
            prev_page_id: 0,
            next_page_id: 0,
            reserved: 0,
        }
    }

    pub fn read_from_page<M: Memory>(
        mem: &M,
        base_offset: u64,
        page_id: u32,
        page_size: u32,
    ) -> Option<Self> {
        let off = page_offset(base_offset, page_id, page_size);
        if mem.size_bytes() < off + ABP_NODE_HEADER_LEN as u64 {
            return None;
        }
        let mut buf = [0u8; ABP_NODE_HEADER_LEN as usize];
        mem.read(off, &mut buf);
        let kind = AbpNodeKind::from_u8(buf[0])?;
        Some(Self {
            kind,
            key_count: u16::from_le_bytes(buf[2..4].try_into().ok()?),
            used_bytes: u16::from_le_bytes(buf[4..6].try_into().ok()?),
            level: u16::from_le_bytes(buf[6..8].try_into().ok()?),
            prev_page_id: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            next_page_id: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            reserved: u32::from_le_bytes(buf[16..20].try_into().ok()?),
        })
    }

    pub fn write_to_page<M: Memory>(
        &self,
        mem: &mut M,
        base_offset: u64,
        page_id: u32,
        page_size: u32,
    ) -> Result<(), MemoryError> {
        let off = page_offset(base_offset, page_id, page_size);
        ensure_size(mem, off + u64::from(page_size))?;
        let mut buf = [0u8; ABP_NODE_HEADER_LEN as usize];
        buf[0] = self.kind as u8;
        // buf[1] reserved
        buf[2..4].copy_from_slice(&self.key_count.to_le_bytes());
        buf[4..6].copy_from_slice(&self.used_bytes.to_le_bytes());
        buf[6..8].copy_from_slice(&self.level.to_le_bytes());
        buf[8..12].copy_from_slice(&self.prev_page_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self.next_page_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.reserved.to_le_bytes());
        mem.write(off, &buf);
        Ok(())
    }
}

pub fn page_offset(base_offset: u64, page_id: u32, page_size: u32) -> u64 {
    base_offset + u64::from(page_id).saturating_mul(u64::from(page_size))
}

pub fn detect_property_store_format<M: Memory>(
    mem: &M,
    region_start: u64,
) -> PropertyStoreFormatTag {
    AbpStoreHeader::read_from(mem, region_start)
        .map(|h| h.format)
        .unwrap_or(PropertyStoreFormatTag::AppendLog)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbpInternalEntry {
    pub separator_key: Vec<u8>,
    pub child_page_id: u32,
}

#[derive(Clone, Debug)]
pub struct AbpByteKv<M: Memory> {
    mem: M,
    region_start: u64,
    header: AbpStoreHeader,
}

impl<M: Memory> AbpByteKv<M> {
    pub fn create(mut mem: M, region_start: u64) -> Result<Self, MemoryError> {
        let header = AbpStoreHeader {
            root_page_id: 1,
            next_page_id: 2,
            ..AbpStoreHeader::default()
        };
        header.write_to(&mut mem, region_start)?;
        AbpNodeHeader::new_leaf().write_to_page(
            &mut mem,
            region_start + ABP_STORE_HEADER_LEN,
            header.root_page_id,
            header.page_size,
        )?;
        Ok(Self {
            mem,
            region_start,
            header,
        })
    }

    pub fn open(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        let header =
            AbpStoreHeader::read_from(&mem, region_start).ok_or(MemoryError::OutOfBounds {
                offset: region_start,
                len: ABP_STORE_HEADER_LEN as usize,
            })?;
        Ok(Self {
            mem,
            region_start,
            header,
        })
    }

    pub fn into_memory(self) -> M {
        self.mem
    }

    pub fn memory(&self) -> &M {
        &self.mem
    }

    pub fn memory_mut(&mut self) -> &mut M {
        &mut self.mem
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut page_id = self.find_leaf_page_id_for_key(key)?;
        let mut guard = 0u32;
        while page_id != 0 && guard < self.header.next_page_id.saturating_add(1) {
            let Some((hdr, payload)) = self.read_leaf(page_id) else {
                break;
            };
            for (k, v, tomb) in scan_leaf_records(&payload) {
                match k.cmp(key) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => return (!tomb).then(|| v.to_vec()),
                    std::cmp::Ordering::Greater => return None,
                }
            }
            page_id = hdr.next_page_id;
            guard = guard.saturating_add(1);
        }
        None
    }

    pub fn upsert(&mut self, key: &[u8], value: &[u8]) -> Result<(), MemoryError> {
        if self.try_upsert_page_local(key, value)? {
            return Ok(());
        }
        let mut recs = self.read_root_leaf_records();
        recs.retain(|(k, _, _)| k.as_slice() != key);
        recs.push((key.to_vec(), value.to_vec(), false));
        recs.sort_by(|a, b| a.0.cmp(&b.0));
        self.write_root_leaf_records(&recs)
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<(), MemoryError> {
        if self.try_delete_page_local(key)? {
            return Ok(());
        }
        let mut recs = self.read_root_leaf_records();
        recs.retain(|(k, _, _)| k.as_slice() != key);
        recs.push((key.to_vec(), Vec::new(), true));
        recs.sort_by(|a, b| a.0.cmp(&b.0));
        self.write_root_leaf_records(&recs)
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut latest = std::collections::BTreeMap::<Vec<u8>, (Vec<u8>, bool)>::new();
        let Some(mut page_id) = self.find_leaf_page_id_for_key(prefix) else {
            return out;
        };
        let mut guard = 0u32;
        while page_id != 0 && guard < self.header.next_page_id.saturating_add(1) {
            let Some((hdr, payload)) = self.read_leaf(page_id) else {
                break;
            };
            for (k, v, tomb) in scan_leaf_records(&payload) {
                if k.starts_with(prefix) {
                    latest.insert(k.to_vec(), (v.to_vec(), tomb));
                } else if !prefix.is_empty() && k > prefix {
                    page_id = 0;
                    break;
                }
            }
            if page_id == 0 {
                break;
            }
            page_id = hdr.next_page_id;
            guard = guard.saturating_add(1);
        }
        for (k, (v, tomb)) in latest {
            if !tomb {
                out.push((k, v));
            }
        }
        out
    }

    /// Scan keys in the range `[start_key, end_key)`.
    ///
    /// All keys that share the given `required_prefix` and fall within the
    /// byte-level range `[start_key, end_key)` are returned in sorted order.
    /// If `end_key` is empty, the scan continues until the prefix no longer matches.
    pub fn scan_range(
        &self,
        required_prefix: &[u8],
        start_key: &[u8],
        end_key: &[u8],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut latest = std::collections::BTreeMap::<Vec<u8>, (Vec<u8>, bool)>::new();
        let Some(mut page_id) = self.find_leaf_page_id_for_key(start_key) else {
            return Vec::new();
        };
        let mut guard = 0u32;
        while page_id != 0 && guard < self.header.next_page_id.saturating_add(1) {
            let Some((hdr, payload)) = self.read_leaf(page_id) else {
                break;
            };
            let mut done = false;
            for (k, v, tomb) in scan_leaf_records(&payload) {
                if !k.starts_with(required_prefix) {
                    if k > required_prefix {
                        done = true;
                        break;
                    }
                    continue;
                }
                if k < start_key {
                    continue;
                }
                if !end_key.is_empty() && k >= end_key {
                    done = true;
                    break;
                }
                latest.insert(k.to_vec(), (v.to_vec(), tomb));
            }
            if done {
                break;
            }
            page_id = hdr.next_page_id;
            guard = guard.saturating_add(1);
        }
        let mut out = Vec::new();
        for (k, (v, tomb)) in latest {
            if !tomb {
                out.push((k, v));
            }
        }
        out
    }

    pub fn compact(&mut self) -> Result<(), MemoryError> {
        let recs = self.read_root_leaf_records();
        self.write_root_leaf_records(&recs)
    }

    fn read_leaf(&self, page_id: u32) -> Option<(AbpNodeHeader, Vec<u8>)> {
        let (hdr, payload) = self.read_node(page_id)?;
        if hdr.kind != AbpNodeKind::Leaf {
            return None;
        }
        Some((hdr, payload))
    }

    fn read_node(&self, page_id: u32) -> Option<(AbpNodeHeader, Vec<u8>)> {
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        let hdr = AbpNodeHeader::read_from_page(&self.mem, base, page_id, self.header.page_size)?;
        let off = page_offset(base, page_id, self.header.page_size);
        let used = usize::from(hdr.used_bytes).min(self.header.page_size as usize);
        let payload_len = used.saturating_sub(usize::from(ABP_NODE_HEADER_LEN));
        let mut payload = vec![0u8; payload_len];
        self.mem
            .read(off + u64::from(ABP_NODE_HEADER_LEN), &mut payload);
        Some((hdr, payload))
    }

    fn leftmost_leaf_page_id(&self) -> Option<u32> {
        self.descend_leftmost_leaf_page_id(self.header.root_page_id)
    }

    fn find_leaf_page_id_for_key(&self, key: &[u8]) -> Option<u32> {
        self.descend_to_leaf_page_id(self.header.root_page_id, key)
    }

    fn descend_leftmost_leaf_page_id(&self, page_id: u32) -> Option<u32> {
        let (hdr, payload) = self.read_node(page_id)?;
        match hdr.kind {
            AbpNodeKind::Leaf => Some(page_id),
            AbpNodeKind::Internal => {
                let (leftmost, _) = decode_internal_payload(&payload)?;
                self.descend_leftmost_leaf_page_id(leftmost)
            }
            AbpNodeKind::Free => None,
        }
    }

    fn descend_to_leaf_page_id(&self, page_id: u32, key: &[u8]) -> Option<u32> {
        let (hdr, payload) = self.read_node(page_id)?;
        match hdr.kind {
            AbpNodeKind::Leaf => Some(page_id),
            AbpNodeKind::Internal => {
                let (leftmost, entries) = decode_internal_payload(&payload)?;
                let child = choose_child_from_internal(leftmost, &entries, key);
                self.descend_to_leaf_page_id(child, key)
            }
            AbpNodeKind::Free => None,
        }
    }

    fn read_root_leaf_records(&self) -> Vec<(Vec<u8>, Vec<u8>, bool)> {
        let mut out = Vec::new();
        let mut page_id = match self.leftmost_leaf_page_id() {
            Some(id) => id,
            None => return out,
        };
        let mut guard = 0u32;
        while page_id != 0 && guard < self.header.next_page_id.saturating_add(1) {
            let Some((hdr, payload)) = self.read_leaf(page_id) else {
                break;
            };
            out.extend(
                scan_leaf_records(&payload).map(|(k, v, tomb)| (k.to_vec(), v.to_vec(), tomb)),
            );
            page_id = hdr.next_page_id;
            guard = guard.saturating_add(1);
        }
        out
    }

    fn try_upsert_page_local(&mut self, key: &[u8], value: &[u8]) -> Result<bool, MemoryError> {
        let (root_hdr, root_payload) = match self.read_node(self.header.root_page_id) {
            Some(v) => v,
            None => return Ok(false),
        };
        let max_payload = self
            .header
            .page_size
            .saturating_sub(u32::from(ABP_NODE_HEADER_LEN)) as usize;
        match root_hdr.kind {
            AbpNodeKind::Leaf => {
                let mut recs = decode_leaf_records_vec(&root_payload);
                recs.retain(|(k, _, _)| k.as_slice() != key);
                recs.push((key.to_vec(), value.to_vec(), false));
                recs.sort_by(|a, b| a.0.cmp(&b.0));
                let needed: usize = recs
                    .iter()
                    .map(|(k, v, t)| encode_leaf_record(k, v, *t).map(|x| x.len()))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .sum();
                if needed <= max_payload {
                    self.write_leaf_page_records(self.header.root_page_id, 0, 0, &recs)?;
                    return Ok(true);
                }
                let (left, right) = split_leaf_records_balanced(&recs)?;
                let left_id = self.alloc_page_id()?;
                let right_id = self.alloc_page_id()?;
                self.write_leaf_page_records(left_id, 0, right_id, &left)?;
                self.write_leaf_page_records(right_id, left_id, 0, &right)?;
                let sep = right
                    .first()
                    .map(|(k, _, _)| k.clone())
                    .ok_or(MemoryError::GrowOverflow)?;
                self.write_root_internal_payload(
                    left_id,
                    &[AbpInternalEntry {
                        separator_key: sep,
                        child_page_id: right_id,
                    }],
                )?;
                return Ok(true);
            }
            AbpNodeKind::Internal => {
                let target = match self.find_leaf_page_id_for_key(key) {
                    Some(id) => id,
                    None => return Ok(false),
                };
                let (leaf_hdr, leaf_payload) = match self.read_leaf(target) {
                    Some(v) => v,
                    None => return Ok(false),
                };
                let mut recs = decode_leaf_records_vec(&leaf_payload);
                recs.retain(|(k, _, _)| k.as_slice() != key);
                recs.push((key.to_vec(), value.to_vec(), false));
                recs.sort_by(|a, b| a.0.cmp(&b.0));
                let needed: usize = recs
                    .iter()
                    .map(|(k, v, t)| encode_leaf_record(k, v, *t).map(|x| x.len()))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .sum();
                if needed <= max_payload {
                    self.write_leaf_page_records(
                        target,
                        leaf_hdr.prev_page_id,
                        leaf_hdr.next_page_id,
                        &recs,
                    )?;
                    self.rebuild_root_internal_from_chain()?;
                    return Ok(true);
                }
                let (left, right) = split_leaf_records_balanced(&recs)?;
                let new_right = self.alloc_page_id()?;
                self.write_leaf_page_records(target, leaf_hdr.prev_page_id, new_right, &left)?;
                self.write_leaf_page_records(new_right, target, leaf_hdr.next_page_id, &right)?;
                if leaf_hdr.next_page_id != 0 {
                    self.patch_leaf_prev(leaf_hdr.next_page_id, new_right)?;
                }
                self.rebuild_root_internal_from_chain()?;
                return Ok(true);
            }
            AbpNodeKind::Free => {}
        }
        Ok(false)
    }

    fn try_delete_page_local(&mut self, key: &[u8]) -> Result<bool, MemoryError> {
        let (root_hdr, root_payload) = match self.read_node(self.header.root_page_id) {
            Some(v) => v,
            None => return Ok(false),
        };
        match root_hdr.kind {
            AbpNodeKind::Leaf => {
                let mut recs = decode_leaf_records_vec(&root_payload);
                recs.retain(|(k, _, _)| k.as_slice() != key);
                recs.push((key.to_vec(), Vec::new(), true));
                recs.sort_by(|a, b| a.0.cmp(&b.0));
                self.write_leaf_page_records(self.header.root_page_id, 0, 0, &recs)?;
                Ok(true)
            }
            AbpNodeKind::Internal => {
                let target = match self.find_leaf_page_id_for_key(key) {
                    Some(id) => id,
                    None => return Ok(false),
                };
                let leaf_ids = self.current_leaf_chain_page_ids();
                let idx = match leaf_ids.iter().position(|id| *id == target) {
                    Some(i) => i,
                    None => return Ok(false),
                };
                let (leaf_hdr, leaf_payload) = match self.read_leaf(target) {
                    Some(v) => v,
                    None => return Ok(false),
                };
                let mut recs = decode_leaf_records_vec(&leaf_payload);
                recs.retain(|(k, _, _)| k.as_slice() != key);
                recs.push((key.to_vec(), Vec::new(), true));
                recs.sort_by(|a, b| a.0.cmp(&b.0));
                self.write_leaf_page_records(
                    target,
                    leaf_hdr.prev_page_id,
                    leaf_hdr.next_page_id,
                    &recs,
                )?;

                // Prototype merge: if leaf payload is sparse and next leaf fits, merge locally.
                let current_used = encoded_leaf_records_len(&recs)?;
                let max_payload = self
                    .header
                    .page_size
                    .saturating_sub(u32::from(ABP_NODE_HEADER_LEN))
                    as usize;
                if current_used < (max_payload / 3)
                    && let Some(next_id) = leaf_ids.get(idx + 1).copied()
                {
                    self.try_merge_adjacent_leaves(target, next_id)?;
                }
                self.rebuild_root_internal_from_chain()?;
                Ok(true)
            }
            AbpNodeKind::Free => Ok(false),
        }
    }

    fn write_root_leaf_records(
        &mut self,
        recs: &[(Vec<u8>, Vec<u8>, bool)],
    ) -> Result<(), MemoryError> {
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        let page_size = self.header.page_size as usize;
        let max_payload = page_size.saturating_sub(usize::from(ABP_NODE_HEADER_LEN));
        let encoded = recs
            .iter()
            .map(|(k, v, tomb)| encode_leaf_record(k, v, *tomb))
            .collect::<Result<Vec<_>, _>>()?;
        if encoded.iter().any(|r| r.len() > max_payload) {
            return Err(MemoryError::GrowOverflow);
        }

        let mut pages: Vec<Vec<Vec<u8>>> = vec![Vec::new()];
        let mut cur_used = 0usize;
        for rec in encoded {
            if cur_used + rec.len() > max_payload && !pages.last().is_none_or(|p| p.is_empty()) {
                pages.push(Vec::new());
                cur_used = 0;
            }
            cur_used += rec.len();
            pages.last_mut().expect("pages non-empty").push(rec);
        }

        let mut existing_ids = self.current_leaf_chain_page_ids();
        if existing_ids.is_empty() {
            existing_ids.push(self.header.root_page_id);
        }
        while existing_ids.len() < pages.len() {
            existing_ids.push(self.alloc_page_id()?);
        }

        let use_internal_root = pages.len() > 1;
        let mut leaf_ids = if use_internal_root {
            let mut ids = self.current_leaf_chain_page_ids_excluding_root();
            while ids.len() < pages.len() {
                ids.push(self.alloc_page_id()?);
            }
            ids
        } else {
            let mut ids = self.current_leaf_chain_page_ids();
            if ids.is_empty() {
                ids.push(self.header.root_page_id);
            }
            while ids.len() < pages.len() {
                ids.push(self.alloc_page_id()?);
            }
            ids
        };
        if leaf_ids.is_empty() {
            leaf_ids.push(if use_internal_root {
                self.alloc_page_id()?
            } else {
                self.header.root_page_id
            });
        }

        for (idx, page_recs) in pages.iter().enumerate() {
            let page_id = leaf_ids[idx];
            let next_page_id = leaf_ids.get(idx + 1).copied().unwrap_or(0);
            let prev_page_id = if idx == 0 { 0 } else { leaf_ids[idx - 1] };
            let payload = page_recs.concat();
            let mut hdr = AbpNodeHeader::new_leaf();
            hdr.key_count = u16::try_from(page_recs.len()).unwrap_or(u16::MAX);
            hdr.used_bytes = u16::try_from(usize::from(ABP_NODE_HEADER_LEN) + payload.len())
                .map_err(|_| MemoryError::GrowOverflow)?;
            hdr.prev_page_id = prev_page_id;
            hdr.next_page_id = next_page_id;
            hdr.write_to_page(&mut self.mem, base, page_id, self.header.page_size)?;
            let off = page_offset(base, page_id, self.header.page_size);
            if !payload.is_empty() {
                self.mem
                    .write(off + u64::from(ABP_NODE_HEADER_LEN), &payload);
            }
        }
        for &extra_page_id in leaf_ids.iter().skip(pages.len()) {
            self.free_page_id(extra_page_id)?;
        }

        let active_leaf_ids = leaf_ids.into_iter().take(pages.len()).collect::<Vec<_>>();
        let _ = (use_internal_root, page_size, base); // root/index layout finalized by rebuild helper
        self.rebuild_root_from_leaf_ids(&active_leaf_ids)
    }

    fn write_leaf_page_records(
        &mut self,
        page_id: u32,
        prev_page_id: u32,
        next_page_id: u32,
        recs: &[(Vec<u8>, Vec<u8>, bool)],
    ) -> Result<(), MemoryError> {
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        let payload = encode_leaf_records_payload(recs)?;
        let page_size = self.header.page_size as usize;
        if usize::from(ABP_NODE_HEADER_LEN) + payload.len() > page_size {
            return Err(MemoryError::GrowOverflow);
        }
        let mut hdr = AbpNodeHeader::new_leaf();
        hdr.key_count = u16::try_from(recs.len()).unwrap_or(u16::MAX);
        hdr.used_bytes = u16::try_from(usize::from(ABP_NODE_HEADER_LEN) + payload.len())
            .map_err(|_| MemoryError::GrowOverflow)?;
        hdr.prev_page_id = prev_page_id;
        hdr.next_page_id = next_page_id;
        hdr.write_to_page(&mut self.mem, base, page_id, self.header.page_size)?;
        let off = page_offset(base, page_id, self.header.page_size);
        if !payload.is_empty() {
            self.mem
                .write(off + u64::from(ABP_NODE_HEADER_LEN), &payload);
        }
        Ok(())
    }

    fn write_root_internal_payload(
        &mut self,
        leftmost_child_page_id: u32,
        entries: &[AbpInternalEntry],
    ) -> Result<(), MemoryError> {
        self.write_internal_page_payload(
            self.header.root_page_id,
            1,
            leftmost_child_page_id,
            entries,
        )
    }

    fn write_internal_page_payload(
        &mut self,
        page_id: u32,
        level: u16,
        leftmost_child_page_id: u32,
        entries: &[AbpInternalEntry],
    ) -> Result<(), MemoryError> {
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        let payload = encode_internal_payload(leftmost_child_page_id, entries)?;
        let page_size = self.header.page_size as usize;
        if usize::from(ABP_NODE_HEADER_LEN) + payload.len() > page_size {
            return Err(MemoryError::GrowOverflow);
        }
        let mut hdr = AbpNodeHeader::new_internal(level);
        hdr.key_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        hdr.used_bytes = u16::try_from(usize::from(ABP_NODE_HEADER_LEN) + payload.len())
            .map_err(|_| MemoryError::GrowOverflow)?;
        hdr.write_to_page(&mut self.mem, base, page_id, self.header.page_size)?;
        let off = page_offset(base, page_id, self.header.page_size);
        self.mem
            .write(off + u64::from(ABP_NODE_HEADER_LEN), &payload);
        Ok(())
    }

    fn rebuild_root_internal_from_chain(&mut self) -> Result<(), MemoryError> {
        let all_leaf_ids = self.current_leaf_chain_page_ids();
        self.rebuild_root_from_leaf_ids(&all_leaf_ids)
    }

    fn rebuild_root_from_leaf_ids(&mut self, all_leaf_ids: &[u32]) -> Result<(), MemoryError> {
        self.free_nonroot_internal_pages()?;
        if all_leaf_ids.len() == 1 {
            let only_leaf_id = all_leaf_ids[0];
            let (hdr, payload) = self
                .read_leaf(only_leaf_id)
                .ok_or(MemoryError::GrowOverflow)?;
            let mut root_hdr = hdr;
            root_hdr.prev_page_id = 0;
            root_hdr.next_page_id = 0;
            let base = self.region_start + ABP_STORE_HEADER_LEN;
            root_hdr.write_to_page(
                &mut self.mem,
                base,
                self.header.root_page_id,
                self.header.page_size,
            )?;
            let root_off = page_offset(base, self.header.root_page_id, self.header.page_size);
            if !payload.is_empty() {
                self.mem
                    .write(root_off + u64::from(ABP_NODE_HEADER_LEN), &payload);
            }
            if only_leaf_id != self.header.root_page_id {
                self.free_page_id(only_leaf_id)?;
            }
            return Ok(());
        }
        self.rebuild_root_index_levels_from_leaf_chain(all_leaf_ids)
    }

    fn patch_leaf_prev(&mut self, page_id: u32, new_prev: u32) -> Result<(), MemoryError> {
        let (mut hdr, payload) = self.read_leaf(page_id).ok_or(MemoryError::GrowOverflow)?;
        hdr.prev_page_id = new_prev;
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        hdr.write_to_page(&mut self.mem, base, page_id, self.header.page_size)?;
        let off = page_offset(base, page_id, self.header.page_size);
        if !payload.is_empty() {
            self.mem
                .write(off + u64::from(ABP_NODE_HEADER_LEN), &payload);
        }
        Ok(())
    }

    fn try_merge_adjacent_leaves(
        &mut self,
        left_id: u32,
        right_id: u32,
    ) -> Result<(), MemoryError> {
        let (left_hdr, left_payload) = match self.read_leaf(left_id) {
            Some(v) => v,
            None => return Ok(()),
        };
        let (right_hdr, right_payload) = match self.read_leaf(right_id) {
            Some(v) => v,
            None => return Ok(()),
        };
        let mut left_recs = decode_leaf_records_vec(&left_payload);
        let right_recs = decode_leaf_records_vec(&right_payload);
        let combined_len =
            encoded_leaf_records_len(&left_recs)? + encoded_leaf_records_len(&right_recs)?;
        let max_payload = self
            .header
            .page_size
            .saturating_sub(u32::from(ABP_NODE_HEADER_LEN)) as usize;
        if combined_len > max_payload {
            return Ok(());
        }
        left_recs.extend(right_recs);
        left_recs.sort_by(|a, b| a.0.cmp(&b.0));
        self.write_leaf_page_records(
            left_id,
            left_hdr.prev_page_id,
            right_hdr.next_page_id,
            &left_recs,
        )?;
        if right_hdr.next_page_id != 0 {
            self.patch_leaf_prev(right_hdr.next_page_id, left_id)?;
        }
        self.free_page_id(right_id)?;
        Ok(())
    }

    fn current_leaf_chain_page_ids(&self) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut page_id = match self.leftmost_leaf_page_id() {
            Some(id) => id,
            None => return ids,
        };
        let mut guard = 0u32;
        while page_id != 0 && guard < self.header.next_page_id.saturating_add(1) {
            ids.push(page_id);
            let Some((hdr, _)) = self.read_leaf(page_id) else {
                break;
            };
            page_id = hdr.next_page_id;
            guard = guard.saturating_add(1);
        }
        ids
    }

    fn current_leaf_chain_page_ids_excluding_root(&self) -> Vec<u32> {
        self.current_leaf_chain_page_ids()
            .into_iter()
            .filter(|id| *id != self.header.root_page_id)
            .collect()
    }

    fn alloc_page_id(&mut self) -> Result<u32, MemoryError> {
        if self.header.free_list_head_page_id != 0 {
            let page_id = self.header.free_list_head_page_id;
            let (hdr, _) = self.read_node(page_id).ok_or(MemoryError::GrowOverflow)?;
            if hdr.kind != AbpNodeKind::Free {
                return Err(MemoryError::GrowOverflow);
            }
            self.header.free_list_head_page_id = hdr.next_page_id;
            self.header.write_to(&mut self.mem, self.region_start)?;
            return Ok(page_id);
        }
        let page_id = self.header.next_page_id;
        self.header.next_page_id = self.header.next_page_id.saturating_add(1);
        self.header.write_to(&mut self.mem, self.region_start)?;
        Ok(page_id)
    }

    fn free_page_id(&mut self, page_id: u32) -> Result<(), MemoryError> {
        if page_id == 0 || page_id == self.header.root_page_id {
            return Ok(());
        }
        let base = self.region_start + ABP_STORE_HEADER_LEN;
        let mut hdr = AbpNodeHeader::new_leaf();
        hdr.kind = AbpNodeKind::Free;
        hdr.used_bytes = ABP_NODE_HEADER_LEN;
        hdr.prev_page_id = 0;
        hdr.next_page_id = self.header.free_list_head_page_id;
        hdr.write_to_page(&mut self.mem, base, page_id, self.header.page_size)?;
        self.header.free_list_head_page_id = page_id;
        self.header.write_to(&mut self.mem, self.region_start)?;
        Ok(())
    }

    fn free_nonroot_internal_pages(&mut self) -> Result<(), MemoryError> {
        let mut ids = Vec::new();
        for page_id in 1..self.header.next_page_id {
            if page_id == self.header.root_page_id {
                continue;
            }
            if let Some((hdr, _)) = self.read_node(page_id)
                && hdr.kind == AbpNodeKind::Internal
            {
                ids.push(page_id);
            }
        }
        for id in ids {
            self.free_page_id(id)?;
        }
        Ok(())
    }

    fn rebuild_root_index_levels_from_leaf_chain(
        &mut self,
        leaf_ids: &[u32],
    ) -> Result<(), MemoryError> {
        let mut leaf_first_keys = Vec::with_capacity(leaf_ids.len());
        for &leaf_id in leaf_ids {
            leaf_first_keys.push(self.first_key_of_leaf(leaf_id)?);
        }

        let mut current_child_ids = leaf_ids.to_vec();
        let mut current_child_first_keys = leaf_first_keys;
        let mut child_level = 0u16; // children are leaves at level 0

        loop {
            let leftmost = *current_child_ids.first().ok_or(MemoryError::GrowOverflow)?;
            let entries = build_internal_entries_from_children(
                &current_child_ids,
                &current_child_first_keys,
            )?;
            if self.internal_payload_fits(leftmost, &entries) {
                return self.write_internal_page_payload(
                    self.header.root_page_id,
                    child_level.saturating_add(1),
                    leftmost,
                    &entries,
                );
            }

            let groups = self.partition_children_for_internal_pages(
                &current_child_ids,
                &current_child_first_keys,
            )?;
            if groups.len() <= 1 {
                return Err(MemoryError::GrowOverflow);
            }

            let mut next_child_ids = Vec::with_capacity(groups.len());
            let mut next_child_first_keys = Vec::with_capacity(groups.len());
            for (group_ids, group_first_keys) in groups {
                let internal_page_id = self.alloc_page_id()?;
                let group_leftmost = group_ids[0];
                let group_entries =
                    build_internal_entries_from_children(&group_ids, &group_first_keys)?;
                self.write_internal_page_payload(
                    internal_page_id,
                    child_level.saturating_add(1),
                    group_leftmost,
                    &group_entries,
                )?;
                next_child_ids.push(internal_page_id);
                next_child_first_keys.push(group_first_keys[0].clone());
            }
            current_child_ids = next_child_ids;
            current_child_first_keys = next_child_first_keys;
            child_level = child_level.saturating_add(1);
        }
    }

    fn internal_payload_fits(
        &self,
        leftmost_child_page_id: u32,
        entries: &[AbpInternalEntry],
    ) -> bool {
        let payload = match encode_internal_payload(leftmost_child_page_id, entries) {
            Ok(p) => p,
            Err(_) => return false,
        };
        usize::from(ABP_NODE_HEADER_LEN) + payload.len() <= self.header.page_size as usize
    }

    #[allow(clippy::type_complexity)]
    fn partition_children_for_internal_pages(
        &self,
        child_ids: &[u32],
        child_first_keys: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u32>, Vec<Vec<u8>>)>, MemoryError> {
        let mut groups: Vec<(Vec<u32>, Vec<Vec<u8>>)> = Vec::new();
        let mut cur_ids: Vec<u32> = Vec::new();
        let mut cur_keys: Vec<Vec<u8>> = Vec::new();
        for (idx, child_id) in child_ids.iter().copied().enumerate() {
            let child_key = child_first_keys[idx].clone();
            let mut trial_ids = cur_ids.clone();
            let mut trial_keys = cur_keys.clone();
            trial_ids.push(child_id);
            trial_keys.push(child_key.clone());
            let leftmost = trial_ids[0];
            let entries = build_internal_entries_from_children(&trial_ids, &trial_keys)?;
            if !trial_ids.is_empty()
                && !self.internal_payload_fits(leftmost, &entries)
                && !cur_ids.is_empty()
            {
                groups.push((cur_ids, cur_keys));
                cur_ids = vec![child_id];
                cur_keys = vec![child_key];
            } else {
                cur_ids = trial_ids;
                cur_keys = trial_keys;
            }
        }
        if !cur_ids.is_empty() {
            groups.push((cur_ids, cur_keys));
        }
        Ok(groups)
    }

    fn first_key_of_leaf(&self, leaf_id: u32) -> Result<Vec<u8>, MemoryError> {
        let (_, payload) = self.read_leaf(leaf_id).ok_or(MemoryError::GrowOverflow)?;
        scan_leaf_records(&payload)
            .next()
            .map(|(k, _, _)| k.to_vec())
            .ok_or(MemoryError::GrowOverflow)
    }
}

fn encode_leaf_record(key: &[u8], value: &[u8], tomb: bool) -> Result<Vec<u8>, MemoryError> {
    let klen = u16::try_from(key.len()).map_err(|_| MemoryError::GrowOverflow)?;
    let vlen = u16::try_from(value.len()).map_err(|_| MemoryError::GrowOverflow)?;
    let mut out = Vec::with_capacity(ABP_LEAF_REC_HEADER_LEN + key.len() + value.len());
    out.extend_from_slice(&klen.to_le_bytes());
    out.extend_from_slice(&vlen.to_le_bytes());
    out.push(if tomb { ABP_REC_FLAG_TOMBSTONE } else { 0 });
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    Ok(out)
}

fn encode_leaf_records_payload(recs: &[(Vec<u8>, Vec<u8>, bool)]) -> Result<Vec<u8>, MemoryError> {
    let mut out = Vec::new();
    for (k, v, tomb) in recs {
        out.extend_from_slice(&encode_leaf_record(k, v, *tomb)?);
    }
    Ok(out)
}

fn encoded_leaf_records_len(recs: &[(Vec<u8>, Vec<u8>, bool)]) -> Result<usize, MemoryError> {
    recs.iter()
        .map(|(k, v, tomb)| encode_leaf_record(k, v, *tomb).map(|r| r.len()))
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.into_iter().sum())
}

fn decode_leaf_records_vec(payload: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, bool)> {
    scan_leaf_records(payload)
        .map(|(k, v, tomb)| (k.to_vec(), v.to_vec(), tomb))
        .collect()
}

#[allow(clippy::type_complexity)]
fn split_leaf_records_balanced(
    recs: &[(Vec<u8>, Vec<u8>, bool)],
) -> Result<(Vec<(Vec<u8>, Vec<u8>, bool)>, Vec<(Vec<u8>, Vec<u8>, bool)>), MemoryError> {
    if recs.len() < 2 {
        return Err(MemoryError::GrowOverflow);
    }
    let total = encoded_leaf_records_len(recs)?;
    let target = total / 2;
    let mut acc = 0usize;
    let mut split_at = 1usize;
    for (idx, (k, v, tomb)) in recs.iter().enumerate().take(recs.len() - 1) {
        acc += encode_leaf_record(k, v, *tomb)?.len();
        split_at = idx + 1;
        if acc >= target {
            break;
        }
    }
    Ok((recs[..split_at].to_vec(), recs[split_at..].to_vec()))
}

pub fn encode_internal_payload(
    leftmost_child_page_id: u32,
    entries: &[AbpInternalEntry],
) -> Result<Vec<u8>, MemoryError> {
    let mut out = Vec::new();
    out.extend_from_slice(&leftmost_child_page_id.to_le_bytes());
    out.extend_from_slice(
        &u16::try_from(entries.len())
            .map_err(|_| MemoryError::GrowOverflow)?
            .to_le_bytes(),
    );
    out.extend_from_slice(&0u16.to_le_bytes());
    for e in entries {
        let klen = u16::try_from(e.separator_key.len()).map_err(|_| MemoryError::GrowOverflow)?;
        out.extend_from_slice(&klen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.child_page_id.to_le_bytes());
        out.extend_from_slice(&e.separator_key);
    }
    Ok(out)
}

pub fn decode_internal_payload(payload: &[u8]) -> Option<(u32, Vec<AbpInternalEntry>)> {
    if payload.len() < ABP_INTERNAL_PAYLOAD_HEADER_LEN {
        return None;
    }
    let leftmost_child_page_id = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let entry_count = u16::from_le_bytes(payload[4..6].try_into().ok()?) as usize;
    let mut cur = ABP_INTERNAL_PAYLOAD_HEADER_LEN;
    let mut out = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if cur + ABP_INTERNAL_ENTRY_HEADER_LEN > payload.len() {
            return None;
        }
        let klen = u16::from_le_bytes(payload[cur..cur + 2].try_into().ok()?) as usize;
        let child_page_id = u32::from_le_bytes(payload[cur + 4..cur + 8].try_into().ok()?);
        cur += ABP_INTERNAL_ENTRY_HEADER_LEN;
        if cur + klen > payload.len() {
            return None;
        }
        out.push(AbpInternalEntry {
            separator_key: payload[cur..cur + klen].to_vec(),
            child_page_id,
        });
        cur += klen;
    }
    Some((leftmost_child_page_id, out))
}

fn build_internal_entries_from_children(
    child_ids: &[u32],
    child_first_keys: &[Vec<u8>],
) -> Result<Vec<AbpInternalEntry>, MemoryError> {
    if child_ids.len() != child_first_keys.len() || child_ids.is_empty() {
        return Err(MemoryError::GrowOverflow);
    }
    let mut entries = Vec::new();
    for idx in 1..child_ids.len() {
        entries.push(AbpInternalEntry {
            separator_key: child_first_keys[idx].clone(),
            child_page_id: child_ids[idx],
        });
    }
    Ok(entries)
}

pub fn choose_child_from_internal(
    leftmost_child_page_id: u32,
    entries: &[AbpInternalEntry],
    search_key: &[u8],
) -> u32 {
    let mut child = leftmost_child_page_id;
    for e in entries {
        if search_key < e.separator_key.as_slice() {
            break;
        }
        child = e.child_page_id;
    }
    child
}

fn scan_leaf_records(payload: &[u8]) -> impl Iterator<Item = (&[u8], &[u8], bool)> {
    let mut out: Vec<(&[u8], &[u8], bool)> = Vec::new();
    let mut cur = 0usize;
    while cur + ABP_LEAF_REC_HEADER_LEN <= payload.len() {
        let klen = u16::from_le_bytes([payload[cur], payload[cur + 1]]) as usize;
        let vlen = u16::from_le_bytes([payload[cur + 2], payload[cur + 3]]) as usize;
        let flags = payload[cur + 4];
        let rec_len = ABP_LEAF_REC_HEADER_LEN + klen + vlen;
        if cur + rec_len > payload.len() {
            break;
        }
        let key_start = cur + ABP_LEAF_REC_HEADER_LEN;
        let val_start = key_start + klen;
        out.push((
            &payload[key_start..val_start],
            &payload[val_start..val_start + vlen],
            (flags & ABP_REC_FLAG_TOMBSTONE) != 0,
        ));
        cur += rec_len;
    }
    out.into_iter()
}

fn ensure_size<M: Memory>(mem: &mut M, required: u64) -> Result<(), MemoryError> {
    let cur = mem.size_bytes();
    if required > cur {
        mem.grow(required - cur)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::VecMemory;

    #[test]
    fn abp_store_header_round_trip_and_format_detection() {
        let mut mem = VecMemory::with_size(128);
        assert_eq!(
            detect_property_store_format(&mem, 0),
            PropertyStoreFormatTag::AppendLog
        );

        let hdr = AbpStoreHeader {
            root_page_id: 7,
            next_page_id: 11,
            free_list_head_page_id: 3,
            ..AbpStoreHeader::default()
        };
        hdr.write_to(&mut mem, 0).unwrap();
        let read = AbpStoreHeader::read_from(&mem, 0).expect("header");
        assert_eq!(read, hdr);
        assert_eq!(
            detect_property_store_format(&mem, 0),
            PropertyStoreFormatTag::AbPlusTree
        );
    }

    #[test]
    fn abp_node_header_round_trip_for_leaf_and_internal() {
        let mut mem = VecMemory::with_size(ABP_PAGE_SIZE as usize * 4);
        let mut leaf = AbpNodeHeader::new_leaf();
        leaf.key_count = 12;
        leaf.used_bytes = 640;
        leaf.next_page_id = 2;
        leaf.write_to_page(&mut mem, 0, 1, ABP_PAGE_SIZE).unwrap();
        let leaf_read =
            AbpNodeHeader::read_from_page(&mem, 0, 1, ABP_PAGE_SIZE).expect("leaf header");
        assert_eq!(leaf_read, leaf);

        let mut internal = AbpNodeHeader::new_internal(2);
        internal.key_count = 3;
        internal.used_bytes = 128;
        internal.prev_page_id = 9;
        internal
            .write_to_page(&mut mem, 0, 3, ABP_PAGE_SIZE)
            .unwrap();
        let internal_read =
            AbpNodeHeader::read_from_page(&mem, 0, 3, ABP_PAGE_SIZE).expect("internal header");
        assert_eq!(internal_read, internal);
    }

    #[test]
    fn abp_byte_kv_root_leaf_core_ops_work() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();
        assert_eq!(kv.get(b"k1"), None);

        kv.upsert(b"k1", b"v1").unwrap();
        kv.upsert(b"k2", b"v2").unwrap();
        kv.upsert(b"ka", b"va").unwrap();
        assert_eq!(kv.get(b"k1"), Some(b"v1".to_vec()));
        assert_eq!(kv.get(b"k2"), Some(b"v2".to_vec()));

        let scan = kv.scan_prefix(b"k");
        assert_eq!(
            scan,
            vec![
                (b"k1".to_vec(), b"v1".to_vec()),
                (b"k2".to_vec(), b"v2".to_vec()),
                (b"ka".to_vec(), b"va".to_vec())
            ]
        );

        kv.delete(b"k2").unwrap();
        assert_eq!(kv.get(b"k2"), None);
        let scan = kv.scan_prefix(b"k");
        assert_eq!(
            scan,
            vec![
                (b"k1".to_vec(), b"v1".to_vec()),
                (b"ka".to_vec(), b"va".to_vec())
            ]
        );

        let mem = kv.into_memory();
        let kv2 = AbpByteKv::open(mem, 0).unwrap();
        assert_eq!(kv2.get(b"k1"), Some(b"v1".to_vec()));
        assert_eq!(kv2.get(b"k2"), None);
    }

    #[test]
    fn abp_byte_kv_spills_into_multiple_leaf_pages_and_scans_chain() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();

        // Force multiple pages by inserting many medium-sized values.
        for i in 0..48u32 {
            let k = format!("k{i:03}");
            let v = vec![b'x'; 120];
            kv.upsert(k.as_bytes(), &v).unwrap();
        }

        // Spot-check reads across the chain.
        assert_eq!(kv.get(b"k000"), Some(vec![b'x'; 120]));
        assert_eq!(kv.get(b"k047"), Some(vec![b'x'; 120]));
        let (root_hdr, _) = kv.read_node(kv.header.root_page_id).expect("root");
        assert_eq!(root_hdr.kind, AbpNodeKind::Internal);

        let scan = kv.scan_prefix(b"k0");
        assert_eq!(scan.len(), 48);
        assert!(scan.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn abp_byte_kv_reuses_freed_pages_via_free_list() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();
        let allocated = kv.alloc_page_id().unwrap();
        let next_before_free = kv.header.next_page_id;
        kv.free_page_id(allocated).unwrap();
        assert_eq!(kv.header.free_list_head_page_id, allocated);

        let reused = kv.alloc_page_id().unwrap();
        assert_eq!(reused, allocated);
        assert_eq!(kv.header.next_page_id, next_before_free);
    }

    #[test]
    fn abp_byte_kv_compact_is_idempotent_and_preserves_data() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();
        for i in 0..40u32 {
            let k = format!("k{i:03}");
            kv.upsert(k.as_bytes(), &[b'x'; 96]).unwrap();
        }
        for i in (0..40u32).step_by(3) {
            let k = format!("k{i:03}");
            kv.delete(k.as_bytes()).unwrap();
        }

        let before = kv.scan_prefix(b"k");
        kv.compact().unwrap();
        let after1 = kv.scan_prefix(b"k");
        kv.compact().unwrap();
        let after2 = kv.scan_prefix(b"k");

        assert_eq!(after1, before);
        assert_eq!(after2, before);
    }

    #[test]
    fn abp_byte_kv_page_local_split_and_merge_paths_preserve_reads() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();

        for i in 0..48u32 {
            let k = format!("k{i:03}");
            kv.upsert(k.as_bytes(), &[b'x'; 120]).unwrap();
        }
        let (root_hdr, _) = kv.read_node(kv.header.root_page_id).unwrap();
        assert_eq!(root_hdr.kind, AbpNodeKind::Internal);
        let next_before_deletes = kv.header.next_page_id;

        for i in 1..48u32 {
            let k = format!("k{i:03}");
            kv.delete(k.as_bytes()).unwrap();
        }
        assert_eq!(kv.get(b"k000"), Some(vec![b'x'; 120]));
        assert_eq!(kv.get(b"k047"), None);
        assert!(
            kv.header.free_list_head_page_id != 0
                || kv.header.next_page_id < next_before_deletes + 2
        );
    }

    #[test]
    fn abp_byte_kv_builds_two_level_root_when_root_internal_payload_overflows() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();
        // Many tiny rows force many leaves; enough separators to overflow a single root internal page.
        for i in 0..1500u32 {
            let k = format!("k{i:04}");
            kv.upsert(k.as_bytes(), b"v").unwrap();
        }
        let (root_hdr, root_payload) = kv.read_node(kv.header.root_page_id).unwrap();
        assert_eq!(root_hdr.kind, AbpNodeKind::Internal);
        assert!(root_hdr.level >= 1);
        if root_hdr.level == 2 {
            let (leftmost_child, root_entries) = decode_internal_payload(&root_payload).unwrap();
            let (child_hdr, _child_payload) = kv.read_node(leftmost_child).unwrap();
            assert_eq!(child_hdr.kind, AbpNodeKind::Internal);
            assert_eq!(child_hdr.level, 1);
            assert!(!root_entries.is_empty());
        }
        assert_eq!(kv.get(b"k0000"), Some(b"v".to_vec()));
        assert_eq!(kv.get(b"k1499"), Some(b"v".to_vec()));
        let pref = kv.scan_prefix(b"k149");
        assert!(!pref.is_empty());
    }

    #[test]
    fn abp_byte_kv_page_local_updates_work_under_multi_level_root() {
        let mem = VecMemory::default();
        let mut kv = AbpByteKv::create(mem, 0).unwrap();
        let pad = "x".repeat(96);
        for i in 0..2000u32 {
            let k = format!("k{i:04}-{pad}");
            kv.upsert(k.as_bytes(), b"v").unwrap();
        }
        let (root_hdr, _) = kv.read_node(kv.header.root_page_id).unwrap();
        assert!(root_hdr.level >= 2);

        let k420 = format!("k0420-{pad}");
        let k421 = format!("k0421-{pad}");
        let k1999 = format!("k1999-{pad}");
        kv.upsert(k420.as_bytes(), b"vv").unwrap();
        kv.delete(k421.as_bytes()).unwrap();

        assert_eq!(kv.get(k420.as_bytes()), Some(b"vv".to_vec()));
        assert_eq!(kv.get(k421.as_bytes()), None);
        assert_eq!(kv.get(k1999.as_bytes()), Some(b"v".to_vec()));
    }

    #[test]
    fn internal_payload_codec_and_child_selection_work() {
        let entries = vec![
            AbpInternalEntry {
                separator_key: b"k100".to_vec(),
                child_page_id: 11,
            },
            AbpInternalEntry {
                separator_key: b"k200".to_vec(),
                child_page_id: 12,
            },
        ];
        let enc = encode_internal_payload(10, &entries).unwrap();
        let (leftmost, dec) = decode_internal_payload(&enc).expect("decode");
        assert_eq!(leftmost, 10);
        assert_eq!(dec, entries);
        assert_eq!(choose_child_from_internal(leftmost, &dec, b"k050"), 10);
        assert_eq!(choose_child_from_internal(leftmost, &dec, b"k100"), 11);
        assert_eq!(choose_child_from_internal(leftmost, &dec, b"k150"), 11);
        assert_eq!(choose_child_from_internal(leftmost, &dec, b"k999"), 12);
    }
}
