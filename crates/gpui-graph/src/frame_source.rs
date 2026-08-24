//! Frame source seam and paint-frame wire format (§18.2, §27.3; ADR 0076).
//!
//! A [`FrameSource`] names where a view's [`PaintFrame`](crate::paint::PaintFrame)
//! is produced. Today exactly one producer exists: the synchronous in-process
//! build during prepaint (`prepare_canvas` →
//! [`build_indexed_paint_frame`](crate::paint::build_indexed_paint_frame)).
//! The `Worker` variant reserves the off-main-thread producer of ADR 0076 so
//! the seam and the transfer contract land ahead of the worker host; it has
//! no execution path yet.
//!
//! [`PaintFrameWire`] is the transfer form used across that seam: a frame
//! linearized into structure-of-arrays planes, one flat buffer per primitive
//! field. This slice pins only the round-trip contract — `encode` then
//! `decode` restores the exact frame, field-for-field, order preserved — via
//! a randomized property test; no transport reads or writes the planes until
//! the S2 worker host exists.

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
    /// the main thread as [`PaintFrameWire`] buffers. Reserved for ADR 0076
    /// S2: the variant exists so the seam is expressible ahead of the host,
    /// but no construction path selects it yet, and reaching it in frame
    /// dispatch fails loudly instead of falling back to an in-process build.
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
/// Until the S2 worker transport arrives, the wire exists only through this
/// pair and its round-trip property test.
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
}
