//! Semantic edge-id sidecars.

use gleaph_graph_kernel::EdgeId;

use super::edge::LogicalEdgeLocator;

/// Minimal `EdgeId -> LogicalEdgeLocator` sidecar.
///
/// This is the canonical semantic-to-logical bridge used by production
/// mutation paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeLogicalLocatorSidecar {
    locators: Vec<Option<LogicalEdgeLocator>>,
}

impl EdgeLogicalLocatorSidecar {
    /// Creates an empty semantic-to-logical locator table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the logical locator currently associated with `edge_id`.
    pub fn get(&self, edge_id: EdgeId) -> Option<LogicalEdgeLocator> {
        let idx = usize::try_from(edge_id).ok()?;
        self.locators.get(idx).copied().flatten()
    }

    /// Stores or replaces the logical locator associated with `edge_id`.
    pub fn set(&mut self, edge_id: EdgeId, locator: LogicalEdgeLocator) {
        let idx = usize::try_from(edge_id).expect("edge id should fit in usize");
        if idx >= self.locators.len() {
            self.locators.resize(idx + 1, None);
        }
        self.locators[idx] = Some(locator);
    }

    /// Removes and returns the logical locator associated with `edge_id`.
    pub fn remove(&mut self, edge_id: EdgeId) -> Option<LogicalEdgeLocator> {
        let idx = usize::try_from(edge_id).ok()?;
        self.locators.get_mut(idx)?.take()
    }

    /// Returns whether a logical locator is stored for `edge_id`.
    pub fn contains(&self, edge_id: EdgeId) -> bool {
        self.get(edge_id).is_some()
    }

    /// Retains only entries whose `(edge_id, locator)` pair matches `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(EdgeId, LogicalEdgeLocator) -> bool) {
        for (idx, slot) in self.locators.iter_mut().enumerate() {
            let Some(locator) = *slot else {
                continue;
            };
            let edge_id = idx as EdgeId;
            if !keep(edge_id, locator) {
                *slot = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EdgeLogicalLocatorSidecar;
    use crate::low_level::{LogicalEdgeLocator, SurfaceKind, VertexRef};

    #[test]
    fn logical_sidecar_maps_edge_id_to_logical_locator() {
        let mut sidecar = EdgeLogicalLocatorSidecar::new();
        let locator = LogicalEdgeLocator::base(SurfaceKind::Forward, VertexRef::from(7u8), 12);

        sidecar.set(42, locator);

        assert_eq!(sidecar.get(42), Some(locator));
        assert!(sidecar.contains(42));
    }
}
