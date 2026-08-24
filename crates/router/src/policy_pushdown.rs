//! Conditional-policy constant resolution and plan lowering ([ADR 0075] §5).
//!
//! At execution time — after plan build and data-plane enforcement, before dispatch —
//! this module lowers every conditional-policy predicate that binds the invoking caller
//! into **ordinary plan machinery**:
//!
//! - `MSG_CALLER()` is substituted with the caller principal as a literal constant
//!   (the second resolution site of [ADR 0075] §5; shard-side runtime resolution keeps
//!   serving user-authored predicates).
//! - Equality on an indexed property at a labeled pipeline-head vertex scan seeds the
//!   existing index-scan anchor path with the resolved value, so index canisters only
//!   ever execute lookups they cannot distinguish from any other lookup.
//! - All remaining comparisons become ordinary [`PlanOp::PropertyFilter`] ops inside
//!   the dispatched plan. Shards execute filters they cannot distinguish from
//!   user-authored ones: no caller identity, policy object, or policy engine crosses
//!   the Router boundary.
//!
//! Semantics ([ADR 0075] §4): predicates are additional conjuncts over the granted
//! resource, always AND-composed across every applicable grant row (`caller ∪ PUBLIC`).
//! Filtered-out rows are absent results, never errors. Policies constrain outputs and
//! add no requirements — lowering therefore happens strictly **after** authorization,
//! so requirement extraction never sees policy-derived property reads.
//!
//! Binding-site treatment: definite labeled vertex scans receive plain conjuncts;
//! may-bind destinations (unlabeled scans, index scans whose op carries no label fact,
//! expansion endpoints) receive guarded filters `NOT IsLabeled(v, L) OR (C₁ AND …)` so
//! rows bound under any other label pass untouched. Variable-length quantifiers bind
//! group lists and shortest paths bind path records, where scalar guards would
//! mis-evaluate, so those sites defer their policy treatment ([ADR 0075] §7 lineage);
//! multi-graph `USE GRAPH` segments keep their own graphs' policies.

use std::collections::BTreeMap;
use std::rc::Rc;

use candid::Principal;

use gleaph_auth::{
    CompiledPredicate, GrantSubject, PredicateLiteral, PredicateOp, PredicateValue, Privilege,
};
use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::plan::{NodeLabelRef, PhysicalPlan, PlanOp, ScanValue, Str};
use gleaph_graph_kernel::entry::{GraphId, PropertyId, VertexLabelId};

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::state::RouterError;

/// Maximum number of per-caller lowered prepared plans retained in heap cache. Entries
/// are exact-key addressed by resolved-constant identity, so eviction is a capacity
/// decision only (same discipline as the derived sorted-plan caches, GAP-2026-07-31-001).
pub(crate) const LOWERED_PREPARED_CACHE_LIMIT: usize = 32;

/// One lowered comparison of a policy group, with catalog names re-resolved and the
/// caller constant substituted ([ADR 0075] §5).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedComparison {
    pub(crate) property_id: u32,
    pub(crate) property_name: String,
    pub(crate) op: PredicateOp,
    pub(crate) value: gleaph_gql::Value,
}

/// One label's applicable policy: every applicable grant row contributes its own
/// AND-conjunction ([ADR 0075] §2), and the label's visible set is the **union** over
/// rows ([ADR 0074] alternatives semantics: grants are additive, each narrowed by its
/// own condition — the demonstrated "own rows plus public rows" shape).
#[derive(Clone, Debug)]
pub(crate) struct ResolvedPolicyGroup {
    label_id: u32,
    label_name: String,
    /// Per-row conjunctions, in canonical grant-key order.
    rows: Vec<ResolvedConjunction>,
}

/// One applicable grant row's compiled condition, lowered.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedConjunction {
    /// Stable row identity feeding the cache fingerprint (subject kind + principal).
    subject_tag: u8,
    comparisons: Vec<ResolvedComparison>,
}

/// Applicable policy groups for one execution plus the exact fingerprint of the
/// lowering inputs (applicable rows' identities plus the resolved caller), used as the
/// cache-key component that keeps plans from being reused across callers with different
/// resolved constants ([ADR 0075] §5 trade-off note).
pub(crate) struct ResolvedPolicies {
    pub(crate) fingerprint: Vec<u8>,
    pub(crate) groups: Vec<ResolvedPolicyGroup>,
}

