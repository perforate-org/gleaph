//! Matrices and graphs, the part the post is really about: the *Frobenius
//! normal form* and how it falls out of a graph's strongly connected components
//! (SCCs), from Tivadar Danka's post (thepalindrome.org).
//!
//! A nonnegative matrix IS a directed graph: `A[i][j]` is the weight of the
//! edge `i -> j`. The payoff is *structural*. Partition the nodes into strongly
//! connected components, then relabel them so each SCC is a contiguous block
//! and edges only ever point from an earlier block to a later one. Under that
//! relabeling the adjacency matrix becomes the *Frobenius normal form*:
//! irreducible (strongly connected) blocks on the diagonal and zeros strictly
//! below the diagonal blocks.
//!
//! This example uses the 13-node reducible digraph from the article. It is
//! defined by a single adjacency matrix (the source of truth), rendered as a
//! graph. The scene's `SccLayoutEngine` computes the strongly connected
//! components and lays the nodes out block-by-block in Frobenius order, so the
//! graph and the matrix are always the same object — you can read the SCC
//! blocks off either one. A local Tarjan pass colors the overlay matrix with
//! the same components the layout used.
//!
//! The matrix below is already in Frobenius order (blocks
//! `{v1,v2,v3}`, `{v4,v5,v6,v7}`, `{v8,v9}`, `{v10}`, `{v11,v12,v13}`); it is
//! the source used to build the graph:
//!
//! ```text
//!        1  2  3  4  5  6  7  8  9 10 11 12 13
//!   1  [ 0  1  1  1  .  .  .  .  .  .  .  .  . ]
//!   2  [ .  0  1  .  .  1  .  .  .  1  .  .  . ]
//!   3  [ 1  .  0  .  .  .  .  1  .  .  .  .  . ]
//!   4  [ .  .  .  0  1  .  1  .  .  .  .  .  . ]
//!   5  [ .  .  .  .  0  1  .  .  .  .  .  .  . ]
//!   6  [ .  .  .  1  .  0  .  .  .  .  .  .  . ]
//!   7  [ .  .  .  .  1  1  0  .  .  .  .  .  . ]
//!   8  [ .  .  .  .  .  .  .  0  1  1  .  .  . ]
//!   9  [ .  .  .  .  .  .  .  1  0  .  .  .  . ]
//!  10  [ .  .  .  .  .  .  .  .  .  0  1  .  . ]
//!  11  [ .  .  .  .  .  .  .  .  .  .  0  1  . ]
//!  12  [ .  .  .  .  .  .  .  .  .  .  .  0  1 ]
//!  13  [ .  .  .  .  .  .  .  .  .  .  1  .  0 ]
//! ```
//!
//! Each SCC is rendered in its own horizontal band; within a band the nodes are
//! strongly connected (a directed cycle, shown as fanned curves), and edges
//! between bands only point to the right. Zero entries never become edges.
//!
//! Demonstrates the four-layer public API (§27): a logical graph merged from a
//! batch, a shared scene with a manual `FixedLayout`, a view state, and a
//! composable `GraphView` — plus a self-contained Tarjan SCC decomposition.

use std::collections::HashMap;

use gpui::{
    App, Application, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rems, size, white,
};
use gpui_graph::{GraphBatch, GraphScene, GraphView, GraphViewState, SccLayoutEngine};

