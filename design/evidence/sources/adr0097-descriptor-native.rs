use super::*;
use crate::traits::{CsrEdge,CsrEdgeTombstone,CsrVertex};
use ic_stable_structures::Memory;
use std::{cell::RefCell,rc::Rc,hint::black_box,ops::ControlFlow};
/// Labeled edge with inline property bytes for inline-property-log / tombstone benches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InlinePropertyBenchEdge {
    target: u32,
    slot_index: u32,
    inline_property: [u8; 8],
    inline_property_len: u16,
}

impl InlinePropertyBenchEdge {
    fn with_inline_property(target: u32, inline_property_len: u16, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("bench inline property fits u16");
        debug_assert_eq!(len, inline_property_len);
        let mut inline_property = [0u8; 8];
        inline_property[..bytes.len()].copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            inline_property,
            inline_property_len,
        }
    }
}

impl CsrEdge for InlinePropertyBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            inline_property: [0u8; 8],
            inline_property_len: 0,
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
            ..self.clone()
        }
    }

    fn with_slot_index(self, slot_index: u32) -> Self {
        Self { slot_index, ..self }
    }

    fn edge_slot_index_raw(&self) -> u32 {
        self.slot_index
    }

    fn edge_inline_property_byte_width(&self) -> u16 {
        self.inline_property_len
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.inline_property[..usize::from(self.inline_property_len)]
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.inline_property = [0u8; 8];
        self.inline_property[..len].copy_from_slice(&bytes[..len]);
        self.inline_property_len =
            u16::try_from(len).expect("bench inline property byte width fits u16");
        self
    }
}

impl CsrEdgeTombstone for InlinePropertyBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            inline_property: [0u8; 8],
            inline_property_len: 0,
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
fn diagnostic_descriptor_pressure() {
 for n in [16,64,256] {
 let counts=Rc::new(RefCell::new([[0u64;6];16]));
 let m:Vec<_>=(0..16).map(|id|CountMemory { inner:Rc::new(RefCell::new(Vec::new())),id,counts:counts.clone() }).collect();
 let graph=LabeledLaraGraph::<InlinePropertyBenchEdge,CountMemory>::new(m[0].clone(),m[1].clone(),m[2].clone(),m[3].clone(),m[4].clone(),m[5].clone(),m[6].clone(),m[7].clone(),m[8].clone(),m[9].clone(),m[10].clone(),m[11].clone(),m[12].clone(),m[13].clone(),m[14].clone(),m[15].clone(),crate::labeled::InitialCapacities::uniform(1<<20),BucketLabelKey::from_raw(1)).unwrap();
 eprintln!("DESCRIPTOR constructor n={n} rwg={:?}", counts.borrow());*counts.borrow_mut()=[[0;6];16];
 let vid=graph.push_vertex(LabeledVertex::default()).unwrap();
 for index in 0..n*2 {
  let label=BucketLabelKey::directed_from_index(u16::try_from(100+index).unwrap());
  graph.ensure_label_bucket_inline_property_byte_width(vid,label,2).unwrap();
  graph.insert_edge_skip_leaf_cascade(vid,label,InlinePropertyBenchEdge::with_inline_property(index,2,&[index as u8;2]),EdgePlacementPolicy::Insertion).unwrap();
  if index%2==0 {graph.remove_edge_matching(vid,label,|edge|edge.target==index).unwrap().unwrap();}
 }
 eprintln!("DESCRIPTOR seed n={n} rwg={:?}", counts.borrow());*counts.borrow_mut()=[[0;6];16];
 for _ in 0..64 {black_box(graph.inline_property_bytes_compaction_needed(3).unwrap());}black_box(graph.vertices().get(vid));
 eprintln!("DESCRIPTOR pressure n={n} rwg={:?}", counts.borrow());
 assert_eq!(graph.edges().header().num_edges,u64::from(n));
 for index in 0..n*2 {
  let label=BucketLabelKey::directed_from_index(u16::try_from(100+index).unwrap());let mut rows=Vec::new();
  assert_eq!(graph.visit_edges_with_inline_property(vid,label,OutEdgeOrder::Ascending,|slot,item|{rows.push((slot.raw(),item.edge.target,item.inline_property.bytes().to_vec()));ControlFlow::<()>::Continue(())}).unwrap(),ControlFlow::Continue(()));
  assert_eq!(rows,if index%2==0 {vec![]} else {vec![(0,index,vec![index as u8;2])]});
 }
 }
}
