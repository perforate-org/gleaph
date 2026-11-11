//! Where labels go, once the frame has decided which ones survive.
//!
//! A label belongs to the marker at its anchor, and a label centred on that anchor covers the very
//! marker it names. Placement therefore stands each label off its marker, tries four sides, avoids
//! every visible marker's disk and every already-placed label, and - when nothing fits near a
//! viewport edge - slides the label inside the viewport rather than dropping it. A label that has no
//! free slot at all is dropped; the frame's label budget has already ordered them by importance.
//!
//! The rules live here, not in a host, so the desktop console and a browser canvas place labels by
//! one implementation.

use glam::Vec2;

use crate::paint::{LabelPriority, PaintFrame};

/// One placed label: its screen-space top-left and the text.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedLabel {
    /// Top-left of the text in canvas points.
    pub position: Vec2,
    /// The label text.
    pub text: String,
    /// The label's rank, so a host can style emphasised labels differently.
    pub priority: LabelPriority,
}

/// Text metrics the placement needs, supplied by the host because the font is the host's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelMetrics {
    /// Advances per character, used to estimate a label's width without measuring the text.
    pub char_width: f32,
    /// Line height, the height of the glyph box.
    pub line_height: f32,
    /// Horizontal padding of the label's plate, part of its footprint.
    pub plate_x: f32,
    /// Vertical padding of the label's plate, part of its footprint.
    pub plate_y: f32,
    /// Clearance between a marker and its label.
    pub gap: f32,
}

impl Default for LabelMetrics {
    fn default() -> Self {
        // 11-point interface text: 6.2 points per digit, 14-point line box, a 3x1 plate.
        Self {
            char_width: 6.2,
            line_height: 14.0,
            plate_x: 3.0,
            plate_y: 1.0,
            gap: 4.0,
        }
    }
}

