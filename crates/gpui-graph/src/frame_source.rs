//! Frame source seam and paint-frame wire format (§18.2, §27.3; ADR 0076).
//!
//! A [`FrameSource`] names where a view's [`PaintFrame`](crate::paint::PaintFrame)
//! is produced. Today exactly one producer exists: the synchronous in-process
//! build during prepaint (`prepare_canvas` →
//! [`build_indexed_paint_frame`](crate::paint::build_indexed_paint_frame)).
//! The `Worker` variant names the off-main-thread producer of ADR 0076; its
//! host side lives in [`crate::worker`].
//!
//! [`PaintFrameWire`] is the transfer form used across that seam: a frame
//! linearized into structure-of-arrays planes, one flat buffer per primitive
//! field. `encode` then `decode` restores the exact frame, field-for-field,
//! order preserved. [`Self::to_wire_bytes`] and [`Self::from_wire_bytes`] are
//! the ArrayBuffer form of those planes for the web worker transport
//! (ADR 0076 S2): decoding validates every plane before constructing the
//! wire, so the canonical-bytes invariant `decode` relies on holds no matter
//! who produced the bytes.

use slotmap::{Key, KeyData};

use glam::Vec2;

use crate::graph::{EdgeDirection, EdgeId, NodeId};
use crate::paint::{
    Bezier, OverlayCategory, PaintEdge, PaintEdgeLabel, PaintFrame, PaintLabel, PaintNode,
};

/// Where a view's [`PaintFrame`](crate::paint::PaintFrame) is produced
/// (ADR 0076).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FrameSource {
    /// The synchronous in-process build: during prepaint the view sizes its
    /// viewport (`prepare_canvas`, including the one-time initial fit) and
    /// builds an indexed paint frame from one synced read of the scene. This
    /// is the default and, until the ADR 0076 worker host lands (S2), the
    /// only mode with an execution path.
    #[default]
    InProcess,
    /// A dedicated worker owns the graph backend and ships finished frames to
    /// the main thread as [`PaintFrameWire`] buffers (ADR 0076 S2). Selecting
    /// it is opt-in: [`crate::view::GraphViewState::set_frame_source`] plus a
    /// connected [`crate::worker::WorkerChannel`]; the in-process build stays
    /// the default.
    Worker,
}

/// A [`PaintFrame`] linearized into structure-of-arrays planes (ADR 0076).
///
/// Every primitive field of the frame becomes one flat buffer: identity
/// planes carry slotmap key bits ([`KeyData::as_ffi`]), geometry planes carry
/// packed `f32`s, flags ride one-byte planes, overlay category and edge
/// direction ride one-byte discriminant planes, and variable-length data
/// (Bézier paths, label text) flattens behind a per-element length plane.
/// [`Self::decode`] walks those planes back into the exact frame, including
/// element order.
///
/// The planes are private and only ever written by [`Self::encode`], so they
/// always hold canonical bytes; decode relies on that invariant and fails
/// loudly if a plane is corrupt rather than silently producing a wrong frame.
/// [`Self::from_wire_bytes`] upholds that invariant at the transport
/// boundary: it validates every byte plane before constructing a wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaintFrameWire {
    nodes: NodePlanes,
    edges: EdgePlanes,
    labels: LabelPlanes,
    edge_labels: EdgeLabelPlanes,
}

impl PaintFrameWire {
    /// Linearize a frame into structure-of-arrays planes.
    pub fn encode(frame: &PaintFrame) -> Self {
        let mut nodes = NodePlanes::default();
        for node in &frame.nodes {
            nodes.id.push(node.id.data().as_ffi());
            nodes.position.extend_from_slice(&node.position.to_array());
            nodes.radius.push(node.radius);
            nodes.selected.push(u8::from(node.selected));
            nodes.hovered.push(u8::from(node.hovered));
            nodes.overlay.push(overlay_byte(node.overlay));
            nodes.simplified.push(u8::from(node.simplified));
        }

        let mut edges = EdgePlanes::default();
        for edge in &frame.edges {
            edges.id.push(edge.id.data().as_ffi());
            edges.source.extend_from_slice(&edge.source.to_array());
            edges.target.extend_from_slice(&edge.target.to_array());
            edges.path.push(&edge.path);
            edges.direction.push(direction_byte(edge.direction));
            edges.selected.push(u8::from(edge.selected));
            edges.hovered.push(u8::from(edge.hovered));
            edges.overlay.push(overlay_byte(edge.overlay));
            edges.omit_arrow.push(u8::from(edge.omit_arrow));
        }

        let mut labels = LabelPlanes::default();
        for label in &frame.labels {
            labels
                .position
                .extend_from_slice(&label.position.to_array());
            labels.text.push(&label.text);
        }

        let mut edge_labels = EdgeLabelPlanes::default();
        for label in &frame.edge_labels {
            edge_labels.edge.push(label.edge.data().as_ffi());
            edge_labels
                .position
                .extend_from_slice(&label.position.to_array());
            edge_labels
                .offset
                .extend_from_slice(&label.offset.to_array());
            edge_labels.text.push(&label.text);
            edge_labels.path.push(&label.path);
            edge_labels.t.push(label.t);
        }

        Self {
            nodes,
            edges,
            labels,
            edge_labels,
        }
    }

