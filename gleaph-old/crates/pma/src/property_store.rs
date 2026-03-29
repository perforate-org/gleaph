use crate::{
    abp_tree::{AbpByteKv, PropertyStoreFormatTag, detect_property_store_format},
    memory::{Memory, MemoryError},
};
use gleaph_types::{PathElement, PropertyMap, Value};
use rapidhash::fast::RapidHashMap;

const FLAG_TOMBSTONE: u8 = 0x01;

#[derive(Clone, Debug)]
pub struct PropertyStore<M: Memory> {
    mem: M,
    log_start: u64,
    log_end: u64,
    index: RapidHashMap<Vec<u8>, IndexEntry>,
}

#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    offset: u64,
    tombstone: bool,
}

#[derive(Clone, Debug)]
pub struct AbpPropertyStore<M: Memory> {
    kv: AbpByteKv<M>,
}

#[derive(Clone, Debug)]
pub struct AbpSecondaryEqIndex<M: Memory> {
    kv: AbpByteKv<M>,
}

#[derive(Clone, Debug)]
pub enum PropertyStoreRuntime<M: Memory> {
    AppendLog(PropertyStore<M>),
    AbPlusLeaf(AbpPropertyStore<M>),
}

impl<M: Memory> PropertyStore<M> {
    /// Detects the on-memory property-store format tag at `region_start`.
    ///
    /// If no `(a,b)+ tree` header is present yet, falls back to append-log format.
    pub fn detect_format(mem: &M, region_start: u64) -> PropertyStoreFormatTag {
        detect_property_store_format(mem, region_start)
    }

    pub fn new(mem: M, log_start: u64) -> Result<Self, MemoryError> {
        let mut store = Self {
            mem,
            log_start,
            log_end: log_start,
            index: RapidHashMap::default(),
        };
        store.ensure_size(log_start)?;
        Ok(store)
    }

    pub fn from_memory(mem: M, log_start: u64, log_end: u64) -> Result<Self, MemoryError> {
        let mut store = Self {
            mem,
            log_start,
            log_end,
            index: RapidHashMap::default(),
        };
        store.ensure_size(log_end)?;
        store.rebuild_index()?;
        Ok(store)
    }

    pub fn into_parts(self) -> (M, u64, u64) {
        (self.mem, self.log_start, self.log_end)
    }

    /// Clears append-log bytes and resets the active log span so the old log region can be reused
    /// after migrating data into an `(a,b)+ tree` backend.
    pub fn mark_log_region_reclaimable(&mut self) {
        if self.log_end > self.log_start {
            let len = (self.log_end - self.log_start) as usize;
            let zeros = vec![0u8; len];
            self.mem.write(self.log_start, &zeros);
        }
        self.log_end = self.log_start;
        self.index.clear();
    }

    pub fn migrate_to_abp_leaf(
        self,
        region_start: u64,
    ) -> Result<AbpPropertyStore<M>, MemoryError> {
        let (mem, log_start, log_end) = self.into_parts();
        let mut src = PropertyStore::from_memory(mem, log_start, log_end)?;
        let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut cur = src.log_start;
        while cur < src.log_end {
            let (key_len, value_len, flags) = src.read_header(cur)?;
            let total_len = 9u64 + key_len as u64 + value_len as u64;

            let mut key = vec![0u8; key_len as usize];
            src.mem.read(cur + 9, &mut key);

            if (flags & FLAG_TOMBSTONE) != 0 {
                ops.push((key, None));
            } else {
                let mut raw = vec![0u8; value_len as usize];
                if value_len > 0 {
                    src.mem.read(cur + 9 + key_len as u64, &mut raw);
                }
                ops.push((key, Some(raw)));
            }
            cur += total_len;
        }

        src.mark_log_region_reclaimable();
        let (mem, _, _) = src.into_parts();
        let mut out = AbpPropertyStore::new(mem, region_start)?;
        for (key, raw) in ops {
            if let Some(raw) = raw {
                out.kv.upsert(&key, &raw)?;
            } else {
                out.kv.delete(&key)?;
            }
        }
        Ok(out)
    }

    pub fn memory(&self) -> &M {
        &self.mem
    }

    pub fn memory_mut(&mut self) -> &mut M {
        &mut self.mem
    }

    pub fn log_end(&self) -> u64 {
        self.log_end
    }

    pub fn vertex_key(vertex_id: u32, prop_name: &str) -> Vec<u8> {
        format!("V:{vertex_id}:{prop_name}").into_bytes()
    }

    pub fn edge_id_key(edge_id: u32, prop_name: &str) -> Vec<u8> {
        format!("EI:{edge_id}:{prop_name}").into_bytes()
    }

    pub fn get_vertex_prop(&self, vertex_id: u32, prop_name: &str) -> Option<Value> {
        let key = Self::vertex_key(vertex_id, prop_name);
        self.get_raw(&key)
    }

