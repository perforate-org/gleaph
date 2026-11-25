//! Column-oriented plan row storage using [`gleaph_gql_planner::BindingLayout`].

use std::collections::BTreeMap;
use std::rc::Rc;

use gleaph_gql_planner::{BindingLayout, PhysicalPlan};

use super::executor::{ExpandDst, PlanBinding};

pub fn empty_row_for_plan(plan: &PhysicalPlan) -> PlanRow {
    if plan.binding_layout().is_empty() {
        PlanRow::new()
    } else {
        PlanRow::with_layout(Rc::new(plan.binding_layout().clone()))
    }
}

pub(crate) fn empty_row_for_plan_with_arena(
    plan: &PhysicalPlan,
    arena: &mut super::arena::QueryArena,
) -> PlanRow {
    if plan.binding_layout().is_empty() {
        PlanRow::new()
    } else {
        let layout = Rc::new(plan.binding_layout().clone());
        let len = layout.len();
        let mut slots = arena.checkout_slots(len);
        if slots.len() < len {
            slots.resize(len, None);
        } else {
            slots.clear();
        }
        PlanRow {
            layout: Some(layout),
            slots,
            spill: BTreeMap::new(),
        }
    }
}

/// A binding offered to [`PlanRow::bind`] together with the element it denotes.
///
/// A vertex binding is compared by the vertex it names, so a vertex already bound on the row
/// unifies with a property-projected record of that same vertex. A binding that names no vertex
/// (an edge, a path, a group, an auxiliary value) unifies only with an equal binding.
#[derive(Clone, Debug)]
pub struct Bound {
    vertex: Option<ExpandDst>,
    binding: PlanBinding,
}

impl Bound {
    /// A binding that stands for `vertex` whatever shape the row stores it in (for example a
    /// property-projected record).
    pub(crate) fn vertex(vertex: ExpandDst, binding: PlanBinding) -> Self {
        Self {
            vertex: Some(vertex),
            binding,
        }
    }

    fn unifies_with(&self, existing: &PlanBinding) -> bool {
        match self.vertex {
            Some(vertex) => existing.vertex_ref() == Some(vertex),
            None => *existing == self.binding,
        }
    }
}

