use super::*;
use crate::traits::{CsrEdge,CsrEdgeTombstone,CsrVertex};
use ic_stable_structures::Memory;
use std::{cell::RefCell,rc::Rc,hint::black_box,ops::ControlFlow};
thread_local! { static DECODES: RefCell<[u64;2]> = const { RefCell::new([0;2]) }; }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchEdge(u32);

impl CsrEdge for BenchEdge {
    const BYTES: usize = 10;

    fn read_from(bytes: &[u8]) -> Self {
        { let n=u32::from_le_bytes(bytes[0..4].try_into().unwrap()); DECODES.with(|c| { let mut x=c.borrow_mut(); x[usize::from(n == u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))]+=1; }); Self(n) }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        bytes[4..10].fill(0);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}

impl CsrEdgeTombstone for BenchEdge {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}


#[derive(Clone)]
struct CountMemory { inner: crate::VectorMemory, id: usize, counts: Rc<RefCell<[[u64;2];16]>>, histogram: Rc<RefCell<std::collections::BTreeMap<(usize,usize),u64>>> }
impl CountMemory {
 fn record(&self,_offset:u64,len:usize) {
 let mut c=self.counts.borrow_mut(); c[self.id][0]+=1;c[self.id][1]+=len as u64;
 *self.histogram.borrow_mut().entry((self.id,len)).or_default()+=1;
 }
}
impl Memory for CountMemory {
 fn size(&self)->u64 {self.inner.size()}
 fn grow(&self,p:u64)->i64 {self.inner.grow(p)}
 fn read(&self,o:u64,b:&mut[u8]) {self.record(o,b.len());self.inner.read(o,b)}
 unsafe fn read_unsafe(&self,o:u64,b:*mut u8,n:usize) {self.record(o,n);unsafe{self.inner.read_unsafe(o,b,n)}}
 fn write(&self,o:u64,b:&[u8]) {self.inner.write(o,b)}
}
fn reset(m:&CountMemory){*m.counts.borrow_mut()=[[0;2];16]; m.histogram.borrow_mut().clear(); DECODES.with(|c|*c.borrow_mut()=[0;2]);}
fn report(m:&CountMemory,phase:&str,visits:u64){eprintln!("PROBE {phase} reads_bytes={:?} decodes_live_dead={:?} predicate_or_callback_visits={visits}",m.counts.borrow(),DECODES.with(|c|*c.borrow())); eprintln!("HIST {phase} {:?}",m.histogram.borrow());}

#[test]
fn diagnostic_mixed_hub() {
 let counts=Rc::new(RefCell::new([[0;2];16]));let histogram=Rc::new(RefCell::new(std::collections::BTreeMap::new()));
 let m:Vec<_>=(0..16).map(|id|CountMemory {inner:Rc::new(RefCell::new(Vec::new())),id,counts:counts.clone(),histogram:histogram.clone()}).collect();
 let graph=LabeledLaraGraph::<BenchEdge,CountMemory>::new(
m[0].clone(),m[1].clone(),m[2].clone(),m[3].clone(),m[4].clone(),m[5].clone(),m[6].clone(),m[7].clone(),m[8].clone(),m[9].clone(),m[10].clone(),m[11].clone(),m[12].clone(),m[13].clone(),m[14].clone(),m[15].clone(),crate::labeled::InitialCapacities::uniform(1<<20),BucketLabelKey::from_raw(1)).unwrap();
 let hub=graph.push_vertex(LabeledVertex::default()).unwrap();let dst=graph.push_vertex(LabeledVertex::default()).unwrap();
 for label_idx in 0..33u16 {let label=BucketLabelKey::from_raw(10000+label_idx);for edge_i in 0..50 {graph.insert_edge_skip_leaf_cascade(hub,label,BenchEdge(edge_i),EdgePlacementPolicy::Insertion).unwrap();}}
 black_box(dst);
 let vertex=graph.vertices().get(hub);
 let mut prefixes=Vec::new();
 for offset in 0..vertex.degree() {let slot=vertex.base_slot_start()+u64::from(offset);let bucket=graph.buckets().read_label_bucket_slot(slot).unwrap();prefixes.push(bucket.stored_slots_raw());}
 eprintln!("MIXED prefix={prefixes:?}");
 reset(&m[0]);let edges=graph.out_edges(hub).unwrap();black_box(edges.len());report(&m[0],"mixed-asc",edges.len() as u64);
 assert_eq!(edges.iter().map(|e|e.0).collect::<Vec<_>>(),(0..33).flat_map(|_|0..50).collect::<Vec<_>>());
 reset(&m[0]);let mut count=0usize;let mut targets=Vec::new();
 for offset in 0..vertex.degree() {let slot=vertex.base_slot_start().saturating_add(u64::from(offset));let bucket=graph.buckets().read_label_bucket_slot(slot).unwrap();let label=bucket.bucket_label_key();graph.visit_edges(hub,label,OutEdgeOrder::Descending,|_slot,edge|{targets.push(edge.0);count+=1;ControlFlow::<()>::Continue(())}).map(|_|()).unwrap();}
 black_box(count);report(&m[0],"mixed-desc",count as u64);assert_eq!(count,1650);assert_eq!(targets,(0..33).flat_map(|_|(0..50).rev()).collect::<Vec<_>>());
}