    pub fn set_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let key = Self::vertex_key(vertex_id, prop_name);
        self.append_record(&key, Some(&value))
    }

    pub fn delete_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        let key = Self::vertex_key(vertex_id, prop_name);
        self.append_record(&key, None)
    }

    pub fn scan_vertex_props(&self, vertex_id: u32) -> PropertyMap {
        let prefix = format!("V:{vertex_id}:").into_bytes();
        self.scan_prefix(&prefix)
            .into_iter()
            .filter_map(|(k, v)| {
                let key = String::from_utf8(k).ok()?;
                let name = key.rsplit(':').next()?.to_string();
                Some((name, v))
            })
            .collect()
    }

    pub fn get_edge_prop_by_id(&self, edge_id: u32, prop_name: &str) -> Option<Value> {
        let key = Self::edge_id_key(edge_id, prop_name);
        self.get_raw(&key)
    }

    pub fn set_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let key = Self::edge_id_key(edge_id, prop_name);
        self.append_record(&key, Some(&value))
    }

    pub fn delete_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        let key = Self::edge_id_key(edge_id, prop_name);
        self.append_record(&key, None)
    }

    pub fn scan_edge_props_by_id(&self, edge_id: u32) -> PropertyMap {
        let prefix = format!("EI:{edge_id}:").into_bytes();
        self.scan_prefix(&prefix)
            .into_iter()
            .filter_map(|(k, v)| {
                let key = String::from_utf8(k).ok()?;
                let name = key.rsplit(':').next()?.to_string();
                Some((name, v))
            })
            .collect()
    }

    pub fn rebuild_index(&mut self) -> Result<(), MemoryError> {
        self.index.clear();
        let mut cur = self.log_start;
        while cur < self.log_end {
            let (key_len, value_len, flags) = self.read_header(cur)?;
            let total_len = 9u64 + key_len as u64 + value_len as u64;
            let mut key = vec![0u8; key_len as usize];
            self.mem.read(cur + 9, &mut key);
            self.index.insert(
                key,
                IndexEntry {
                    offset: cur,
                    tombstone: (flags & FLAG_TOMBSTONE) != 0,
                },
            );
            cur += total_len;
        }
        Ok(())
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Value)> {
        let mut out = Vec::new();
        for (key, entry) in &self.index {
            if entry.tombstone || !key.starts_with(prefix) {
                continue;
            }
            if let Some(value) = self.read_value_at(entry) {
                out.push((key.clone(), value));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn get_raw(&self, key: &[u8]) -> Option<Value> {
        let entry = self.index.get(key)?;
        if entry.tombstone {
            return None;
        }
        self.read_value_at(entry)
    }

    fn read_value_at(&self, entry: &IndexEntry) -> Option<Value> {
        let (key_len, value_len, flags) = self.read_header(entry.offset).ok()?;
        if (flags & FLAG_TOMBSTONE) != 0 || value_len == 0 {
            return None;
        }
        let mut bytes = vec![0u8; value_len as usize];
        self.mem.read(entry.offset + 9 + key_len as u64, &mut bytes);
        decode_value(&bytes).ok()
    }

    fn read_header(&self, offset: u64) -> Result<(u32, u32, u8), MemoryError> {
        let mut buf = [0u8; 9];
        self.mem.read(offset, &mut buf);
        let key_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let value_len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let flags = buf[8];
        Ok((key_len, value_len, flags))
    }

    fn append_record(&mut self, key: &[u8], value: Option<&Value>) -> Result<(), MemoryError> {
        let encoded = value.map(encode_value).transpose()?.unwrap_or_default();
        let key_len = u32::try_from(key.len()).map_err(|_| MemoryError::GrowOverflow)?;
        let value_len = u32::try_from(encoded.len()).map_err(|_| MemoryError::GrowOverflow)?;
        let total_len = 9u64 + key_len as u64 + value_len as u64;
        let start = self.log_end;
        self.ensure_size(start + total_len)?;

        let mut header = [0u8; 9];
        header[0..4].copy_from_slice(&key_len.to_le_bytes());
        header[4..8].copy_from_slice(&value_len.to_le_bytes());
        if value.is_none() {
            header[8] |= FLAG_TOMBSTONE;
        }
        self.mem.write(start, &header);
        self.mem.write(start + 9, key);
        if !encoded.is_empty() {
            self.mem.write(start + 9 + key_len as u64, &encoded);
        }

        self.log_end = start + total_len;
        self.index.insert(
            key.to_vec(),
            IndexEntry {
                offset: start,
                tombstone: value.is_none(),
            },
        );
        Ok(())
    }

    fn ensure_size(&mut self, required: u64) -> Result<(), MemoryError> {
        let cur = self.mem.size_bytes();
        if required > cur {
            self.mem.grow(required - cur)?;
        }
        Ok(())
    }
}

impl<M: Memory> AbpPropertyStore<M> {
    pub fn new(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::create(mem, region_start)?,
        })
    }

    pub fn from_memory(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::open(mem, region_start)?,
        })
    }

    pub fn into_memory(self) -> M {
        self.kv.into_memory()
    }

    pub fn compact(&mut self) -> Result<(), MemoryError> {
        self.kv.compact()
    }

    pub fn get_vertex_prop(&self, vertex_id: u32, prop_name: &str) -> Option<Value> {
        let key = PropertyStore::<M>::vertex_key(vertex_id, prop_name);
        self.get_raw(&key)
    }

    pub fn set_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let key = PropertyStore::<M>::vertex_key(vertex_id, prop_name);
        self.put_raw(&key, &value)
    }

    pub fn delete_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        let key = PropertyStore::<M>::vertex_key(vertex_id, prop_name);
        self.kv.delete(&key)
    }

    pub fn scan_vertex_props(&self, vertex_id: u32) -> PropertyMap {
        let prefix = format!("V:{vertex_id}:").into_bytes();
        self.scan_prefix(&prefix)
            .into_iter()
            .filter_map(|(k, v)| {
                let key = String::from_utf8(k).ok()?;
                let name = key.rsplit(':').next()?.to_string();
                Some((name, v))
            })
            .collect()
    }

    pub fn get_edge_prop_by_id(&self, edge_id: u32, prop_name: &str) -> Option<Value> {
        let key = PropertyStore::<M>::edge_id_key(edge_id, prop_name);
        self.get_raw(&key)
    }

    pub fn set_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let key = PropertyStore::<M>::edge_id_key(edge_id, prop_name);
        self.put_raw(&key, &value)
    }

    pub fn delete_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        let key = PropertyStore::<M>::edge_id_key(edge_id, prop_name);
        self.kv.delete(&key)
    }

    pub fn scan_edge_props_by_id(&self, edge_id: u32) -> PropertyMap {
        let prefix = format!("EI:{edge_id}:").into_bytes();
        self.scan_prefix(&prefix)
            .into_iter()
            .filter_map(|(k, v)| {
                let key = String::from_utf8(k).ok()?;
                let name = key.rsplit(':').next()?.to_string();
                Some((name, v))
            })
            .collect()
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Value)> {
        self.kv
            .scan_prefix(prefix)
            .into_iter()
            .filter_map(|(k, raw)| decode_value(&raw).ok().map(|v| (k, v)))
            .collect()
    }

    fn get_raw(&self, key: &[u8]) -> Option<Value> {
        let raw = self.kv.get(key)?;
        decode_value(&raw).ok()
    }

    fn put_raw(&mut self, key: &[u8], value: &Value) -> Result<(), MemoryError> {
        let raw = encode_value(value)?;
        self.kv.upsert(key, &raw)
    }
}

