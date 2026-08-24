//! Deterministic, resumable k-way merge over sorted posting runs.
//!
//! Merging K strictly increasing runs produces one strictly increasing union with
//! duplicate docids deduplicated (keep-first wins — equal docids are indistinguishable,
//! so run order never affects output). The merge state is driven by a [`MergeCursor`]
//! capturing per-run consumed counts plus the last emitted docid; the cursor serializes
//! to bytes and restores exactly, so a merge can be suspended after any
//! [`MergeState::merge_step`] and resumed with byte-identical results.
//!
//! Determinism: the only ordering key is the docid itself; ties advance every cursor at
//! the minimum in one step. No hashing, no floats. Restore cost is proportional to the
//! skipped prefix of each run (positions are replayed through the readers) — acceptable
//! at PoC scale and honest about the stable-store future, where positions become stored
//! cursors instead of replays.

use crate::enc::PostingReader;

/// Resumable position of a k-way merge: consumed count per run and the last emitted id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCursor {
    /// Consumed postings per run, indexed by run.
    pub positions: Vec<u32>,
    /// Last emitted docid; `None` before the first emission.
    pub last_emitted: Option<u32>,
}

impl MergeCursor {
    /// Serializes to little-endian bytes: `k`, then `k` positions, then a presence tag
    /// and the last emitted id when present.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.positions.len() * 4 + 5);
        out.extend_from_slice(&(self.positions.len() as u32).to_le_bytes());
        for pos in &self.positions {
            out.extend_from_slice(&pos.to_le_bytes());
        }
        match self.last_emitted {
            None => out.push(0),
            Some(id) => {
                out.push(1);
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
        out
    }

    /// Restores a cursor produced by [`MergeCursor::to_bytes`].
    ///
    /// # Panics
    /// Panics on truncated or malformed input with a `corrupt merge cursor:` message;
    /// length mismatches are rejected like any other skew.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() < 4 {
            panic!("corrupt merge cursor: header truncated");
        }
        let k = u32::from_le_bytes(bytes[..4].try_into().expect("fixed-size header")) as usize;
        let tag_pos = 4 + k * 4;
        let total_len = match bytes.get(tag_pos).copied() {
            Some(0) => tag_pos + 1,
            Some(1) => tag_pos + 5,
            _ => panic!("corrupt merge cursor: unknown presence tag"),
        };
        if bytes.len() != total_len {
            panic!("corrupt merge cursor: length mismatch");
        }
        let mut positions = Vec::with_capacity(k);
        for i in 0..k {
            let start = 4 + i * 4;
            positions.push(u32::from_le_bytes(
                bytes[start..start + 4]
                    .try_into()
                    .expect("fixed-size field"),
            ));
        }
        let last_emitted = match bytes[tag_pos] {
            0 => None,
            1 => Some(u32::from_le_bytes(
                bytes[tag_pos + 1..tag_pos + 5]
                    .try_into()
                    .expect("fixed-size field"),
            )),
            _ => unreachable!("tag validated above"),
        };
        Self {
            positions,
            last_emitted,
        }
    }
}

/// K-way merge state over homogeneous reader runs; restore replays each run's consumed
/// prefix so merging continues exactly where the saved cursor stopped.
pub struct MergeState<R: PostingReader> {
    runs: Vec<R>,
    last_emitted: Option<u32>,
}

impl<R: PostingReader> MergeState<R> {
    /// Starts a fresh merge over `runs`.
    pub fn new(runs: Vec<R>) -> Self {
        Self {
            runs,
            last_emitted: None,
        }
    }

    /// Restores a merge from rewound runs plus a saved cursor.
    ///
    /// # Panics
    /// Panics when the cursor's run count differs or a stored position exceeds a run's
    /// length (`corrupt merge cursor:` family).
    pub fn restore(mut runs: Vec<R>, cursor: &MergeCursor) -> Self {
        assert_eq!(
            runs.len(),
            cursor.positions.len(),
            "corrupt merge cursor: run count mismatch"
        );
        for (run, &consumed) in runs.iter_mut().zip(&cursor.positions) {
            for _ in 0..consumed {
                assert!(
                    run.next().is_some(),
                    "corrupt merge cursor: position beyond run"
                );
            }
        }
        Self {
            runs,
            last_emitted: cursor.last_emitted,
        }
    }

