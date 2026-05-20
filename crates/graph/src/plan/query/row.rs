//! Column-oriented plan row storage using [`gleaph_gql_planner::BindingLayout`].

use std::collections::BTreeMap;
use std::rc::Rc;

use gleaph_gql_planner::{BindingLayout, PhysicalPlan};

use super::executor::PlanBinding;

pub fn empty_row_for_plan(plan: &PhysicalPlan) -> PlanRow {
    if plan.binding_layout.is_empty() {
        PlanRow::new()
    } else {
        PlanRow::with_layout(Rc::new(plan.binding_layout.clone()))
    }
}

/// One executor row: dense slots when a [`BindingLayout`] is present, else a map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanRow {
    layout: Option<Rc<BindingLayout>>,
    slots: Vec<Option<PlanBinding>>,
    spill: BTreeMap<String, PlanBinding>,
}

impl PlanRow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_layout(layout: Rc<BindingLayout>) -> Self {
        let len = layout.len();
        Self {
            layout: Some(layout),
            slots: vec![None; len],
            spill: BTreeMap::new(),
        }
    }

    pub fn with_layout_and_binding(
        layout: Rc<BindingLayout>,
        name: &str,
        binding: PlanBinding,
    ) -> Self {
        let mut row = Self::with_layout(layout);
        row.insert(name.to_string(), binding);
        row
    }

    pub fn len(&self) -> usize {
        if let Some(layout) = &self.layout {
            layout
                .len()
                .max(self.slots.iter().filter(|b| b.is_some()).count())
        } else {
            self.spill.len()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of variables currently bound on this row.
    pub fn binding_count(&self) -> usize {
        let slot_count = self.slots.iter().filter(|b| b.is_some()).count();
        slot_count + self.spill.len()
    }

    /// True when `name` is the only binding on this row.
    pub fn is_singleton_binding(&self, name: &str) -> bool {
        self.get(name).is_some() && self.binding_count() == 1
    }

    pub fn shared_layout(&self) -> Option<Rc<BindingLayout>> {
        self.layout.as_ref().map(Rc::clone)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&PlanBinding> {
        if let Some(layout) = &self.layout {
            if let Some(idx) = layout.index_of(name) {
                return self.slots.get(idx).and_then(|b| b.as_ref());
            }
        }
        self.spill.get(name)
    }

    pub fn insert(&mut self, name: String, binding: PlanBinding) {
        if let Some(layout) = &self.layout {
            if let Some(idx) = layout.index_of(&name) {
                if idx >= self.slots.len() {
                    self.slots.resize(idx + 1, None);
                }
                self.slots[idx] = Some(binding);
                return;
            }
        }
        self.spill.insert(name, binding);
    }

    pub fn iter(&self) -> PlanRowIter<'_> {
        PlanRowIter {
            row: self,
            map_idx: 0,
            slot_idx: 0,
        }
    }

    /// Clone row storage and apply binding updates in one pass (expand / scan hot path).
    pub fn fork<'a>(
        &self,
        updates: impl IntoIterator<Item = (&'a str, PlanBinding)>,
    ) -> Self {
        match &self.layout {
            Some(layout) => {
                let mut slots = self.slots.clone();
                let mut spill = self.spill.clone();
                for (name, binding) in updates {
                    if let Some(idx) = layout.index_of(name) {
                        if idx >= slots.len() {
                            slots.resize(idx + 1, None);
                        }
                        slots[idx] = Some(binding);
                    } else {
                        spill.insert(name.to_string(), binding);
                    }
                }
                Self {
                    layout: Some(Rc::clone(layout)),
                    slots,
                    spill,
                }
            }
            None => {
                let mut spill = self.spill.clone();
                for (name, binding) in updates {
                    spill.insert(name.to_string(), binding);
                }
                Self {
                    layout: None,
                    slots: Vec::new(),
                    spill,
                }
            }
        }
    }

    /// Clone and set or replace bindings (indexed fast path when layout matches).
    pub fn clone_with_bindings(
        &self,
        updates: impl IntoIterator<Item = (String, PlanBinding)>,
    ) -> Self {
        let mut out = self.clone();
        for (name, binding) in updates {
            out.insert(name, binding);
        }
        out
    }

    /// Merge `right` into a clone of `self`. Conflicting bindings return `None`.
    ///
    /// When both rows share the same [`BindingLayout`], merges dense slots directly
    /// instead of iterating a map. Names in `skip_names` are taken from `self` only
    /// (join-key fast path).
    pub fn try_merge(&self, right: &Self, skip_names: &[&str]) -> Option<Self> {
        if let (Some(left_layout), Some(right_layout)) = (&self.layout, &right.layout) {
            if Rc::ptr_eq(left_layout, right_layout) || left_layout.as_ref() == right_layout.as_ref()
            {
                return self.try_merge_indexed(right, left_layout, skip_names);
            }
        }
        self.try_merge_fallback(right, skip_names)
    }

    fn try_merge_indexed(
        &self,
        right: &Self,
        layout: &Rc<BindingLayout>,
        skip_names: &[&str],
    ) -> Option<Self> {
        let skip_single = match skip_names {
            [] => None,
            [only] => layout.index_of(only),
            _ => None,
        };
        let skip_many: Vec<usize> = if skip_names.len() <= 1 {
            Vec::new()
        } else {
            skip_names
                .iter()
                .filter_map(|name| layout.index_of(name))
                .collect()
        };

        let mut slots = self.slots.clone();
        let mut spill = self.spill.clone();

        let skip_slot = |idx: usize| {
            skip_single == Some(idx) || skip_many.iter().any(|&s| s == idx)
        };

        for (name, right_binding) in right.iter() {
            if skip_names.contains(&name) {
                continue;
            }
            if let Some(idx) = layout.index_of(name) {
                if skip_slot(idx) {
                    continue;
                }
                match slots.get(idx) {
                    Some(Some(left_binding)) if left_binding != right_binding => return None,
                    Some(None) | None => {
                        if idx >= slots.len() {
                            slots.resize(idx + 1, None);
                        }
                        slots[idx] = Some(right_binding.clone());
                    }
                    Some(Some(_)) => {}
                }
            } else {
                match self.get(name).or_else(|| spill.get(name)) {
                    Some(left_binding) if left_binding != right_binding => return None,
                    Some(_) => {}
                    None => {
                        spill.insert(name.to_string(), right_binding.clone());
                    }
                }
            }
        }

        Some(Self {
            layout: Some(Rc::clone(layout)),
            slots,
            spill,
        })
    }

    fn try_merge_fallback(&self, right: &Self, skip_names: &[&str]) -> Option<Self> {
        let mut merged = self.clone();
        for (name, right_binding) in right.iter() {
            if skip_names.contains(&name) {
                continue;
            }
            match merged.get(name) {
                Some(left_binding) if left_binding != right_binding => return None,
                Some(_) => {}
                None => {
                    merged.insert(name.to_string(), right_binding.clone());
                }
            }
        }
        Some(merged)
    }

    /// Row containing only the listed variables (used after shortest-path narrowing).
    pub fn retain_only(
        layout: Rc<BindingLayout>,
        source: &Self,
        keep: &[&str],
    ) -> Self {
        let mut out = Self::with_layout(layout);
        for name in keep {
            if let Some(binding) = source.get(name) {
                out.insert(name.to_string(), binding.clone());
            }
        }
        out
    }

    pub fn into_btree_map(self) -> BTreeMap<String, PlanBinding> {
        let mut out = self.spill;
        if let Some(layout) = &self.layout {
            for (idx, binding) in self.slots.into_iter().enumerate() {
                if let Some(binding) = binding {
                    if let Some(name) = layout.name_at(idx) {
                        out.insert(name.to_string(), binding);
                    }
                }
            }
        }
        out
    }

    pub fn from_btree_map(map: BTreeMap<String, PlanBinding>) -> Self {
        Self {
            layout: None,
            slots: Vec::new(),
            spill: map,
        }
    }
}

impl From<BTreeMap<String, PlanBinding>> for PlanRow {
    fn from(map: BTreeMap<String, PlanBinding>) -> Self {
        Self::from_btree_map(map)
    }
}

pub struct PlanRowIter<'a> {
    row: &'a PlanRow,
    map_idx: usize,
    slot_idx: usize,
}