impl<M: Memory> AbpSecondaryEqIndex<M> {
    pub fn new(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::create(mem, region_start)?,
        })
    }

    pub fn from_memory(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::open(mem, region_start)?,
        })
    }

    pub fn into_memory(self) -> M {
        self.kv.into_memory()
    }

    pub fn add_vertex_eq(
        &mut self,
        property_name: &str,
        property_value: &Value,
        vertex_id: u32,
    ) -> Result<(), MemoryError> {
        let key = vertex_eq_index_key(property_name, property_value, vertex_id)?;
        self.kv.upsert(&key, &[])
    }

    pub fn remove_vertex_eq(
        &mut self,
        property_name: &str,
        property_value: &Value,
        vertex_id: u32,
    ) -> Result<(), MemoryError> {
        let key = vertex_eq_index_key(property_name, property_value, vertex_id)?;
        self.kv.delete(&key)
    }

    pub fn scan_vertices_eq(
        &self,
        property_name: &str,
        property_value: &Value,
    ) -> Result<Vec<u32>, MemoryError> {
        let prefix = vertex_eq_index_prefix(property_name, property_value)?;
        let mut out = Vec::new();
        for (k, _) in self.kv.scan_prefix(&prefix) {
            if let Some(vid) = decode_vertex_eq_index_key_vertex_id(&k, &prefix) {
                out.push(vid);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Range index operations
    // -----------------------------------------------------------------------

    pub fn add_vertex_range(
        &mut self,
        property_name: &str,
        property_value: &Value,
        vertex_id: u32,
    ) -> Result<(), MemoryError> {
        let key = vertex_range_index_key(property_name, property_value, vertex_id)?;
        self.kv.upsert(&key, &[])
    }

    pub fn remove_vertex_range(
        &mut self,
        property_name: &str,
        property_value: &Value,
        vertex_id: u32,
    ) -> Result<(), MemoryError> {
        let key = vertex_range_index_key(property_name, property_value, vertex_id)?;
        self.kv.delete(&key)
    }

    /// Scan vertices where `property >= value` (or >, <=, <).
    ///
    /// `cmp_op` semantics: Ge (>=), Gt (>), Le (<=), Lt (<).
    pub fn scan_vertices_range(
        &self,
        property_name: &str,
        bound_value: &Value,
        cmp_op: RangeOp,
    ) -> Result<Vec<u32>, MemoryError> {
        let prop_prefix = vertex_range_index_property_prefix(property_name)?;
        let val_prefix = vertex_range_index_value_prefix(property_name, bound_value)?;

        // Build start_key and end_key for the B+ tree scan_range call.
        // scan_range returns keys in [start_key, end_key).
        let (start_key, end_key) = match cmp_op {
            RangeOp::Ge => {
                // key >= val_prefix → [val_prefix, prop_prefix_end)
                (val_prefix, increment_prefix(&prop_prefix))
            }
            RangeOp::Gt => {
                // key > val_prefix → [val_prefix + 0xFF*4 + 1 ..., prop_prefix_end)
                // The max vertex_id suffix is 4 bytes (u32::MAX = 0xFFFFFFFF).
                // So any key with the same value prefix + vertex_id <= u32::MAX is
                // included in [val_prefix, val_prefix + 5 zero bytes).
                // We want keys AFTER the value, so start after all vertex_ids for this value.
                let mut after_val = val_prefix.clone();
                after_val.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                // increment to get the exclusive start
                let start = increment_prefix(&after_val);
                (start, increment_prefix(&prop_prefix))
            }
            RangeOp::Le => {
                // key <= val_prefix → [prop_prefix, val_prefix + 0xFF*4 + 1)
                let mut end = val_prefix.clone();
                end.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                (prop_prefix.clone(), increment_prefix(&end))
            }
            RangeOp::Lt => {
                // key < val_prefix → [prop_prefix, val_prefix)
                (prop_prefix.clone(), val_prefix)
            }
        };

        let entries = self.kv.scan_range(&prop_prefix, &start_key, &end_key);
        let mut out = Vec::new();
        let pp_len = prop_prefix.len();
        for (k, _) in &entries {
            if let Some(vid) = decode_vertex_range_index_key_vertex_id(k, pp_len) {
                out.push(vid);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    pub fn compact(&mut self) -> Result<(), MemoryError> {
        self.kv.compact()
    }
}

/// Comparison operator for range index scans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOp {
    Ge,
    Gt,
    Le,
    Lt,
}

/// Increment a byte vector to get the exclusive upper bound.
/// Returns a vector that is the smallest byte sequence greater than `prefix`.
fn increment_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut result = prefix.to_vec();
    // Carry-propagate increment from the last byte.
    for byte in result.iter_mut().rev() {
        if *byte < 0xFF {
            *byte += 1;
            return result;
        }
        *byte = 0x00;
    }
    // All bytes were 0xFF → append 0x00 (effectively prefix + 1 in length).
    result.push(0x00);
    result
}

impl<M: Memory> PropertyStoreRuntime<M> {
    pub fn open_auto(mem: M, region_start: u64, log_end: u64) -> Result<Self, MemoryError> {
        match PropertyStore::detect_format(&mem, region_start) {
            PropertyStoreFormatTag::AbPlusTree => Ok(Self::AbPlusLeaf(
                AbpPropertyStore::from_memory(mem, region_start)?,
            )),
            PropertyStoreFormatTag::AppendLog => Ok(Self::AppendLog(PropertyStore::from_memory(
                mem,
                region_start,
                log_end,
            )?)),
        }
    }

    pub fn new_append(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self::AppendLog(PropertyStore::new(mem, region_start)?))
    }

    pub fn new_abp_leaf(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self::AbPlusLeaf(AbpPropertyStore::new(mem, region_start)?))
    }

    pub fn detect_format(mem: &M, region_start: u64) -> PropertyStoreFormatTag {
        PropertyStore::detect_format(mem, region_start)
    }

    pub fn get_vertex_prop(&self, vertex_id: u32, prop_name: &str) -> Option<Value> {
        match self {
            Self::AppendLog(s) => s.get_vertex_prop(vertex_id, prop_name),
            Self::AbPlusLeaf(s) => s.get_vertex_prop(vertex_id, prop_name),
        }
    }

    pub fn set_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        match self {
            Self::AppendLog(s) => s.set_vertex_prop(vertex_id, prop_name, value),
            Self::AbPlusLeaf(s) => s.set_vertex_prop(vertex_id, prop_name, value),
        }
    }

    pub fn delete_vertex_prop(
        &mut self,
        vertex_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        match self {
            Self::AppendLog(s) => s.delete_vertex_prop(vertex_id, prop_name),
            Self::AbPlusLeaf(s) => s.delete_vertex_prop(vertex_id, prop_name),
        }
    }

    pub fn scan_vertex_props(&self, vertex_id: u32) -> PropertyMap {
        match self {
            Self::AppendLog(s) => s.scan_vertex_props(vertex_id),
            Self::AbPlusLeaf(s) => s.scan_vertex_props(vertex_id),
        }
    }

    pub fn get_edge_prop_by_id(&self, edge_id: u32, prop_name: &str) -> Option<Value> {
        match self {
            Self::AppendLog(s) => s.get_edge_prop_by_id(edge_id, prop_name),
            Self::AbPlusLeaf(s) => s.get_edge_prop_by_id(edge_id, prop_name),
        }
    }

    pub fn set_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        match self {
            Self::AppendLog(s) => s.set_edge_prop_by_id(edge_id, prop_name, value),
            Self::AbPlusLeaf(s) => s.set_edge_prop_by_id(edge_id, prop_name, value),
        }
    }

    pub fn delete_edge_prop_by_id(
        &mut self,
        edge_id: u32,
        prop_name: &str,
    ) -> Result<(), MemoryError> {
        match self {
            Self::AppendLog(s) => s.delete_edge_prop_by_id(edge_id, prop_name),
            Self::AbPlusLeaf(s) => s.delete_edge_prop_by_id(edge_id, prop_name),
        }
    }

    pub fn scan_edge_props_by_id(&self, edge_id: u32) -> PropertyMap {
        match self {
            Self::AppendLog(s) => s.scan_edge_props_by_id(edge_id),
            Self::AbPlusLeaf(s) => s.scan_edge_props_by_id(edge_id),
        }
    }

    pub fn migrate_append_to_abp_leaf(self, region_start: u64) -> Result<Self, MemoryError> {
        match self {
            Self::AppendLog(s) => Ok(Self::AbPlusLeaf(s.migrate_to_abp_leaf(region_start)?)),
            Self::AbPlusLeaf(s) => Ok(Self::AbPlusLeaf(s)),
        }
    }

    pub fn compact(&mut self) -> Result<(), MemoryError> {
        match self {
            Self::AppendLog(_) => Ok(()),
            Self::AbPlusLeaf(s) => s.compact(),
        }
    }
}