    /// Emits up to `out_budget` deduplicated union docids into `out`, appending in
    /// ascending order. Returns how many were emitted; zero means fully exhausted.
    pub fn merge_step(&mut self, out_budget: usize, out: &mut Vec<u32>) -> usize {
        let mut emitted = 0usize;
        while emitted < out_budget {
            let min = self
                .runs
                .iter_mut()
                .fold(None::<u32>, |acc, run| match (acc, run.peek()) {
                    (_, None) => acc,
                    (None, Some(v)) => Some(v),
                    (Some(m), Some(v)) => Some(m.min(v)),
                });
            let Some(min) = min else { break };
            for run in &mut self.runs {
                if run.peek() == Some(min) {
                    run.next();
                }
            }
            if self.last_emitted != Some(min) {
                out.push(min);
                emitted += 1;
            }
            self.last_emitted = Some(min);
        }
        emitted
    }

    /// True once every run is exhausted (sticky).
    pub fn is_done(&mut self) -> bool {
        self.runs.iter_mut().all(|run| run.peek().is_none())
    }

    /// Snapshot of the resumable position.
    pub fn cursor(&self) -> MergeCursor {
        MergeCursor {
            positions: self.runs.iter().map(|run| run.pos()).collect(),
            last_emitted: self.last_emitted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::{
        AnyPostingReader, EfReader, ForReader, PefReader, PlainReader, VarintReader,
        encode_elias_fano, encode_frame_of_reference, encode_partitioned_ef, encode_varint,
    };

    const BUDGETS: [usize; 3] = [1, 3, 2];

    /// The K=7 mixed-encoding run set: varint, FOR, EF, PEF, plus three plain runs
    /// (one typically empty) so heterogeneous and exhausted runs share one merge.
    fn mixed_runs<'a>(
        buffers: &'a Buffers,
        plain_a: &'a [u32],
        plain_b: &'a [u32],
        plain_c: &'a [u32],
    ) -> Vec<AnyPostingReader<'a>> {
        vec![
            AnyPostingReader::Varint(VarintReader::new(&buffers.varint)),
            AnyPostingReader::For(ForReader::new(&buffers.r#for)),
            AnyPostingReader::Ef(EfReader::new(&buffers.ef)),
            AnyPostingReader::Pef(PefReader::new(&buffers.pef)),
            AnyPostingReader::Plain(PlainReader::new(plain_a)),
            AnyPostingReader::Plain(PlainReader::new(plain_b)),
            AnyPostingReader::Plain(PlainReader::new(plain_c)),
        ]
    }

    #[derive(Default)]
    struct Buffers {
        varint: Vec<u8>,
        r#for: Vec<u8>,
        ef: Vec<u8>,
        pef: Vec<u8>,
    }

    fn encode_buffers(
        varint_docs: &[u32],
        for_docs: &[u32],
        ef_docs: &[u32],
        pef_docs: &[u32],
    ) -> Buffers {
        Buffers {
            varint: encode_varint(varint_docs),
            r#for: encode_frame_of_reference(for_docs),
            ef: encode_elias_fano(ef_docs),
            pef: encode_partitioned_ef(pef_docs),
        }
    }

    fn merge_all(runs: Vec<AnyPostingReader<'_>>) -> Vec<u32> {
        let mut state = MergeState::new(runs);
        let mut out = Vec::new();
        while state.merge_step(usize::MAX, &mut out) > 0 {}
        out
    }

    #[test]
    fn k7_mixed_runs_one_shot_equals_stepped_resume_equivalence() {
        let varint_docs = vec![2u32, 3, 10, 11, 12, 500, 700_000];
        let for_docs = vec![1u32, 3, 4, 11, 900_000];
        let ef_docs = vec![
            5u32, 6, 7, 8, 9, 10, 11, 12, 13, 700_000, 900_001, 4_000_000,
        ];
        let pef_docs = vec![12u32, 500, 501, 502, 2_000_000];
        let plain_a = vec![3u32, 10, 501, 700_001];
        let plain_b = vec![1u32, 2, 13, 900_002];
        // Empty plain run exercises exhausted-run handling inside the big merge too.
        let plain_empty: [u32; 0] = [];
        let buffers = encode_buffers(&varint_docs, &for_docs, &ef_docs, &pef_docs);

        let one_shot = merge_all(mixed_runs(&buffers, &plain_a, &plain_b, &plain_empty));
        let oracle = {
            let mut all: Vec<u32> = Vec::new();
            all.extend(&varint_docs);
            all.extend(&for_docs);
            all.extend(&ef_docs);
            all.extend(&pef_docs);
            all.extend(&plain_a);
            all.extend(&plain_b);
            all.sort_unstable();
            all.dedup();
            all
        };
        assert_eq!(one_shot, oracle, "one-shot merge equals sorted dedup union");

        // Stepped: tiny alternating budgets, saving/restoring the cursor through its
        // byte form between every step.
        let mut stepped: Vec<u32> = Vec::new();
        let fresh = MergeState::new(mixed_runs(&buffers, &plain_a, &plain_b, &plain_empty));
        let mut cursor_bytes = fresh.cursor().to_bytes();
        loop {
            let cursor = MergeCursor::from_bytes(&cursor_bytes);
            let mut state = MergeState::restore(
                mixed_runs(&buffers, &plain_a, &plain_b, &plain_empty),
                &cursor,
            );
            let budget = BUDGETS[stepped.len() % BUDGETS.len()];
            state.merge_step(budget, &mut stepped);
            let done = state.is_done();
            cursor_bytes = state.cursor().to_bytes();
            if done {
                break;
            }
        }
        assert_eq!(stepped, one_shot, "resume equivalence across save/restore");
        let final_cursor = MergeCursor::from_bytes(&cursor_bytes);
        assert_eq!(final_cursor.last_emitted, one_shot.last().copied());
        assert_eq!(
            final_cursor.positions,
            vec![
                varint_docs.len() as u32,
                for_docs.len() as u32,
                ef_docs.len() as u32,
                pef_docs.len() as u32,
                plain_a.len() as u32,
                plain_b.len() as u32,
                plain_empty.len() as u32,
            ]
        );
    }

    #[test]
    fn duplicate_suppression_yields_the_strictly_increasing_union() {
        let buffers = encode_buffers(&[7u32, 135, 300], &[100u32], &[299u32], &[7u32, 300]);
        let plain = [42u32];
        let mut state = MergeState::new(mixed_runs(&buffers, &plain, &[], &[]));
        let mut out = Vec::new();
        while state.merge_step(3, &mut out) > 0 {}
        assert!(
            out.windows(2).all(|w| w[0] < w[1]),
            "output must be strictly increasing"
        );
        let expected: Vec<u32> = vec![7, 42, 100, 135, 299, 300];
        assert_eq!(out, expected);
    }

    #[test]
    fn only_empty_and_exhausted_runs_merge_to_nothing() {
        let mut state = MergeState::new(Vec::<AnyPostingReader<'_>>::new());
        let mut out = Vec::new();
        assert_eq!(state.merge_step(5, &mut out), 0);
        assert!(state.is_done());

        let plain_empty: [u32; 0] = [];
        let buffers = encode_buffers(&[9u32], &[9u32], &[9u32], &[9u32]);
        let mut state = MergeState::new(mixed_runs(&buffers, &plain_empty, &[], &[]));
        while state.merge_step(2, &mut out) > 0 {}
        assert_eq!(out, vec![9]);
        assert!(state.is_done());
    }

    #[test]
    fn cursor_round_trip_preserves_positions_and_last_emitted() {
        let cursor = MergeCursor {
            positions: vec![0, 3, u32::MAX],
            last_emitted: Some(u32::MAX - 1),
        };
        assert_eq!(MergeCursor::from_bytes(&cursor.to_bytes()), cursor);
        let none = MergeCursor {
            positions: vec![],
            last_emitted: None,
        };
        assert_eq!(MergeCursor::from_bytes(&none.to_bytes()), none);
    }

    #[test]
    #[should_panic(expected = "corrupt merge cursor")]
    fn truncated_cursor_panics() {
        MergeCursor::from_bytes(&[3, 0, 0]);
    }

    #[test]
    #[should_panic(expected = "corrupt merge cursor")]
    fn unknown_presence_tag_panics() {
        // k=0 cursor: 4-byte count, tag byte 9, then the 4 bytes the Some branch would
        // read — full legal length so the panic comes from the tag check itself.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(9);
        bytes.extend_from_slice(&[0u8; 4]);
        MergeCursor::from_bytes(&bytes);
    }

    #[test]
    #[should_panic(expected = "corrupt merge cursor")]
    fn restore_rejects_run_count_mismatch() {
        let cursor = MergeCursor {
            positions: vec![0, 0],
            last_emitted: None,
        };
        MergeState::restore(Vec::<AnyPostingReader<'static>>::new(), &cursor);
    }
}