impl ResolvedPolicies {
    pub(crate) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Collects every conditional-policy predicate applicable to `caller ∪ PUBLIC` on
/// `graph`, merging conjuncts per label in canonical grant-row order (always AND,
/// [ADR 0075] §4), resolving catalog names, substituting constants, and computing the
/// fingerprint.
///
/// Implicit-root exemption ([ADR 0074] §3 invariant 3): ownership-derived tenancy is
/// the root of data-plane authority and never consults grant rows, so grant-attached
/// predicates cannot narrow a tenant's view either.
///
/// Fails closed on unresolvable vocabulary: stored predicates reference monotonic ids
/// that the drop sweep removes together with their resources, so an unresolvable id is
/// corrupt state rather than skippable input.
pub(crate) fn effective_policies(
    store: &RouterStore,
    caller: &Principal,
    graph: GraphId,
) -> Result<ResolvedPolicies, RouterError> {
    if store.is_graph_tenant(graph, *caller) {
        return Ok(ResolvedPolicies {
            fingerprint: Vec::new(),
            groups: Vec::new(),
        });
    }
    let now_ns = crate::facade::store::ic_time_ns();
    let mut merged: BTreeMap<u32, ResolvedPolicyGroup> = BTreeMap::new();
    let mut fingerprint = Vec::new();

    // Rows arrive in canonical key order, so per-row conjunctions and the union order
    // stay deterministic.
    for row in auth::grant_rows() {
        let Privilege::Graph(gp) = &row.privilege else {
            continue;
        };
        if gp.graph != graph.raw() || !subject_applies(&row.subject, caller) {
            continue;
        }
        if row.expires_at_ns.is_some_and(|expiry| expiry < now_ns) {
            continue;
        }
        let Some(predicate) = &row.predicate else {
            continue;
        };
        let subject_tag =
            encode_fingerprint_row(&mut fingerprint, &row.subject, predicate.as_ref());

        let group = match merged.entry(predicate.label) {
            std::collections::btree_map::Entry::Occupied(occupied) => occupied.into_mut(),
            std::collections::btree_map::Entry::Vacant(vacant) => {
                let Ok(label_u16) = u16::try_from(predicate.label) else {
                    return Err(RouterError::Internal(
                        "policy label id exceeds catalog space".into(),
                    ));
                };
                vacant.insert(ResolvedPolicyGroup {
                    label_id: predicate.label,
                    label_name: store
                        .reverse_vertex_label_name(graph, VertexLabelId::from_raw(label_u16))?,
                    rows: Vec::new(),
                })
            }
        };
        let mut conjunction = ResolvedConjunction {
            subject_tag,
            comparisons: Vec::new(),
        };
        for conjunct in &predicate.conjuncts {
            let property_name =
                store.reverse_property_name(graph, PropertyId::from_raw(conjunct.property))?;
            conjunction.comparisons.push(ResolvedComparison {
                property_id: conjunct.property,
                property_name,
                op: conjunct.op,
                value: resolve_constant(caller, conjunct.value.clone())?,
            });
        }
        group.rows.push(conjunction);
    }

    // The caller identity participates in the fingerprint even when no MSG_CALLER()
    // appears: it is the complete discriminator between callers, so distinct callers can
    // never alias one lowered-plan cache entry.
    fingerprint.extend_from_slice(caller.as_slice());

    Ok(ResolvedPolicies {
        fingerprint,
        groups: merged.into_values().collect(),
    })
}

/// Whether a grant row's subject binds this execution: the caller's own principal or
/// the `PUBLIC` baseline (which anonymous callers evaluate as).
fn subject_applies(subject: &GrantSubject, caller: &Principal) -> bool {
    match subject {
        GrantSubject::Public => true,
        GrantSubject::Principal(p) => p == caller,
    }
}

/// Append one row's identity to the exact fingerprint: subject kind, principal bytes,
/// label id, then every conjunct's `(property, op, value)` encoding. Returns the
/// subject kind tag used in the per-row conjunction identity.
fn encode_fingerprint_row(
    out: &mut Vec<u8>,
    subject: &GrantSubject,
    predicate: &CompiledPredicate,
) -> u8 {
    let kind = match subject {
        GrantSubject::Public => 0u8,
        GrantSubject::Principal(p) => {
            out.extend_from_slice(p.as_slice());
            1
        }
    };
    out.push(kind);
    out.extend_from_slice(&predicate.label.to_le_bytes());
    out.push(predicate.conjuncts.len() as u8);
    for conjunct in &predicate.conjuncts {
        out.extend_from_slice(&conjunct.property.to_le_bytes());
        out.push(match conjunct.op {
            PredicateOp::Eq => 0,
            PredicateOp::Ne => 1,
            PredicateOp::Lt => 2,
            PredicateOp::Le => 3,
            PredicateOp::Gt => 4,
            PredicateOp::Ge => 5,
        });
        match &conjunct.value {
            PredicateValue::MsgCaller => out.push(0),
            PredicateValue::Literal(PredicateLiteral::Bool(b)) => {
                out.push(1);
                out.push(u8::from(*b));
            }
            PredicateValue::Literal(PredicateLiteral::Int(i)) => {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
            }
            PredicateValue::Literal(PredicateLiteral::Float(f)) => {
                out.push(3);
                out.extend_from_slice(&f.to_le_bytes());
            }
            PredicateValue::Literal(PredicateLiteral::String(s)) => {
                out.push(4);
                out.extend_from_slice(&(s.len() as u8).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    kind
}

/// Substitute `MSG_CALLER()` with the invoking caller as a literal constant
/// ([ADR 0075] §5 second resolution site).
fn resolve_constant(
    caller: &Principal,
    value: PredicateValue,
) -> Result<gleaph_gql::Value, RouterError> {
    Ok(match value {
        PredicateValue::MsgCaller => gleaph_gql_ic::principal_to_value(*caller),
        PredicateValue::Literal(PredicateLiteral::Bool(b)) => gleaph_gql::Value::Bool(b),
        PredicateValue::Literal(PredicateLiteral::Int(i)) => gleaph_gql::Value::Int64(i),
        PredicateValue::Literal(PredicateLiteral::Float(f)) => gleaph_gql::Value::Float64(f),
        PredicateValue::Literal(PredicateLiteral::String(s)) => gleaph_gql::Value::Text(s),
    })
}

// ──── Plan lowering ────

/// Catalog access needed by index-seed eligibility checks.
pub(crate) struct LoweringContext<'a> {
    graph: GraphId,
    #[allow(dead_code, reason = "retained for catalog-aware seed decisions")]
    store: &'a RouterStore,
}

impl<'a> LoweringContext<'a> {
    pub(crate) fn new(store: &'a RouterStore, graph: GraphId) -> Self {
        Self { store, graph }
    }
}

/// Lower `policies` into `plan` in place. No-op when no policy applies.
pub(crate) fn lower_into_plan(
    ctx: &LoweringContext<'_>,
    plan: &mut PhysicalPlan,
    policies: &ResolvedPolicies,
) {
    if policies.groups.is_empty() {
        return;
    }
    lower_ops_slice(ctx, &mut plan.ops, policies, true);
}

/// Lowers one ops pipeline. Insertions go directly after their binding site; nested
/// sub-plans lower independently (`is_root` marks the position eligible for index-seed
/// replacement, which is valid only where an anchor may start a pipeline).
fn lower_ops_slice(
    ctx: &LoweringContext<'_>,
    ops: &mut Vec<PlanOp>,
    policies: &ResolvedPolicies,
    is_root: bool,
) {
    let mut index = 0usize;
    while index < ops.len() {
        recurse_nested_subplans(
            ops.get_mut(index).expect("index within slice"),
            policies,
            ctx,
        );

        // Pass A — inspect immutably and decide every mutation for this position.
        let decision = decide_site(
            ctx,
            ops.get(index).expect("index within slice"),
            policies,
            is_root,
        );
        let mut offset = 1usize;

        // Pass B — apply: hydrate projections, optional anchor replacement, filters.
        if !decision.hydrate.is_empty() {
            hydrate_site(
                ops.get_mut(index).expect("index within slice"),
                &decision.hydrate,
            );
        }
        if let Some(replacement) = decision.replacement {
            *ops.get_mut(index).expect("index within slice") = replacement;
        }
        if !decision.dst_injections.is_empty()
            && let PlanOp::ExpandFilter { dst_filter, .. } =
                ops.get_mut(index).expect("index within slice")
        {
            for expr in &decision.dst_injections {
                if !dst_filter.contains(expr) {
                    dst_filter.push(expr.clone());
                }
            }
        }
        for expr in decision.filters {
            ops.insert(
                index + offset,
                PlanOp::PropertyFilter {
                    predicates: vec![expr],
                    stage: 0,
                },
            );
            offset += 1;
        }
        index += offset;
    }
}

/// One position's lowering plan.
struct SiteDecision {
    /// Property names to add to this site's hydration lists.
    hydrate: Vec<String>,
    /// Fully-built operator replacing the current one (index-seeded scan).
    replacement: Option<PlanOp>,
    /// Guard/plain filter expressions inserted directly after this position.
    filters: Vec<Expr>,
    /// Union predicates appended into this `ExpandFilter`'s `dst_filter`.
    dst_injections: Vec<Expr>,
}

fn decide_site(
    ctx: &LoweringContext<'_>,
    op: &PlanOp,
    policies: &ResolvedPolicies,
    is_root: bool,
) -> SiteDecision {
    let none = || SiteDecision {
        hydrate: Vec::new(),
        replacement: None,
        filters: Vec::new(),
        dst_injections: Vec::new(),
    };
    match op {
        // Definite labeled vertex scan: the label's policy filter, index-seed eligible.
        PlanOp::NodeScan {
            variable,
            label: Some(label_ref),
            ..
        } => {
            let Some(group) = find_group(policies, label_ref) else {
                return none();
            };
            let seed = if is_root {
                indexed_equality(ctx, group)
            } else {
                None
            };
            let replacement = seed.as_ref().map(|(seed_cmp, _)| PlanOp::IndexScan {
                variable: variable.clone(),
                property: seed_cmp.property_name.clone().into(),
                value: ScanValue::Literal(seed_cmp.value.clone()),
                cmp: CmpOp::Eq,
                property_projection: site_projection(op),
            });
            // The seeded row's remaining conjuncts stay as residual filters; when
            // several rows cover the label (or no seed applied) the full union filter
            // runs instead — both shapes evaluate identically over bound rows.
            let filters = match (seed.as_ref(), group.rows.as_slice()) {
                (Some((_, residual)), [row]) if row.comparisons.len() > residual.len() => {
                    conjunction_filters(variable, residual)
                }
                _ => vec![label_filter(variable, group)],
            };
            SiteDecision {
                hydrate: group_property_names(group),
                replacement,
                filters,
                dst_injections: Vec::new(),
            }
        }
        PlanOp::ExpandFilter {
            dst,
            dst_filter,
            var_len: None,
            ..
        } => {
            // Expansion destinations carry their label fact inside `dst_filter`
            // (`IsLabeled(p, Post)`), which the wire's label-resolution collection
            // registers. Inject plain conjuncts per matching group directly there —
            // no separate op, and the label name is guaranteed resolvable because the
            // planner proved it from the pattern.
            let mut dst_injections = Vec::new();
            for group in &policies.groups {
                if dst_has_decidable_fact(dst_filter, dst, &group.label_name) {
                    dst_injections.push(label_filter(dst, group));
                }
            }
            if dst_injections.is_empty() {
                return none();
            }
            SiteDecision {
                hydrate: group_property_names_all(policies),
                replacement: None,
                filters: Vec::new(),
                dst_injections,
            }
        }
        // Variable-length quantifiers bind group lists; shortest paths bind path
        // records; conditional scans carry parameter-driven fallback shapes; unlabeled
        // scans and edge endpoints expose no decidable label fact to attach to. These
        // defer their policy treatment (module docs, [ADR 0075] §7 lineage).
        _ => none(),
    }
}

/// Adds `names` to whichever hydration lists this binding site carries.
fn hydrate_site(op: &mut PlanOp, names: &[String]) {
    fn extend(list: &mut Option<Rc<[Str]>>, names: &[String]) {
        if let Some(existing) = list {
            let mut updated = existing.to_vec();
            for name in names {
                let name: Str = Str::from(name.clone());
                if !updated.contains(&name) {
                    updated.push(name);
                }
            }
            *list = Some(updated.into());
        }
    }
    match op {
        PlanOp::NodeScan {
            property_projection,
            ..
        }
        | PlanOp::IndexScan {
            property_projection,
            ..
        }
        | PlanOp::ConditionalIndexScan {
            property_projection,
            ..
        } => extend(property_projection, names),
        PlanOp::Expand {
            dst_property_projection,
            ..
        }
        | PlanOp::ExpandFilter {
            dst_property_projection,
            ..
        } => extend(dst_property_projection, names),
        PlanOp::EdgeBindEndpoints {
            near_property_projection,
            far_property_projection,
            ..
        } => {
            extend(near_property_projection, names);
            extend(far_property_projection, names);
        }
        _ => {}
    }
}

/// The node-projection list carried by a scan operator, for preservation across
/// anchor replacement.
fn site_projection(op: &PlanOp) -> Option<Rc<[Str]>> {
    match op {
        PlanOp::NodeScan {
            property_projection,
            ..
        } => property_projection.clone(),
        _ => None,
    }
}

/// Recurses into the nested sub-plan operands of composite operators. `USE GRAPH`
/// segments are skipped: another graph's catalogs own its names, and this slice scopes
/// policies to the root graph.
fn recurse_nested_subplans(
    op: &mut PlanOp,
    policies: &ResolvedPolicies,
    ctx: &LoweringContext<'_>,
) {
    match op {
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            lower_ops_slice(ctx, left, policies, false);
            lower_ops_slice(ctx, right, policies, false);
        }
        PlanOp::SetOperation { right, .. } => lower_ops_slice(ctx, &mut right.ops, policies, false),
        PlanOp::OptionalMatch { sub_plan } => lower_ops_slice(ctx, sub_plan, policies, false),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            lower_ops_slice(ctx, &mut sub_plan.ops, policies, false)
        }
        _ => {}
    }
}

fn find_group<'a>(
    policies: &'a ResolvedPolicies,
    label_ref: &NodeLabelRef,
) -> Option<&'a ResolvedPolicyGroup> {
    policies
        .groups
        .iter()
        .find(|group| group.label_name == **label_ref)
}