pub fn encode_value(value: &Value) -> Result<Vec<u8>, MemoryError> {
    let mut out = Vec::new();
    encode_value_into(value, &mut out)?;
    Ok(out)
}

fn vertex_eq_index_prefix(
    property_name: &str,
    property_value: &Value,
) -> Result<Vec<u8>, MemoryError> {
    let prop = property_name.as_bytes();
    let prop_len = u16::try_from(prop.len()).map_err(|_| MemoryError::GrowOverflow)?;
    let enc = encode_value(property_value)?;
    let val_len = u32::try_from(enc.len()).map_err(|_| MemoryError::GrowOverflow)?;
    let mut out = Vec::with_capacity(3 + 2 + prop.len() + 4 + enc.len());
    out.extend_from_slice(b"IVE");
    out.extend_from_slice(&prop_len.to_be_bytes());
    out.extend_from_slice(prop);
    out.extend_from_slice(&val_len.to_be_bytes());
    out.extend_from_slice(&enc);
    Ok(out)
}

fn vertex_eq_index_key(
    property_name: &str,
    property_value: &Value,
    vertex_id: u32,
) -> Result<Vec<u8>, MemoryError> {
    let mut out = vertex_eq_index_prefix(property_name, property_value)?;
    out.extend_from_slice(&vertex_id.to_be_bytes());
    Ok(out)
}

fn decode_vertex_eq_index_key_vertex_id(key: &[u8], prefix: &[u8]) -> Option<u32> {
    if !key.starts_with(prefix) || key.len() != prefix.len() + 4 {
        return None;
    }
    Some(u32::from_be_bytes(key[prefix.len()..].try_into().ok()?))
}

// ---------------------------------------------------------------------------
// Range index key helpers
// ---------------------------------------------------------------------------
//
// Range index keys use order-preserving encoding so that byte-level comparison
// in the ABP B+ tree corresponds to value ordering.
//
// Key format: "IVR" + prop_len_be(2) + prop_name + type_tag(1) + order_preserving_value + vid_be(4)
//
// Order-preserving encoding rules:
// - Int (i64):   XOR with 0x8000_0000_0000_0000, then big-endian
// - Float (f64): IEEE 754 total-order trick, then big-endian
// - Text:        raw UTF-8 bytes (naturally lexicographic)
// - Timestamp:   big-endian u64
// - Date:        XOR sign bit, big-endian i32
// - Time:        big-endian u64
// - DateTime:    XOR sign bit big-endian i64 + big-endian u32
// - Duration:    XOR sign bit big-endian i32 + XOR sign bit big-endian i64
// - Bool:        0x00 / 0x01
// - Bytes:       raw bytes
// - Principal:   raw bytes

/// Encode a value in order-preserving format suitable for range index keys.
pub fn encode_value_ordered(value: &Value) -> Result<Vec<u8>, MemoryError> {
    let mut out = Vec::new();
    encode_value_ordered_into(value, &mut out)?;
    Ok(out)
}

