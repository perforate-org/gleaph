use gleaph_types::VertexIdSet;
use rapidhash::fast::RapidHashMap;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct LabelIndex {
    next_label_id: u32,
    label_to_id: RapidHashMap<String, u32>,
    id_to_label: RapidHashMap<u32, String>,
    vertex_postings: BTreeMap<u32, VertexIdSet>,
}

impl LabelIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn label_id(&self, label: &str) -> Option<u32> {
        self.label_to_id.get(label).copied()
    }

    pub fn label_name(&self, label_id: u32) -> Option<&str> {
        self.id_to_label.get(&label_id).map(String::as_str)
    }

    pub fn ensure_label_id(&mut self, label: &str) -> u32 {
        if let Some(id) = self.label_to_id.get(label).copied() {
            return id;
        }
        let id = self.next_label_id;
        self.next_label_id = self.next_label_id.saturating_add(1);
        self.label_to_id.insert(label.to_string(), id);
        self.id_to_label.insert(id, label.to_string());
        id
    }

    pub fn add_vertex_label(&mut self, vertex_id: u32, label: &str) {
        let label_id = self.ensure_label_id(label);
        self.add_vertex_label_id(vertex_id, label_id);
    }

    pub fn add_vertex_label_id(&mut self, vertex_id: u32, label_id: u32) {
        self.vertex_postings
            .entry(label_id)
            .or_default()
            .insert(vertex_id);
    }

    pub fn remove_vertex_label(&mut self, vertex_id: u32, label: &str) {
        if let Some(label_id) = self.label_id(label) {
            self.remove_vertex_label_id(vertex_id, label_id);
        }
    }

    pub fn remove_vertex_label_id(&mut self, vertex_id: u32, label_id: u32) {
        if let Some(posting) = self.vertex_postings.get_mut(&label_id) {
            posting.remove(vertex_id);
        }
    }

    pub fn scan_vertices_by_label(&self, label: &str) -> VertexIdSet {
        let Some(label_id) = self.label_id(label) else {
            return VertexIdSet::new();
        };
        self.vertex_postings
            .get(&label_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the complete `label_id → name` mapping for snapshot persistence.
    ///
    /// This allows `restore_overlay_snapshot` to pre-seed the LabelIndex with
    /// the exact same IDs that are baked into `EdgeEntry.label_id` in stable memory.
    pub fn label_id_map_snapshot(&self) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = self
            .id_to_label
            .iter()
            .map(|(&id, name)| (id, name.clone()))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Pre-seeds the label index from a persisted `label_id → name` mapping.
    ///
    /// Must be called on a freshly-defaulted `LabelIndex` **before** any
    /// `ensure_label_id` / `add_vertex_label` calls so that
    /// subsequent calls reuse the existing IDs rather than assigning new ones.
    pub fn restore_from_label_id_map(&mut self, map: Vec<(u32, String)>) {
        let mut max_id = 0u32;
        for (id, name) in map {
            self.restore_known_label_id(id, &name);
            max_id = max_id.max(id + 1);
        }
        self.next_label_id = max_id;
    }

    pub fn restore_known_label_id(&mut self, id: u32, label: &str) {
        self.id_to_label.insert(id, label.to_string());
        self.label_to_id.insert(label.to_string(), id);
        self.next_label_id = self.next_label_id.max(id.saturating_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_scan_vertex_labels() {
        let mut idx = LabelIndex::new();
        idx.add_vertex_label(1, "User");
        idx.add_vertex_label(2, "User");
        idx.add_vertex_label(3, "Company");
        assert_eq!(
            idx.scan_vertices_by_label("User"),
            VertexIdSet::from_iter([1, 2])
        );
        idx.remove_vertex_label(1, "User");
        assert_eq!(
            idx.scan_vertices_by_label("User"),
            VertexIdSet::from_iter([2])
        );
    }

    #[test]
    fn label_ids_round_trip() {
        let mut idx = LabelIndex::new();
        let knows = idx.ensure_label_id("KNOWS");
        let likes = idx.ensure_label_id("LIKES");
        let snap = idx.label_id_map_snapshot();

        let mut restored = LabelIndex::new();
        restored.restore_from_label_id_map(snap);
        assert_eq!(restored.label_name(knows), Some("KNOWS"));
        assert_eq!(restored.label_name(likes), Some("LIKES"));
        assert_eq!(restored.label_id("KNOWS"), Some(knows));
        assert_eq!(restored.label_id("LIKES"), Some(likes));
    }
}