fn group_property_names(group: &ResolvedPolicyGroup) -> Vec<String> {
    group
        .rows
        .iter()
        .flat_map(|row| {
            row.comparisons
                .iter()
                .map(|comparison| comparison.property_name.clone())
        })
        .collect()
}

fn group_property_names_all(policies: &ResolvedPolicies) -> Vec<String> {
    policies
        .groups
        .iter()
        .flat_map(group_property_names)
        .collect()
}

/// Equality comparisons whose property has an active vertex physical index — the seed
/// candidate set of the [ADR 0075] §5 pushdown path. Seeding applies only when exactly
/// one row covers the label (a union of rows cannot share one lookup value); the
/// returned residual holds that row's remaining comparisons.
fn indexed_equality(
    ctx: &LoweringContext<'_>,
    group: &ResolvedPolicyGroup,
) -> Option<(ResolvedComparison, Vec<ResolvedComparison>)> {
    let [row] = group.rows.as_slice() else {
        return None;
    };
    let Ok(label_u16) = u16::try_from(group.label_id) else {
        return None;
    };
    use crate::facade::stable::indexed_catalog::active_vertex_physical_index;
    let seed_at = row.comparisons.iter().position(|comparison| {
        comparison.op == PredicateOp::Eq
            && active_vertex_physical_index(
                ctx.graph,
                Some(VertexLabelId::from_raw(label_u16)),
                PropertyId::from_raw(comparison.property_id),
            )
            .is_ok()
    })?;
    let mut residual = row.comparisons.clone();
    let seed = residual.remove(seed_at);
    Some((seed, residual))
}

