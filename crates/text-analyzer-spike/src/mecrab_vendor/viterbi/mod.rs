//! Viterbi algorithm implementation for finding the optimal path
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/viterbi/mod.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: `nbest` and `simd` submodules are EXCLUDED from the vendored
//! runtime path (recorded as deletions); the dictionary parameter itself is untouched
//! by the ByteImage refactor (the solver only sees `&Dictionary`).

pub mod analysis;

use crate::mecrab_vendor::dict::Dictionary;
use crate::mecrab_vendor::error::{Error, Result};
use crate::mecrab_vendor::lattice::{Lattice, LatticeNode};

/// Result node after Viterbi path finding
#[derive(Debug, Clone)]
pub struct PathNode {
    pub surface: String,
    pub word_id: u32,
    pub pos_id: u16,
    pub wcost: i16,
    pub feature: String,
}

/// Entry in the Viterbi table
#[derive(Debug, Clone)]
struct ViterbiEntry<'a> {
    node: &'a LatticeNode<'a>,
    cost: i64,
    prev: Option<usize>,
    pos: usize,
}

/// Viterbi solver for finding the optimal path through the lattice
pub struct ViterbiSolver<'a> {
    dictionary: &'a Dictionary,
}

impl<'a> ViterbiSolver<'a> {
    pub const fn new(dictionary: &'a Dictionary) -> Self {
        Self { dictionary }
    }

    /// Solve the lattice and return the optimal path
    pub fn solve<'b>(&self, lattice: &'b Lattice<'b>) -> Result<Vec<PathNode>> {
        if lattice.is_empty() {
            return Err(Error::LatticeError("Empty lattice".to_string()));
        }

        let entries = self.forward_pass(lattice);
        let path = Self::backward_pass(&entries, lattice)?;
        Ok(path)
    }

    /// Forward pass: compute minimum costs to reach each node
    fn forward_pass<'b>(&self, lattice: &'b Lattice<'b>) -> Vec<Vec<ViterbiEntry<'b>>> {
        let n = lattice.len();
        let mut entries: Vec<Vec<ViterbiEntry>> = vec![Vec::new(); n];

        // BOS seed
        if let Some(bos) = lattice.nodes_at[0].first() {
            entries[0].push(ViterbiEntry {
                node: bos,
                cost: 0,
                prev: None,
                pos: 0,
            });
        }

        for pos in 0..n {
            for node in &lattice.nodes_at[pos] {
                let mut best_cost = i64::MAX;
                let mut best_prev: Option<usize> = None;
                let mut best_prev_pos: usize = 0;

                let prev_pos = if node.start == 0 && pos > 0 {
                    0
                } else {
                    node.start + 1
                };

                if prev_pos < entries.len() {
                    for (prev_idx, prev_entry) in entries[prev_pos].iter().enumerate() {
                        let conn_cost = self
                            .dictionary
                            .connection_cost(prev_entry.node.right_id, node.left_id)
                            as i64;
                        let total_cost = prev_entry.cost + conn_cost + node.wcost as i64;
                        if total_cost < best_cost {
                            best_cost = total_cost;
                            best_prev = Some(prev_idx);
                            best_prev_pos = prev_pos;
                        }
                    }
                }

                // Also check connections from earlier positions (for longer words)
                for check_pos in 1..prev_pos {
                    if check_pos < entries.len() {
                        for (prev_idx, prev_entry) in entries[check_pos].iter().enumerate() {
                            if prev_entry.node.end == node.start {
                                let conn_cost = self
                                    .dictionary
                                    .connection_cost(prev_entry.node.right_id, node.left_id)
                                    as i64;
                                let total_cost = prev_entry.cost + conn_cost + node.wcost as i64;
                                if total_cost < best_cost {
                                    best_cost = total_cost;
                                    best_prev = Some(prev_idx);
                                    best_prev_pos = check_pos;
                                }
                            }
                        }
                    }
                }

                if best_cost < i64::MAX {
                    entries[pos].push(ViterbiEntry {
                        node,
                        cost: best_cost,
                        prev: best_prev,
                        pos: best_prev_pos,
                    });
                }
            }
        }

        entries
    }

    /// Backward pass: trace the optimal path
    fn backward_pass<'b>(
        entries: &[Vec<ViterbiEntry<'b>>],
        lattice: &'b Lattice<'b>,
    ) -> Result<Vec<PathNode>> {
        let n = lattice.len();

        let eos_entries = &entries[n - 1];
        if eos_entries.is_empty() {
            return Err(Error::ViterbiError("No path to EOS found".to_string()));
        }
        // Plan 0333 vendored patch: upstream unconditionally treats the min-cost entry
        // at the last slot as EOS and DROPS ITS SURFACE. When a real word node wins the
        // last slot (common: word cost < EOS connection cost), the final word of every
        // sentence was silently truncated (e.g. 一丁目 -> 一). Prefer the actual EOS
        // node entry (empty surface); fall back to the min-cost word entry and keep its
        // surface (the sentence's final word, the implicit EOS transition).
        let eos_node_entry = eos_entries.iter().find(|e| e.node.surface.is_empty());
        let best_eos = match eos_node_entry {
            Some(e) => e,
            None => eos_entries
                .iter()
                .min_by_key(|e| e.cost)
                .ok_or_else(|| Error::ViterbiError("No EOS entry found".to_string()))?,
        };

        let mut path = Vec::new();
        let mut current_idx = best_eos.prev;
        let mut prev_pos = best_eos.pos;

        while let Some(idx) = current_idx {
            if prev_pos >= entries.len() || idx >= entries[prev_pos].len() {
                break;
            }

            let entry = &entries[prev_pos][idx];
            if !entry.node.surface.is_empty() {
                path.push(PathNode {
                    surface: entry.node.surface.to_string(),
                    word_id: entry.node.word_id,
                    pos_id: entry.node.pos_id,
                    wcost: entry.node.wcost,
                    feature: entry.node.feature.clone(),
                });
            }

            current_idx = entry.prev;
            prev_pos = entry.pos;
        }

        path.reverse();
        Ok(path)
    }
}