type Rect = (f32, f32, f32, f32);

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// Place the frame's labels inside a viewport of `size` points.
pub fn place_labels(frame: &PaintFrame, size: Vec2, metrics: &LabelMetrics) -> Vec<PlacedLabel> {
    let mut occupied: Vec<Rect> = Vec::new();
    let mut placed = Vec::new();
    let disks: Vec<(f32, f32, f32)> = frame
        .nodes
        .iter()
        .map(|node| (node.position.x, node.position.y, node.radius + 1.5))
        .collect();
    let hits_disk = |rect: &Rect, skip: (f32, f32)| {
        disks.iter().any(|(x, y, r)| {
            if (x - skip.0).abs() < 0.5 && (y - skip.1).abs() < 0.5 {
                return false;
            }
            let nx = x.clamp(rect.0, rect.2);
            let ny = y.clamp(rect.1, rect.3);
            (nx - x) * (nx - x) + (ny - y) * (ny - y) < r * r
        })
    };
    let fits = |rect: &Rect| rect.0 >= 0.0 && rect.1 >= 0.0 && rect.2 <= size.x && rect.3 <= size.y;
    for label in &frame.labels {
        let width = label.text.len() as f32 * metrics.char_width;
        let (cx, cy) = (label.position.x, label.position.y);
        // The marker at the anchor decides the stand-off; a label off its own node is not a
        // candidate here because it would cover the marker it names.
        let radius = disks
            .iter()
            .filter(|(x, y, _)| (x - cx).abs() < 1.0 && (y - cy).abs() < 1.0)
            .map(|(_, _, r)| *r)
            .fold(0.0f32, f32::max);
        let candidates = [
            (cx + radius + metrics.gap, cy),
            (cx - radius - metrics.gap - width, cy),
            (
                cx - width * 0.5,
                cy - radius - metrics.gap - metrics.line_height * 0.5,
            ),
            (
                cx - width * 0.5,
                cy + radius + metrics.gap - metrics.line_height * 0.5,
            ),
        ];
        let rect_of = |left: f32, top: f32| {
            (
                left - metrics.plate_x,
                top - metrics.plate_y,
                left + width + metrics.plate_x,
                top + metrics.line_height + metrics.plate_y,
            )
        };
        let mut slot = None;
        for (left, centre_y) in candidates {
            let top = centre_y - metrics.line_height * 0.5;
            let rect = rect_of(left, top);
            let glyphs = (left, top, left + width, top + metrics.line_height);
            if !fits(&rect)
                || occupied.iter().any(|other| overlaps(other, &rect))
                || hits_disk(&glyphs, (cx, cy))
            {
                continue;
            }
            slot = Some((left, top, rect));
            break;
        }
        // Near a viewport edge nothing fits: slide the box inside instead of losing the label. The
        // slid box may touch any marker, including its own, so it is checked against all of them.
        if slot.is_none() {
            for (left, centre_y) in candidates {
                let top = centre_y - metrics.line_height * 0.5;
                let rect = rect_of(left, top);
                let shift_x = if rect.0 < 0.0 {
                    -rect.0
                } else if rect.2 > size.x {
                    size.x - rect.2
                } else {
                    0.0
                };
                let shift_y = if rect.1 < 0.0 {
                    -rect.1
                } else if rect.3 > size.y {
                    size.y - rect.3
                } else {
                    0.0
                };
                let slid = (
                    rect.0 + shift_x,
                    rect.1 + shift_y,
                    rect.2 + shift_x,
                    rect.3 + shift_y,
                );
                if !fits(&slid) || occupied.iter().any(|other| overlaps(other, &slid)) {
                    continue;
                }
                let glyphs = (
                    slid.0 + metrics.plate_x,
                    slid.1 + metrics.plate_y,
                    slid.2 - metrics.plate_x,
                    slid.3 - metrics.plate_y,
                );
                if hits_disk(&glyphs, (f32::NAN, f32::NAN)) {
                    continue;
                }
                slot = Some((slid.0 + metrics.plate_x, slid.1 + metrics.plate_y, slid));
                break;
            }
        }
        let Some((left, top, rect)) = slot else {
            continue;
        };
        occupied.push(rect);
        placed.push(PlacedLabel {
            position: Vec2::new(left, top),
            text: label.text.clone(),
            priority: label.priority,
        });
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::{PaintLabel, PaintNode};
    use crate::scene::NodeId;

    fn frame(nodes: Vec<(f32, f32, f32)>, labels: Vec<(&str, f32, f32)>) -> PaintFrame {
        PaintFrame {
            nodes: nodes
                .into_iter()
                .enumerate()
                .map(|(index, (x, y, radius))| PaintNode {
                    id: NodeId::new(index),
                    position: Vec2::new(x, y),
                    radius,
                    selected: false,
                    hovered: false,
                    overlay: crate::OverlayCategory::None,
                    simplified: false,
                })
                .collect(),
            edges: Vec::new(),
            labels: labels
                .into_iter()
                .map(|(text, x, y)| PaintLabel {
                    position: Vec2::new(x, y),
                    text: text.to_owned(),
                    priority: LabelPriority::Normal,
                })
                .collect(),
            edge_labels: Vec::new(),
            edge_splats: Vec::new(),
        }
    }

    #[test]
    fn a_label_stands_off_the_marker_it_names() {
        let placed = place_labels(
            &frame(vec![(100.0, 100.0, 5.0)], vec![("A", 100.0, 100.0)]),
            Vec2::new(400.0, 300.0),
            &LabelMetrics::default(),
        );
        assert_eq!(placed.len(), 1);
        let label = &placed[0];
        // The marker disk (radius 5 + 1.5) is not under the text.
        let marker = (100.0 - 6.5, 100.0 - 6.5, 100.0 + 6.5, 100.0 + 6.5);
        let text = (
            label.position.x,
            label.position.y,
            label.position.x + label.text.len() as f32 * 6.2,
            label.position.y + 14.0,
        );
        assert!(!overlaps(&marker, &text), "the label covers its own marker");
    }

    #[test]
    fn no_two_placed_labels_overlap() {
        let mut labels = Vec::new();
        let mut nodes = Vec::new();
        for index in 0..12 {
            let x = 40.0 + (index % 4) as f32 * 60.0;
            let y = 40.0 + (index / 4) as f32 * 60.0;
            nodes.push((x, y, 5.0));
            labels.push(("N", x, y));
        }
        let placed = place_labels(
            &frame(nodes, labels),
            Vec2::new(400.0, 300.0),
            &LabelMetrics::default(),
        );
        assert!(!placed.is_empty());
        for (index, a) in placed.iter().enumerate() {
            let rect_a = (
                a.position.x - 3.0,
                a.position.y - 1.0,
                a.position.x + a.text.len() as f32 * 6.2 + 3.0,
                a.position.y + 15.0,
            );
            for b in &placed[index + 1..] {
                let rect_b = (
                    b.position.x - 3.0,
                    b.position.y - 1.0,
                    b.position.x + b.text.len() as f32 * 6.2 + 3.0,
                    b.position.y + 15.0,
                );
                assert!(!overlaps(&rect_a, &rect_b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_label_at_the_viewport_edge_slides_inside() {
        let placed = place_labels(
            &frame(vec![(2.0, 150.0, 5.0)], vec![("EDGE", 2.0, 150.0)]),
            Vec2::new(200.0, 200.0),
            &LabelMetrics::default(),
        );
        assert_eq!(placed.len(), 1, "the label survives by sliding");
        let label = &placed[0];
        assert!(label.position.x >= 0.0 && label.position.y >= 0.0);
        let width = label.text.len() as f32 * 6.2;
        assert!(
            label.position.x + width + 3.0 <= 200.0,
            "it stays inside the canvas"
        );
    }
}