/// One label's complete policy predicate over a bound variable: the union (OR) of every
/// applicable row's AND-conjunction. A single row lowers to its plain conjunction.
fn label_filter(variable: &str, group: &ResolvedPolicyGroup) -> Expr {
    group
        .rows
        .iter()
        .map(|row| {
            row.comparisons
                .iter()
                .map(|comparison| comparison_expr(variable, comparison))
                .reduce(|left, right| Expr::new(ExprKind::And(Box::new(left), Box::new(right))))
                .expect("policy rows hold at least one comparison")
        })
        .reduce(|left, right| Expr::new(ExprKind::Or(Box::new(left), Box::new(right))))
        .expect("policy groups hold at least one row")
}

fn conjunction_filters(variable: &str, comparisons: &[ResolvedComparison]) -> Vec<Expr> {
    comparisons
        .iter()
        .map(|comparison| comparison_expr(variable, comparison))
        .collect()
}

/// Builds one `<variable>.<property> <op> <literal>` comparison expression.
/// Whether `dst_filter` contains a decidable positive `IsLabeled(dst, label)` fact —
/// the planner's proof that the destination variable binds this policy label.
fn dst_has_decidable_fact(dst_filter: &[Expr], dst: &str, label_name: &str) -> bool {
    dst_filter
        .iter()
        .any(|expr| expr_has_positive_is_labeled(expr, dst, label_name))
}

