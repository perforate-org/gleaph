pub mod bfs;
pub mod budget;
pub mod pagerank;
pub mod recommend;
pub mod sssp;

use gleaph_types::TimestampRange;

/// Outcome of a resumable algorithm execution.
#[derive(Clone, Debug)]
pub enum AlgoOutcome<R, C> {
    /// Algorithm completed; final result available.
    Done(R),
    /// Algorithm suspended due to budget exhaustion.
    /// Contains partial result so far and a checkpoint for resumption.
    Suspended { partial: R, checkpoint: C },
}

pub type Neighbor = (u32, f32, u64);

pub trait GraphView {
    fn vertex_count(&self) -> u64;
    fn edge_count(&self) -> u64;
    fn neighbors(&self, vertex_id: u32) -> Vec<Neighbor>;
    fn neighbors_filtered(&self, vertex_id: u32, ts_range: Option<TimestampRange>)
    -> Vec<Neighbor>;
    fn reverse_neighbors(&self, target: u32) -> Vec<Neighbor>;
    fn reverse_neighbors_filtered(
        &self,
        target: u32,
        ts_range: Option<TimestampRange>,
    ) -> Vec<Neighbor> {
        let neighbors = self.reverse_neighbors(target);
        match ts_range {
            None => neighbors,
            Some(ref range) => neighbors
                .into_iter()
                .filter(|(_, _, ts)| ts_in_range(*ts, Some(range)))
                .collect(),
        }
    }
    fn is_vertex_active(&self, vertex_id: u32) -> bool;
    fn vertex_has_label(&self, vertex_id: u32, label: &str) -> bool;
    fn edge_has_label(&self, src: u32, dst: u32, label: &str) -> bool;
    fn edge_label_ref(&self, src: u32, dst: u32) -> Option<&str>;
    fn label_name_by_id(&self, label_id: u32) -> Option<&str>;
    fn all_vertices(&self) -> Vec<u32>;
}

pub(crate) fn ts_in_range(ts: u64, range: Option<&TimestampRange>) -> bool {
    let Some(range) = range else {
        return true;
    };
    if let Some(start) = range.start
        && ts < start
    {
        return false;
    }
    if let Some(end) = range.end
        && ts > end
    {
        return false;
    }
    true
}
