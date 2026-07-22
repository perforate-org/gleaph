//! Stable label stats delta log and graph mutation journal (ADR 0015).

use gleaph_graph_kernel::entry::{EdgeLabelId, VertexLabelId};
use gleaph_graph_kernel::federation::LocalVertexId;
use gleaph_graph_kernel::plan_exec::{
    GraphBulkMutationProgress, GraphBulkMutationProgressV1, GraphMutationJournalEntryWire,
    LabelStatsDeltaEventWire, MutationId, MutationJournalState, ShardEventSeq,
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::ops::Bound as StdBound;

#[cfg(feature = "canbench")]
use canbench_rs::bench_scope as canbench_scope;

/// Versioned graph-local mutation journal entry (ADR 0015, ADR 0044).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum GraphMutationJournalEntry {
    V1(GraphMutationJournalEntryV1),
}

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct GraphMutationJournalEntryV1 {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
    /// IC time (ns) when this entry was last recorded, the sole basis for time-based
    /// retention (ADR 0027). The current fixed-length stable layout writes the
    /// timestamp slot for every persisted entry; `None` remains an in-memory
    /// state used by retention tests and legacy-state handling is intentionally
    /// outside the fresh-install layout contract.
    #[serde(default)]
    pub recorded_at_ns: Option<u64>,
    /// Bulk operation cursor: for a bulk mutation, points at the next unexecuted operation
    /// index. For a single mutation it is `None`.
    #[serde(default)]
    pub next_index: Option<u32>,
    /// Bulk-specific progress metadata; present only when `next_index` is used.
    #[serde(default)]
    pub bulk_progress: Option<GraphBulkMutationProgress>,
}

impl GraphMutationJournalEntry {
    pub fn incomplete(
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        recorded_at_ns: u64,
    ) -> Self {
        Self::V1(GraphMutationJournalEntryV1 {
            mutation_id,
            state: MutationJournalState::Incomplete,
            row_count: 0,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: Vec::new(),
            recorded_at_ns: Some(recorded_at_ns),
            next_index: None,
            bulk_progress: None,
        })
    }

    pub fn completed(
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        recorded_at_ns: u64,
    ) -> Self {
        Self::V1(GraphMutationJournalEntryV1 {
            mutation_id,
            state: MutationJournalState::Completed,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            recorded_at_ns: Some(recorded_at_ns),
            next_index: None,
            bulk_progress: None,
        })
    }

    fn as_v1(&self) -> &GraphMutationJournalEntryV1 {
        match self {
            GraphMutationJournalEntry::V1(v1) => v1,
        }
    }

    fn as_v1_mut(&mut self) -> &mut GraphMutationJournalEntryV1 {
        match self {
            GraphMutationJournalEntry::V1(v1) => v1,
        }
    }

    pub fn mutation_id(&self) -> MutationId {
        self.as_v1().mutation_id
    }
    pub fn state(&self) -> MutationJournalState {
        self.as_v1().state
    }
    pub fn row_count(&self) -> u64 {
        self.as_v1().row_count
    }
    pub fn emitted_delta_first_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_first_seq
    }
    pub fn emitted_delta_last_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_last_seq
    }
    pub fn hot_forward_vertices(&self) -> &Vec<LocalVertexId> {
        &self.as_v1().hot_forward_vertices
    }
    pub fn recorded_at_ns(&self) -> Option<u64> {
        self.as_v1().recorded_at_ns
    }
    pub fn next_index(&self) -> Option<u32> {
        self.as_v1().next_index
    }
    pub fn bulk_progress(&self) -> &Option<GraphBulkMutationProgress> {
        &self.as_v1().bulk_progress
    }

    pub fn set_state(&mut self, state: MutationJournalState) {
        self.as_v1_mut().state = state;
    }
    pub fn set_row_count(&mut self, row_count: u64) {
        self.as_v1_mut().row_count = row_count;
    }
    pub fn set_emitted_delta_first_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_first_seq = seq;
    }
    pub fn set_emitted_delta_last_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_last_seq = seq;
    }
    pub fn set_hot_forward_vertices(&mut self, vertices: Vec<LocalVertexId>) {
        self.as_v1_mut().hot_forward_vertices = vertices;
    }
    pub fn set_recorded_at_ns(&mut self, recorded_at_ns: Option<u64>) {
        self.as_v1_mut().recorded_at_ns = recorded_at_ns;
    }
    pub fn set_next_index(&mut self, next_index: Option<u32>) {
        self.as_v1_mut().next_index = next_index;
    }
    pub fn set_bulk_progress(&mut self, bulk_progress: Option<GraphBulkMutationProgress>) {
        self.as_v1_mut().bulk_progress = bulk_progress;
    }

    pub fn wire(&self) -> GraphMutationJournalEntryWire {
        let mut wire = GraphMutationJournalEntryWire::new(
            self.as_v1().mutation_id,
            self.as_v1().state,
            self.as_v1().row_count,
            self.as_v1().emitted_delta_first_seq,
            self.as_v1().emitted_delta_last_seq,
            self.as_v1().hot_forward_vertices.clone(),
        );
        wire.set_next_index(self.as_v1().next_index);
        wire.set_bulk_progress(self.as_v1().bulk_progress.clone());
        wire
    }

    pub fn is_completed(&self) -> bool {
        matches!(self.as_v1().state, MutationJournalState::Completed)
    }
}

