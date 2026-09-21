use super::*;
use crate::{traits::{CsrEdge, CsrEdgeTombstone, CsrVertex}, VectorMemory};
use std::ops::ControlFlow;

#[derive(Clone, Debug, PartialEq)]
struct Edge<const N: usize>(u32);
impl<const N: usize> CsrEdge for Edge<N> {
    const BYTES: usize = N;
    fn read_from(b: &[u8]) -> Self { Self(u32::from_le_bytes(b[..4].try_into().unwrap())) }
    fn write_to(&self, b: &mut [u8]) { b.fill(0); b[..4].copy_from_slice(&self.0.to_le_bytes()); }
    fn neighbor_vid(&self) -> VertexId { VertexId::from(self.0) }
    fn with_neighbor_vid(&self, v: VertexId) -> Self { Self(u32::from(v)) }
}
impl<const N: usize> CsrEdgeTombstone for Edge<N> {
    fn tombstone_edge() -> Self { Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL)) }
}
fn retained<const N: usize>(used: u32) -> LabeledLaraGraph<Edge<N>, VectorMemory> {
    let m = crate::test_support::vector_memory;
    let g = LabeledLaraGraph::new(m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),m(),crate::labeled::InitialCapacities::uniform(256),BucketLabelKey::from_raw(1)).unwrap();
    let v = g.push_vertex(crate::labeled::record::LabeledVertex::default()).unwrap();
    for i in 0..67 { g.insert_edge_skip_leaf_cascade(v,BucketLabelKey::from_raw(2),Edge(i),super::super::EdgePlacementPolicy::Insertion).unwrap(); }
    eprintln!("FIXTURE seeded {:?} free={:?}",bucket(&g,BucketLabelKey::from_raw(2)),g.edges().free_byte_spans());
    g.enable_default_edge_bypass(v).unwrap();
    eprintln!("FIXTURE bypass {:?} free={:?}",g.vertices().get(v),g.edges().free_byte_spans());
    let prefix = g.edges().initialized_slab_slots(&g.vertices().get(v)).unwrap();
    assert!(prefix > 0);
    for i in 67..prefix+used { g.insert_edge(v,g.default_label(),Edge(i),super::super::EdgePlacementPolicy::Insertion).unwrap(); }
    eprintln!("FIXTURE appended {:?} free={:?}",g.vertices().get(v),g.edges().free_byte_spans());
    g.promote_bypass_to_bucket_mode(v).unwrap();
    eprintln!("FIXTURE converted {:?} free={:?}",bucket(&g,g.default_label()),g.edges().free_byte_spans());
    g
}
fn bucket<E: CsrEdgeTombstone>(g: &LabeledLaraGraph<E, VectorMemory>, label: BucketLabelKey) -> LabelBucket {
    match g.find_bucket(VertexId::from(0),&g.vertices().get(VertexId::from(0)),label).unwrap() { BucketSearch::Found{bucket,..}=>bucket,_=>panic!("missing bucket") }
}
fn rows<E: CsrEdgeTombstone>(g: &LabeledLaraGraph<E, VectorMemory>, label: BucketLabelKey, order: super::super::OutEdgeOrder) -> Vec<(u32,u32,Vec<u8>)> {
    let mut out=Vec::new();
    let flow=g.visit_edges_with_inline_property(VertexId::from(0),label,order,|slot,item| { out.push((slot.raw(),u32::from(item.edge.neighbor_vid()),item.inline_property.bytes().to_vec()));ControlFlow::<()>::Continue(()) }).unwrap();
    assert_eq!(flow,ControlFlow::Continue(()));out
}
fn check_promotion(deletes: u32) {
    let g=retained::<4>(crate::labeled::spill_run_store::MAX_ROWS);let v=VertexId::from(0);let label=g.default_label();
    for i in 0..deletes {assert_eq!(g.remove_edge_at_slot(v,label,i).unwrap().unwrap().0,i);}
    let old=bucket(&g,label); let run=g.edges().spill_mint(old.edge_spill_run()).unwrap();
    let extent=old.stored_slots()+g.edges().spill_used_len(run);let count=g.edges().header().num_edges;
    let before=rows(&g,label,super::super::OutEdgeOrder::Ascending);
    assert_eq!(before.iter().map(|r|(r.0,r.1)).collect::<Vec<_>>(),(deletes..extent).map(|i|(i,i)).collect::<Vec<_>>());
    promote_bucket_to_tree_mode(&g,v,label).unwrap();
    let after=bucket(&g,label);assert!(after.is_tree_mode());assert_eq!(after.stored_slots(),extent);assert_eq!(after.degree(),old.degree());assert_eq!(g.edges().header().num_edges,count);
    assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Ascending),before);
    assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Descending),before.into_iter().rev().collect::<Vec<_>>());
}
#[test] fn promotion_retained_no_holes(){check_promotion(0);}
#[test] fn promotion_retained_eight_holes(){check_promotion(8);}
#[test] fn promotion_retained_sixteen_holes(){check_promotion(16);}
#[test] fn promotion_retained_below_cap_rejects_without_mutation(){
    let g=retained::<4>(128);let label=g.default_label();let old=bucket(&g,label);let run=g.edges().spill_mint(old.edge_spill_run()).unwrap();let used=g.edges().spill_used_len(run);let before=rows(&g,label,super::super::OutEdgeOrder::Ascending);let count=g.edges().header().num_edges;
    let result=promote_bucket_to_tree_mode(&g,VertexId::from(0),label);
    assert!(matches!(result,Err(LabeledOperationError::AllocSpaceCapReached{current_alloc_space,cap:T_PROMOTE,mode:BucketMode::Slab}) if current_alloc_space==old.stored_slots()+used),"{result:?}");
    assert_eq!(bucket(&g,label),old);assert_eq!(g.edges().spill_used_len(run),used);assert_eq!(g.edges().header().num_edges,count);assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Ascending),before);
}
#[test] fn promotion_retained_wide_rejects_without_mutation(){
    let g=retained::<10>(crate::labeled::spill_run_store::MAX_ROWS);let label=g.default_label();let old=bucket(&g,label);let run=g.edges().spill_mint(old.edge_spill_run()).unwrap();let used=g.edges().spill_used_len(run);let before=rows(&g,label,super::super::OutEdgeOrder::Ascending);
    assert!(matches!(promote_bucket_to_tree_mode(&g,VertexId::from(0),label),Err(LabeledOperationError::TreeModeEdgeWidthUnsupported{actual:10,expected:4})));
    assert_eq!(bucket(&g,label),old);assert_eq!(g.edges().spill_used_len(run),used);assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Ascending),before);
}
#[test] fn promotion_retained_sidecars_keep_slot_identity(){
    use super::super::test_support::{inline_property_test_graph,InlinePropertyTestEdge};
    for (width, holes) in [(8,false),(8,true),(33,false),(33,true)] {
        let g=inline_property_test_graph();let v=g.push_vertex(crate::labeled::record::LabeledVertex::default()).unwrap();let label=BucketLabelKey::from_raw(2);
        g.ensure_label_bucket_inline_property_byte_width(v,label,width).unwrap();
        for i in 0..T_PROMOTE {let value:Vec<u8>=(0..width).map(|j|(i.wrapping_mul(17)+u32::from(j)) as u8).collect();g.insert_edge_skip_leaf_cascade(v,label,InlinePropertyTestEdge::with_bytes(i,&value),super::super::EdgePlacementPolicy::Insertion).unwrap();}
        assert_ne!(bucket(&g,label).values_spill_run(),crate::traits::NO_SPILL_RUN);
        if holes { for i in [0,3,31,32,511,1023] {g.remove_edge_at_slot(v,label,i).unwrap().unwrap();} }
        let old=bucket(&g,label);assert!(!old.is_tree_mode());assert_ne!(old.edge_spill_run(),crate::traits::NO_SPILL_RUN);if !holes { assert_ne!(old.values_spill_run(),crate::traits::NO_SPILL_RUN); }
        assert_ne!(old.stored_slots(),old.inline_property_bytes_slab_slots(),"independent source prefixes");
        let before=rows(&g,label,super::super::OutEdgeOrder::Ascending);
        for (slot,target,value) in &before {assert_eq!(slot,target);assert_eq!(*value,(0..width).map(|j|(target.wrapping_mul(17)+u32::from(j)) as u8).collect::<Vec<_>>());}
        promote_bucket_to_tree_mode(&g,v,label).unwrap();let after=bucket(&g,label);assert!(after.is_tree_mode());assert_eq!(after.degree(),old.degree());assert_eq!(after.stored_slots(),T_PROMOTE);
        assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Ascending),before);assert_eq!(rows(&g,label,super::super::OutEdgeOrder::Descending),before.into_iter().rev().collect::<Vec<_>>());
    }
}