    /// Restore the exact frame this wire encodes: every primitive field equal
    /// to the encoded frame's, identical element order.
    pub fn decode(&self) -> PaintFrame {
        let nodes = self
            .nodes
            .id
            .iter()
            .enumerate()
            .map(|(i, &bits)| PaintNode {
                id: NodeId::from(KeyData::from_ffi(bits)),
                position: Vec2::from_slice(&self.nodes.position[i * 2..i * 2 + 2]),
                radius: self.nodes.radius[i],
                selected: self.nodes.selected[i] != 0,
                hovered: self.nodes.hovered[i] != 0,
                overlay: overlay_from_byte(self.nodes.overlay[i]),
                simplified: self.nodes.simplified[i] != 0,
            })
            .collect();

        let mut path_points_read = 0usize;
        let mut edges = Vec::with_capacity(self.edges.id.len());
        for i in 0..self.edges.id.len() {
            let segments = self.edges.path.segments_per_element[i] as usize;
            let points_end = path_points_read + segments * 6;
            edges.push(PaintEdge {
                id: EdgeId::from(KeyData::from_ffi(self.edges.id[i])),
                source: Vec2::from_slice(&self.edges.source[i * 2..i * 2 + 2]),
                target: Vec2::from_slice(&self.edges.target[i * 2..i * 2 + 2]),
                path: beziers(&self.edges.path.points[path_points_read..points_end]),
                direction: direction_from_byte(self.edges.direction[i]),
                selected: self.edges.selected[i] != 0,
                hovered: self.edges.hovered[i] != 0,
                overlay: overlay_from_byte(self.edges.overlay[i]),
                omit_arrow: self.edges.omit_arrow[i] != 0,
            });
            path_points_read = points_end;
        }

        let mut text_bytes_read = 0usize;
        let labels = (0..self.labels.text.bytes_per_label.len())
            .map(|i| {
                let end = text_bytes_read + self.labels.text.bytes_per_label[i] as usize;
                let text = str_from_canonical_bytes(&self.labels.text.bytes[text_bytes_read..end]);
                text_bytes_read = end;
                PaintLabel {
                    position: Vec2::from_slice(&self.labels.position[i * 2..i * 2 + 2]),
                    text,
                }
            })
            .collect();

        let mut label_path_points_read = 0usize;
        let mut label_text_bytes_read = 0usize;
        let mut edge_labels = Vec::with_capacity(self.edge_labels.edge.len());
        for i in 0..self.edge_labels.edge.len() {
            let segments = self.edge_labels.path.segments_per_element[i] as usize;
            let points_end = label_path_points_read + segments * 6;
            let text_end =
                label_text_bytes_read + self.edge_labels.text.bytes_per_label[i] as usize;
            edge_labels.push(PaintEdgeLabel {
                edge: EdgeId::from(KeyData::from_ffi(self.edge_labels.edge[i])),
                position: Vec2::from_slice(&self.edge_labels.position[i * 2..i * 2 + 2]),
                offset: Vec2::from_slice(&self.edge_labels.offset[i * 2..i * 2 + 2]),
                text: str_from_canonical_bytes(
                    &self.edge_labels.text.bytes[label_text_bytes_read..text_end],
                ),
                path: beziers(&self.edge_labels.path.points[label_path_points_read..points_end]),
                t: self.edge_labels.t[i],
            });
            label_path_points_read = points_end;
            label_text_bytes_read = text_end;
        }

        PaintFrame {
            nodes,
            edges,
            labels,
            edge_labels,
        }
    }

    /// Serialize this wire into little-endian bytes for the ADR 0076 worker
    /// transport (one transferable `ArrayBuffer` per message).
    ///
    /// The layout is the plane sequence of [`NodePlanes`], [`EdgePlanes`],
    /// [`LabelPlanes`], and [`EdgeLabelPlanes`] in declaration order; each
    /// plane is a `u64` little-endian element count followed by that many
    /// little-endian elements. [`Self::from_wire_bytes`] is the only defined
    /// reader.
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let nodes = &self.nodes;
        push_u64_plane(&mut out, &nodes.id);
        push_f32_plane(&mut out, &nodes.position);
        push_f32_plane(&mut out, &nodes.radius);
        push_u8_plane(&mut out, &nodes.selected);
        push_u8_plane(&mut out, &nodes.hovered);
        push_u8_plane(&mut out, &nodes.overlay);
        push_u8_plane(&mut out, &nodes.simplified);