// -----------------------------------------------------------------------------
// Manual fixed-length stable layouts (Plan 0120)
// -----------------------------------------------------------------------------

const JOURNAL_LAYOUT_VERSION: u8 = 1;
const LABEL_DELTA_LAYOUT_VERSION: u8 = 1;

// Sensible production bounds. They are fail-closed: encoding panics if exceeded.
const MAX_HOT_FORWARD_VERTICES: u32 = 4096;
const MAX_BULK_OPERATION_COUNT: u32 = 4096;
const MAX_BULK_ROW_COUNTS: u32 = 4096;
const MAX_LABEL_DELTAS_PER_KIND: u32 = 256;

// Primary record sizes.
// Journal primary is fixed regardless of which Option fields are set: a validity
// bitmap records which of the optional fixed-width slots are live.
const JOURNAL_PRIMARY_SIZE: usize = 52;
const LABEL_DELTA_PRIMARY_SIZE: usize = 21;

// Validity bitmap bits inside the journal primary header.
const VALID_FIRST_SEQ: u8 = 0x01;
const VALID_LAST_SEQ: u8 = 0x02;
const VALID_RECORDED_AT: u8 = 0x04;
const VALID_NEXT_INDEX: u8 = 0x08;

// Appendix flags for journal entries.
const APPENDIX_HOT_FORWARD: u8 = 0x01;
const APPENDIX_BULK_PROGRESS: u8 = 0x02;

fn encode_u8(buf: &mut Vec<u8>, val: u8) {
    buf.push(val);
}

fn encode_u16_le(buf: &mut Vec<u8>, val: u16) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn encode_u32_le(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn encode_u64_le(buf: &mut Vec<u8>, val: u64) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn encode_i64_le(buf: &mut Vec<u8>, val: i64) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn decode_u8(bytes: &[u8], offset: &mut usize) -> u8 {
    let val = bytes[*offset];
    *offset += 1;
    val
}

fn decode_u16_le(bytes: &[u8], offset: &mut usize) -> u16 {
    let val = u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]);
    *offset += 2;
    val
}

fn decode_u32_le(bytes: &[u8], offset: &mut usize) -> u32 {
    let val = u32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    val
}

fn decode_u64_le(bytes: &[u8], offset: &mut usize) -> u64 {
    let val = u64::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
        bytes[*offset + 4],
        bytes[*offset + 5],
        bytes[*offset + 6],
        bytes[*offset + 7],
    ]);
    *offset += 8;
    val
}

fn decode_i64_le(bytes: &[u8], offset: &mut usize) -> i64 {
    decode_u64_le(bytes, offset) as i64
}

fn encode_mutation_journal_state(buf: &mut Vec<u8>, state: MutationJournalState) {
    let tag = match state {
        MutationJournalState::Incomplete => 0u8,
        MutationJournalState::Completed => 1u8,
    };
    encode_u8(buf, tag);
}

fn decode_mutation_journal_state(bytes: &[u8], offset: &mut usize) -> MutationJournalState {
    match decode_u8(bytes, offset) {
        0 => MutationJournalState::Incomplete,
        1 => MutationJournalState::Completed,
        tag => panic!("unknown MutationJournalState tag {}", tag),
    }
}