/// A reducible 13-node adjacency matrix, already in Frobenius (SCC-block) order.
///
/// Rows are outgoing, columns incoming: `A[i][j] == 1` becomes edge `i -> j`.
/// A `.` is a zero entry (no edge). This is the single source of truth; the
/// graph is derived from it, so the two layers never disagree.
const MATRIX: [&[u8]; 13] = [
    &[0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    &[0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    &[1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
    &[0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0],
    &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    &[0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0],
    &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
    &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
];

/// Tarjan's algorithm: the strongly connected components of a directed graph.
///
/// `adj` maps each node to its out-neighbors. Returns the SCCs, each a set of
/// nodes, in reverse topological order of the condensation DAG. Self-contained
/// so the example computes the Frobenius structure rather than hard-coding it.
fn strongly_connected_components(adj: &HashMap<usize, Vec<usize>>) -> Vec<Vec<usize>> {
    let n = adj.keys().max().map_or(0, |&m| m + 1);
    let mut index = vec![0usize; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut components: Vec<Vec<usize>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strong_connect(
        v: usize,
        adj: &HashMap<usize, Vec<usize>>,
        index: &mut [usize],
        lowlink: &mut [usize],
        on_stack: &mut [bool],
        stack: &mut Vec<usize>,
        next_index: &mut usize,
        components: &mut Vec<Vec<usize>>,
    ) {
        index[v] = *next_index;
        lowlink[v] = *next_index;
        *next_index += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in adj.get(&v).into_iter().flatten() {
            if index[w] == 0 {
                strong_connect(
                    w, adj, index, lowlink, on_stack, stack, next_index, components,
                );
                lowlink[v] = lowlink[v].min(lowlink[w]);
            } else if on_stack[w] {
                lowlink[v] = lowlink[v].min(index[w]);
            }
        }

        if lowlink[v] == index[v] {
            let mut component = Vec::new();
            loop {
                let w = stack.pop().expect("stack non-empty");
                on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            components.push(component);
        }
    }

    for v in 0..n {
        if index[v] == 0 {
            strong_connect(
                v,
                adj,
                &mut index,
                &mut lowlink,
                &mut on_stack,
                &mut stack,
                &mut next_index,
                &mut components,
            );
        }
    }
    components
}

/// Order the nodes so each SCC is a contiguous block and edges only point
/// rightward (from an earlier block to a later one) — the relabeling behind the
/// Frobenius normal form.
///
/// Tarjan yields components in reverse topological order of the condensation
/// DAG, so reversing puts sources first. This reordering is the relabeling
/// behind the Frobenius normal form.
fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(Example::new),
        )
        .unwrap();
        cx.activate(true);
    });
}

struct Example {
    view: Entity<GraphViewState<String, String, String, ()>>,
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        // Build the graph from the matrix (single source of truth). Node labels
        // are 1-based ("1".."13") to match the article's v1..v13.
        let mut batch = GraphBatch::new();
        for i in 0..13 {
            batch = batch.node(i.to_string(), (i + 1).to_string());
        }
        for (i, row) in MATRIX.iter().enumerate() {
            for (j, &entry) in row.iter().enumerate() {
                if entry == 1 {
                    batch = batch.edge(
                        format!("{i}->{j}"),
                        i.to_string(),
                        j.to_string(),
                        gpui_graph::EdgeDirection::Directed,
                        (),
                    );
                }
            }
        }

        let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(SccLayoutEngine)));
        scene.update(cx, |scene, cx| {
            scene.merge(batch);
            // The SCC layout is deterministic and settles in one step; running
            // it copies the computed positions into the scene for painting.
            scene.step_layout(gpui_graph::LayoutBudget::default());
            cx.notify();
        });

        let view = cx.new(|cx| GraphViewState::new(scene, cx));
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.8).into(),
                ..TextStyle::default()
            };
            // Label each node with its index ("1".."13"); edges carry no label.
            view.set_node_label(|_id, node| Some(node.clone()));
            cx.notify();
        });

        Self { view }
    }
}

impl Render for Example {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Recompute the SCCs at render time so the overlay matrix and the graph
        // are always generated from the same source (the matrix).
        let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, row) in MATRIX.iter().enumerate() {
            let out: Vec<usize> = row
                .iter()
                .enumerate()
                .filter(|(_, e)| **e == 1)
                .map(|(j, _)| j)
                .collect();
            adj.insert(i, out);
        }
        let sccs = strongly_connected_components(&adj);

        // Render the Frobenius matrix as a small monospace grid, with 1s in
        // their row position and blocks outlined with their component index.
        let mut grid = String::new();
        for (row_index, row) in MATRIX.iter().enumerate() {
            grid.push_str(&format!("{:>2} ", row_index + 1));
            for (col, &entry) in row.iter().enumerate() {
                let in_same_scc = sccs
                    .iter()
                    .any(|c| c.contains(&row_index) && c.contains(&col));
                if entry == 1 {
                    // In-block: "X"; cross-block: "o".
                    grid.push(if in_same_scc { 'x' } else { 'o' });
                } else {
                    grid.push('.');
                }
                grid.push(' ');
            }
            grid.push('\n');
        }

        div()
            .size_full()
            .bg(gpui::hsla(0.0, 0.0, 0.1, 1.0)) // Dark charcoal background
            .child(
                div().absolute().top(px(16.)).right(px(16.)).child(
                    div()
                        .bg(gpui::hsla(0.0, 0.0, 0.16, 1.0))
                        .p_4()
                        .rounded_md()
                        .child(
                            div()
                                .text_color(white())
                                .text_sm()
                                .child("Frobenius normal form (SCCs -> diagonal blocks)"),
                        )
                        .child(
                            div()
                                .mt_2()
                                .text_color(gpui::hsla(0.0, 0.0, 0.7, 1.0))
                                .text_xs()
                                .child(grid),
                        ),
                ),
            )
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