        let edges = &self.edges;
        push_u64_plane(&mut out, &edges.id);
        push_f32_plane(&mut out, &edges.source);
        push_f32_plane(&mut out, &edges.target);
        push_u32_plane(&mut out, &edges.path.segments_per_element);
        push_f32_plane(&mut out, &edges.path.points);
        push_u8_plane(&mut out, &edges.direction);
        push_u8_plane(&mut out, &edges.selected);
        push_u8_plane(&mut out, &edges.hovered);
        push_u8_plane(&mut out, &edges.overlay);
        push_u8_plane(&mut out, &edges.omit_arrow);

        let labels = &self.labels;
        push_f32_plane(&mut out, &labels.position);
        push_u32_plane(&mut out, &labels.text.bytes_per_label);
        push_u8_plane(&mut out, &labels.text.bytes);

        let edge_labels = &self.edge_labels;
        push_u64_plane(&mut out, &edge_labels.edge);
        push_f32_plane(&mut out, &edge_labels.position);
        push_f32_plane(&mut out, &edge_labels.offset);
        push_u32_plane(&mut out, &edge_labels.text.bytes_per_label);
        push_u8_plane(&mut out, &edge_labels.text.bytes);
        push_u32_plane(&mut out, &edge_labels.path.segments_per_element);
        push_f32_plane(&mut out, &edge_labels.path.points);
        push_f32_plane(&mut out, &edge_labels.t);
        out
    }

    /// Parse bytes produced by [`Self::to_wire_bytes`].
    ///
    /// This is the trust boundary of the wire: every count, length total, and
    /// flag/discriminant byte is validated here so the parsed wire's planes
    /// hold canonical bytes and [`Self::decode`] keeps its "never sees a
    /// corrupt plane" guarantee. Anything else — truncation, contradictory
    /// plane lengths, unknown discriminants, trailing bytes — returns an
    /// error instead of a partially wrong frame.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, WireFormatError> {
        let mut r = WireReader::new(bytes);

        let id = r.u64_plane()?;
        let position = r.f32_plane()?;
        let radius = r.f32_plane()?;
        let selected = r.flag_plane("node selected")?;
        let hovered = r.flag_plane("node hovered")?;
        let overlay = r.overlay_plane("node overlay")?;
        let simplified = r.flag_plane("node simplified")?;
        if position.len() != id.len() * 2
            || radius.len() != id.len()
            || selected.len() != id.len()
            || hovered.len() != id.len()
            || overlay.len() != id.len()
            || simplified.len() != id.len()
        {
            return Err(WireFormatError::InconsistentPlanes);
        }

        let edges_id = r.u64_plane()?;
        let source = r.f32_plane()?;
        let target = r.f32_plane()?;
        let path_segments = r.u32_plane()?;
        let path_points = r.f32_plane()?;
        let direction = r.direction_plane("edge direction")?;
        let edges_selected = r.flag_plane("edge selected")?;
        let edges_hovered = r.flag_plane("edge hovered")?;
        let edges_overlay = r.overlay_plane("edge overlay")?;
        let omit_arrow = r.flag_plane("edge omit-arrow")?;
        if source.len() != edges_id.len() * 2
            || target.len() != edges_id.len() * 2
            || path_points.len() != segment_point_len(&path_segments)?
            || direction.len() != edges_id.len()
            || edges_selected.len() != edges_id.len()
            || edges_hovered.len() != edges_id.len()
            || edges_overlay.len() != edges_id.len()
            || omit_arrow.len() != edges_id.len()
        {
            return Err(WireFormatError::InconsistentPlanes);
        }

        let labels_position = r.f32_plane()?;
        let text_lengths = r.u32_plane()?;
        let text_bytes = r.u8_plane()?;
        if labels_position.len() != text_lengths.len() * 2
            || text_bytes.len() != text_byte_len(&text_lengths)?
        {
            return Err(WireFormatError::InconsistentPlanes);
        }

        let edge_bits = r.u64_plane()?;
        let el_position = r.f32_plane()?;
        let el_offset = r.f32_plane()?;
        let el_text_lengths = r.u32_plane()?;
        let el_text_bytes = r.u8_plane()?;
        let el_path_segments = r.u32_plane()?;
        let el_path_points = r.f32_plane()?;
        let t = r.f32_plane()?;
        if el_position.len() != edge_bits.len() * 2
            || el_offset.len() != edge_bits.len() * 2
            || el_text_lengths.len() != edge_bits.len()
            || el_text_bytes.len() != text_byte_len(&el_text_lengths)?
            || el_path_points.len() != segment_point_len(&el_path_segments)?
            || t.len() != edge_bits.len()
        {
            return Err(WireFormatError::InconsistentPlanes);
        }

        if !r.is_empty() {
            return Err(WireFormatError::TrailingBytes { extra: r.len() });
        }

        Ok(Self {
            nodes: NodePlanes {
                id,
                position,
                radius,
                selected,
                hovered,
                overlay,
                simplified,
            },
            edges: EdgePlanes {
                id: edges_id,
                source,
                target,
                path: SegmentPlane {
                    segments_per_element: path_segments,
                    points: path_points,
                },
                direction,
                selected: edges_selected,
                hovered: edges_hovered,
                overlay: edges_overlay,
                omit_arrow,
            },
            labels: LabelPlanes {
                position: labels_position,
                text: TextPlane {
                    bytes_per_label: text_lengths,
                    bytes: text_bytes,
                },
            },
            edge_labels: EdgeLabelPlanes {
                edge: edge_bits,
                position: el_position,
                offset: el_offset,
                text: TextPlane {
                    bytes_per_label: el_text_lengths,
                    bytes: el_text_bytes,
                },
                path: SegmentPlane {
                    segments_per_element: el_path_segments,
                    points: el_path_points,
                },
                t,
            },
        })
    }
}