fn encode_journal_v1(v1: &GraphMutationJournalEntryV1) -> Vec<u8> {
    let mut buf = Vec::with_capacity(JOURNAL_PRIMARY_SIZE);

    encode_u8(&mut buf, JOURNAL_LAYOUT_VERSION);
    encode_u64_le(&mut buf, v1.mutation_id);
    encode_mutation_journal_state(&mut buf, v1.state);
    encode_u64_le(&mut buf, v1.row_count);

    let mut validity: u8 = 0;
    if v1.emitted_delta_first_seq.is_some() {
        validity |= VALID_FIRST_SEQ;
    }
    if v1.emitted_delta_last_seq.is_some() {
        validity |= VALID_LAST_SEQ;
    }
    if v1.recorded_at_ns.is_some() {
        validity |= VALID_RECORDED_AT;
    }
    if v1.next_index.is_some() {
        validity |= VALID_NEXT_INDEX;
    }
    encode_u8(&mut buf, validity);

    encode_u64_le(&mut buf, v1.emitted_delta_first_seq.unwrap_or(0));
    encode_u64_le(&mut buf, v1.emitted_delta_last_seq.unwrap_or(0));
    encode_u64_le(&mut buf, v1.recorded_at_ns.unwrap_or(0));
    encode_u32_le(&mut buf, v1.next_index.unwrap_or(0));

    let mut flags: u8 = 0;
    if !v1.hot_forward_vertices.is_empty() {
        flags |= APPENDIX_HOT_FORWARD;
    }
    if v1.bulk_progress.is_some() {
        flags |= APPENDIX_BULK_PROGRESS;
    }
    encode_u8(&mut buf, flags);

    // Compute and reserve appendix length slot.
    let appendix_len_offset = buf.len();
    encode_u32_le(&mut buf, 0); // placeholder

    let appendix_start = buf.len();

    if !v1.hot_forward_vertices.is_empty() {
        let count = v1.hot_forward_vertices.len() as u32;
        assert!(
            count <= MAX_HOT_FORWARD_VERTICES,
            "hot_forward_vertices {} exceeds bound {}",
            count,
            MAX_HOT_FORWARD_VERTICES
        );
        encode_u32_le(&mut buf, count);
        for &vid in &v1.hot_forward_vertices {
            encode_u32_le(&mut buf, vid);
        }
    }

    if let Some(GraphBulkMutationProgress::V1(progress)) = &v1.bulk_progress {
        assert!(
            progress.operation_count <= MAX_BULK_OPERATION_COUNT,
            "bulk operation_count {} exceeds bound {}",
            progress.operation_count,
            MAX_BULK_OPERATION_COUNT
        );
        let row_count_len = progress.operation_row_counts.len() as u32;
        assert!(
            row_count_len <= progress.operation_count && row_count_len <= MAX_BULK_ROW_COUNTS,
            "bulk operation_row_counts length {} exceeds bounds",
            row_count_len
        );
        encode_u32_le(&mut buf, progress.operation_count);
        encode_u32_le(&mut buf, progress.completed_count);
        encode_u32_le(&mut buf, row_count_len);
        for &rc in &progress.operation_row_counts {
            encode_u64_le(&mut buf, rc);
        }
    }

    let appendix_len = (buf.len() - appendix_start) as u32;
    buf[appendix_len_offset..appendix_len_offset + 4].copy_from_slice(&appendix_len.to_le_bytes());

    buf
}