fn encode_value_ordered_into(value: &Value, out: &mut Vec<u8>) -> Result<(), MemoryError> {
    match value {
        Value::Null => out.push(0),
        Value::Bool(v) => {
            out.push(1);
            out.push(u8::from(*v));
        }
        Value::Int64(v) => {
            out.push(2);
            // Flip sign bit so that negative < positive in unsigned byte order.
            let u = (*v as u64) ^ 0x8000_0000_0000_0000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Float64(v) => {
            out.push(3);
            let bits = v.to_bits();
            // IEEE 754 total-order trick: if sign bit set, flip all; else flip sign bit.
            let ordered = if bits & (1u64 << 63) != 0 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Timestamp(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Date(d) => {
            out.push(9);
            let u = (*d as u32) ^ 0x8000_0000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Time(t) => {
            out.push(10);
            out.extend_from_slice(&t.to_be_bytes());
        }
        Value::DateTime(secs, sub) => {
            out.push(11);
            let u = (*secs as u64) ^ 0x8000_0000_0000_0000;
            out.extend_from_slice(&u.to_be_bytes());
            out.extend_from_slice(&sub.to_be_bytes());
        }
        Value::Duration(months, nanos) => {
            out.push(12);
            let um = (*months as u32) ^ 0x8000_0000;
            out.extend_from_slice(&um.to_be_bytes());
            let un = (*nanos as u64) ^ 0x8000_0000_0000_0000;
            out.extend_from_slice(&un.to_be_bytes());
        }
        Value::Bytes(b) => {
            out.push(8);
            out.extend_from_slice(b);
        }
        Value::Principal(p) => {
            out.push(13);
            out.extend_from_slice(p.as_slice());
        }
        Value::Decimal(d) => {
            out.push(14);
            // Normalize to canonical form, then convert to i128 at max scale (28)
            // so that equivalent decimals produce the same byte encoding.
            let norm = d.0.normalize();
            let scale = norm.scale();
            let mantissa = norm.mantissa();
            // Scale to a fixed scale of 28 for uniform comparison.
            let factor = 10i128.pow(28 - scale);
            let canonical = mantissa.checked_mul(factor).unwrap_or(mantissa);
            // XOR sign bit for order-preserving unsigned comparison.
            let u = (canonical as u128) ^ (1u128 << 127);
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Uint64(v) => {
            out.push(15);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Int8(v) => {
            out.push(16);
            let u = (*v as u8) ^ 0x80;
            out.push(u);
        }
        Value::Int16(v) => {
            out.push(17);
            let u = (*v as u16) ^ 0x8000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Int32(v) => {
            out.push(18);
            let u = (*v as u32) ^ 0x8000_0000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Int128(v) => {
            out.push(19);
            let u = (*v as u128) ^ (1u128 << 127);
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Int256(v) => {
            out.push(20);
            let mut be = v.0.to_be_bytes();
            // XOR the sign bit (most significant byte, bit 7)
            be[0] ^= 0x80;
            out.extend_from_slice(&be);
        }
        Value::Uint8(v) => {
            out.push(21);
            out.push(*v);
        }
        Value::Uint16(v) => {
            out.push(22);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Uint32(v) => {
            out.push(23);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Uint128(v) => {
            out.push(24);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Uint256(v) => {
            out.push(25);
            out.extend_from_slice(&v.0.to_be_bytes());
        }
        Value::Float32(v) => {
            out.push(26);
            let bits = v.to_bits();
            // IEEE 754 total-order trick (same as Float64 but 32-bit):
            // if sign bit set, flip all; else flip sign bit.
            let ordered = if bits & (1u32 << 31) != 0 {
                !bits
            } else {
                bits ^ (1u32 << 31)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        // List/Path are not meaningfully orderable; encode type tag only.
        Value::List(_) | Value::Path(_) => {
            return Err(MemoryError::GrowOverflow);
        }
    }
    Ok(())
}

/// Build the property-only prefix for range index keys.
/// Format: "IVR" + prop_len_be(2) + prop_name
pub fn vertex_range_index_property_prefix(property_name: &str) -> Result<Vec<u8>, MemoryError> {
    let prop = property_name.as_bytes();
    let prop_len = u16::try_from(prop.len()).map_err(|_| MemoryError::GrowOverflow)?;
    let mut out = Vec::with_capacity(3 + 2 + prop.len());
    out.extend_from_slice(b"IVR");
    out.extend_from_slice(&prop_len.to_be_bytes());
    out.extend_from_slice(prop);
    Ok(out)
}

/// Build a range index key prefix up to (and including) the encoded value.
/// Format: "IVR" + prop_len_be(2) + prop_name + order_preserving_value
fn vertex_range_index_value_prefix(
    property_name: &str,
    property_value: &Value,
) -> Result<Vec<u8>, MemoryError> {
    let mut out = vertex_range_index_property_prefix(property_name)?;
    let enc = encode_value_ordered(property_value)?;
    out.extend_from_slice(&enc);
    Ok(out)
}

/// Build the full range index key including vertex_id suffix.
pub fn vertex_range_index_key(
    property_name: &str,
    property_value: &Value,
    vertex_id: u32,
) -> Result<Vec<u8>, MemoryError> {
    let mut out = vertex_range_index_value_prefix(property_name, property_value)?;
    out.extend_from_slice(&vertex_id.to_be_bytes());
    Ok(out)
}

fn decode_vertex_range_index_key_vertex_id(key: &[u8], prop_prefix_len: usize) -> Option<u32> {
    // The key must be at least prop_prefix + type_tag(1) + some_value + vid(4).
    if key.len() < prop_prefix_len + 1 + 4 {
        return None;
    }
    let vid_start = key.len() - 4;
    Some(u32::from_be_bytes(key[vid_start..].try_into().ok()?))
}

fn encode_value_into(value: &Value, out: &mut Vec<u8>) -> Result<(), MemoryError> {
    match value {
        Value::Null => out.push(0),
        Value::Bool(v) => {
            out.push(1);
            out.push(u8::from(*v));
        }
        Value::Int64(v) => {
            out.push(2);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Float64(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            let len = u32::try_from(s.len()).map_err(|_| MemoryError::GrowOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Timestamp(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::List(items) => {
            out.push(6);
            let len = u32::try_from(items.len()).map_err(|_| MemoryError::GrowOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            for item in items {
                encode_value_into(item, out)?;
            }
        }
        Value::Bytes(b) => {
            out.push(8);
            let len = u32::try_from(b.len()).map_err(|_| MemoryError::GrowOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Date(d) => {
            out.push(9);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Time(t) => {
            out.push(10);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::DateTime(secs, sub) => {
            out.push(11);
            out.extend_from_slice(&secs.to_le_bytes());
            out.extend_from_slice(&sub.to_le_bytes());
        }
        Value::Duration(months, nanos) => {
            out.push(12);
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&nanos.to_le_bytes());
        }
        Value::Path(items) => {
            out.push(7);
            let len = u32::try_from(items.len()).map_err(|_| MemoryError::GrowOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            for item in items {
                match item {
                    PathElement::Node(id) => {
                        out.push(0);
                        out.extend_from_slice(&id.to_le_bytes());
                    }
                    PathElement::Edge { src, dst, label } => {
                        out.push(1);
                        out.extend_from_slice(&src.to_le_bytes());
                        out.extend_from_slice(&dst.to_le_bytes());
                        match label {
                            Some(label) => {
                                out.push(1);
                                let len = u32::try_from(label.len())
                                    .map_err(|_| MemoryError::GrowOverflow)?;
                                out.extend_from_slice(&len.to_le_bytes());
                                out.extend_from_slice(label.as_bytes());
                            }
                            None => out.push(0),
                        }
                    }
                }
            }
        }
        Value::Principal(p) => {
            out.push(13);
            let bytes = p.as_slice();
            let len = u32::try_from(bytes.len()).map_err(|_| MemoryError::GrowOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Value::Decimal(d) => {
            out.push(14);
            out.extend_from_slice(&d.0.serialize());
        }
        Value::Uint64(v) => {
            out.push(15);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int8(v) => {
            out.push(16);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int16(v) => {
            out.push(17);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int32(v) => {
            out.push(18);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int128(v) => {
            out.push(19);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int256(v) => {
            out.push(20);
            out.extend_from_slice(&v.0.to_le_bytes());
        }
        Value::Uint8(v) => {
            out.push(21);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Uint16(v) => {
            out.push(22);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Uint32(v) => {
            out.push(23);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Uint128(v) => {
            out.push(24);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Uint256(v) => {
            out.push(25);
            out.extend_from_slice(&v.0.to_le_bytes());
        }
        Value::Float32(v) => {
            out.push(26);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    Ok(())
}

pub fn decode_value(bytes: &[u8]) -> Result<Value, MemoryError> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    let mut cur = 0usize;
    decode_value_at(bytes, &mut cur)
}

fn decode_value_at(bytes: &[u8], cur: &mut usize) -> Result<Value, MemoryError> {
    let tag = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
        offset: 0,
        len: *cur + 1,
    })?;
    *cur += 1;
    match tag {
        0 => Ok(Value::Null),
        1 => {
            let b = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
                offset: 0,
                len: *cur + 1,
            })?;
            *cur += 1;
            Ok(Value::Bool(b != 0))
        }
        2 => {
            let arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Int64(i64::from_le_bytes(arr)))
        }
        3 => {
            let arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Float64(f64::from_le_bytes(arr)))
        }
        4 => {
            let len_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let len = u32::from_le_bytes(len_arr) as usize;
            let s = bytes
                .get(*cur..*cur + len)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + len,
                })?;
            *cur += len;
            Ok(Value::Text(String::from_utf8_lossy(s).to_string()))
        }
        5 => {
            let arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Timestamp(u64::from_le_bytes(arr)))
        }
        6 => {
            let len_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let len = u32::from_le_bytes(len_arr) as usize;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(decode_value_at(bytes, cur)?);
            }
            Ok(Value::List(items))
        }
        7 => {
            let len_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let len = u32::from_le_bytes(len_arr) as usize;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                let etag = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 1,
                })?;
                *cur += 1;
                match etag {
                    0 => {
                        let arr: [u8; 4] = bytes
                            .get(*cur..*cur + 4)
                            .ok_or(MemoryError::OutOfBounds {
                                offset: 0,
                                len: *cur + 4,
                            })?
                            .try_into()
                            .unwrap();
                        *cur += 4;
                        items.push(PathElement::Node(u32::from_le_bytes(arr)));
                    }
                    1 => {
                        let src: [u8; 4] = bytes
                            .get(*cur..*cur + 4)
                            .ok_or(MemoryError::OutOfBounds {
                                offset: 0,
                                len: *cur + 4,
                            })?
                            .try_into()
                            .unwrap();
                        *cur += 4;
                        let dst: [u8; 4] = bytes
                            .get(*cur..*cur + 4)
                            .ok_or(MemoryError::OutOfBounds {
                                offset: 0,
                                len: *cur + 4,
                            })?
                            .try_into()
                            .unwrap();
                        *cur += 4;
                        let has_label = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
                            offset: 0,
                            len: *cur + 1,
                        })?;
                        *cur += 1;
                        let label = if has_label == 0 {
                            None
                        } else {
                            let len_arr: [u8; 4] = bytes
                                .get(*cur..*cur + 4)
                                .ok_or(MemoryError::OutOfBounds {
                                    offset: 0,
                                    len: *cur + 4,
                                })?
                                .try_into()
                                .unwrap();
                            *cur += 4;
                            let len = u32::from_le_bytes(len_arr) as usize;
                            let s =
                                bytes
                                    .get(*cur..*cur + len)
                                    .ok_or(MemoryError::OutOfBounds {
                                        offset: 0,
                                        len: *cur + len,
                                    })?;
                            *cur += len;
                            Some(String::from_utf8_lossy(s).to_string())
                        };
                        items.push(PathElement::Edge {
                            src: u32::from_le_bytes(src),
                            dst: u32::from_le_bytes(dst),
                            label,
                        });
                    }
                    _ => {
                        return Err(MemoryError::OutOfBounds {
                            offset: 0,
                            len: bytes.len(),
                        });
                    }
                }
            }
            Ok(Value::Path(items))
        }
        8 => {
            let len_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let len = u32::from_le_bytes(len_arr) as usize;
            let b = bytes
                .get(*cur..*cur + len)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + len,
                })?;
            *cur += len;
            Ok(Value::Bytes(b.to_vec()))
        }
        9 => {
            let arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            Ok(Value::Date(i32::from_le_bytes(arr)))
        }
        10 => {
            let arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Time(u64::from_le_bytes(arr)))
        }
        11 => {
            let secs_arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            let sub_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            Ok(Value::DateTime(
                i64::from_le_bytes(secs_arr),
                u32::from_le_bytes(sub_arr),
            ))
        }
        12 => {
            let months_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let nanos_arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Duration(
                i32::from_le_bytes(months_arr),
                i64::from_le_bytes(nanos_arr),
            ))
        }
        13 => {
            let len_arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            let len = u32::from_le_bytes(len_arr) as usize;
            let b = bytes
                .get(*cur..*cur + len)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + len,
                })?;
            *cur += len;
            Ok(Value::Principal(candid::Principal::from_slice(b)))
        }
        14 => {
            let arr: [u8; 16] = bytes
                .get(*cur..*cur + 16)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 16,
                })?
                .try_into()
                .unwrap();
            *cur += 16;
            Ok(Value::Decimal(gleaph_types::Decimal::new(
                rust_decimal::Decimal::deserialize(arr),
            )))
        }
        15 => {
            let arr: [u8; 8] = bytes
                .get(*cur..*cur + 8)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 8,
                })?
                .try_into()
                .unwrap();
            *cur += 8;
            Ok(Value::Uint64(u64::from_le_bytes(arr)))
        }
        16 => {
            let b = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
                offset: 0,
                len: *cur + 1,
            })?;
            *cur += 1;
            Ok(Value::Int8(b as i8))
        }
        17 => {
            let arr: [u8; 2] = bytes
                .get(*cur..*cur + 2)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 2,
                })?
                .try_into()
                .unwrap();
            *cur += 2;
            Ok(Value::Int16(i16::from_le_bytes(arr)))
        }
        18 => {
            let arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            Ok(Value::Int32(i32::from_le_bytes(arr)))
        }
        19 => {
            let arr: [u8; 16] = bytes
                .get(*cur..*cur + 16)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 16,
                })?
                .try_into()
                .unwrap();
            *cur += 16;
            Ok(Value::Int128(i128::from_le_bytes(arr)))
        }
        20 => {
            let arr: [u8; 32] = bytes
                .get(*cur..*cur + 32)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 32,
                })?
                .try_into()
                .unwrap();
            *cur += 32;
            Ok(Value::Int256(gleaph_types::Int256(
                ethnum::I256::from_le_bytes(arr),
            )))
        }
        21 => {
            let b = *bytes.get(*cur).ok_or(MemoryError::OutOfBounds {
                offset: 0,
                len: *cur + 1,
            })?;
            *cur += 1;
            Ok(Value::Uint8(b))
        }
        22 => {
            let arr: [u8; 2] = bytes
                .get(*cur..*cur + 2)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 2,
                })?
                .try_into()
                .unwrap();
            *cur += 2;
            Ok(Value::Uint16(u16::from_le_bytes(arr)))
        }
        23 => {
            let arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            Ok(Value::Uint32(u32::from_le_bytes(arr)))
        }
        24 => {
            let arr: [u8; 16] = bytes
                .get(*cur..*cur + 16)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 16,
                })?
                .try_into()
                .unwrap();
            *cur += 16;
            Ok(Value::Uint128(u128::from_le_bytes(arr)))
        }
        25 => {
            let arr: [u8; 32] = bytes
                .get(*cur..*cur + 32)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 32,
                })?
                .try_into()
                .unwrap();
            *cur += 32;
            Ok(Value::Uint256(gleaph_types::Uint256(
                ethnum::U256::from_le_bytes(arr),
            )))
        }
        26 => {
            let arr: [u8; 4] = bytes
                .get(*cur..*cur + 4)
                .ok_or(MemoryError::OutOfBounds {
                    offset: 0,
                    len: *cur + 4,
                })?
                .try_into()
                .unwrap();
            *cur += 4;
            Ok(Value::Float32(f32::from_le_bytes(arr)))
        }
        _ => Err(MemoryError::OutOfBounds {
            offset: 0,
            len: bytes.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::VecMemory;

    #[test]
    fn write_read_round_trip_for_all_types() {
        let mut store = PropertyStore::new(VecMemory::default(), 0).unwrap();
        let values = vec![
            ("n", Value::Null),
            ("b", Value::Bool(true)),
            ("i", Value::Int64(-42)),
            ("f", Value::Float64(3.5)),
            ("t", Value::Text("abc".into())),
            ("ts", Value::Timestamp(7)),
            (
                "l",
                Value::List(vec![Value::Int64(1), Value::Text("x".into())]),
            ),
            (
                "p",
                Value::Path(vec![
                    PathElement::Node(1),
                    PathElement::Edge {
                        src: 1,
                        dst: 2,
                        label: Some("X".into()),
                    },
                    PathElement::Node(2),
                ]),
            ),
            (
                "principal",
                Value::Principal(candid::Principal::from_text("aaaaa-aa").unwrap()),
            ),
        ];
        for (k, v) in &values {
            store.set_vertex_prop(1, k, v.clone()).unwrap();
        }
        for (k, v) in values {
            assert_eq!(store.get_vertex_prop(1, k), Some(v));
        }
    }

    #[test]
    fn scan_and_tombstone_filtering() {
        let mut store = PropertyStore::new(VecMemory::default(), 0).unwrap();
        store
            .set_vertex_prop(10, "name", Value::Text("A".into()))
            .unwrap();
        store.set_vertex_prop(10, "age", Value::Int64(20)).unwrap();
        store
            .set_vertex_prop(11, "name", Value::Text("B".into()))
            .unwrap();
        store.delete_vertex_prop(10, "age").unwrap();

        let props = store.scan_vertex_props(10);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].0, "name");
        assert_eq!(store.get_vertex_prop(10, "age"), None);
    }

    #[test]
    fn edge_prop_round_trip_and_scan() {
        let mut store = PropertyStore::new(VecMemory::default(), 0).unwrap();
        let edge_id = 7;
        store
            .set_edge_prop_by_id(edge_id, "since", Value::Timestamp(123))
            .unwrap();
        store
            .set_edge_prop_by_id(edge_id, "strength", Value::Float64(0.9))
            .unwrap();

        assert_eq!(
            store.get_edge_prop_by_id(edge_id, "since"),
            Some(Value::Timestamp(123))
        );
        assert_eq!(store.scan_edge_props_by_id(edge_id).len(), 2);
    }

    #[test]
    fn rebuild_index_from_log() {
        let mut store = PropertyStore::new(VecMemory::default(), 0).unwrap();
        store
            .set_vertex_prop(1, "name", Value::Text("A".into()))
            .unwrap();
        store.set_vertex_prop(1, "age", Value::Int64(1)).unwrap();
        store.delete_vertex_prop(1, "age").unwrap();
        let log_end = store.log_end();
        let (mem, log_start, _) = store.into_parts();

        let rebuilt = PropertyStore::from_memory(mem, log_start, log_end).unwrap();
        assert_eq!(
            rebuilt.get_vertex_prop(1, "name"),
            Some(Value::Text("A".into()))
        );
        assert_eq!(rebuilt.get_vertex_prop(1, "age"), None);
    }

    #[test]
    fn mark_log_region_reclaimable_clears_log_and_resets_span() {
        let mut store = PropertyStore::new(VecMemory::default(), 64).unwrap();
        store
            .set_vertex_prop(1, "name", Value::Text("A".into()))
            .unwrap();
        store.set_vertex_prop(1, "age", Value::Int64(1)).unwrap();
        let before_end = store.log_end();
        assert!(before_end > 64);

        store.mark_log_region_reclaimable();
        assert_eq!(store.log_end(), 64);
        assert!(store.get_vertex_prop(1, "name").is_none());

        let (mem, start, end) = store.into_parts();
        assert_eq!(start, 64);
        assert_eq!(end, 64);
        let mut buf = vec![0u8; (before_end - start) as usize];
        mem.read(start, &mut buf);
        assert!(buf.iter().all(|b| *b == 0));
    }

    #[test]
    fn abp_property_store_round_trip_and_scan() {
        let mut store = AbpPropertyStore::new(VecMemory::default(), 0).unwrap();
        store
            .set_vertex_prop(10, "name", Value::Text("Alice".into()))
            .unwrap();
        store.set_vertex_prop(10, "age", Value::Int64(20)).unwrap();
        store
            .set_edge_prop_by_id(7, "since", Value::Timestamp(123))
            .unwrap();
        assert_eq!(
            store.get_vertex_prop(10, "name"),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(store.get_vertex_prop(10, "age"), Some(Value::Int64(20)));
        let mut props = store.scan_vertex_props(10);
        props.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(props.len(), 2);
        assert_eq!(
            store.get_edge_prop_by_id(7, "since"),
            Some(Value::Timestamp(123))
        );
        store.delete_vertex_prop(10, "age").unwrap();
        assert_eq!(store.get_vertex_prop(10, "age"), None);

        let mem = store.into_memory();
        let reopened = AbpPropertyStore::from_memory(mem, 0).unwrap();
        assert_eq!(
            reopened.get_vertex_prop(10, "name"),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(reopened.get_vertex_prop(10, "age"), None);
    }

    #[test]
    fn migrate_append_log_property_store_to_abp_leaf_preserves_visible_rows() {
        let mut store = PropertyStore::new(VecMemory::default(), 0).unwrap();
        store
            .set_vertex_prop(1, "name", Value::Text("A".into()))
            .unwrap();
        store.set_vertex_prop(1, "age", Value::Int64(1)).unwrap();
        store.delete_vertex_prop(1, "age").unwrap();
        store
            .set_edge_prop_by_id(7, "since", Value::Timestamp(7))
            .unwrap();

        let abp = store.migrate_to_abp_leaf(0).unwrap();
        assert_eq!(
            abp.get_vertex_prop(1, "name"),
            Some(Value::Text("A".into()))
        );
        assert_eq!(abp.get_vertex_prop(1, "age"), None);
        assert_eq!(
            abp.get_edge_prop_by_id(7, "since"),
            Some(Value::Timestamp(7))
        );
    }

    #[test]
    fn property_store_runtime_auto_detect_and_migrate() {
        let mut rt = PropertyStoreRuntime::new_append(VecMemory::default(), 0).unwrap();
        rt.set_vertex_prop(1, "name", Value::Text("A".into()))
            .unwrap();
        assert_eq!(
            PropertyStoreRuntime::detect_format(
                match &rt {
                    PropertyStoreRuntime::AppendLog(s) => s.memory(),
                    PropertyStoreRuntime::AbPlusLeaf(_) => panic!("unexpected"),
                },
                0
            ),
            PropertyStoreFormatTag::AppendLog
        );

        rt = rt.migrate_append_to_abp_leaf(0).unwrap();
        assert_eq!(rt.get_vertex_prop(1, "name"), Some(Value::Text("A".into())));

        let mem = match rt {
            PropertyStoreRuntime::AbPlusLeaf(s) => s.into_memory(),
            PropertyStoreRuntime::AppendLog(_) => panic!("expected abp"),
        };
        let reopened = PropertyStoreRuntime::open_auto(mem, 0, 0).unwrap();
        assert_eq!(
            reopened.get_vertex_prop(1, "name"),
            Some(Value::Text("A".into()))
        );
    }

    #[test]
    fn property_store_runtime_migration_is_idempotent() {
        let mut rt = PropertyStoreRuntime::new_append(VecMemory::default(), 0).unwrap();
        rt.set_vertex_prop(1, "name", Value::Text("A".into()))
            .unwrap();
        rt.set_edge_prop_by_id(7, "since", Value::Timestamp(7))
            .unwrap();

        let rt = rt.migrate_append_to_abp_leaf(0).unwrap();
        let rt = rt.migrate_append_to_abp_leaf(0).unwrap();

        assert_eq!(rt.get_vertex_prop(1, "name"), Some(Value::Text("A".into())));
        assert_eq!(
            rt.get_edge_prop_by_id(7, "since"),
            Some(Value::Timestamp(7))
        );
    }

    #[test]
    fn abp_property_store_compact_preserves_rows() {
        let mut store = AbpPropertyStore::new(VecMemory::default(), 0).unwrap();
        for i in 0..48u32 {
            store
                .set_vertex_prop(i, "name", Value::Text(format!("v{i}")))
                .unwrap();
        }
        for i in (0..48u32).step_by(4) {
            store.delete_vertex_prop(i, "name").unwrap();
        }

        let before: Vec<_> = (0..48u32)
            .map(|i| (i, store.get_vertex_prop(i, "name")))
            .collect();
        store.compact().unwrap();
        let after: Vec<_> = (0..48u32)
            .map(|i| (i, store.get_vertex_prop(i, "name")))
            .collect();
        assert_eq!(after, before);
    }

    #[test]
    fn abp_secondary_eq_index_round_trip_and_compact() {
        let mut idx = AbpSecondaryEqIndex::new(VecMemory::default(), 0).unwrap();
        idx.add_vertex_eq("email", &Value::Text("a@example.com".into()), 10)
            .unwrap();
        idx.add_vertex_eq("email", &Value::Text("a@example.com".into()), 3)
            .unwrap();
        idx.add_vertex_eq("email", &Value::Text("b@example.com".into()), 7)
            .unwrap();
        idx.add_vertex_eq("age", &Value::Int64(42), 10).unwrap();

        assert_eq!(
            idx.scan_vertices_eq("email", &Value::Text("a@example.com".into()))
                .unwrap(),
            vec![3, 10]
        );
        assert_eq!(
            idx.scan_vertices_eq("age", &Value::Int64(42)).unwrap(),
            vec![10]
        );

        idx.remove_vertex_eq("email", &Value::Text("a@example.com".into()), 3)
            .unwrap();
        assert_eq!(
            idx.scan_vertices_eq("email", &Value::Text("a@example.com".into()))
                .unwrap(),
            vec![10]
        );

        idx.compact().unwrap();
        let mem = idx.into_memory();
        let reopened = AbpSecondaryEqIndex::from_memory(mem, 0).unwrap();
        assert_eq!(
            reopened
                .scan_vertices_eq("email", &Value::Text("a@example.com".into()))
                .unwrap(),
            vec![10]
        );
        assert_eq!(
            reopened
                .scan_vertices_eq("email", &Value::Text("b@example.com".into()))
                .unwrap(),
            vec![7]
        );
    }
}