fn expr_has_positive_is_labeled(expr: &Expr, dst: &str, label_name: &str) -> bool {
    match &expr.kind {
        ExprKind::IsLabeled {
            expr: base,
            label,
            negated,
        } => {
            !*negated
                && base_kind_is_variable(base, dst)
                && matches!(label, LabelExpr::Name(name) if name.as_str() == label_name)
        }
        ExprKind::And(left, right) => {
            expr_has_positive_is_labeled(left, dst, label_name)
                || expr_has_positive_is_labeled(right, dst, label_name)
        }
        _ => false,
    }
}

fn base_kind_is_variable(base: &Expr, name: &str) -> bool {
    matches!(&base.kind, ExprKind::Variable(v) if v == name)
}

fn comparison_expr(variable: &str, comparison: &ResolvedComparison) -> Expr {
    Expr::new(ExprKind::Compare {
        left: Box::new(Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(variable.to_owned()))),
            property: comparison.property_name.clone(),
        })),
        op: cmp_op(comparison.op),
        right: Box::new(Expr::new(ExprKind::Literal(comparison.value.clone()))),
    })
}

fn cmp_op(op: PredicateOp) -> CmpOp {
    match op {
        PredicateOp::Eq => CmpOp::Eq,
        PredicateOp::Ne => CmpOp::Ne,
        PredicateOp::Lt => CmpOp::Lt,
        PredicateOp::Le => CmpOp::Le,
        PredicateOp::Gt => CmpOp::Gt,
        PredicateOp::Ge => CmpOp::Ge,
    }
}