fn decode_journal_v1(bytes: &[u8]) -> GraphMutationJournalEntryV1 {
    assert!(
        bytes.len() >= JOURNAL_PRIMARY_SIZE,
        "journal entry truncated: {} bytes",
        bytes.len()
    );
    let mut offset = 0usize;

    let version = decode_u8(bytes, &mut offset);
    assert_eq!(
        version, JOURNAL_LAYOUT_VERSION,
        "unknown journal layout version {}",
        version
    );

    let mutation_id = decode_u64_le(bytes, &mut offset);
    let state = decode_mutation_journal_state(bytes, &mut offset);
    let row_count = decode_u64_le(bytes, &mut offset);
    let validity = decode_u8(bytes, &mut offset);
    let emitted_delta_first_seq = if validity & VALID_FIRST_SEQ != 0 {
        Some(decode_u64_le(bytes, &mut offset))
    } else {
        decode_u64_le(bytes, &mut offset);
        None
    };
    let emitted_delta_last_seq = if validity & VALID_LAST_SEQ != 0 {
        Some(decode_u64_le(bytes, &mut offset))
    } else {
        decode_u64_le(bytes, &mut offset);
        None
    };
    let recorded_at_ns = if validity & VALID_RECORDED_AT != 0 {
        Some(decode_u64_le(bytes, &mut offset))
    } else {
        decode_u64_le(bytes, &mut offset);
        None
    };
    let next_index = if validity & VALID_NEXT_INDEX != 0 {
        Some(decode_u32_le(bytes, &mut offset))
    } else {
        decode_u32_le(bytes, &mut offset);
        None
    };

    let flags = decode_u8(bytes, &mut offset);
    let appendix_len = decode_u32_le(bytes, &mut offset) as usize;

    let expected_len = JOURNAL_PRIMARY_SIZE + appendix_len;
    assert_eq!(
        bytes.len(),
        expected_len,
        "journal entry length mismatch: got {} expected {}",
        bytes.len(),
        expected_len
    );

    let appendix_end = JOURNAL_PRIMARY_SIZE + appendix_len;
    assert!(
        appendix_len <= JOURNAL_MAX_APPENDIX as usize,
        "journal appendix exceeds bound"
    );
    let mut hot_forward_vertices = Vec::new();
    let mut bulk_progress = None;

    let mut appendix_offset = JOURNAL_PRIMARY_SIZE;
    if flags & APPENDIX_HOT_FORWARD != 0 {
        let count = decode_u32_le(bytes, &mut appendix_offset) as usize;
        assert!(
            count <= MAX_HOT_FORWARD_VERTICES as usize,
            "hot_forward count exceeds bound"
        );
        assert!(
            appendix_offset + count * 4 <= appendix_end,
            "hot_forward appendix overflow"
        );
        hot_forward_vertices.reserve(count);
        for _ in 0..count {
            hot_forward_vertices.push(decode_u32_le(bytes, &mut appendix_offset));
        }
    }

    if flags & APPENDIX_BULK_PROGRESS != 0 {
        let operation_count = decode_u32_le(bytes, &mut appendix_offset);
        let completed_count = decode_u32_le(bytes, &mut appendix_offset);
        let row_count_len = decode_u32_le(bytes, &mut appendix_offset);
        assert!(
            operation_count <= MAX_BULK_OPERATION_COUNT,
            "bulk operation_count exceeds bound"
        );
        assert!(
            completed_count <= operation_count,
            "bulk completed_count exceeds operation_count"
        );
        assert!(
            row_count_len <= operation_count && row_count_len <= MAX_BULK_ROW_COUNTS,
            "bulk row-count length exceeds bound"
        );
        assert!(
            appendix_offset + (row_count_len as usize) * 8 <= appendix_end,
            "bulk_progress appendix overflow"
        );
        let mut operation_row_counts = Vec::with_capacity(row_count_len as usize);
        for _ in 0..row_count_len {
            operation_row_counts.push(decode_u64_le(bytes, &mut appendix_offset));
        }
        bulk_progress = Some(GraphBulkMutationProgress::V1(GraphBulkMutationProgressV1 {
            operation_count,
            completed_count,
            operation_row_counts,
        }));
    }

    assert_eq!(
        appendix_offset, appendix_end,
        "journal appendix did not consume exactly {} bytes",
        appendix_len
    );

    GraphMutationJournalEntryV1 {
        mutation_id,
        state,
        row_count,
        emitted_delta_first_seq,
        emitted_delta_last_seq,
        hot_forward_vertices,
        recorded_at_ns,
        next_index,
        bulk_progress,
    }
}

const JOURNAL_MAX_APPENDIX: u32 = {
    let hot_forward = 4 + MAX_HOT_FORWARD_VERTICES * 4;
    let bulk = 4 + 4 + 4 + MAX_BULK_ROW_COUNTS * 8;
    hot_forward + bulk
};