/// Byte length of a flattened six-`f32`-per-segment point plane behind
/// per-element segment counts.
fn segment_point_len(segments: &[u32]) -> Result<usize, WireFormatError> {
    segments
        .iter()
        .try_fold(0usize, |acc, &s| acc.checked_add(s as usize * 6))
        .ok_or(WireFormatError::InconsistentPlanes)
}

/// Byte length of a flattened UTF-8 text plane behind per-label byte lengths.
fn text_byte_len(lengths: &[u32]) -> Result<usize, WireFormatError> {
    lengths
        .iter()
        .try_fold(0usize, |acc, &n| acc.checked_add(n as usize))
        .ok_or(WireFormatError::InconsistentPlanes)
}

/// Why wire bytes were rejected by [`PaintFrameWire::from_wire_bytes`] or a
/// request codec. Variants carry enough context to name the exact failure;
/// callers should match on them rather than string-match messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireFormatError {
    /// The byte slice ended before a complete value could be read.
    Truncated {
        /// Byte count the read required.
        needed: usize,
        /// Bytes actually left.
        remaining: usize,
    },
    /// A plane's element count cannot address the bytes it claims.
    ExcessiveLength(u64),
    /// Two planes that describe the same elements disagree on their lengths.
    InconsistentPlanes,
    /// A one-byte discriminant is outside the wire vocabulary.
    BadDiscriminant {
        /// Which named field held the bad byte.
        field: &'static str,
        /// The rejected byte.
        value: u8,
    },
    /// Complete message followed by unconsumed bytes.
    TrailingBytes {
        /// How many bytes remain.
        extra: usize,
    },
    /// The request kind carries application-typed payloads (`GraphBatch`,
    /// `GraphPatch`) whose byte form belongs to an application-supplied
    /// payload codec; the library wire covers only library-owned content.
    PayloadCodecRequired,
}

impl std::fmt::Display for WireFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { needed, remaining } => {
                write!(
                    f,
                    "truncated wire: needed {needed} bytes, {remaining} remain"
                )
            }
            Self::ExcessiveLength(count) => {
                write!(f, "wire plane length {count} is not addressable")
            }
            Self::InconsistentPlanes => write!(
                f,
                "inconsistent wire planes: length planes contradict each other"
            ),
            Self::BadDiscriminant { field, value } => {
                write!(f, "corrupt wire: unknown {field} discriminant {value}")
            }
            Self::TrailingBytes { extra } => {
                write!(f, "trailing bytes after complete wire message: {extra}")
            }
            Self::PayloadCodecRequired => {
                write!(
                    f,
                    "request carries application-typed payloads; an application-supplied payload codec owns its byte form"
                )
            }
        }
    }
}

impl std::error::Error for WireFormatError {}