// ──── Per-caller lowered prepared plans (heap cache) ────
//
// Prepared base plans stay policy-free (registration-time artifacts; their static
// requirement sets are unaffected — [ADR 0075] §5). The lowered shape is derived per
// execution and cached under the exact resolved-constant identity, so plans are never
// reused across callers with different resolutions. Any grant write invalidates the
// whole cache, making grants/revocations immediately effective.

/// Cache key: the exact fingerprint of one execution's lowering inputs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LoweredPlanKey(Vec<u8>);

thread_local! {
    static LOWERED_PREPARED_CACHE:
        RefCell<BTreeMap<LoweredPlanKey, CachedLoweredPlan>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// One derived lowered plan plus its dispatch blob (mirrors the base prepared cache's
/// payload so a hit serves both seed resolution and shard dispatch).
#[derive(Clone)]
pub(crate) struct CachedLoweredPlan {
    pub(crate) plan: PhysicalPlan,
    pub(crate) plan_blob: Vec<u8>,
}

use std::cell::RefCell;

/// Removes every cached lowered prepared plan. Called from the grant write facade.
pub(crate) fn invalidate_lowered_plan_cache() {
    LOWERED_PREPARED_CACHE.with_borrow_mut(|cache| cache.clear());
}

/// Looks up a lowered prepared plan by exact fingerprint.
pub(crate) fn cached_lowered_plan(fingerprint: &[u8]) -> Option<CachedLoweredPlan> {
    LOWERED_PREPARED_CACHE
        .with_borrow(|cache| cache.get(&LoweredPlanKey(fingerprint.to_vec())).cloned())
}

/// Inserts a lowered prepared plan, evicting the smallest key past the capacity bound.
pub(crate) fn insert_lowered_plan(fingerprint: &[u8], plan: PhysicalPlan, plan_blob: Vec<u8>) {
    LOWERED_PREPARED_CACHE.with_borrow_mut(|cache| {
        if cache.len() >= LOWERED_PREPARED_CACHE_LIMIT {
            let first = cache.keys().next().cloned();
            if let Some(key) = first {
                cache.remove(&key);
            }
        }
        cache.insert(
            LoweredPlanKey(fingerprint.to_vec()),
            CachedLoweredPlan { plan, plan_blob },
        );
    });
}

#[cfg(test)]
mod tests {
    //! Unit matrix for [ADR 0075] §5 lowering: union-over-rows / AND-within-row
    //! composition, per-caller constant resolution with fingerprint-sensitive caching,
    //! index-seed eligibility, and the wrong-implementation probe (a stub that ignores
    //! stored predicates fails these assertions).

    use super::*;
    use gleaph_auth::{CompiledPredicate, PredicateComparison};
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    fn comparison(property: u32, op: PredicateOp, value: PredicateValue) -> PredicateComparison {
        PredicateComparison {
            property,
            op,
            value,
        }
    }

    /// Registered graph with interned vocabulary; returns `(graph_id, label_id,
    /// [visibility, owner, stars] property ids)`. Stable-backed state is thread-local,
    /// so every test thread starts from empty state.
    fn fixture() -> (GraphId, u32, Vec<u32>) {
        let admin = principal(u8::MAX);
        crate::facade::auth::grant_admins(&[admin]);
        let store = RouterStore::new();
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner: principal(9),
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: true,
                },
                "policy-unit",
            )
            .expect("fixture graph");
        let graph =
            crate::facade::stable::graph_catalog::lookup_graph_id("policy-unit").expect("graph id");
        let label = store
            .admin_intern_vertex_label(admin, "policy-unit", "Post")
            .expect("intern Post");
        let props = store
            .admin_intern_properties(
                admin,
                "policy-unit",
                &[
                    "visibility".to_owned(),
                    "owner".to_owned(),
                    "stars".to_owned(),
                ],
            )
            .expect("intern properties");
        (
            graph,
            u32::from(label.raw()),
            props.iter().map(|p| p.raw()).collect(),
        )
    }

    fn grant_conditional(
        subject: GrantSubject,
        graph: u32,
        label_id: u32,
        conjuncts: Vec<PredicateComparison>,
        expires_at_ns: Option<u64>,
    ) {
        let predicate = Rc::new(CompiledPredicate {
            label: label_id,
            conjuncts,
        });
        crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant(
                    subject,
                    &Privilege::Graph(gleaph_auth::GraphPrivilege {
                        graph,
                        operation: gleaph_auth::GraphOperation::Read,
                        resource: gleaph_auth::GraphResource::VertexLabel(1),
                    }),
                    expires_at_ns,
                    Some(predicate),
                )
                .expect("non-anonymous grant fixture");
        });
    }

    fn clear_grants(graph: GraphId) {
        invalidate_lowered_plan_cache();
        crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants.revoke_all_for_graph(graph.raw());
        });
    }

    #[test]
    fn rows_compose_union_across_and_within_each_row() {
        let (graph, label, props) = fixture();
        // Two applicable rows for the caller (own + PUBLIC), each AND-only inside:
        // the caller's visible set is the OR of both conjunctions.
        grant_conditional(
            GrantSubject::Principal(principal(1)),
            graph.raw(),
            label,
            vec![
                comparison(
                    props[0],
                    PredicateOp::Eq,
                    PredicateValue::Literal(PredicateLiteral::String("public".into())),
                ),
                comparison(props[1], PredicateOp::Eq, PredicateValue::MsgCaller),
            ],
            None,
        );
        grant_conditional(
            GrantSubject::Public,
            graph.raw(),
            label,
            vec![comparison(
                props[2],
                PredicateOp::Ge,
                PredicateValue::Literal(PredicateLiteral::Int(3)),
            )],
            None,
        );

        let resolved =
            effective_policies(&RouterStore::new(), &principal(1), graph).expect("resolves");
        assert_eq!(resolved.groups.len(), 1);
        // Canonical key order: PUBLIC (kind 0) sorts before the principal row.
        assert_eq!(resolved.groups[0].rows.len(), 2, "both rows apply");
        assert_eq!(resolved.groups[0].rows[0].comparisons.len(), 1);
        assert_eq!(resolved.groups[0].rows[1].comparisons.len(), 2);

        // A stranger holds only the PUBLIC row.
        let stranger =
            effective_policies(&RouterStore::new(), &principal(2), graph).expect("resolves");
        assert_eq!(stranger.groups[0].rows.len(), 1);
        assert_eq!(stranger.groups[0].rows[0].comparisons.len(), 1);
        clear_grants(graph);
    }

    #[test]
    fn msg_caller_resolves_per_caller_and_fingerprints_never_alias() {
        let (graph, label, props) = fixture();
        grant_conditional(
            GrantSubject::Public,
            graph.raw(),
            label,
            vec![comparison(
                props[0],
                PredicateOp::Eq,
                PredicateValue::MsgCaller,
            )],
            None,
        );
        let alice = principal(3);
        let bob = principal(4);
        let a = effective_policies(&RouterStore::new(), &alice, graph).expect("alice resolves");
        let b = effective_policies(&RouterStore::new(), &bob, graph).expect("bob resolves");

        // Constants differ per invoking caller ([ADR 0075] §5).
        let constant_a = &a.groups[0].rows[0].comparisons[0].value;
        let constant_b = &b.groups[0].rows[0].comparisons[0].value;
        assert!(matches!(constant_a, gleaph_gql::Value::Extension(_)));
        assert!(matches!(constant_b, gleaph_gql::Value::Extension(_)));
        assert_ne!(constant_a, constant_b);

        assert_ne!(a.fingerprint, b.fingerprint, "callers never share a key");
        assert_eq!(
            a.fingerprint,
            effective_policies(&RouterStore::new(), &alice, graph)
                .expect("again")
                .fingerprint,
            "one caller re-resolves identically"
        );

        // Cache sensitivity: an entry stored under Alice's identity is unreachable
        // through Bob's — plans cannot be reused across different resolutions.
        insert_lowered_plan(&a.fingerprint, PhysicalPlan::default(), Vec::new());
        assert!(cached_lowered_plan(&a.fingerprint).is_some());
        assert!(cached_lowered_plan(&b.fingerprint).is_none());
        clear_grants(graph);
    }

    #[test]
    fn lowering_inserts_filters_after_matching_scans_and_is_noop_without_policies() {
        let mut policies_state = ResolvedPolicies {
            fingerprint: Vec::new(),
            groups: Vec::new(),
        };
        policies_state.groups.push(ResolvedPolicyGroup {
            label_id: 9,
            label_name: "Post".to_owned(),
            rows: vec![ResolvedConjunction {
                subject_tag: 0,
                comparisons: vec![ResolvedComparison {
                    property_id: 30,
                    property_name: "visibility".to_owned(),
                    op: PredicateOp::Eq,
                    value: gleaph_gql::Value::Text("public".to_owned()),
                }],
            }],
        });

        // Wrong-implementation probe: a stub evaluator ignoring stored predicates would
        // leave the plan untouched and fail this assertion.
        let store = RouterStore::new();
        let ctx = LoweringContext::new(&store, GraphId::from_raw(7));
        let mut plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: "p".into(),
            label: Some(NodeLabelRef::from("Post")),
            property_projection: None,
        }]);
        lower_into_plan(&ctx, &mut plan, &policies_state);
        assert_eq!(plan.ops.len(), 2, "filter inserted after the scan");
        assert!(matches!(
            plan.ops[1],
            PlanOp::PropertyFilter { stage: 0, .. }
        ));

        // No policies → plan unchanged.
        let empty = ResolvedPolicies {
            fingerprint: Vec::new(),
            groups: Vec::new(),
        };
        let mut untouched = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: "p".into(),
            label: Some(NodeLabelRef::from("Post")),
            property_projection: None,
        }]);
        lower_into_plan(&ctx, &mut untouched, &empty);
        assert_eq!(untouched.ops.len(), 1);
    }

    #[test]
    fn expansion_destination_injects_into_planner_label_fact() {
        let _ = fixture();
        // ExpandFilter destinations carry the planner's `IsLabeled(p, Post)` fact; the
        // policy conjuncts must inject into that existing filter (whose label name is
        // wire-registered) instead of a separate op.
        let mut parsed = crate::prepared::plan_prepared_query(
            "MATCH (u:User)-[:POSTED]->(p:Post) RETURN p.tag AS tag",
            principal(9),
        )
        .map(|(plan, _, _)| plan)
        .unwrap_or_else(|e| panic!("plan failed: {e:?}"));
        assert!(
            parsed
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::ExpandFilter { .. })),
            "expected an ExpandFilter binding p"
        );

        let policies = ResolvedPolicies {
            fingerprint: Vec::new(),
            groups: vec![ResolvedPolicyGroup {
                label_id: 9,
                label_name: "Post".to_owned(),
                rows: vec![ResolvedConjunction {
                    subject_tag: 0,
                    comparisons: vec![ResolvedComparison {
                        property_id: 30,
                        property_name: "visibility".to_owned(),
                        op: PredicateOp::Eq,
                        value: gleaph_gql::Value::Text("public".to_owned()),
                    }],
                }],
            }],
        };
        let store = RouterStore::new();
        let ctx = LoweringContext::new(&store, GraphId::from_raw(7));
        lower_into_plan(&ctx, &mut parsed, &policies);

        // Every comparison conjunct lands inside the planner's dst_filter — no
        // standalone PropertyFilter is added.
        let mut saw_visibility_in_dst_filter = false;
        for op in &parsed.ops {
            if let PlanOp::ExpandFilter { dst_filter, .. } = op {
                saw_visibility_in_dst_filter = dst_filter
                    .iter()
                    .any(|expr| format!("{expr:?}").contains("visibility"));
            }
        }
        assert!(
            saw_visibility_in_dst_filter,
            "policy conjunct must join the planner label-fact filter"
        );
        assert_eq!(parsed.ops.len(), 3, "no extra ops inserted");
    }

    #[test]
    fn expired_rows_contribute_nothing_to_the_visible_set() {
        let (graph, label, props) = fixture();
        grant_conditional(
            GrantSubject::Public,
            graph.raw(),
            label,
            vec![comparison(
                props[0],
                PredicateOp::Eq,
                PredicateValue::MsgCaller,
            )],
            Some(1), // expired relative to the fixed test clock (now_ns == 0)
        );
        // Sanity: with now_ns pinned to 0, an unexpired row applies.
        let live = effective_policies(&RouterStore::new(), &principal(5), graph).expect("resolves");
        assert_eq!(live.groups.len(), 1, "expiry in the future still applies");

        // Advance past expiry: the row confers nothing.
        crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant(
                    GrantSubject::Public,
                    &Privilege::Graph(gleaph_auth::GraphPrivilege {
                        graph: graph.raw(),
                        operation: gleaph_auth::GraphOperation::Read,
                        resource: gleaph_auth::GraphResource::VertexLabel(1),
                    }),
                    None,
                    None,
                )
                .expect("replace with unconditional row");
        });
        let resolved =
            effective_policies(&RouterStore::new(), &principal(5), graph).expect("resolves");
        assert!(
            resolved.is_empty(),
            "replacing the conditional row leaves no policy"
        );
        clear_grants(graph);
    }
}
