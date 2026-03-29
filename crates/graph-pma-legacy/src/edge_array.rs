use std::collections::BTreeMap;

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::{EdgeId, EdgeRecord, Expansion, NodeId};

use crate::layout::{LayoutError, LayoutResult};
use crate::node_catalog::NodeCatalog;
use crate::prop_codec::{read_string, read_u32, read_u64, write_string, write_u32, write_u64};

type LabelId = u16;
const TOMBSTONE_MASK: u16 = 1 << 15;
const LABEL_MASK: u16 = !TOMBSTONE_MASK;

#[derive(Clone, Debug, Default)]
pub struct AdjacencyPma {
    outgoing_index: Vec<VertexEntry>,
    outgoing_entries: Vec<AdjacencyEntry>,
    outgoing_edge_ids: Vec<EdgeId>,
    outgoing_label_ranges: Vec<VertexLabelRange>,
    incoming_index: Vec<VertexEntry>,
    incoming_entries: Vec<AdjacencyEntry>,
    incoming_edge_ids: Vec<EdgeId>,
    incoming_label_ranges: Vec<VertexLabelRange>,
    locator_by_id: Vec<Option<EdgeLocator>>,
    labels: AdjacencyLabelCatalog,
    edges: Vec<Option<EdgeRecord>>,
    edge_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VertexEntry {
    node_id: NodeId,
    edge_index: u32,
    degree: u32,
    label_range_start: u32,
    label_range_len: u32,
    log_offset: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdjacencyEntry(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EdgeLocator {
    vertex: NodeId,
    ordinal: u32,
    direction: EdgeDirection,
}

#[derive(Clone, Copy)]
struct AdjacencyRef<'a> {
    entry: &'a AdjacencyEntry,
    edge_id: EdgeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VertexLabelRange {
    label_id: LabelId,
    start: u32,
    len: u32,
}

#[derive(Clone, Debug, Default)]
struct AdjacencyLabelCatalog {
    by_name: BTreeMap<String, LabelId>,
    by_id: Vec<Option<String>>,
}

impl AdjacencyPma {
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.labels.encode(&mut out);
        encode_adjacency_index(
            &self.outgoing_index,
            &self.outgoing_entries,
            &self.outgoing_edge_ids,
            &self.outgoing_label_ranges,
            &mut out,
        );
        encode_adjacency_index(
            &self.incoming_index,
            &self.incoming_entries,
            &self.incoming_edge_ids,
            &self.incoming_label_ranges,
            &mut out,
        );
        out
    }

    pub fn from_snapshot(bytes: &[u8], edges: Vec<EdgeRecord>) -> LayoutResult<Self> {
        let mut cursor = 0;
        let labels = AdjacencyLabelCatalog::decode(bytes, &mut cursor)?;
        let (outgoing_index, outgoing_entries, outgoing_edge_ids, outgoing_label_ranges) =
            decode_adjacency_index(bytes, &mut cursor)?;
        let (incoming_index, incoming_entries, incoming_edge_ids, incoming_label_ranges) =
            decode_adjacency_index(bytes, &mut cursor)?;
        let (edges, edge_count) = edge_slots_from_records(edges);
        let locator_by_id =
            build_locator_table(&outgoing_index, &outgoing_edge_ids, edge_count_hint(&edges));
        Ok(Self {
            outgoing_index,
            outgoing_entries,
            outgoing_edge_ids,
            outgoing_label_ranges,
            incoming_index,
            incoming_entries,
            incoming_edge_ids,
            incoming_label_ranges,
            locator_by_id,
            labels,
            edges,
            edge_count,
        })
    }

    pub fn insert(&mut self, edge: EdgeRecord) -> EdgeId {
        let edge_id = edge.id;
        self.upsert_edge(edge);
        self.rebuild_indexes();
        edge_id
    }

    pub fn edge(&self, edge_id: EdgeId) -> Option<&EdgeRecord> {
        let locator = self.locator(edge_id)?;
        let edge_id = self.edge_id_for_locator(locator)?;
        let index = edge_slot_index(edge_id)?;
        self.edges.get(index)?.as_ref()
    }

    pub fn edge_mut(&mut self, edge_id: EdgeId) -> Option<&mut EdgeRecord> {
        let locator = self.locator(edge_id)?;
        let edge_id = self.edge_id_for_locator(locator)?;
        let index = edge_slot_index(edge_id)?;
        self.edges.get_mut(index)?.as_mut()
    }

    pub fn remove_edge(&mut self, edge_id: EdgeId) -> Option<EdgeRecord> {
        let locator = self.locator(edge_id)?;
        let edge_id = self.edge_id_for_locator(locator)?;
        let index = edge_slot_index(edge_id)?;
        let edge = self.edges.get_mut(index)?.take()?;
        self.edge_count -= 1;
        self.rebuild_indexes();
        Some(edge)
    }

    pub fn scan_by_property(&self, property: &str, value: &gleaph_gql::Value) -> Vec<EdgeRecord> {
        self.edges
            .iter()
            .filter_map(|edge| edge.as_ref())
            .filter(|edge| edge.properties.get(property) == Some(value))
            .cloned()
            .collect()
    }

    pub fn expand(
        &self,
        nodes: &NodeCatalog,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> Vec<Expansion> {
        let mut out = Vec::new();
        for entry in self.entries_for(from, direction, label) {
            let Some(edge) = self.edge(entry.edge_id) else {
                continue;
            };
            if let Some(node) = nodes.get(entry.entry.other()) {
                out.push(Expansion {
                    edge: edge.clone(),
                    node: node.clone(),
                });
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.edge_count
    }

    pub fn iter(&self) -> impl Iterator<Item = &EdgeRecord> {
        self.edges.iter().filter_map(|edge| edge.as_ref())
    }

    pub fn from_edges(edges: Vec<EdgeRecord>) -> Self {
        let (edges, edge_count) = edge_slots_from_records(edges);
        let mut pma = Self {
            outgoing_index: Vec::new(),
            outgoing_entries: Vec::new(),
            outgoing_edge_ids: Vec::new(),
            outgoing_label_ranges: Vec::new(),
            incoming_index: Vec::new(),
            incoming_entries: Vec::new(),
            incoming_edge_ids: Vec::new(),
            incoming_label_ranges: Vec::new(),
            locator_by_id: Vec::new(),
            labels: AdjacencyLabelCatalog::default(),
            edges,
            edge_count,
        };
        pma.rebuild_indexes();
        pma
    }

    pub fn has_incident_edges(&self, node_id: NodeId) -> bool {
        !self
            .entries_for(node_id, EdgeDirection::AnyDirection, None)
            .is_empty()
    }

    pub fn remove_incident_edges(&mut self, node_id: NodeId) -> Vec<EdgeRecord> {
        let mut ids = self
            .entries_for(node_id, EdgeDirection::AnyDirection, None)
            .into_iter()
            .map(|entry| entry.edge_id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();

        let mut removed = Vec::new();
        for edge_id in ids {
            let Some(index) = edge_slot_index(edge_id) else {
                continue;
            };
            if let Some(edge) = self.edges.get_mut(index).and_then(Option::take) {
                self.edge_count -= 1;
                removed.push(edge);
            }
        }
        if !removed.is_empty() {
            self.rebuild_indexes();
        }
        removed
    }

    fn entries_for(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> Vec<AdjacencyRef<'_>> {
        match direction {
            EdgeDirection::PointingRight => self.entries_from_index(
                &self.outgoing_index,
                &self.outgoing_entries,
                &self.outgoing_edge_ids,
                &self.outgoing_label_ranges,
                from,
                label,
            ),
            EdgeDirection::PointingLeft => self.entries_from_index(
                &self.incoming_index,
                &self.incoming_entries,
                &self.incoming_edge_ids,
                &self.incoming_label_ranges,
                from,
                label,
            ),
            EdgeDirection::LeftOrRight
            | EdgeDirection::Undirected
            | EdgeDirection::AnyDirection => {
                let mut entries = self.entries_from_index(
                    &self.outgoing_index,
                    &self.outgoing_entries,
                    &self.outgoing_edge_ids,
                    &self.outgoing_label_ranges,
                    from,
                    label,
                );
                entries.extend(self.entries_from_index(
                    &self.incoming_index,
                    &self.incoming_entries,
                    &self.incoming_edge_ids,
                    &self.incoming_label_ranges,
                    from,
                    label,
                ));
                entries
            }
            EdgeDirection::LeftOrUndirected => self.entries_from_index(
                &self.incoming_index,
                &self.incoming_entries,
                &self.incoming_edge_ids,
                &self.incoming_label_ranges,
                from,
                label,
            ),
            EdgeDirection::UndirectedOrRight => self.entries_from_index(
                &self.outgoing_index,
                &self.outgoing_entries,
                &self.outgoing_edge_ids,
                &self.outgoing_label_ranges,
                from,
                label,
            ),
        }
    }

    fn rebuild_indexes(&mut self) {
        let mut labels = AdjacencyLabelCatalog::default();
        let mut outgoing = Vec::<(NodeId, AdjacencyEntry, EdgeId)>::with_capacity(self.edge_count);
        let mut incoming = Vec::<(NodeId, AdjacencyEntry, EdgeId)>::with_capacity(self.edge_count);
        for edge in self.edges.iter().filter_map(|edge| edge.as_ref()) {
            let meta = labels.meta_for_label(edge.label.as_deref());
            outgoing.push((edge.src, AdjacencyEntry::new(edge.dst, meta), edge.id));
            incoming.push((edge.dst, AdjacencyEntry::new(edge.src, meta), edge.id));
        }
        (
            self.outgoing_index,
            self.outgoing_entries,
            self.outgoing_edge_ids,
            self.outgoing_label_ranges,
        ) = build_compact_index(outgoing);
        (
            self.incoming_index,
            self.incoming_entries,
            self.incoming_edge_ids,
            self.incoming_label_ranges,
        ) = build_compact_index(incoming);
        self.locator_by_id = build_locator_table(
            &self.outgoing_index,
            &self.outgoing_edge_ids,
            self.edges.len(),
        );
        self.labels = labels;
    }

    fn entries_from_index<'a>(
        &'a self,
        index: &[VertexEntry],
        entries: &'a [AdjacencyEntry],
        edge_ids: &'a [EdgeId],
        label_ranges: &[VertexLabelRange],
        node_id: NodeId,
        label: Option<&str>,
    ) -> Vec<AdjacencyRef<'a>> {
        let Ok(pos) = index.binary_search_by_key(&node_id, |entry| entry.node_id) else {
            return Vec::new();
        };
        let vertex_entry = index[pos];
        let (start, end) = if let Some(label) = label {
            let Some(label_id) = self.labels.id_for_name(label) else {
                return Vec::new();
            };
            let range_start = vertex_entry.label_range_start as usize;
            let range_end = range_start + vertex_entry.label_range_len as usize;
            let Some(label_range) = label_ranges[range_start..range_end]
                .iter()
                .find(|range| range.label_id == label_id)
            else {
                return Vec::new();
            };
            (
                label_range.start as usize,
                (label_range.start + label_range.len) as usize,
            )
        } else {
            (
                vertex_entry.edge_index as usize,
                (vertex_entry.edge_index + vertex_entry.degree) as usize,
            )
        };
        entries[start..end]
            .iter()
            .zip(edge_ids[start..end].iter().copied())
            .map(|(entry, edge_id)| AdjacencyRef { entry, edge_id })
            .collect()
    }

    fn upsert_edge(&mut self, edge: EdgeRecord) {
        let index = edge_slot_index(edge.id).expect("edge ids start at 1");
        if self.edges.len() <= index {
            self.edges.resize(index + 1, None);
        }
        if self.edges[index].is_none() {
            self.edge_count += 1;
        }
        self.edges[index] = Some(edge);
    }

    fn locator(&self, edge_id: EdgeId) -> Option<EdgeLocator> {
        let index = edge_slot_index(edge_id)?;
        self.locator_by_id.get(index).copied().flatten()
    }

    fn edge_id_for_locator(&self, locator: EdgeLocator) -> Option<EdgeId> {
        let (_, edge_id) = self.entry_for_locator(locator)?;
        Some(edge_id)
    }

    fn entry_for_locator(&self, locator: EdgeLocator) -> Option<(AdjacencyEntry, EdgeId)> {
        let (index, entries, edge_ids) = match locator.direction {
            EdgeDirection::PointingRight | EdgeDirection::UndirectedOrRight => (
                &self.outgoing_index,
                &self.outgoing_entries,
                &self.outgoing_edge_ids,
            ),
            EdgeDirection::PointingLeft | EdgeDirection::LeftOrUndirected => (
                &self.incoming_index,
                &self.incoming_entries,
                &self.incoming_edge_ids,
            ),
            _ => return None,
        };
        let pos = index
            .binary_search_by_key(&locator.vertex, |entry| entry.node_id)
            .ok()?;
        let vertex_entry = index[pos];
        if locator.ordinal >= vertex_entry.degree {
            return None;
        }
        let slot = vertex_entry.edge_index as usize + locator.ordinal as usize;
        Some((*entries.get(slot)?, *edge_ids.get(slot)?))
    }
}

fn build_compact_index(
    mut entries: Vec<(NodeId, AdjacencyEntry, EdgeId)>,
) -> (
    Vec<VertexEntry>,
    Vec<AdjacencyEntry>,
    Vec<EdgeId>,
    Vec<VertexLabelRange>,
) {
    entries.sort_unstable_by(
        |(left_node, left_entry, left_edge_id), (right_node, right_entry, right_edge_id)| {
            left_node
                .cmp(right_node)
                .then_with(|| left_entry.other().cmp(&right_entry.other()))
                .then_with(|| label_id(left_entry.meta()).cmp(&label_id(right_entry.meta())))
                .then_with(|| left_edge_id.cmp(right_edge_id))
        },
    );
    let mut index = Vec::new();
    let mut hot_entries = Vec::with_capacity(entries.len());
    let mut edge_ids = Vec::with_capacity(entries.len());
    let mut label_ranges = Vec::new();
    let mut cursor = 0;
    while cursor < entries.len() {
        let node_id = entries[cursor].0;
        let edge_index = hot_entries.len() as u32;
        let label_range_start = label_ranges.len() as u32;
        while cursor < entries.len() && entries[cursor].0 == node_id {
            let range_start = hot_entries.len() as u32;
            let current_label_id = label_id(entries[cursor].1.meta());
            hot_entries.push(entries[cursor].1);
            edge_ids.push(entries[cursor].2);
            cursor += 1;
            while cursor < entries.len()
                && entries[cursor].0 == node_id
                && label_id(entries[cursor].1.meta()) == current_label_id
            {
                hot_entries.push(entries[cursor].1);
                edge_ids.push(entries[cursor].2);
                cursor += 1;
            }
            if current_label_id != 0 {
                label_ranges.push(VertexLabelRange {
                    label_id: current_label_id,
                    start: range_start,
                    len: hot_entries.len() as u32 - range_start,
                });
            }
        }
        index.push(VertexEntry {
            node_id,
            edge_index,
            degree: hot_entries.len() as u32 - edge_index,
            label_range_start,
            label_range_len: label_ranges.len() as u32 - label_range_start,
            log_offset: -1,
        });
    }
    (index, hot_entries, edge_ids, label_ranges)
}

fn edge_slots_from_records(edges: Vec<EdgeRecord>) -> (Vec<Option<EdgeRecord>>, usize) {
    let max_id = edges.iter().map(|edge| edge.id).max().unwrap_or(0);
    let mut slots = vec![None; max_id as usize];
    let mut count = 0;
    for edge in edges {
        let index = edge_slot_index(edge.id).expect("edge ids start at 1");
        if slots[index].is_none() {
            count += 1;
        }
        slots[index] = Some(edge);
    }
    (slots, count)
}

fn edge_count_hint(edges: &[Option<EdgeRecord>]) -> usize {
    edges.len()
}

fn edge_slot_index(edge_id: EdgeId) -> Option<usize> {
    edge_id.checked_sub(1).map(|index| index as usize)
}

fn build_locator_table(
    index: &[VertexEntry],
    edge_ids: &[EdgeId],
    slot_len: usize,
) -> Vec<Option<EdgeLocator>> {
    let mut locators = vec![None; slot_len];
    for vertex_entry in index {
        let start = vertex_entry.edge_index as usize;
        let end = start + vertex_entry.degree as usize;
        for (ordinal, edge_id) in edge_ids[start..end].iter().copied().enumerate() {
            let Some(slot) = edge_slot_index(edge_id) else {
                continue;
            };
            if slot >= locators.len() {
                locators.resize(slot + 1, None);
            }
            locators[slot] = Some(EdgeLocator {
                vertex: vertex_entry.node_id,
                ordinal: ordinal as u32,
                direction: EdgeDirection::PointingRight,
            });
        }
    }
    locators
}

fn encode_adjacency_index(
    index: &[VertexEntry],
    entries: &[AdjacencyEntry],
    edge_ids: &[EdgeId],
    label_ranges: &[VertexLabelRange],
    out: &mut Vec<u8>,
) {
    write_u32(out, index.len() as u32);
    for entry in index {
        write_u64(out, entry.node_id.into());
        write_u32(out, entry.edge_index);
        write_u32(out, entry.degree);
        write_u32(out, entry.label_range_start);
        write_u32(out, entry.label_range_len);
        write_u32(out, entry.log_offset as u32);
    }
    write_u32(out, entries.len() as u32);
    for (entry, edge_id) in entries.iter().zip(edge_ids.iter().copied()) {
        write_u64(out, entry.0);
        write_u64(out, edge_id);
    }
    write_u32(out, label_ranges.len() as u32);
    for range in label_ranges {
        write_u32(out, range.label_id as u32);
        write_u32(out, range.start);
        write_u32(out, range.len);
    }
}

fn decode_adjacency_index(
    bytes: &[u8],
    cursor: &mut usize,
) -> LayoutResult<(
    Vec<VertexEntry>,
    Vec<AdjacencyEntry>,
    Vec<EdgeId>,
    Vec<VertexLabelRange>,
)> {
    let len = read_u32(bytes, cursor)? as usize;
    let mut index = Vec::with_capacity(len);
    for _ in 0..len {
        let node_id = NodeId::try_from(read_u64(bytes, cursor)?)
            .map_err(|_| LayoutError::InvalidPayload)?;
        let edge_index = read_u32(bytes, cursor)?;
        let degree = read_u32(bytes, cursor)?;
        if index
            .iter()
            .any(|entry: &VertexEntry| entry.node_id == node_id)
        {
            return Err(LayoutError::InvalidPayload);
        }
        index.push(VertexEntry {
            node_id,
            edge_index,
            degree,
            label_range_start: read_u32(bytes, cursor)?,
            label_range_len: read_u32(bytes, cursor)?,
            log_offset: read_u32(bytes, cursor)? as i32,
        });
    }
    let entry_len = read_u32(bytes, cursor)? as usize;
    let mut entries = Vec::with_capacity(entry_len);
    let mut edge_ids = Vec::with_capacity(entry_len);
    for _ in 0..entry_len {
        entries.push(AdjacencyEntry(read_u64(bytes, cursor)?));
        edge_ids.push(read_u64(bytes, cursor)?);
    }
    for entry in &index {
        let end = entry.edge_index as usize + entry.degree as usize;
        if end > entries.len() {
            return Err(LayoutError::InvalidPayload);
        }
    }
    let range_len = read_u32(bytes, cursor)? as usize;
    let mut label_ranges = Vec::with_capacity(range_len);
    for _ in 0..range_len {
        label_ranges.push(VertexLabelRange {
            label_id: read_u32(bytes, cursor)? as LabelId,
            start: read_u32(bytes, cursor)?,
            len: read_u32(bytes, cursor)?,
        });
    }
    for entry in &index {
        let end = entry.label_range_start as usize + entry.label_range_len as usize;
        if end > label_ranges.len() {
            return Err(LayoutError::InvalidPayload);
        }
    }
    Ok((index, entries, edge_ids, label_ranges))
}

impl AdjacencyLabelCatalog {
    fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.by_name.len() as u32);
        let mut entries = self
            .by_name
            .iter()
            .map(|(name, id)| (*id, name))
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(id, _)| *id);
        for (id, name) in entries {
            write_u32(out, id as u32);
            write_string(out, name);
        }
    }

    fn decode(bytes: &[u8], cursor: &mut usize) -> LayoutResult<Self> {
        let len = read_u32(bytes, cursor)? as usize;
        let mut catalog = Self::default();
        for _ in 0..len {
            let id = read_u32(bytes, cursor)? as LabelId;
            let name = read_string(bytes, cursor)?;
            catalog.insert_with_id(id, name)?;
        }
        Ok(catalog)
    }

    fn meta_for_label(&mut self, label: Option<&str>) -> u16 {
        let Some(label) = label else {
            return 0;
        };
        let label_id = self.intern(label);
        with_label(0, label_id)
    }

    fn id_for_name(&self, label: &str) -> Option<LabelId> {
        self.by_name.get(label).copied()
    }

    fn intern(&mut self, label: &str) -> LabelId {
        if let Some(id) = self.by_name.get(label).copied() {
            return id;
        }
        let id = self.next_label_id();
        self.by_name.insert(label.to_owned(), id);
        if self.by_id.len() <= id as usize {
            self.by_id.resize(id as usize + 1, None);
        }
        self.by_id[id as usize] = Some(label.to_owned());
        id
    }

    fn insert_with_id(&mut self, id: LabelId, label: String) -> LayoutResult<()> {
        if id == 0 || self.by_name.contains_key(&label) {
            return Err(LayoutError::InvalidPayload);
        }
        if self.by_id.len() <= id as usize {
            self.by_id.resize(id as usize + 1, None);
        }
        if self.by_id[id as usize].is_some() {
            return Err(LayoutError::InvalidPayload);
        }
        self.by_name.insert(label.clone(), id);
        self.by_id[id as usize] = Some(label);
        Ok(())
    }

    fn next_label_id(&self) -> LabelId {
        std::cmp::max(1, self.by_id.len()) as LabelId
    }
}

fn label_id(meta: u16) -> LabelId {
    meta & LABEL_MASK
}

fn with_label(meta: u16, label_id: LabelId) -> u16 {
    (meta & TOMBSTONE_MASK) | (label_id & LABEL_MASK)
}

const PACKED_NODE_ID_MAX: u64 = (1u64 << 48) - 1;

impl AdjacencyEntry {
    fn new(other: NodeId, meta: u16) -> Self {
        let other = u64::from(other);
        assert!(
            other <= PACKED_NODE_ID_MAX,
            "node id exceeds 48-bit adjacency layout"
        );
        Self(((meta as u64) << 48) | other)
    }

    fn other(self) -> NodeId {
        NodeId::try_from(self.0 & PACKED_NODE_ID_MAX)
            .expect("packed adjacency entry must contain a valid NodeId")
    }

    fn meta(self) -> u16 {
        (self.0 >> 48) as u16
    }
}
