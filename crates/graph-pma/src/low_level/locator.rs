//! Semantic-to-physical locator sidecars.

use gleaph_graph_kernel::EdgeId;

use super::edge::EdgeLocator;

/// Minimal `EdgeId -> EdgeLocator` sidecar.
///
/// This is the semantic-to-physical bridge used by higher layers:
/// `EdgeId` remains the stable semantic handle, while `EdgeLocator` describes
/// where the edge currently lives inside a directional surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeLocatorSidecar {
    locators: Vec<Option<EdgeLocator>>,
}

impl EdgeLocatorSidecar {
    /// Creates an empty semantic-to-physical locator table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the locator currently associated with `edge_id`.
    pub fn get(&self, edge_id: EdgeId) -> Option<EdgeLocator> {
        let idx = usize::try_from(edge_id).ok()?;
        self.locators.get(idx).copied().flatten()
    }

    /// Stores or replaces the locator associated with `edge_id`.
    pub fn set(&mut self, edge_id: EdgeId, locator: EdgeLocator) {
        let idx = usize::try_from(edge_id).expect("edge id should fit in usize");
        if idx >= self.locators.len() {
            self.locators.resize(idx + 1, None);
        }
        self.locators[idx] = Some(locator);
    }

    /// Removes and returns the locator associated with `edge_id`.
    pub fn remove(&mut self, edge_id: EdgeId) -> Option<EdgeLocator> {
        let idx = usize::try_from(edge_id).ok()?;
        self.locators.get_mut(idx)?.take()
    }

    /// Returns whether a locator is stored for `edge_id`.
    pub fn contains(&self, edge_id: EdgeId) -> bool {
        self.get(edge_id).is_some()
    }

    /// Counts live locator mappings.
    pub fn len(&self) -> usize {
        self.locators.iter().filter(|entry| entry.is_some()).count()
    }

    /// Returns whether the sidecar currently stores no mappings.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Retains only entries whose `(edge_id, locator)` pair matches `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(EdgeId, EdgeLocator) -> bool) {
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
    use super::EdgeLocatorSidecar;
    use crate::low_level::{EdgeLocator, SurfaceKind};
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn sidecar_maps_edge_id_to_locator() {
        let mut sidecar = EdgeLocatorSidecar::new();
        let locator = EdgeLocator::new(SurfaceKind::Forward, NodeId::from(7u8), 12);

        sidecar.set(42, locator);

        assert_eq!(sidecar.get(42), Some(locator));
        assert!(sidecar.contains(42));
        assert_eq!(sidecar.len(), 1);
    }

    #[test]
    fn sidecar_can_remove_locator() {
        let mut sidecar = EdgeLocatorSidecar::new();
        let locator = EdgeLocator::new(SurfaceKind::Reverse, NodeId::from(3u8), 9);
        sidecar.set(5, locator);

        assert_eq!(sidecar.remove(5), Some(locator));
        assert_eq!(sidecar.get(5), None);
        assert!(sidecar.is_empty());
    }

    #[test]
    fn sidecar_can_retain_subset_of_locators() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            1,
            EdgeLocator::new(SurfaceKind::Forward, NodeId::from(3u8), 1),
        );
        sidecar.set(
            2,
            EdgeLocator::new(SurfaceKind::Forward, NodeId::from(4u8), 2),
        );

        sidecar.retain(|edge_id, _| edge_id == 2);

        assert_eq!(sidecar.get(1), None);
        assert!(sidecar.contains(2));
        assert_eq!(sidecar.len(), 1);
    }
}
