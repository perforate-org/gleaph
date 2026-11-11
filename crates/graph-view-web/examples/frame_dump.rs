//! Print one frame as JSON: the contract the canvas host draws from.
//!
//! Run with `cargo run -p graph-view-web --example frame_dump`. The output is one object with the
//! three planes (`nodes`, `edges`, `labels`) a preset canvas renderer consumes, so the JavaScript
//! side can be developed and diffed against a native render of the same session.

use graph_view_web::Session;

fn main() {
    // A small ring: ten nodes on a circle, labelled in three slots, connected around the ring.
    let mut positions = Vec::new();
    for index in 0..10 {
        let angle = index as f32 / 10.0 * std::f32::consts::TAU;
        positions.push(angle.cos() * 100.0);
        positions.push(angle.sin() * 100.0);
    }
    let mut edges = Vec::new();
    for index in 0..10u32 {
        edges.push(index);
        edges.push((index + 1) % 10);
    }
    let slots: Vec<u32> = (0..10).map(|index| index % 3).collect();

    let mut session = Session::new(&positions, &edges, &slots, 800.0, 600.0);
    session.step(1);
    let frame = session.frame_data();

    let numbers = |values: &[f32]| {
        values
            .iter()
            .map(|value| format!("{value:.2}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "{{\"nodes\":[{}],\"edges\":[{}],\"labels\":[{}]}}",
        numbers(&frame.nodes),
        numbers(&frame.edges),
        numbers(&frame.labels)
    );
}