impl<'a> Iterator for PlanRowIter<'a> {
    type Item = (&'a str, &'a PlanBinding);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(layout) = &self.row.layout {
            while self.slot_idx < self.row.slots.len() {
                let idx = self.slot_idx;
                self.slot_idx += 1;
                if let Some(binding) = self.row.slots[idx].as_ref() {
                    let name = layout.name_at(idx)?;
                    return Some((name, binding));
                }
            }
        }
        let spill = &self.row.spill;
        let keys: Vec<_> = spill.keys().collect();
        if self.map_idx < keys.len() {
            let key = keys[self.map_idx];
            self.map_idx += 1;
            return spill.get(key).map(|b| (key.as_str(), b));
        }
        None
    }
}

/// Public alias used across graph plan execution.
pub type PlanQueryRow = PlanRow;

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_lara::VertexId;
    use gleaph_gql_planner::BindingLayout;

    #[test]
    fn singleton_binding_detects_single_slot() {
        let layout = Rc::new(BindingLayout::single("p".into()));
        let row = PlanRow::with_layout_and_binding(
            layout,
            "p",
            PlanBinding::Value(gleaph_gql::Value::Null),
        );
        assert!(row.is_singleton_binding("p"));
        assert!(!row.is_singleton_binding("q"));
    }

    #[test]
    fn try_merge_indexed_rows_combine_disjoint_slots() {
        use gleaph_gql_planner::{derive_binding_layout, PlanOp};
        let layout = Rc::new(derive_binding_layout(&[
            PlanOp::NodeScan {
                variable: "a".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "b".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: None,
                property_projection: None,
            },
        ]));
        let mut left = PlanRow::with_layout(Rc::clone(&layout));
        left.insert("a".into(), PlanBinding::Vertex(VertexId::from(1)));
        left.insert("b".into(), PlanBinding::Vertex(VertexId::from(2)));

        let mut right = PlanRow::with_layout(Rc::clone(&layout));
        right.insert("a".into(), PlanBinding::Vertex(VertexId::from(1)));
        right.insert("c".into(), PlanBinding::Vertex(VertexId::from(3)));

        let merged = left.try_merge(&right, &["a"]).expect("merge");
        assert_eq!(
            merged.get("b").and_then(|b| match b {
                PlanBinding::Vertex(v) => Some(*v),
                _ => None,
            }),
            Some(VertexId::from(2))
        );
        assert_eq!(
            merged.get("c").and_then(|b| match b {
                PlanBinding::Vertex(v) => Some(*v),
                _ => None,
            }),
            Some(VertexId::from(3))
        );
    }

    #[test]
    fn try_merge_indexed_rows_reject_conflicting_slots() {
        let layout = Rc::new(BindingLayout::single("a".into()));
        let left =
            PlanRow::with_layout_and_binding(Rc::clone(&layout), "a", PlanBinding::Vertex(VertexId::from(1)));
        let right =
            PlanRow::with_layout_and_binding(layout, "a", PlanBinding::Vertex(VertexId::from(2)));
        assert!(left.try_merge(&right, &[]).is_none());
    }

    #[test]
    fn fork_updates_layout_slot() {
        let layout = Rc::new(BindingLayout::single("a".into()));
        let row = PlanRow::with_layout_and_binding(
            Rc::clone(&layout),
            "a",
            PlanBinding::Vertex(VertexId::from(1)),
        );
        let out = row.fork([("a", PlanBinding::Vertex(VertexId::from(2)))]);
        assert_eq!(
            out.get("a").and_then(|b| match b {
                PlanBinding::Vertex(v) => Some(*v),
                _ => None,
            }),
            Some(VertexId::from(2))
        );
        assert!(out.spill.is_empty());
    }
}
