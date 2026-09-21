use super::*;
use crate::traits::{CsrEdge,CsrEdgeTombstone,CsrVertex};
use ic_stable_structures::Memory;
use std::{cell::RefCell,rc::Rc,ops::ControlFlow};
const INLINE_VALUE_WIDTH:u16=8;
#[derive(Clone, Debug, PartialEq, Eq)]
struct InlinePropertyBenchEdge {
    target: u32,
    inline_property: [u8; 8],
}

impl InlinePropertyBenchEdge {
    fn new(target: u32, value: u64) -> Self {
        Self {
            target,
            inline_property: value.to_le_bytes(),
        }
    }
}

impl CsrEdge for InlinePropertyBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            inline_property: [0u8; 8],
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.target)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            target: u32::from(vid),
            inline_property: self.inline_property,
        }
    }

    fn edge_inline_property_byte_width(&self) -> u16 {
        INLINE_VALUE_WIDTH
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.inline_property
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.inline_property = [0u8; 8];
        self.inline_property[..len].copy_from_slice(&bytes[..len]);
        self
    }
}

impl CsrEdgeTombstone for InlinePropertyBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            inline_property: [0u8; 8],
        }
    }
}
#[derive(Clone)]
struct CountMemory {inner:crate::VectorMemory,id:usize,counts:Rc<RefCell<[[u64;6];16]>>}
impl CountMemory {fn record(&self,kind:usize,n:u64){let mut c=self.counts.borrow_mut();c[self.id][kind]+=1;c[self.id][kind+1]+=n;}}
impl Memory for CountMemory {
 fn size(&self)->u64{self.inner.size()}
 fn grow(&self,p:u64)->i64{self.record(4,p);self.inner.grow(p)}
 fn read(&self,o:u64,b:&mut[u8]){self.record(0,b.len() as u64);self.inner.read(o,b)}
 unsafe fn read_unsafe(&self,o:u64,b:*mut u8,n:usize){self.record(0,n as u64);unsafe{self.inner.read_unsafe(o,b,n)}}
 fn write(&self,o:u64,b:&[u8]){self.record(2,b.len() as u64);self.inner.write(o,b)}
}

#[test]
fn diagnostic_budget_property() {
 let counts=Rc::new(RefCell::new([[0u64;6];16]));
 let m:Vec<_>=(0..16).map(|id|CountMemory {inner:Rc::new(RefCell::new(Vec::new())),id,counts:counts.clone()}).collect();
 let graph=LabeledLaraGraph::<InlinePropertyBenchEdge,CountMemory>::new(m[0].clone(),m[1].clone(),m[2].clone(),m[3].clone(),m[4].clone(),m[5].clone(),m[6].clone(),m[7].clone(),m[8].clone(),m[9].clone(),m[10].clone(),m[11].clone(),m[12].clone(),m[13].clone(),m[14].clone(),m[15].clone(),crate::labeled::InitialCapacities::uniform(1<<16),BucketLabelKey::from_raw(1)).unwrap();
 let src=graph.push_vertex(LabeledVertex::default()).unwrap();let label=BucketLabelKey::from_raw(2);
 graph.ensure_label_bucket_inline_property_byte_width(src,label,8).unwrap();
 for i in 0..256 {graph.insert_edge_skip_leaf_cascade(src,label,InlinePropertyBenchEdge::new(i+10,u64::from(i)),EdgePlacementPolicy::Insertion).unwrap();}
 let vertex=graph.vertices().get(src);let (_,bucket)=match graph.find_bucket(src,&vertex,label).unwrap(){BucketSearch::Found{slot,bucket}=>(slot,bucket),_=>panic!()};
 eprintln!("PROPERTY shape prefix={} edge_run_used={} value_prefix={} value_run_used={}",bucket.stored_slots(),graph.edges().spill_used_len(graph.edges().spill_mint(bucket.edge_spill_run()).unwrap()),bucket.inline_property_bytes_slab_slots(),if bucket.values_spill_run()==crate::traits::NO_SPILL_RUN {0} else {graph.values().spill_used_len(graph.values().spill_mint(bucket.values_spill_run()).unwrap())});
 *counts.borrow_mut()=[[0;6];16];let mut scratch=crate::labeled::LabeledInlinePropertyValueBatchScratch::default();let mut selected=Vec::new();let mut values=Vec::new();let mut batches=0;
 graph.visit_out_inline_property_batches_for_label(src,label,OutEdgeOrder::Ascending,&mut scratch,|batch|{batches+=1;selected.extend(batch.slot_indices.iter().copied().map(crate::traverse::BucketEntryPosition::new));values.extend_from_slice(batch.values);}).unwrap();
 eprintln!("PROPERTY phase1 rwg={:?} batches={batches}",counts.borrow());
 assert_eq!(values,(0..256u64).flat_map(u64::to_le_bytes).collect::<Vec<_>>());assert_eq!(selected.iter().map(|s|s.raw()).collect::<Vec<_>>(),(0..256).collect::<Vec<_>>());
 *counts.borrow_mut()=[[0;6];16];let mut targets=Vec::new();
 assert_eq!(graph.visit_edges_at_with_replay(src,label,&selected,OutEdgeOrder::Ascending,Some(&scratch.hybrid_overflow_replay),|slot,edge|{targets.push((slot.raw(),u32::from(edge.neighbor_vid())));ControlFlow::<()>::Continue(())}).unwrap(),ControlFlow::Continue(()));
 eprintln!("PROPERTY phase2 rwg={:?} visits={}",counts.borrow(),targets.len());assert_eq!(targets,(0..256).map(|s|(s,s+10)).collect::<Vec<_>>());
}