impl Storable for GraphMutationJournalEntry {
    // Deliberately Unbounded: the manual layout already enforces encode-time bounds.
    // Bounded values make StableBTreeMap fresh-key insert regression because the
    // allocated node grows with max_size (see Plan 0120 measurements).
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_journal_v1(self.as_v1()))
    }

    fn into_bytes(self) -> Vec<u8> {
        #[cfg(feature = "canbench")]
        let _scope = canbench_scope("journal_entry_encode");
        let bytes = encode_journal_v1(self.as_v1());
        #[cfg(feature = "canbench")]
        drop(_scope);
        bytes
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::V1(decode_journal_v1(bytes.as_ref()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredLabelStatsDeltaEvent(LabelStatsDeltaEventWire);

impl From<LabelStatsDeltaEventWire> for StoredLabelStatsDeltaEvent {
    fn from(value: LabelStatsDeltaEventWire) -> Self {
        Self(value)
    }
}

impl From<StoredLabelStatsDeltaEvent> for LabelStatsDeltaEventWire {
    fn from(value: StoredLabelStatsDeltaEvent) -> Self {
        value.0
    }
}

#[cfg(feature = "canbench")]
pub(crate) fn bench_encode_label_stats_event(event: LabelStatsDeltaEventWire) -> Vec<u8> {
    StoredLabelStatsDeltaEvent::from(event).into_bytes()
}

fn encode_label_stats_delta_event(event: &LabelStatsDeltaEventWire) -> Vec<u8> {
    let mut buf = Vec::with_capacity(LABEL_DELTA_PRIMARY_SIZE);

    encode_u8(&mut buf, LABEL_DELTA_LAYOUT_VERSION);
    encode_u64_le(&mut buf, event.mutation_id);
    encode_u64_le(&mut buf, event.shard_event_seq);

    let appendix_len_offset = buf.len();
    encode_u32_le(&mut buf, 0); // placeholder

    let appendix_start = buf.len();

    let vertex = &event.label_stats_delta.vertex;
    let vertex_count = vertex.len() as u32;
    assert!(
        vertex_count <= MAX_LABEL_DELTAS_PER_KIND,
        "vertex label deltas {} exceeds bound {}",
        vertex_count,
        MAX_LABEL_DELTAS_PER_KIND
    );
    encode_u32_le(&mut buf, vertex_count);
    for &(label, delta) in vertex {
        encode_u16_le(&mut buf, label.raw());
        encode_i64_le(&mut buf, delta);
    }

    let edge = &event.label_stats_delta.edge;
    let edge_count = edge.len() as u32;
    assert!(
        edge_count <= MAX_LABEL_DELTAS_PER_KIND,
        "edge label deltas {} exceeds bound {}",
        edge_count,
        MAX_LABEL_DELTAS_PER_KIND
    );
    encode_u32_le(&mut buf, edge_count);
    for &(label, delta) in edge {
        encode_u16_le(&mut buf, label.raw());
        encode_i64_le(&mut buf, delta);
    }

    let appendix_len = (buf.len() - appendix_start) as u32;
    buf[appendix_len_offset..appendix_len_offset + 4].copy_from_slice(&appendix_len.to_le_bytes());

    buf
}

fn decode_label_stats_delta_event(bytes: &[u8]) -> LabelStatsDeltaEventWire {
    assert!(
        bytes.len() >= LABEL_DELTA_PRIMARY_SIZE,
        "label delta event truncated: {} bytes",
        bytes.len()
    );
    let mut offset = 0usize;

    let version = decode_u8(bytes, &mut offset);
    assert_eq!(
        version, LABEL_DELTA_LAYOUT_VERSION,
        "unknown label delta layout version {}",
        version
    );

    let mutation_id = decode_u64_le(bytes, &mut offset);
    let shard_event_seq = decode_u64_le(bytes, &mut offset);
    let appendix_len = decode_u32_le(bytes, &mut offset) as usize;

    let expected_len = LABEL_DELTA_PRIMARY_SIZE + appendix_len;
    assert_eq!(
        bytes.len(),
        expected_len,
        "label delta event length mismatch: got {} expected {}",
        bytes.len(),
        expected_len
    );

    let appendix_end = LABEL_DELTA_PRIMARY_SIZE + appendix_len;
    assert!(
        appendix_len <= LABEL_DELTA_MAX_APPENDIX as usize,
        "label delta appendix exceeds bound"
    );

    let vertex_count = decode_u32_le(bytes, &mut offset) as usize;
    assert!(
        vertex_count <= MAX_LABEL_DELTAS_PER_KIND as usize,
        "vertex label delta count exceeds bound"
    );
    assert!(
        offset + vertex_count * 10 <= appendix_end,
        "vertex label delta appendix overflow"
    );
    let mut vertex = Vec::with_capacity(vertex_count);
    for _ in 0..vertex_count {
        let label = VertexLabelId::from_raw(decode_u16_le(bytes, &mut offset));
        let delta = decode_i64_le(bytes, &mut offset);
        vertex.push((label, delta));
    }

    let edge_count = decode_u32_le(bytes, &mut offset) as usize;
    assert!(
        edge_count <= MAX_LABEL_DELTAS_PER_KIND as usize,
        "edge label delta count exceeds bound"
    );
    assert!(
        offset + edge_count * 10 <= appendix_end,
        "edge label delta appendix overflow"
    );
    let mut edge = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        let label = EdgeLabelId::from_raw(decode_u16_le(bytes, &mut offset));
        let delta = decode_i64_le(bytes, &mut offset);
        edge.push((label, delta));
    }

    assert_eq!(
        offset, appendix_end,
        "label delta appendix did not consume exactly {} bytes",
        appendix_len
    );

    LabelStatsDeltaEventWire {
        mutation_id,
        shard_event_seq,
        label_stats_delta: gleaph_graph_kernel::plan_exec::LabelStatsDelta { vertex, edge },
    }
}

const LABEL_DELTA_MAX_APPENDIX: u32 = {
    let kind = 4 + MAX_LABEL_DELTAS_PER_KIND * 10;
    kind * 2
};

impl Storable for StoredLabelStatsDeltaEvent {
    // Deliberately Unbounded: the manual layout already enforces encode-time bounds.
    // Bounded values make StableBTreeMap fresh-key insert regress for the same node-size reason.
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_label_stats_delta_event(&self.0))
    }

    fn into_bytes(self) -> Vec<u8> {
        #[cfg(feature = "canbench")]
        let _scope = canbench_scope("label_stats_delta_event_encode");
        let bytes = encode_label_stats_delta_event(&self.0);
        #[cfg(feature = "canbench")]
        drop(_scope);
        bytes
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(decode_label_stats_delta_event(bytes.as_ref()))
    }
}

pub struct LabelStatsDeltaLog<M: Memory> {
    map: StableBTreeMap<ShardEventSeq, StoredLabelStatsDeltaEvent, M>,
}

