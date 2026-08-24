//! Upstream `queries/block_posting_metadata.rs` trimmed for the extraction: the
//! epserde persistence (`Epserde` derive, `serialize`/`deserialize_eps`,
//! `load_file`/`create_file`), `log::info!` progress lines, and the DS2I-binary
//! construction path (readers/indicatif/config dependencies) are stripped. What remains
//! is the in-memory metadata layout and accessors the WAND-family operators consume.

use std::marker::PhantomData;

use super::scorers::DocScorer;

/// Upstream struct verbatim minus the `#[derive(Epserde)]`.
///
/// In an internal crate this would be constructed from our own block-max tables instead
/// of deserialized from a `.mdata` file; the field layout is kept identical so the
/// operator code stays byte-for-byte upstream.
pub struct BlockPostingMetadata<Scorer: DocScorer> {
    processed_postings: usize,
    norms_len: Box<[f32]>,
    max_term_weight: Box<[f32]>,
    blocks_start: Box<[usize]>,
    blocks_docid: Box<[u32]>,
    blocks_max_term_weight: Box<[f32]>,
    _phantom: PhantomData<Scorer>,
}

impl<Scorer: DocScorer> BlockPostingMetadata<Scorer> {
    /// Extraction-only constructor replacing the upstream epserde file loaders: builds
    /// the same layout from caller-owned slices (test fixtures / our own stores).
    pub fn from_parts(
        norms_len: Box<[f32]>,
        max_term_weight: Box<[f32]>,
        blocks_start: Box<[usize]>,
        blocks_docid: Box<[u32]>,
        blocks_max_term_weight: Box<[f32]>,
    ) -> Self {
        Self {
            processed_postings: 0,
            norms_len,
            max_term_weight,
            blocks_start,
            blocks_docid,
            blocks_max_term_weight,
            _phantom: PhantomData,
        }
    }

    pub fn get_norm_len(&self, i: usize) -> f32 {
        unsafe { *self.norms_len.get_unchecked(i) }
    }

    pub fn get_max_term_weight(&self, i: usize) -> f32 {
        unsafe { *self.max_term_weight.get_unchecked(i) }
    }

    pub fn get_block_posting_metadata_iterator(
        &'_ self,
        i: usize,
    ) -> BlockPostingMDataEnumerator<'_, Scorer> {
        let block_start = self.blocks_start[i];
        let block_number = self.blocks_start[i + 1] - self.blocks_start[i];

        BlockPostingMDataEnumerator {
            current_pos: 0,
            block_number,
            block_max_term_weight: &self.blocks_max_term_weight
                [self.blocks_start[i]..self.blocks_start[i + 1]],
            block_docid: &self.blocks_docid[self.blocks_start[i]..self.blocks_start[i + 1]],
            phantom: PhantomData,
        }
    }
}

pub struct BlockPostingMDataEnumerator<'a, Scorer: DocScorer> {
    current_pos: usize,
    block_number: usize,
    block_max_term_weight: &'a [f32],
    block_docid: &'a [u32],
    phantom: PhantomData<Scorer>,
}

impl<'a, Scorer: DocScorer> BlockPostingMDataEnumerator<'a, Scorer> {
    pub fn block_next_geq(&mut self, lower_bound: u64) {
        while self.current_pos + 1 < self.block_number
            && (self.block_docid[self.current_pos] as usize) < lower_bound as usize
        {
            self.current_pos += 1;
        }
    }

    pub fn block_max_score(&self) -> f32 {
        self.block_max_term_weight[self.current_pos]
    }

    pub fn block_docid(&self) -> u64 {
        self.block_docid[self.current_pos] as u64
    }
}