impl From<PlanBinding> for Bound {
    fn from(binding: PlanBinding) -> Self {
        Self {
            vertex: binding.vertex_ref(),
            binding,
        }
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

    /// A new map row holding one binding.
    pub fn single(name: impl Into<String>, binding: PlanBinding) -> Self {
        let mut row = Self::new();
        row.insert(name.into(), binding);
        row
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

    pub(crate) fn layout(&self) -> Option<&Rc<BindingLayout>> {
        self.layout.as_ref()
    }

    pub(crate) fn take_slots(&mut self) -> Vec<Option<PlanBinding>> {
        std::mem::take(&mut self.slots)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&PlanBinding> {
        if let Some(layout) = &self.layout
            && let Some(idx) = layout.index_of(name)
        {
            return self.slots.get(idx).and_then(|b| b.as_ref());
        }
        self.spill.get(name)
    }

    fn insert(&mut self, name: String, binding: PlanBinding) {
        if let Some(layout) = &self.layout
            && let Some(idx) = layout.index_of(&name)
        {
            if idx >= self.slots.len() {
                self.slots.resize(idx + 1, None);
            }
            self.slots[idx] = Some(binding);
            return;
        }
        self.spill.insert(name, binding);
    }

    /// Add a variable this row does not bind yet: a LET or FOR variable, a SEARCH alias, a
    /// projection column, an aggregate slot. A row that already binds `name` is a plan defect
    /// (validation rejects rebinding), reported as [`PlanQueryError::VariableAlreadyBound`]
    /// instead of silently replacing the bound value. Pattern variables use [`Self::bind`].
    pub fn introduce(
        &mut self,
        name: String,
        binding: PlanBinding,
    ) -> Result<(), super::error::PlanQueryError> {
        if self.contains_key(&name) {
            return Err(super::error::PlanQueryError::VariableAlreadyBound { variable: name });
        }
        self.insert(name, binding);
        Ok(())
    }

    /// Bind each name in `names` this row leaves unbound to NULL: the row an OPTIONAL subquery
    /// passes through when it matched nothing.
    pub fn pad_unbound_with_null<'a>(&mut self, names: impl IntoIterator<Item = &'a String>) {
        for name in names {
            if !self.contains_key(name) {
                self.insert(name.clone(), PlanBinding::Value(gleaph_gql::Value::Null));
            }
        }
    }

    pub fn iter(&self) -> PlanRowIter<'_> {
        PlanRowIter {
            row: self,
            map_idx: 0,
            slot_idx: 0,
        }
    }

    /// Bind pattern variables on a copy of this row, by unification: an unbound variable takes
    /// the new binding, a variable already bound to the same element keeps its binding, and a
    /// variable bound to a different element rejects the row (`None`).
    ///
    /// This is the only way an executor operator binds a pattern variable. It is what GQL means by
    /// a variable that appears twice in a pattern: both occurrences denote one element. A scan or
    /// expand that runs over rows already binding its variable (a Router seed, an earlier pattern,
    /// a correlated subquery) therefore filters those rows instead of overwriting them.
    pub fn bind<'a>(&self, updates: impl IntoIterator<Item = (&'a str, Bound)>) -> Option<Self> {
        let mut out = self.clone();
        out.unify_all(updates).then_some(out)
    }

    /// Like [`Self::bind`] but checks out slot storage from the query [`super::arena::QueryArena`].
    pub(crate) fn bind_with_arena<'a>(
        &self,
        arena: &mut super::arena::QueryArena,
        updates: impl IntoIterator<Item = (&'a str, Bound)>,
    ) -> Option<Self> {
        let mut out = match &self.layout {
            Some(layout) if arena.has_pooled_slots() => Self {
                layout: Some(Rc::clone(layout)),
                slots: arena.copy_slots_from(&self.slots),
                spill: self.spill.clone(),
            },
            _ => self.clone(),
        };
        out.unify_all(updates).then_some(out)
    }

    /// True when binding `name` to the vertex `vertex` would unify: `name` is unbound, or bound to
    /// that same vertex. Operators call it before building a binding they would otherwise discard.
    pub(crate) fn admits_vertex(&self, name: &str, vertex: ExpandDst) -> bool {
        self.get(name)
            .is_none_or(|existing| existing.vertex_ref() == Some(vertex))
    }

    fn unify_all<'a>(&mut self, updates: impl IntoIterator<Item = (&'a str, Bound)>) -> bool {
        for (name, bound) in updates {
            match self.get(name) {
                None => self.insert(name.to_string(), bound.binding),
                Some(existing) if bound.unifies_with(existing) => {}
                Some(_) => return false,
            }
        }
        true
    }

    /// Merge `right` into a clone of `self`. Conflicting bindings return `None`.
    ///
    /// When both rows share the same [`BindingLayout`], merges dense slots directly
    /// instead of iterating a map. Names in `skip_names` are taken from `self` only
    /// (join-key fast path).
    pub fn try_merge(&self, right: &Self, skip_names: &[&str]) -> Option<Self> {
        match skip_names {
            [only] => self.try_merge_skip_one(right, only),
            _ => self.try_merge_skip_many(right, skip_names),
        }
    }

    /// Hash-join hot path: one join key, no per-merge skip-name allocation.
    pub fn try_merge_skip_one(&self, right: &Self, skip_name: &str) -> Option<Self> {
        if let (Some(left_layout), Some(right_layout)) = (&self.layout, &right.layout)
            && (Rc::ptr_eq(left_layout, right_layout)
                || left_layout.as_ref() == right_layout.as_ref())
        {
            return self.try_merge_indexed_skip(right, left_layout, &[skip_name]);
        }
        self.try_merge_fallback(right, &[skip_name])
    }

    fn try_merge_skip_many(&self, right: &Self, skip_names: &[&str]) -> Option<Self> {
        if let (Some(left_layout), Some(right_layout)) = (&self.layout, &right.layout)
            && (Rc::ptr_eq(left_layout, right_layout)
                || left_layout.as_ref() == right_layout.as_ref())
        {
            return self.try_merge_indexed_skip(right, left_layout, skip_names);
        }
        self.try_merge_fallback(right, skip_names)
    }

    fn try_merge_indexed_skip(
        &self,
        right: &Self,
        layout: &Rc<BindingLayout>,
        skip_names: &[&str],
    ) -> Option<Self> {
        let mut slots = self.slots.clone();
        let mut spill = if self.spill.is_empty() {
            BTreeMap::new()
        } else {
            self.spill.clone()
        };
        Self::apply_right_to_merged_slots(
            &mut slots,
            &mut spill,
            &self.slots,
            &self.spill,
            right,
            layout,
            skip_names,
        )?;
        Some(Self {
            layout: Some(Rc::clone(layout)),
            slots,
            spill,
        })
    }

    fn apply_right_to_merged_slots(
        slots: &mut Vec<Option<PlanBinding>>,
        spill: &mut BTreeMap<String, PlanBinding>,
        left_slots: &[Option<PlanBinding>],
        left_spill: &BTreeMap<String, PlanBinding>,
        right: &Self,
        layout: &BindingLayout,
        skip_names: &[&str],
    ) -> Option<()> {
        let should_skip = |name: &str| match skip_names {
            [only] => name == *only,
            names => names.contains(&name),
        };

        for (name, right_binding) in right.iter() {
            if should_skip(name) {
                continue;
            }
            if let Some(idx) = layout.index_of(name) {
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
                let left_binding = left_slots
                    .iter()
                    .enumerate()
                    .find_map(|(i, b)| layout.name_at(i).filter(|n| *n == name).and(b.as_ref()))
                    .or_else(|| left_spill.get(name));
                match left_binding {
                    Some(left_binding) if left_binding != right_binding => return None,
                    Some(_) => {}
                    None => {
                        spill.insert(name.to_string(), right_binding.clone());
                    }
                }
            }
        }
        Some(())
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
    pub fn retain_only(layout: Rc<BindingLayout>, source: &Self, keep: &[&str]) -> Self {
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
                if let Some(binding) = binding
                    && let Some(name) = layout.name_at(idx)
                {
                    out.insert(name.to_string(), binding);
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
    use gleaph_gql_planner::BindingLayout;
    use gleaph_graph_kernel::entry::VertexId;

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
        use gleaph_gql_planner::{PlanOp, derive_binding_layout};
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
        let left = PlanRow::with_layout_and_binding(
            Rc::clone(&layout),
            "a",
            PlanBinding::Vertex(VertexId::from(1)),
        );
        let right =
            PlanRow::with_layout_and_binding(layout, "a", PlanBinding::Vertex(VertexId::from(2)));
        assert!(left.try_merge(&right, &[]).is_none());
    }

    fn vertex_in(row: &PlanRow, name: &str) -> Option<VertexId> {
        match row.get(name) {
            Some(PlanBinding::Vertex(v)) => Some(*v),
            _ => None,
        }
    }

    fn bound_a(vertex: u32) -> PlanRow {
        PlanRow::with_layout_and_binding(
            Rc::new(BindingLayout::single("a".into())),
            "a",
            PlanBinding::Vertex(VertexId::from(vertex)),
        )
    }

    #[test]
    fn bind_sets_an_unbound_variable_in_its_layout_slot() {
        let row = PlanRow::with_layout(Rc::new(BindingLayout::single("a".into())));
        let out = row
            .bind([("a", PlanBinding::Vertex(VertexId::from(2)).into())])
            .expect("unbound variable binds");
        assert_eq!(vertex_in(&out, "a"), Some(VertexId::from(2)));
        assert!(out.spill.is_empty());
    }

    #[test]
    fn bind_keeps_a_variable_already_bound_to_the_same_vertex() {
        let out = bound_a(1)
            .bind([("a", PlanBinding::Vertex(VertexId::from(1)).into())])
            .expect("same vertex unifies");
        assert_eq!(vertex_in(&out, "a"), Some(VertexId::from(1)));
    }

    #[test]
    fn bind_rejects_a_variable_already_bound_to_another_vertex() {
        assert!(
            bound_a(1)
                .bind([("a", PlanBinding::Vertex(VertexId::from(2)).into())])
                .is_none()
        );
    }

    #[test]
    fn bind_unifies_a_projected_record_by_the_vertex_it_stands_for() {
        let record = PlanBinding::Value(gleaph_gql::Value::Record(Vec::new()));
        let same = ExpandDst::Local(VertexId::from(1));
        let out = bound_a(1)
            .bind([("a", Bound::vertex(same, record.clone()))])
            .expect("projection of the bound vertex unifies");
        assert_eq!(
            vertex_in(&out, "a"),
            Some(VertexId::from(1)),
            "the existing vertex binding is kept"
        );
        let other = ExpandDst::Local(VertexId::from(2));
        assert!(
            bound_a(1)
                .bind([("a", Bound::vertex(other, record))])
                .is_none()
        );
    }

    #[test]
    fn bind_unifies_non_vertex_bindings_by_equality() {
        let mut row = PlanRow::new();
        row.insert("x".into(), PlanBinding::Value(gleaph_gql::Value::Int64(1)));
        assert!(
            row.bind([("x", PlanBinding::Value(gleaph_gql::Value::Int64(1)).into())])
                .is_some()
        );
        assert!(
            row.bind([("x", PlanBinding::Value(gleaph_gql::Value::Int64(2)).into())])
                .is_none()
        );
    }

    #[test]
    fn bind_rejects_two_different_vertices_for_one_variable_in_one_call() {
        let row = PlanRow::with_layout(Rc::new(BindingLayout::single("a".into())));
        assert!(
            row.bind([
                ("a", PlanBinding::Vertex(VertexId::from(1)).into()),
                ("a", PlanBinding::Vertex(VertexId::from(2)).into()),
            ])
            .is_none()
        );
    }

    #[test]
    fn introduce_adds_a_new_variable_and_rejects_a_bound_one() {
        let mut row = bound_a(1);
        row.introduce("x".into(), PlanBinding::Value(gleaph_gql::Value::Int64(1)))
            .expect("new variable");
        assert!(row.get("x").is_some());
        let err = row
            .introduce("a".into(), PlanBinding::Value(gleaph_gql::Value::Int64(2)))
            .expect_err("a is bound");
        assert!(matches!(
            err,
            super::super::error::PlanQueryError::VariableAlreadyBound { ref variable } if variable == "a"
        ));
        assert_eq!(
            vertex_in(&row, "a"),
            Some(VertexId::from(1)),
            "the bound vertex stays"
        );
    }

    #[test]
    fn pad_unbound_with_null_keeps_bound_variables() {
        let mut row = bound_a(1);
        row.pad_unbound_with_null(&["a".to_string(), "b".to_string()]);
        assert_eq!(vertex_in(&row, "a"), Some(VertexId::from(1)));
        assert_eq!(
            row.get("b"),
            Some(&PlanBinding::Value(gleaph_gql::Value::Null))
        );
    }

    #[test]
    fn admits_vertex_answers_what_bind_would() {
        let row = bound_a(1);
        assert!(row.admits_vertex("a", ExpandDst::Local(VertexId::from(1))));
        assert!(!row.admits_vertex("a", ExpandDst::Local(VertexId::from(2))));
        assert!(row.admits_vertex("b", ExpandDst::Local(VertexId::from(2))));
    }

    #[test]
    fn spill_map_row_stores_bindings_outside_layout() {
        let mut row = PlanRow::new();
        row.insert("extra".into(), PlanBinding::Value(gleaph_gql::Value::Null));
        assert_eq!(row.binding_count(), 1);
        assert!(row.get("extra").is_some());
        assert!(!row.contains_key("missing"));
    }

    #[test]
    fn bind_with_arena_unifies_like_bind() {
        use super::super::arena::QueryArena;

        let layout = Rc::new(BindingLayout::single("a".into()));
        QueryArena::with(|arena| {
            arena.reset();
            let row = PlanRow::with_layout(Rc::clone(&layout));
            let bound = row
                .bind_with_arena(
                    arena,
                    [("a", PlanBinding::Vertex(VertexId::from(9)).into())],
                )
                .expect("unbound variable binds");
            assert_eq!(vertex_in(&bound, "a"), Some(VertexId::from(9)));
            assert!(
                bound
                    .bind_with_arena(
                        arena,
                        [("a", PlanBinding::Vertex(VertexId::from(8)).into())]
                    )
                    .is_none()
            );
        });
    }

    #[test]
    fn empty_row_for_plan_uses_layout_when_present() {
        use gleaph_gql_planner::{PhysicalPlan, PlanOp};
        let ops = vec![PlanOp::NodeScan {
            variable: "n".into(),
            label: None,
            property_projection: None,
        }];
        let plan = PhysicalPlan::from_ops(ops);
        let row = empty_row_for_plan(&plan);
        assert!(row.layout().is_some());
        assert_eq!(row.layout().unwrap().len(), 1);
    }
}