impl<M: Memory> LabelStatsDeltaLog<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(&mut self, event: LabelStatsDeltaEventWire) {
        self.map.insert(event.shard_event_seq, event.into());
    }

    pub fn remove_through(&mut self, through_seq: ShardEventSeq) {
        let to_remove: Vec<ShardEventSeq> = self
            .map
            .range(..=through_seq)
            .map(|entry| *entry.key())
            .collect();
        for seq in to_remove {
            self.map.remove(&seq);
        }
    }

    pub fn list_from(&self, from_seq: ShardEventSeq, limit: u32) -> Vec<LabelStatsDeltaEventWire> {
        let mut out = Vec::new();
        for entry in self.map.range(from_seq..) {
            out.push(entry.value().into());
            if out.len() >= limit as usize {
                break;
            }
        }
        out
    }
}

pub struct GraphMutationJournal<M: Memory> {
    map: StableBTreeMap<MutationId, GraphMutationJournalEntry, M>,
}

impl<M: Memory> GraphMutationJournal<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    pub fn get(&self, mutation_id: MutationId) -> Option<GraphMutationJournalEntry> {
        self.map.get(&mutation_id)
    }

    pub fn insert(&mut self, entry: GraphMutationJournalEntry) {
        self.map.insert(entry.mutation_id(), entry);
    }

    /// One amortized, budgeted retention pass over the keyspace starting strictly after
    /// `start_after` (ADR 0027). Entries older than `retention_ns` are evicted; legacy
    /// entries with no timestamp are lazy-stamped to `now` (so they age out from upgrade
    /// time, never prematurely). Returns `(scanned, removed, last_examined_key)`; the
    /// caller persists `last_examined_key` as the next round-robin cursor.
    pub fn evict_expired(
        &mut self,
        start_after: Option<MutationId>,
        budget: usize,
        now: u64,
        retention_ns: u64,
    ) -> (u32, u32, Option<MutationId>) {
        let lower = match start_after {
            Some(id) => StdBound::Excluded(id),
            None => StdBound::Unbounded,
        };
        let mut scanned: u32 = 0;
        let mut last_key: Option<MutationId> = None;
        let mut to_remove: Vec<MutationId> = Vec::new();
        let mut to_stamp: Vec<GraphMutationJournalEntry> = Vec::new();
        for entry in self.map.range((lower, StdBound::Unbounded)).take(budget) {
            let id = *entry.key();
            let value = entry.value();
            scanned += 1;
            last_key = Some(id);
            match value.recorded_at_ns() {
                None => {
                    let mut stamped = value;
                    stamped.set_recorded_at_ns(Some(now));
                    to_stamp.push(stamped);
                }
                Some(ts) if now.saturating_sub(ts) > retention_ns => to_remove.push(id),
                Some(_) => {}
            }
        }
        let removed = to_remove.len() as u32;
        for id in &to_remove {
            self.map.remove(id);
        }
        for entry in to_stamp {
            self.map.insert(entry.mutation_id(), entry);
        }
        (scanned, removed, last_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::VertexLabelId;
    use gleaph_graph_kernel::plan_exec::LabelStatsDelta;
    use ic_stable_structures::VectorMemory;

    const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
    const RETENTION_NS: u64 = 9 * DAY_NS;

    fn journal() -> GraphMutationJournal<VectorMemory> {
        GraphMutationJournal::init(VectorMemory::default())
    }

    fn completed_at(mutation_id: MutationId, recorded_at_ns: u64) -> GraphMutationJournalEntry {
        GraphMutationJournalEntry::completed(mutation_id, 1, None, None, Vec::new(), recorded_at_ns)
    }

    #[test]
    fn wire_preserves_bulk_cursor_and_progress() {
        let mut entry = completed_at(9, 0);
        entry.set_next_index(Some(1));
        entry.set_bulk_progress(Some(GraphBulkMutationProgress::new(2, 1, vec![3])));
        let wire = entry.wire();
        assert_eq!(wire.next_index(), Some(1));
        let progress = wire.bulk_progress().as_ref().expect("bulk progress");
        assert_eq!(progress.operation_count(), 2);
        assert_eq!(progress.completed_count(), 1);
        assert_eq!(progress.operation_row_counts(), &[3]);
    }

    #[test]
    fn evicts_completed_entry_past_retention() {
        let mut j = journal();
        j.insert(completed_at(1, 0));
        let (scanned, removed, _) = j.evict_expired(None, 8, RETENTION_NS + 1, RETENTION_NS);
        assert_eq!((scanned, removed), (1, 1));
        assert!(j.get(1).is_none());
    }

    #[test]
    fn retains_entry_within_retention() {
        let mut j = journal();
        j.insert(completed_at(1, 0));
        let (scanned, removed, _) = j.evict_expired(None, 8, RETENTION_NS - 1, RETENTION_NS);
        assert_eq!((scanned, removed), (1, 0));
        assert!(j.get(1).is_some());
    }

    #[test]
    fn evicts_incomplete_entry_past_retention() {
        // Persisted Incomplete entries are also replay-dedup markers, so they are
        // age-bounded too (must not leak, must not be dropped within the replay window).
        let mut j = journal();
        j.insert(GraphMutationJournalEntry::incomplete(2, None, None, 0));
        let (_, removed_early, _) = j.evict_expired(None, 8, RETENTION_NS - 1, RETENTION_NS);
        assert_eq!(removed_early, 0);
        let (_, removed_late, _) = j.evict_expired(None, 8, RETENTION_NS + 1, RETENTION_NS);
        assert_eq!(removed_late, 1);
        assert!(j.get(2).is_none());
    }

    #[test]
    fn lazy_stamps_legacy_entry_then_evicts_after_retention() {
        let mut j = journal();
        // Pre-ADR-0027 bytes decode with no timestamp.
        j.insert(GraphMutationJournalEntry::V1(GraphMutationJournalEntryV1 {
            mutation_id: 3,
            state: MutationJournalState::Completed,
            row_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            recorded_at_ns: None,
            next_index: None,
            bulk_progress: None,
        }));
        let upgrade_ns = 1_000 * DAY_NS;
        // First visit lazy-stamps to "now" instead of evicting, even though "now" is huge.
        let (_, removed, _) = j.evict_expired(None, 8, upgrade_ns, RETENTION_NS);
        assert_eq!(removed, 0);
        assert_eq!(j.get(3).unwrap().recorded_at_ns(), Some(upgrade_ns));
        // It then ages out from the stamp time, not from epoch.
        let (_, removed_before, _) =
            j.evict_expired(None, 8, upgrade_ns + RETENTION_NS - 1, RETENTION_NS);
        assert_eq!(removed_before, 0);
        let (_, removed_after, _) =
            j.evict_expired(None, 8, upgrade_ns + RETENTION_NS + 1, RETENTION_NS);
        assert_eq!(removed_after, 1);
        assert!(j.get(3).is_none());
    }

    #[test]
    fn round_robin_cursor_evicts_all_across_laps() {
        let mut j = journal();
        for id in 1..=5u64 {
            j.insert(completed_at(id, 0));
        }
        let now = RETENTION_NS + 1;
        let mut cursor = None;
        let mut total_removed = 0u32;
        loop {
            let (scanned, removed, last) = j.evict_expired(cursor, 2, now, RETENTION_NS);
            total_removed += removed;
            if scanned < 2 {
                break;
            }
            cursor = last;
        }
        assert_eq!(total_removed, 5);
        for id in 1..=5u64 {
            assert!(j.get(id).is_none());
        }
    }

    #[test]
    fn encoded_byte_lengths_are_printed() {
        let scalar_completed =
            GraphMutationJournalEntry::completed(1, 1, None, None, Vec::new(), 0);
        let edge_completed =
            GraphMutationJournalEntry::completed(2, 3, Some(4), Some(5), vec![7u32, 42u32], 0);
        let mut bulk =
            GraphMutationJournalEntry::completed(3, 10, Some(6), Some(9), vec![100u32], 0);
        bulk.set_next_index(Some(2));
        bulk.set_bulk_progress(Some(GraphBulkMutationProgress::new(4, 2, vec![1, 3, 5, 7])));
        let label_event = LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 1,
            label_stats_delta: LabelStatsDelta {
                vertex: vec![(VertexLabelId::from_raw(1), 1)],
                edge: vec![],
            },
        };
        eprintln!(
            "encoded byte lengths: scalar_journal={} edge_journal={} bulk_journal={} label_delta_event={}",
            scalar_completed.into_bytes().len(),
            edge_completed.into_bytes().len(),
            bulk.into_bytes().len(),
            StoredLabelStatsDeltaEvent::from(label_event)
                .into_bytes()
                .len(),
        );
    }

    #[test]
    fn roundtrip_journal_scalar_completed() {
        let original = GraphMutationJournalEntry::completed(42, 7, None, None, Vec::new(), 12345);
        let bytes = original.clone().into_bytes();
        let decoded = GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_journal_scalar_incomplete() {
        let original = GraphMutationJournalEntry::incomplete(99, Some(1), Some(2), 99999);
        let bytes = original.clone().into_bytes();
        let decoded = GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_journal_edge_with_hot_forward() {
        let original = GraphMutationJournalEntry::completed(
            7,
            3,
            Some(10),
            Some(20),
            vec![1u32, 2, 3, 4],
            5555,
        );
        let bytes = original.clone().into_bytes();
        let decoded = GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_journal_bulk_with_progress() {
        let mut original =
            GraphMutationJournalEntry::completed(3, 10, Some(6), Some(9), vec![100u32], 0);
        original.set_next_index(Some(2));
        original.set_bulk_progress(Some(GraphBulkMutationProgress::new(4, 2, vec![1, 3, 5, 7])));
        let bytes = original.clone().into_bytes();
        let decoded = GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_label_delta_empty() {
        let original = LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 2,
            label_stats_delta: LabelStatsDelta {
                vertex: vec![],
                edge: vec![],
            },
        };
        let bytes = StoredLabelStatsDeltaEvent::from(original.clone()).into_bytes();
        let decoded: LabelStatsDeltaEventWire =
            StoredLabelStatsDeltaEvent::from_bytes(Cow::Owned(bytes)).into();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_label_delta_multi() {
        let original = LabelStatsDeltaEventWire {
            mutation_id: 7,
            shard_event_seq: 99,
            label_stats_delta: LabelStatsDelta {
                vertex: vec![
                    (VertexLabelId::from_raw(1), 5),
                    (VertexLabelId::from_raw(2), -3),
                ],
                edge: vec![(EdgeLabelId::from_raw(10), 1)],
            },
        };
        let bytes = StoredLabelStatsDeltaEvent::from(original.clone()).into_bytes();
        let decoded: LabelStatsDeltaEventWire =
            StoredLabelStatsDeltaEvent::from_bytes(Cow::Owned(bytes)).into();
        assert_eq!(decoded, original);
    }

    #[test]
    #[should_panic(expected = "unknown journal layout version")]
    fn malformed_journal_unknown_version() {
        let bytes = vec![99u8; JOURNAL_PRIMARY_SIZE];
        GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "journal entry truncated")]
    fn malformed_journal_truncated() {
        GraphMutationJournalEntry::from_bytes(Cow::Owned(vec![JOURNAL_LAYOUT_VERSION; 3]));
    }

    #[test]
    #[should_panic(expected = "journal entry length mismatch")]
    fn malformed_journal_length_mismatch() {
        let mut bytes =
            GraphMutationJournalEntry::completed(1, 1, None, None, Vec::new(), 0).into_bytes();
        bytes.push(0);
        GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "unknown label delta layout version")]
    fn malformed_label_delta_unknown_version() {
        let bytes = vec![99u8; LABEL_DELTA_PRIMARY_SIZE];
        StoredLabelStatsDeltaEvent::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "label delta event truncated")]
    fn malformed_label_delta_truncated() {
        StoredLabelStatsDeltaEvent::from_bytes(Cow::Owned(vec![LABEL_DELTA_LAYOUT_VERSION; 3]));
    }

    #[test]
    #[should_panic(expected = "hot_forward_vertices")]
    fn bound_enforced_hot_forward_overflow() {
        let mut entry = GraphMutationJournalEntry::completed(1, 1, None, None, Vec::new(), 0);
        entry.set_hot_forward_vertices(vec![0u32; (MAX_HOT_FORWARD_VERTICES + 1) as usize]);
        let _ = entry.into_bytes();
    }

    #[test]
    #[should_panic(expected = "vertex label deltas")]
    fn bound_enforced_label_vertex_overflow() {
        let event = LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 1,
            label_stats_delta: LabelStatsDelta {
                vertex: vec![
                    (VertexLabelId::from_raw(1), 1);
                    (MAX_LABEL_DELTAS_PER_KIND + 1) as usize
                ],
                edge: vec![],
            },
        };
        let _ = StoredLabelStatsDeltaEvent::from(event).into_bytes();
    }

    #[test]
    #[should_panic(expected = "hot_forward count exceeds bound")]
    fn malformed_journal_decode_rejects_oversized_hot_forward_count() {
        let mut bytes =
            GraphMutationJournalEntry::completed(1, 1, None, None, vec![7], 0).into_bytes();
        bytes[52..56].copy_from_slice(&(MAX_HOT_FORWARD_VERTICES + 1).to_le_bytes());
        GraphMutationJournalEntry::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "vertex label delta count exceeds bound")]
    fn malformed_label_delta_decode_rejects_oversized_count() {
        let event = LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 1,
            label_stats_delta: LabelStatsDelta {
                vertex: vec![(VertexLabelId::from_raw(1), 1)],
                edge: vec![],
            },
        };
        let mut bytes = StoredLabelStatsDeltaEvent::from(event).into_bytes();
        bytes[21..25].copy_from_slice(&(MAX_LABEL_DELTAS_PER_KIND + 1).to_le_bytes());
        StoredLabelStatsDeltaEvent::from_bytes(Cow::Owned(bytes));
    }
}