/// Sequential little-endian reader over one wire message.
struct WireReader<'a> {
    bytes: &'a [u8],
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn take(&mut self, needed: usize) -> Result<&'a [u8], WireFormatError> {
        if self.bytes.len() < needed {
            return Err(WireFormatError::Truncated {
                needed,
                remaining: self.bytes.len(),
            });
        }
        let (head, tail) = self.bytes.split_at(needed);
        self.bytes = tail;
        Ok(head)
    }

    /// Read one `u64` element count as a usable `usize` element count.
    fn count(&mut self) -> Result<usize, WireFormatError> {
        let raw = self.u64_raw()?;
        usize::try_from(raw).map_err(|_| WireFormatError::ExcessiveLength(raw))
    }

    /// Element count for a plane with `element_width`-byte elements,
    /// rejecting counts whose byte span overflows `usize`.
    fn plane_len(&mut self, element_width: usize) -> Result<usize, WireFormatError> {
        let raw = self.count()?;
        raw.checked_mul(element_width)
            .map(|_| raw)
            .ok_or(WireFormatError::ExcessiveLength(raw as u64))
    }

    fn u64_raw(&mut self) -> Result<u64, WireFormatError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    fn u64_plane(&mut self) -> Result<Vec<u64>, WireFormatError> {
        let n = self.plane_len(8)?;
        Ok(self
            .take(n * 8)?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect())
    }

    fn u32_plane(&mut self) -> Result<Vec<u32>, WireFormatError> {
        let n = self.plane_len(4)?;
        Ok(self
            .take(n * 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect())
    }

    fn f32_plane(&mut self) -> Result<Vec<f32>, WireFormatError> {
        let n = self.plane_len(4)?;
        Ok(self
            .take(n * 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect())
    }

    fn u8_plane(&mut self) -> Result<Vec<u8>, WireFormatError> {
        let n = self.count()?;
        Ok(self.take(n)?.to_vec())
    }

    /// One-byte-per-element boolean plane restricted to the canonical `{0,1}`.
    fn flag_plane(&mut self, field: &'static str) -> Result<Vec<u8>, WireFormatError> {
        self.discriminant_plane(field, 1)
    }

    /// Overlay-category plane restricted to the canonical `0..=3`.
    fn overlay_plane(&mut self, field: &'static str) -> Result<Vec<u8>, WireFormatError> {
        self.discriminant_plane(field, 3)
    }

    /// Edge-direction plane restricted to the canonical `{0,1}`.
    fn direction_plane(&mut self, field: &'static str) -> Result<Vec<u8>, WireFormatError> {
        self.discriminant_plane(field, 1)
    }

    fn discriminant_plane(
        &mut self,
        field: &'static str,
        max: u8,
    ) -> Result<Vec<u8>, WireFormatError> {
        let mut plane = self.u8_plane()?;
        for &value in &plane {
            if value > max {
                return Err(WireFormatError::BadDiscriminant { field, value });
            }
        }
        Ok(std::mem::take(&mut plane))
    }
}

fn push_count(out: &mut Vec<u8>, count: usize) {
    let count = u64::try_from(count).expect("plane length exceeds u64 wire range");
    out.extend_from_slice(&count.to_le_bytes());
}

fn push_u64_plane(out: &mut Vec<u8>, plane: &[u64]) {
    push_count(out, plane.len());
    for v in plane {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn push_u32_plane(out: &mut Vec<u8>, plane: &[u32]) {
    push_count(out, plane.len());
    for v in plane {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn push_f32_plane(out: &mut Vec<u8>, plane: &[f32]) {
    push_count(out, plane.len());
    for v in plane {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn push_u8_plane(out: &mut Vec<u8>, plane: &[u8]) {
    push_count(out, plane.len());
    out.extend_from_slice(plane);
}

/// Variable-length Bézier paths flattened behind a per-element segment count.
#[derive(Debug, Clone, Default, PartialEq)]
struct SegmentPlane {
    segments_per_element: Vec<u32>,
    /// Six `f32`s per segment: `p0.x, p0.y, p1.x, p1.y, p2.x, p2.y`.
    points: Vec<f32>,
}

impl SegmentPlane {
    fn push(&mut self, path: &[Bezier]) {
        self.segments_per_element
            .push(u32::try_from(path.len()).expect("segment count exceeds u32 wire range"));
        for (p0, p1, p2) in path {
            self.points
                .extend_from_slice(&[p0.x, p0.y, p1.x, p1.y, p2.x, p2.y]);
        }
    }
}

/// Label text flattened behind a per-label UTF-8 byte length.
#[derive(Debug, Clone, Default, PartialEq)]
struct TextPlane {
    bytes_per_label: Vec<u32>,
    bytes: Vec<u8>,
}

impl TextPlane {
    fn push(&mut self, text: &str) {
        self.bytes_per_label
            .push(u32::try_from(text.len()).expect("label byte length exceeds u32 wire range"));
        self.bytes.extend_from_slice(text.as_bytes());
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct NodePlanes {
    id: Vec<u64>,
    position: Vec<f32>,
    radius: Vec<f32>,
    selected: Vec<u8>,
    hovered: Vec<u8>,
    overlay: Vec<u8>,
    simplified: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct EdgePlanes {
    id: Vec<u64>,
    source: Vec<f32>,
    target: Vec<f32>,
    path: SegmentPlane,
    direction: Vec<u8>,
    selected: Vec<u8>,
    hovered: Vec<u8>,
    overlay: Vec<u8>,
    omit_arrow: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct LabelPlanes {
    position: Vec<f32>,
    text: TextPlane,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct EdgeLabelPlanes {
    edge: Vec<u64>,
    position: Vec<f32>,
    offset: Vec<f32>,
    text: TextPlane,
    path: SegmentPlane,
    t: Vec<f32>,
}

/// Rebuild Bézier segments from a six-`f32`-per-segment slice.
fn beziers(points: &[f32]) -> Vec<Bezier> {
    points
        .as_chunks::<6>()
        .0
        .iter()
        .map(|p| {
            (
                Vec2::from_array([p[0], p[1]]),
                Vec2::from_array([p[2], p[3]]),
                Vec2::from_array([p[4], p[5]]),
            )
        })
        .collect()
}

/// Decode canonical-wire UTF-8 bytes. Encode only ever stores valid `str`
/// bytes, so a failure here means the plane was corrupted after encoding.
fn str_from_canonical_bytes(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("corrupt wire: label text plane is not UTF-8")
}

fn overlay_byte(category: OverlayCategory) -> u8 {
    match category {
        OverlayCategory::None => 0,
        OverlayCategory::Dimmed => 1,
        OverlayCategory::Emphasized => 2,
        OverlayCategory::Accent => 3,
    }
}

fn overlay_from_byte(byte: u8) -> OverlayCategory {
    match byte {
        0 => OverlayCategory::None,
        1 => OverlayCategory::Dimmed,
        2 => OverlayCategory::Emphasized,
        3 => OverlayCategory::Accent,
        other => panic!("corrupt wire: unknown overlay discriminant {other}"),
    }
}

fn direction_byte(direction: EdgeDirection) -> u8 {
    match direction {
        EdgeDirection::Directed => 0,
        EdgeDirection::Undirected => 1,
    }
}

fn direction_from_byte(byte: u8) -> EdgeDirection {
    match byte {
        0 => EdgeDirection::Directed,
        1 => EdgeDirection::Undirected,
        other => panic!("corrupt wire: unknown edge-direction discriminant {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic splitmix64 generator. Kept local like
    /// `layout::placement`'s RNG so the property test needs no dependency and
    /// stays reproducible from a seed.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// A finite float in `(-magnitude, magnitude)`.
        ///
        /// Paint-pipeline frames never carry NaN or infinities, and IEEE
        /// equality makes a NaN round trip unfalsifiable under `assert_eq!`,
        /// so the generator stays finite; bit-exact transfer is covered
        /// separately by `float_bit_patterns_survive_the_wire`.
        fn next_finite_f32(&mut self, magnitude: f32) -> f32 {
            let unit = (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32;
            (unit * 2.0 - 1.0) * magnitude
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }

        fn flip(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }
    }

    const TEXTS: [&str; 6] = ["", "a", "node", "ノード", "l'étiquette", "nœud 📈"];

    fn random_text(rng: &mut SplitMix64) -> String {
        let mut text = TEXTS[rng.below(TEXTS.len())].to_string();
        if rng.flip() {
            // A random printable ASCII tail exercises lengths planes beyond
            // the fixed table entries.
            for _ in 0..rng.below(8) {
                text.push((b'a' + rng.below(26) as u8) as char);
            }
        }
        text
    }

    fn random_overlay(rng: &mut SplitMix64) -> OverlayCategory {
        match rng.below(4) {
            0 => OverlayCategory::None,
            1 => OverlayCategory::Dimmed,
            2 => OverlayCategory::Emphasized,
            _ => OverlayCategory::Accent,
        }
    }

    fn random_path(rng: &mut SplitMix64) -> Vec<Bezier> {
        (0..rng.below(5))
            .map(|_| {
                (
                    Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                    Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                    Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                )
            })
            .collect()
    }

    /// A frame with randomized content in every plane: arbitrary identity
    /// bits, mixed flags (so a dropped plane cannot pass by all-luck
    /// defaults), every overlay category, empty and non-empty paths, and
    /// empty through multibyte label text.
    fn random_frame(seed: u64) -> PaintFrame {
        let mut rng = SplitMix64(seed ^ 0x5DEE_CE66_D197_7E2D);
        let mut frame = PaintFrame::new();

        for _ in 0..rng.below(13) {
            frame.nodes.push(PaintNode {
                id: NodeId::from(KeyData::from_ffi(rng.next_u64())),
                position: Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                radius: rng.next_finite_f32(64.0).abs(),
                selected: rng.flip(),
                hovered: rng.flip(),
                overlay: random_overlay(&mut rng),
                simplified: rng.flip(),
            });
        }

        for _ in 0..rng.below(21) {
            frame.edges.push(PaintEdge {
                id: EdgeId::from(KeyData::from_ffi(rng.next_u64())),
                source: Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                target: Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                path: random_path(&mut rng),
                direction: match rng.below(2) {
                    0 => EdgeDirection::Directed,
                    _ => EdgeDirection::Undirected,
                },
                selected: rng.flip(),
                hovered: rng.flip(),
                overlay: random_overlay(&mut rng),
                omit_arrow: rng.flip(),
            });
        }

        for _ in 0..rng.below(13) {
            frame.labels.push(PaintLabel {
                position: Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                text: random_text(&mut rng),
            });
        }

        for _ in 0..rng.below(13) {
            frame.edge_labels.push(PaintEdgeLabel {
                edge: EdgeId::from(KeyData::from_ffi(rng.next_u64())),
                position: Vec2::new(rng.next_finite_f32(4096.0), rng.next_finite_f32(4096.0)),
                offset: Vec2::new(rng.next_finite_f32(1.0), rng.next_finite_f32(1.0))
                    .normalize_or_zero(),
                text: random_text(&mut rng),
                path: random_path(&mut rng),
                t: rng.next_finite_f32(1.0),
            });
        }

        frame
    }

    #[test]
    fn in_process_is_the_default_frame_source() {
        assert_eq!(FrameSource::default(), FrameSource::InProcess);
    }

    #[test]
    fn random_frames_round_trip_field_for_field() {
        for seed in 0..256u64 {
            let frame = random_frame(seed);
            assert_eq!(
                PaintFrameWire::encode(&frame).decode(),
                frame,
                "seed {seed} must round trip exactly"
            );
        }
    }

    #[test]
    fn empty_frame_round_trips() {
        let frame = PaintFrame::new();
        assert!(frame.is_empty());
        assert_eq!(PaintFrameWire::encode(&frame).decode(), frame);
    }

    #[test]
    fn extreme_identity_and_boundary_values_round_trip() {
        let mut frame = PaintFrame::new();
        frame.nodes.push(PaintNode {
            id: NodeId::from(KeyData::from_ffi(u64::MAX)),
            position: Vec2::new(f32::MIN_POSITIVE, -f32::MIN_POSITIVE),
            radius: f32::MAX,
            selected: true,
            hovered: true,
            overlay: OverlayCategory::Accent,
            simplified: true,
        });
        frame.edges.push(PaintEdge {
            id: EdgeId::from(KeyData::from_ffi(u64::MAX)),
            source: Vec2::ZERO,
            target: Vec2::ONE,
            path: Vec::new(),
            direction: EdgeDirection::Undirected,
            selected: true,
            hovered: true,
            overlay: OverlayCategory::Dimmed,
            omit_arrow: true,
        });
        frame.labels.push(PaintLabel {
            position: Vec2::NEG_ONE,
            text: String::new(),
        });
        frame.edge_labels.push(PaintEdgeLabel {
            edge: EdgeId::from(KeyData::from_ffi(0)),
            position: Vec2::ZERO,
            offset: Vec2::X,
            text: String::new(),
            path: Vec::new(),
            t: 1.0,
        });
        assert_eq!(PaintFrameWire::encode(&frame).decode(), frame);
    }

    #[test]
    fn float_bit_patterns_survive_the_wire() {
        // -0.0 compares equal to 0.0 under IEEE equality, so the wire's "no
        // arithmetic on transferred floats" guarantee is asserted on the bit
        // patterns themselves.
        let mut frame = PaintFrame::new();
        frame.nodes.push(PaintNode {
            id: NodeId::from(KeyData::from_ffi(7)),
            position: Vec2::new(-0.0, f32::MIN_POSITIVE),
            radius: -0.0,
            selected: false,
            hovered: false,
            overlay: OverlayCategory::None,
            simplified: false,
        });
        frame.labels.push(PaintLabel {
            position: Vec2::new(-0.0, 0.0),
            text: "t".to_string(),
        });

        let decoded = PaintFrameWire::encode(&frame).decode();
        assert_eq!(
            decoded.nodes[0].position.to_array().map(f32::to_bits),
            [(-0.0f32).to_bits(), f32::MIN_POSITIVE.to_bits()]
        );
        assert_eq!(decoded.nodes[0].radius.to_bits(), (-0.0f32).to_bits());
        assert_eq!(
            decoded.labels[0].position.to_array().map(f32::to_bits),
            [(-0.0f32).to_bits(), 0.0f32.to_bits()]
        );
    }

    #[test]
    #[should_panic(expected = "corrupt wire: label text plane is not UTF-8")]
    fn decode_fails_closed_on_corrupt_text_plane() {
        let mut frame = PaintFrame::new();
        frame.labels.push(PaintLabel {
            position: Vec2::ZERO,
            text: "abc".to_string(),
        });
        let mut wire = PaintFrameWire::encode(&frame);
        wire.labels.text.bytes[0] = 0xFF;
        let _ = wire.decode();
    }

    /// One node and one edge make every plane's byte offset computable: each
    /// plane is an 8-byte count plus fixed-width elements, so the corruption
    /// targets below are derived arithmetically in declaration order.
    fn one_node_one_edge_frame() -> PaintFrame {
        let mut frame = PaintFrame::new();
        frame.nodes.push(PaintNode {
            id: NodeId::from(KeyData::from_ffi(11)),
            position: Vec2::new(1.5, -2.5),
            radius: 3.0,
            selected: true,
            hovered: false,
            overlay: OverlayCategory::Dimmed,
            simplified: false,
        });
        frame.edges.push(PaintEdge {
            id: EdgeId::from(KeyData::from_ffi(12)),
            source: Vec2::new(-1.0, 0.0),
            target: Vec2::new(1.0, 0.0),
            path: vec![(Vec2::ZERO, Vec2::X, Vec2::ONE)],
            direction: EdgeDirection::Undirected,
            selected: true,
            hovered: true,
            overlay: OverlayCategory::Accent,
            omit_arrow: false,
        });
        frame
    }

    #[test]
    fn random_frames_round_trip_through_wire_bytes() {
        for seed in 0..64u64 {
            let frame = random_frame(seed ^ 0xABCD_1234_0000_0001);
            let bytes = PaintFrameWire::encode(&frame).to_wire_bytes();
            let parsed = PaintFrameWire::from_wire_bytes(&bytes).expect("writer output must parse");
            assert_eq!(parsed.decode(), frame, "seed {seed} must survive bytes");
        }
    }

    #[test]
    fn wire_bytes_reject_truncation_and_trailing_bytes() {
        let bytes = PaintFrameWire::encode(&one_node_one_edge_frame()).to_wire_bytes();

        // Every prefix cut short of the full message must be rejected, not
        // silently accepted as a smaller-but-valid message.
        let mut cut_points = vec![bytes.len() - 1];
        cut_points.extend([0usize, 7, 31, 99, 200]);
        for cut in cut_points {
            assert!(
                matches!(
                    PaintFrameWire::from_wire_bytes(&bytes[..cut]),
                    Err(WireFormatError::Truncated { .. })
                ),
                "prefix of length {cut} must be rejected as truncated"
            );
        }

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            PaintFrameWire::from_wire_bytes(&trailing),
            Err(WireFormatError::TrailingBytes { extra: 1 })
        );
    }

    #[test]
    fn wire_bytes_reject_noncanonical_discriminants() {
        let mut bytes = PaintFrameWire::encode(&one_node_one_edge_frame()).to_wire_bytes();

        // Plane walk for this exact frame (each plane = 8-byte count +
        // elements; the edge carries one Bézier segment = 24 point bytes):
        // node overlay data sits at byte 70, edge direction data at 180, and
        // the node selected flag at 52.
        assert_eq!(bytes.len(), 305);
        bytes[70] = 4;
        assert_eq!(
            PaintFrameWire::from_wire_bytes(&bytes),
            Err(WireFormatError::BadDiscriminant {
                field: "node overlay",
                value: 4
            })
        );
        bytes[70] = 1;

        bytes[180] = 5;
        assert_eq!(
            PaintFrameWire::from_wire_bytes(&bytes),
            Err(WireFormatError::BadDiscriminant {
                field: "edge direction",
                value: 5
            })
        );
        bytes[180] = 1;

        bytes[52] = 0xFF;
        assert_eq!(
            PaintFrameWire::from_wire_bytes(&bytes),
            Err(WireFormatError::BadDiscriminant {
                field: "node selected",
                value: 0xFF
            })
        );
    }

    #[test]
    fn float_bit_patterns_survive_the_byte_transport() {
        // LE serialization must be bit-preserving, not float-arithmetic: -0.0
        // compares equal to 0.0 under IEEE equality, so assert the bits.
        let mut frame = PaintFrame::new();
        frame.nodes.push(PaintNode {
            id: NodeId::from(KeyData::from_ffi(3)),
            position: Vec2::new(-0.0, f32::MIN_POSITIVE),
            radius: -0.0,
            selected: false,
            hovered: false,
            overlay: OverlayCategory::None,
            simplified: false,
        });

        let parsed =
            PaintFrameWire::from_wire_bytes(&PaintFrameWire::encode(&frame).to_wire_bytes())
                .expect("parses");
        let decoded = parsed.decode();
        assert_eq!(decoded.nodes[0].radius.to_bits(), (-0.0f32).to_bits());
        assert_eq!(
            decoded.nodes[0].position.to_array().map(f32::to_bits),
            [(-0.0f32).to_bits(), f32::MIN_POSITIVE.to_bits()]
        );
    }
}
