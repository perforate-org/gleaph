//! Measurement-only candidate size models for Plan 0158.
//!
//! These functions do not define a production wire format. They conservatively include a small
//! per-sequence header and restart metadata so candidate comparisons do not pretend that payload
//! bits are free.

#![cfg_attr(not(test), allow(dead_code))]

const SEQUENCE_HEADER_BYTES: u64 = 8;
const SHARED_MAGIC: [u8; 2] = *b"SO";
const SHARED_VERSION: u8 = 1;
const SAMPLED_MAGIC: [u8; 2] = *b"SR";
const SAMPLED_VERSION: u8 = 1;

/// Derive counterpart-slot sequences grouped by owner/orientation from the same physical fixture
/// used by the alias/rank gate. The ordering is canonical occurrence order; rows are never sorted
/// by mate slot, preserving rank semantics.
pub(crate) fn mate_slot_sequences(
    identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    undirected: bool,
) -> Result<Vec<Vec<u32>>, String> {
    let mut groups = std::collections::BTreeMap::<(u32, u32, u8), Vec<u32>>::new();
    for identity in identities {
        let orientation = if undirected { 0 } else { identity.orientation };
        groups
            .entry((identity.owner, identity.target, orientation))
            .or_default()
            .push(identity.slot);
    }
    for slots in groups.values_mut() {
        slots.sort_unstable();
    }
    let mut output = std::collections::BTreeMap::<(u32, u8), Vec<(u32, u32)>>::new();
    for identity in identities {
        let orientation = if undirected { 0 } else { identity.orientation };
        let counterpart_orientation = if undirected { 0 } else { 1 - orientation };
        let source = groups
            .get(&(identity.owner, identity.target, orientation))
            .ok_or_else(|| "compression source group missing".to_owned())?;
        let rank = source
            .binary_search(&identity.slot)
            .map_err(|_| "compression source slot missing".to_owned())?;
        let counterpart = groups
            .get(&(identity.target, identity.owner, counterpart_orientation))
            .and_then(|slots| slots.get(rank))
            .copied()
            .ok_or_else(|| "compression counterpart missing".to_owned())?;
        output
            .entry((identity.owner, orientation))
            .or_default()
            .push((identity.slot, counterpart));
    }
    Ok(output
        .into_values()
        .map(|mut rows| {
            rows.sort_unstable_by_key(|(source, _)| *source);
            rows.into_iter().map(|(_, mate)| mate).collect()
        })
        .collect())
}

/// Conservative logical size estimate for a directed shared-orientation map.
///
/// One unordered endpoint pair owns both slot streams and a single directory entry. This is a
/// measurement model only: it does not define a production locator or permit lookup without the
/// canonical bucket identity and rank validation.
pub(crate) fn shared_orientation_bytes(
    identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    undirected: bool,
) -> Result<u64, String> {
    if undirected {
        return Err("shared orientation requires directed identities".to_owned());
    }
    let mut groups = std::collections::BTreeMap::<(u32, u32), Vec<u32>>::new();
    for identity in identities {
        groups
            .entry((identity.owner, identity.target))
            .or_default()
            .push(identity.slot);
    }
    for slots in groups.values_mut() {
        slots.sort_unstable();
    }
    let mut bytes = SEQUENCE_HEADER_BYTES;
    for (&(owner, target), forward) in &groups {
        if owner > target {
            continue;
        }
        let reverse = groups
            .get(&(target, owner))
            .ok_or_else(|| "shared counterpart group missing".to_owned())?;
        if forward.len() != reverse.len() {
            return Err("shared counterpart cardinality mismatch".to_owned());
        }
        let maximum = forward
            .iter()
            .chain(reverse)
            .copied()
            .max()
            .ok_or_else(|| "shared group is empty".to_owned())?;
        let width: u64 = if maximum <= 0xff {
            1
        } else if maximum <= 0xffff {
            2
        } else if maximum <= 0x00ff_ffff {
            3
        } else {
            4
        };
        // Pair directory: endpoints plus width/count metadata. Both slot streams are retained.
        bytes = bytes
            .checked_add(12)
            .and_then(|value| {
                value.checked_add(u64::try_from(forward.len()).ok()?.checked_mul(2 * width)?)
            })
            .ok_or_else(|| "shared orientation size overflow".to_owned())?;
    }
    Ok(bytes)
}

/// Measurement-only lookup handle for the shared-orientation model.
#[derive(Clone, Debug)]
pub(crate) struct SharedOrientationLookup {
    groups: std::collections::BTreeMap<(u32, u32), Vec<u32>>,
}

/// Measurement-only orientation-free rank mirror for undirected non-self edges.
#[derive(Clone, Debug)]
pub(crate) struct UndirectedPairRankLookup {
    groups: std::collections::BTreeMap<(u32, u32), Vec<u32>>,
}

/// Measurement-only fallback for a pair whose two physical buckets do not preserve the same rank
/// order. The input is logical edge order; both directions are retained only for mismatched pairs.
#[derive(Clone, Debug)]
pub(crate) struct UndirectedPairRankExceptionLookup {
    groups: std::collections::BTreeMap<(u32, u32), Vec<u32>>,
}

#[derive(Clone, Debug)]
enum BlockRankMode {
    Identity,
    Permutation {
        forward: Vec<u32>,
        reverse: Vec<u32>,
        width: u8,
    },
    Raw {
        forward: Vec<u32>,
        reverse: Vec<u32>,
    },
}

#[derive(Clone, Debug)]
struct BlockRankSegment {
    start_rank: u32,
    count: u32,
    mode: BlockRankMode,
}

/// Measurement-only block-local permutation fallback for one unordered endpoint pair.
#[derive(Clone, Debug)]
pub(crate) struct UndirectedBlockRankPermutationLookup {
    owner: u32,
    target: u32,
    block_size: u32,
    source_slots: Vec<u32>,
    target_slots: Vec<u32>,
    blocks: Vec<BlockRankSegment>,
}

impl UndirectedBlockRankPermutationLookup {
    pub(crate) fn from_ordered_pairs(
        owner: u32,
        target: u32,
        pairs: &[(u32, u32)],
        block_size: u32,
    ) -> Result<Self, String> {
        if owner >= target || pairs.is_empty() || !(1..=256).contains(&block_size) {
            return Err("invalid undirected block permutation input".to_owned());
        }
        let mut source = pairs
            .iter()
            .enumerate()
            .map(|(logical, &(slot, _))| (logical, slot))
            .collect::<Vec<_>>();
        let mut target_rows = pairs
            .iter()
            .enumerate()
            .map(|(logical, &(_, slot))| (logical, slot))
            .collect::<Vec<_>>();
        for slots in [&mut source, &mut target_rows] {
            slots.sort_unstable_by_key(|(_, slot)| *slot);
            if slots.windows(2).any(|pair| pair[0].1 == pair[1].1) {
                return Err("duplicate undirected block slot".to_owned());
            }
        }
        let mut source_rank_by_logical = vec![0u32; pairs.len()];
        let mut target_rank_by_logical = vec![0u32; pairs.len()];
        for (rank, (logical, _)) in source.iter().enumerate() {
            source_rank_by_logical[*logical] =
                u32::try_from(rank).map_err(|_| "source rank overflow".to_owned())?;
        }
        for (rank, (logical, _)) in target_rows.iter().enumerate() {
            target_rank_by_logical[*logical] =
                u32::try_from(rank).map_err(|_| "target rank overflow".to_owned())?;
        }
        let mut permutation = vec![0u32; pairs.len()];
        let mut inverse = vec![0u32; pairs.len()];
        for logical in 0..pairs.len() {
            let source_rank = usize::try_from(source_rank_by_logical[logical])
                .map_err(|_| "source rank conversion failed".to_owned())?;
            let target_rank = target_rank_by_logical[logical];
            permutation[source_rank] = target_rank;
            inverse[usize::try_from(target_rank)
                .map_err(|_| "target rank conversion failed".to_owned())?] =
                u32::try_from(source_rank).map_err(|_| "reverse rank overflow".to_owned())?;
        }
        let mut blocks = Vec::new();
        for (block_index, chunk) in permutation
            .chunks(usize::try_from(block_size).expect("block size"))
            .enumerate()
        {
            let block_start = block_index * usize::try_from(block_size).expect("block size");
            let mode = if chunk.iter().enumerate().all(|(rank, value)| {
                *value == u32::try_from(block_start + rank).expect("block rank")
            }) {
                BlockRankMode::Identity
            } else {
                let width = if chunk.len() <= usize::from(u8::MAX) {
                    1
                } else {
                    2
                };
                if width <= 2 {
                    BlockRankMode::Permutation {
                        forward: chunk.to_vec(),
                        reverse: inverse[block_index
                            * usize::try_from(block_size).expect("block size")
                            ..block_index * usize::try_from(block_size).expect("block size")
                                + chunk.len()]
                            .to_vec(),
                        width,
                    }
                } else {
                    let raw_forward = chunk
                        .iter()
                        .map(|target_rank| {
                            target_rows[usize::try_from(*target_rank).expect("target rank")].1
                        })
                        .collect();
                    let raw_reverse = inverse[block_index
                        * usize::try_from(block_size).expect("block size")
                        ..block_index * usize::try_from(block_size).expect("block size")
                            + chunk.len()]
                        .iter()
                        .map(|source_rank| {
                            source[usize::try_from(*source_rank).expect("source rank")].1
                        })
                        .collect();
                    BlockRankMode::Raw {
                        forward: raw_forward,
                        reverse: raw_reverse,
                    }
                }
            };
            let start_rank = u32::try_from(block_index)
                .ok()
                .and_then(|index| index.checked_mul(block_size))
                .ok_or_else(|| "block start rank overflow".to_owned())?;
            blocks.push(BlockRankSegment {
                start_rank,
                count: u32::try_from(chunk.len()).map_err(|_| "block count overflow".to_owned())?,
                mode,
            });
        }
        Ok(Self {
            owner,
            target,
            block_size,
            source_slots: source.into_iter().map(|(_, slot)| slot).collect(),
            target_slots: target_rows.into_iter().map(|(_, slot)| slot).collect(),
            blocks,
        })
    }

    pub(crate) fn lookup(&self, owner: u32, target: u32, rank: u32) -> Option<u32> {
        if !((owner == self.owner && target == self.target)
            || (owner == self.target && target == self.owner))
        {
            return None;
        }
        let block_index = usize::try_from(rank / self.block_size).ok()?;
        let block = self.blocks.get(block_index)?;
        let local_rank = usize::try_from(rank % self.block_size).ok()?;
        if local_rank >= usize::try_from(block.count).ok()? {
            return None;
        }
        let (target_slots, reverse) = if owner == self.owner {
            (&self.target_slots, false)
        } else {
            (&self.source_slots, true)
        };
        let global_rank = usize::try_from(block.start_rank)
            .ok()?
            .checked_add(local_rank)?;
        match &block.mode {
            BlockRankMode::Identity => target_slots.get(global_rank).copied(),
            BlockRankMode::Permutation {
                forward,
                reverse: inverse,
                ..
            } => {
                let mapped = if reverse { inverse } else { forward };
                target_slots
                    .get(usize::try_from(*mapped.get(local_rank)?).ok()?)
                    .copied()
            }
            BlockRankMode::Raw {
                forward,
                reverse: inverse,
            } => {
                let mapped = if reverse { inverse } else { forward };
                mapped.get(local_rank).copied()
            }
        }
    }

    pub(crate) fn logical_bytes(&self) -> Option<u64> {
        let mut bytes = 20u64;
        for block in &self.blocks {
            bytes = bytes.checked_add(4)?;
            let count = u64::from(block.count);
            let payload = match &block.mode {
                BlockRankMode::Identity => 0,
                BlockRankMode::Permutation { width, .. } => count.checked_mul(u64::from(*width))?,
                BlockRankMode::Raw { .. } => count.checked_mul(8)?,
            };
            bytes = bytes.checked_add(payload)?;
        }
        Some(bytes)
    }

    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl UndirectedPairRankExceptionLookup {
    pub(crate) fn from_ordered_pairs(
        owner: u32,
        target: u32,
        pairs: &[(u32, u32)],
    ) -> Result<Self, String> {
        if owner >= target || pairs.is_empty() {
            return Err("invalid undirected exception pair".to_owned());
        }
        let mut left = Vec::with_capacity(pairs.len());
        let mut right = Vec::with_capacity(pairs.len());
        for &(left_slot, right_slot) in pairs {
            left.push(left_slot);
            right.push(right_slot);
        }
        for slots in [&left, &right] {
            let mut sorted = slots.to_vec();
            sorted.sort_unstable();
            if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("duplicate undirected exception slot".to_owned());
            }
        }
        let mut groups = std::collections::BTreeMap::new();
        groups.insert((owner, target), left);
        groups.insert((target, owner), right);
        Ok(Self { groups })
    }

    pub(crate) fn lookup(&self, owner: u32, target: u32, rank: u32) -> Option<u32> {
        self.groups
            .get(&(owner, target))?
            .get(usize::try_from(rank).ok()?)
            .copied()
    }

    pub(crate) fn logical_bytes(&self) -> Option<u64> {
        let count = self.groups.get(self.groups.keys().next()?)?.len();
        8u64.checked_add(12)?
            .checked_add(u64::try_from(count).ok()?.checked_mul(8)?)
    }

    pub(crate) fn mismatch_count(&self) -> Option<usize> {
        let (&(owner, target), left) = self.groups.iter().next()?;
        let right = self.groups.get(&(target, owner))?;
        let mut left_rank = left.iter().copied().enumerate().collect::<Vec<_>>();
        let mut right_rank = right.iter().copied().enumerate().collect::<Vec<_>>();
        left_rank.sort_unstable_by_key(|(_, slot)| *slot);
        right_rank.sort_unstable_by_key(|(_, slot)| *slot);
        let mut left_by_logical = vec![0usize; left.len()];
        let mut right_by_logical = vec![0usize; right.len()];
        for (rank, (logical, _)) in left_rank.into_iter().enumerate() {
            left_by_logical[logical] = rank;
        }
        for (rank, (logical, _)) in right_rank.into_iter().enumerate() {
            right_by_logical[logical] = rank;
        }
        Some(
            left_by_logical
                .iter()
                .zip(right_by_logical)
                .filter(|(left_rank, right_rank)| **left_rank != *right_rank)
                .count(),
        )
    }

    pub(crate) fn within_mismatch_budget(&self, max_mismatches: usize) -> Option<bool> {
        Some(self.mismatch_count()? <= max_mismatches)
    }
}

impl UndirectedPairRankLookup {
    pub(crate) fn build(
        identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    ) -> Result<Self, String> {
        let mut groups = std::collections::BTreeMap::<(u32, u32), Vec<u32>>::new();
        for identity in identities {
            if identity.orientation != 0 {
                return Err("undirected pair rank requires orientation zero".to_owned());
            }
            groups
                .entry((identity.owner, identity.target))
                .or_default()
                .push(identity.slot);
        }
        for slots in groups.values_mut() {
            slots.sort_unstable();
            if slots.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("duplicate undirected physical slot".to_owned());
            }
        }
        for &(owner, target) in groups.keys() {
            if owner == target {
                continue;
            }
            let counterpart = groups
                .get(&(target, owner))
                .ok_or_else(|| "undirected counterpart group missing".to_owned())?;
            if groups[&(owner, target)].len() != counterpart.len() {
                return Err("undirected counterpart cardinality mismatch".to_owned());
            }
        }
        Ok(Self { groups })
    }

    pub(crate) fn logical_bytes(&self) -> Option<u64> {
        let mut bytes = 8u64;
        for &(owner, target) in self.groups.keys() {
            if owner >= target {
                continue;
            }
            bytes = bytes.checked_add(12)?;
        }
        Some(bytes)
    }

    pub(crate) fn lookup(&self, owner: u32, target: u32, rank: u32) -> Option<u32> {
        if owner == target {
            return self
                .groups
                .get(&(owner, target))?
                .get(rank as usize)
                .copied();
        }
        self.groups
            .get(&(target, owner))?
            .get(usize::try_from(rank).ok()?)
            .copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampledBlockMode {
    Residual8,
    Residual16,
    Raw,
}

#[derive(Clone, Debug)]
struct SampledBlock {
    start_rank: u32,
    mode: SampledBlockMode,
    values: Vec<i32>,
}

const SAMPLED_RAW_REVERSE_FLAG: u32 = 1 << 31;

fn sampled_mode_width(mode: SampledBlockMode) -> (u8, usize) {
    match mode {
        SampledBlockMode::Residual8 => (1, 1),
        SampledBlockMode::Residual16 => (2, 2),
        SampledBlockMode::Raw => (3, 4),
    }
}

fn encode_sampled_blocks(bytes: &mut Vec<u8>, blocks: &[SampledBlock]) -> Result<(), String> {
    for block in blocks {
        let (mode, width) = sampled_mode_width(block.mode);
        let count = u16::try_from(block.values.len())
            .map_err(|_| "sampled block size overflow".to_owned())?;
        bytes.extend_from_slice(&block.start_rank.to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        bytes.push(mode);
        bytes.push(0);
        for &value in &block.values {
            let encoded = value.to_be_bytes();
            bytes.extend_from_slice(&encoded[4 - width..]);
        }
    }
    Ok(())
}

fn decode_sampled_blocks(
    bytes: &[u8],
    cursor: &mut usize,
    block_count: usize,
) -> Result<Vec<SampledBlock>, String> {
    let mut blocks = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let block_end = (*cursor)
            .checked_add(8)
            .ok_or_else(|| "sampled block header overflow".to_owned())?;
        if block_end > bytes.len() {
            return Err("truncated sampled block header".to_owned());
        }
        let start_rank =
            u32::from_be_bytes(bytes[*cursor..*cursor + 4].try_into().expect("start rank"));
        let count =
            u16::from_be_bytes(bytes[*cursor + 4..*cursor + 6].try_into().expect("count")) as usize;
        let mode = bytes[*cursor + 6];
        let (sampled_mode, width) = match mode {
            1 => (SampledBlockMode::Residual8, 1usize),
            2 => (SampledBlockMode::Residual16, 2usize),
            3 => (SampledBlockMode::Raw, 4usize),
            _ => return Err("invalid sampled block mode".to_owned()),
        };
        *cursor = block_end;
        let payload_end = (*cursor)
            .checked_add(
                count
                    .checked_mul(width)
                    .ok_or_else(|| "sampled payload overflow".to_owned())?,
            )
            .ok_or_else(|| "sampled payload cursor overflow".to_owned())?;
        if payload_end > bytes.len() {
            return Err("truncated sampled payload".to_owned());
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let mut raw = 0u32;
            for &byte in &bytes[*cursor..*cursor + width] {
                raw = (raw << 8) | u32::from(byte);
            }
            let value = match sampled_mode {
                SampledBlockMode::Residual8 => i32::from(i8::from_be_bytes([raw as u8])),
                SampledBlockMode::Residual16 => {
                    i32::from(i16::from_be_bytes((raw as u16).to_be_bytes()))
                }
                SampledBlockMode::Raw => {
                    i32::try_from(raw).map_err(|_| "sampled raw slot overflow".to_owned())?
                }
            };
            values.push(value);
            *cursor += width;
        }
        if blocks
            .last()
            .is_some_and(|previous: &SampledBlock| previous.start_rank >= start_rank)
        {
            return Err("sampled blocks are not ordered".to_owned());
        }
        blocks.push(SampledBlock {
            start_rank,
            mode: sampled_mode,
            values,
        });
    }
    Ok(blocks)
}

/// Measurement-only checkpointed paired-residual lookup.
#[derive(Clone, Debug)]
pub(crate) struct SampledPairedResidualLookup {
    block_size: usize,
    groups: std::collections::BTreeMap<(u32, u32), Vec<SampledBlock>>,
}

fn build_sampled_blocks(
    source: &[u32],
    mate: &[u32],
    block_size: usize,
) -> Result<Vec<SampledBlock>, String> {
    let mut blocks = Vec::new();
    for start in (0..source.len()).step_by(block_size) {
        let end = (start + block_size).min(source.len());
        let source_block = &source[start..end];
        let mate_block = &mate[start..end];
        let residuals = source_block
            .iter()
            .zip(mate_block)
            .map(|(&source, &mate)| i64::from(mate) - i64::from(source))
            .collect::<Vec<_>>();
        let mode = if residuals.iter().all(|&value| i8::try_from(value).is_ok()) {
            SampledBlockMode::Residual8
        } else if residuals.iter().all(|&value| i16::try_from(value).is_ok()) {
            SampledBlockMode::Residual16
        } else {
            SampledBlockMode::Raw
        };
        let values = if mode == SampledBlockMode::Raw {
            mate_block.iter().map(|&value| value as i32).collect()
        } else {
            residuals
                .into_iter()
                .map(|value| i32::try_from(value).expect("residual width checked"))
                .collect()
        };
        blocks.push(SampledBlock {
            start_rank: u32::try_from(start).map_err(|_| "sampled rank overflow".to_owned())?,
            mode,
            values,
        });
    }
    Ok(blocks)
}

impl SampledPairedResidualLookup {
    pub(crate) fn build(
        identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
        block_size: usize,
    ) -> Result<Self, String> {
        if block_size == 0 || block_size > 256 {
            return Err("invalid sampled block size".to_owned());
        }
        let shared = SharedOrientationLookup::build(identities, false)?;
        let mut groups = std::collections::BTreeMap::new();
        for (&(owner, target), forward) in &shared.groups {
            if owner >= target {
                continue;
            }
            let reverse = shared
                .groups
                .get(&(target, owner))
                .ok_or_else(|| "sampled counterpart group missing".to_owned())?;
            let forward_blocks = build_sampled_blocks(forward, reverse, block_size)?;
            let reverse_blocks = build_sampled_blocks(reverse, forward, block_size)?;
            groups.insert((owner, target), forward_blocks);
            groups.insert((target, owner), reverse_blocks);
        }
        Ok(Self { block_size, groups })
    }

    pub(crate) fn logical_bytes(&self) -> Option<u64> {
        let mut bytes = 8u64;
        for (&(owner, target), blocks) in &self.groups {
            if owner >= target {
                continue;
            }
            bytes = bytes.checked_add(12)?;
            for block in blocks {
                let (_, width) = sampled_mode_width(block.mode);
                bytes = bytes.checked_add(8)?.checked_add(
                    u64::try_from(block.values.len())
                        .ok()?
                        .checked_mul(u64::try_from(width).ok()?)?,
                )?;
            }
            if blocks
                .iter()
                .any(|block| block.mode == SampledBlockMode::Raw)
            {
                let reverse = self.groups.get(&(target, owner))?;
                for block in reverse {
                    let (_, width) = sampled_mode_width(block.mode);
                    bytes = bytes.checked_add(8)?.checked_add(
                        u64::try_from(block.values.len())
                            .ok()?
                            .checked_mul(u64::try_from(width).ok()?)?,
                    )?;
                }
            }
        }
        Some(bytes)
    }

    /// Encode the measurement model. Raw fallback blocks carry an explicit reverse stream because
    /// absolute mate slots cannot be reconstructed by negating a residual.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        let pairs = self
            .groups
            .keys()
            .filter(|(owner, target)| owner < target)
            .copied()
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SAMPLED_MAGIC);
        bytes.push(SAMPLED_VERSION);
        bytes.push(0);
        bytes.extend_from_slice(
            &u32::try_from(pairs.len())
                .map_err(|_| "sampled pair count overflow".to_owned())?
                .to_be_bytes(),
        );
        for (owner, target) in pairs {
            let blocks = self
                .groups
                .get(&(owner, target))
                .ok_or_else(|| "sampled forward group missing".to_owned())?;
            bytes.extend_from_slice(&owner.to_be_bytes());
            bytes.extend_from_slice(&target.to_be_bytes());
            let reverse = self
                .groups
                .get(&(target, owner))
                .ok_or_else(|| "sampled reverse group missing".to_owned())?;
            let has_raw = blocks
                .iter()
                .any(|block| block.mode == SampledBlockMode::Raw);
            let block_count = u32::try_from(blocks.len())
                .map_err(|_| "sampled block count overflow".to_owned())?;
            bytes.extend_from_slice(
                &(block_count | if has_raw { SAMPLED_RAW_REVERSE_FLAG } else { 0 }).to_be_bytes(),
            );
            encode_sampled_blocks(&mut bytes, blocks)?;
            if has_raw {
                encode_sampled_blocks(&mut bytes, reverse)?;
            }
        }
        Ok(bytes)
    }

    /// Decode the sampled model. Residual-only pairs synthesize the reverse stream by negating
    /// each residual; raw-fallback pairs carry both directions explicitly. All structural errors
    /// fail closed.
    pub(crate) fn decode(bytes: &[u8], block_size: usize) -> Result<Self, String> {
        if block_size == 0 || block_size > 256 {
            return Err("invalid sampled block size".to_owned());
        }
        if bytes.len() < 8 || bytes[..2] != SAMPLED_MAGIC || bytes[2] != SAMPLED_VERSION {
            return Err("invalid sampled header".to_owned());
        }
        let pair_count = u32::from_be_bytes(bytes[4..8].try_into().expect("sampled header"));
        let mut cursor = 8usize;
        let mut groups = std::collections::BTreeMap::new();
        for _ in 0..pair_count {
            let header_end = cursor
                .checked_add(12)
                .ok_or_else(|| "sampled header overflow".to_owned())?;
            if header_end > bytes.len() {
                return Err("truncated sampled pair header".to_owned());
            }
            let owner = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().expect("owner"));
            let target =
                u32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().expect("target"));
            if owner >= target {
                return Err("sampled pair is not canonicalized".to_owned());
            }
            let encoded_block_count = u32::from_be_bytes(
                bytes[cursor + 8..cursor + 12]
                    .try_into()
                    .expect("block count"),
            );
            let has_raw_reverse = encoded_block_count & SAMPLED_RAW_REVERSE_FLAG != 0;
            let block_count = usize::try_from(encoded_block_count & !SAMPLED_RAW_REVERSE_FLAG)
                .map_err(|_| "sampled block count overflow".to_owned())?;
            cursor = header_end;
            let forward_blocks = decode_sampled_blocks(bytes, &mut cursor, block_count)?;
            /*
            for _ in 0..block_count {
                let block_end = cursor
                    .checked_add(8)
                    .ok_or_else(|| "sampled block header overflow".to_owned())?;
                if block_end > bytes.len() {
                    return Err("truncated sampled block header".to_owned());
                }
                let start_rank =
                    u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().expect("start rank"));
                let count =
                    u16::from_be_bytes(bytes[cursor + 4..cursor + 6].try_into().expect("count"))
                        as usize;
                let mode = bytes[cursor + 6];
                let (sampled_mode, width) = match mode {
                    1 => (SampledBlockMode::Residual8, 1usize),
                    2 => (SampledBlockMode::Residual16, 2),
                    _ => return Err("invalid sampled residual mode".to_owned()),
                };
                cursor = block_end;
                let payload_end = cursor
                    .checked_add(
                        count
                            .checked_mul(width)
                            .ok_or_else(|| "sampled payload overflow".to_owned())?,
                    )
                    .ok_or_else(|| "sampled payload cursor overflow".to_owned())?;
                if payload_end > bytes.len() {
                    return Err("truncated sampled payload".to_owned());
                }
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut raw = 0u32;
                    for &byte in &bytes[cursor..cursor + width] {
                        raw = (raw << 8) | u32::from(byte);
                    }
                    let value = if width == 1 {
                        i32::from(i8::from_be_bytes([raw as u8]))
                    } else {
                        i32::from(i16::from_be_bytes((raw as u16).to_be_bytes()))
                    };
                    values.push(value);
                    cursor += width;
                }
                if forward_blocks
                    .last()
                    .is_some_and(|previous: &SampledBlock| previous.start_rank >= start_rank)
                {
                    return Err("sampled blocks are not ordered".to_owned());
                }
                forward_blocks.push(SampledBlock {
                    start_rank,
                    mode: sampled_mode,
                    values,
                });
            }
            */
            if groups
                .insert((owner, target), forward_blocks.clone())
                .is_some()
            {
                return Err("duplicate sampled pair".to_owned());
            }
            let reverse_blocks = if has_raw_reverse {
                let reverse_blocks = decode_sampled_blocks(bytes, &mut cursor, block_count)?;
                if !forward_blocks
                    .iter()
                    .any(|block| block.mode == SampledBlockMode::Raw)
                    || !reverse_blocks
                        .iter()
                        .any(|block| block.mode == SampledBlockMode::Raw)
                {
                    return Err("sampled raw reverse stream flag mismatch".to_owned());
                }
                reverse_blocks
            } else {
                if forward_blocks
                    .iter()
                    .any(|block| block.mode == SampledBlockMode::Raw)
                {
                    return Err("sampled raw block missing reverse stream".to_owned());
                }
                forward_blocks
                    .iter()
                    .map(|block| SampledBlock {
                        start_rank: block.start_rank,
                        mode: block.mode,
                        values: block.values.iter().map(|value| -*value).collect(),
                    })
                    .collect()
            };
            if groups.insert((target, owner), reverse_blocks).is_some() {
                return Err("duplicate sampled reverse pair".to_owned());
            }
        }
        if cursor != bytes.len() {
            return Err("trailing sampled bytes".to_owned());
        }
        Ok(Self { block_size, groups })
    }

    pub(crate) fn lookup(
        &self,
        owner: u32,
        target: u32,
        rank: u32,
        source_slot: u32,
    ) -> Option<u32> {
        let blocks = self.groups.get(&(owner, target))?;
        let block_index = usize::try_from(rank).ok()? / self.block_size;
        let block = blocks.get(block_index)?;
        let offset = usize::try_from(rank.checked_sub(block.start_rank)?).ok()?;
        let encoded = *block.values.get(offset)?;
        match block.mode {
            SampledBlockMode::Residual8 | SampledBlockMode::Residual16 => {
                u32::try_from(i64::from(source_slot) + i64::from(encoded)).ok()
            }
            SampledBlockMode::Raw => u32::try_from(encoded).ok(),
        }
    }

    /// Measurement-only bounded local reconstruction. Unlike `lookup`, this walks from the
    /// beginning of the selected block to the requested rank, modelling the scan work that a
    /// checkpointed representation would pay when it does not retain every direct offset.
    pub(crate) fn lookup_local_scan(
        &self,
        owner: u32,
        target: u32,
        rank: u32,
        source_slots: &[u32],
    ) -> Option<u32> {
        let blocks = self.groups.get(&(owner, target))?;
        let block_index = usize::try_from(rank).ok()? / self.block_size;
        let block = blocks.get(block_index)?;
        let offset = usize::try_from(rank.checked_sub(block.start_rank)?).ok()?;
        let end = offset.checked_add(1)?.min(block.values.len());
        let start = usize::try_from(block.start_rank).ok()?;
        let source_end = start.checked_add(end)?;
        if source_end > source_slots.len() {
            return None;
        }
        let mut result = None;
        for (encoded, &source_slot) in block
            .values
            .iter()
            .zip(&source_slots[start..source_end])
            .take(end)
        {
            let encoded = std::hint::black_box(*encoded);
            let source_slot = std::hint::black_box(source_slot);
            result = match block.mode {
                SampledBlockMode::Residual8 | SampledBlockMode::Residual16 => {
                    u32::try_from(i64::from(source_slot) + i64::from(encoded)).ok()
                }
                SampledBlockMode::Raw => u32::try_from(encoded).ok(),
            };
        }
        result
    }
}

impl SharedOrientationLookup {
    pub(crate) fn build(
        identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
        undirected: bool,
    ) -> Result<Self, String> {
        if undirected {
            return Err("shared orientation requires directed identities".to_owned());
        }
        let mut groups = std::collections::BTreeMap::<(u32, u32), Vec<u32>>::new();
        for identity in identities {
            if identity.orientation > 1 {
                return Err("invalid directed orientation".to_owned());
            }
            groups
                .entry((identity.owner, identity.target))
                .or_default()
                .push(identity.slot);
        }
        for slots in groups.values_mut() {
            slots.sort_unstable();
            if slots.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("duplicate physical slot".to_owned());
            }
        }
        for &(owner, target) in groups.keys() {
            let reverse = groups
                .get(&(target, owner))
                .ok_or_else(|| "shared counterpart group missing".to_owned())?;
            if groups[&(owner, target)].len() != reverse.len() {
                return Err("shared counterpart cardinality mismatch".to_owned());
            }
        }
        Ok(Self { groups })
    }

    pub(crate) fn rank_for(&self, owner: u32, target: u32, slot: u32) -> Option<u32> {
        self.groups
            .get(&(owner, target))?
            .binary_search(&slot)
            .ok()
            .and_then(|rank| u32::try_from(rank).ok())
    }

    pub(crate) fn lookup(&self, owner: u32, target: u32, rank: u32) -> Option<u32> {
        let target_slots = self.groups.get(&(target, owner))?;
        target_slots.get(usize::try_from(rank).ok()?).copied()
    }

    /// Encode the measurement-only shared-orientation candidate.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        let pair_count = self
            .groups
            .keys()
            .filter(|(owner, target)| owner < target)
            .count();
        let pair_count = u32::try_from(pair_count).map_err(|_| "pair count overflow".to_owned())?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SHARED_MAGIC);
        bytes.push(SHARED_VERSION);
        bytes.push(0);
        bytes.extend_from_slice(&pair_count.to_be_bytes());
        for (&(owner, target), forward) in &self.groups {
            if owner > target {
                continue;
            }
            let reverse = self
                .groups
                .get(&(target, owner))
                .ok_or_else(|| "shared counterpart group missing".to_owned())?;
            let maximum = forward
                .iter()
                .chain(reverse)
                .copied()
                .max()
                .ok_or_else(|| "shared group is empty".to_owned())?;
            let width = if maximum <= 0xff {
                1u8
            } else if maximum <= 0xffff {
                2
            } else if maximum <= 0x00ff_ffff {
                3
            } else {
                4
            };
            let count = u16::try_from(forward.len())
                .map_err(|_| "shared group entry count overflow".to_owned())?;
            bytes.extend_from_slice(&owner.to_be_bytes());
            bytes.extend_from_slice(&target.to_be_bytes());
            bytes.extend_from_slice(&count.to_be_bytes());
            bytes.push(width);
            bytes.push(0);
            for slots in [forward, reverse] {
                for &slot in slots {
                    let encoded = slot.to_be_bytes();
                    bytes.extend_from_slice(&encoded[4 - usize::from(width)..]);
                }
            }
        }
        Ok(bytes)
    }

    /// Decode the measurement-only candidate and reject malformed/trailing bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 || bytes[..2] != SHARED_MAGIC || bytes[2] != SHARED_VERSION {
            return Err("invalid shared-orientation header".to_owned());
        }
        let pair_count = u32::from_be_bytes(bytes[4..8].try_into().expect("header length"));
        let mut cursor = 8usize;
        let mut groups = std::collections::BTreeMap::new();
        for _ in 0..pair_count {
            let header_end = cursor
                .checked_add(12)
                .ok_or_else(|| "shared header overflow".to_owned())?;
            if header_end > bytes.len() {
                return Err("truncated shared group header".to_owned());
            }
            let owner = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().expect("owner"));
            let target =
                u32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().expect("target"));
            if owner >= target {
                return Err("shared group is not canonicalized".to_owned());
            }
            let count =
                u16::from_be_bytes(bytes[cursor + 8..cursor + 10].try_into().expect("count"))
                    as usize;
            let width = bytes[cursor + 10];
            if !(1..=4).contains(&width) {
                return Err("invalid shared slot width".to_owned());
            }
            cursor = header_end;
            let stream_bytes = count
                .checked_mul(2)
                .and_then(|value| value.checked_mul(usize::from(width)))
                .ok_or_else(|| "shared stream size overflow".to_owned())?;
            let stream_end = cursor
                .checked_add(stream_bytes)
                .ok_or_else(|| "shared stream cursor overflow".to_owned())?;
            if stream_end > bytes.len() {
                return Err("truncated shared streams".to_owned());
            }
            let mut streams = [Vec::with_capacity(count), Vec::with_capacity(count)];
            for stream in &mut streams {
                for _ in 0..count {
                    let end = cursor + usize::from(width);
                    let mut slot = 0u32;
                    for &byte in &bytes[cursor..end] {
                        slot = (slot << 8) | u32::from(byte);
                    }
                    if stream.last().is_some_and(|previous| *previous >= slot) {
                        return Err("shared slots are not strictly increasing".to_owned());
                    }
                    stream.push(slot);
                    cursor = end;
                }
            }
            if groups.insert((owner, target), streams[0].clone()).is_some()
                || groups.insert((target, owner), streams[1].clone()).is_some()
            {
                return Err("duplicate shared endpoint pair".to_owned());
            }
        }
        if cursor != bytes.len() {
            return Err("trailing shared bytes".to_owned());
        }
        Ok(Self { groups })
    }
}

fn varint_bytes(mut value: u64) -> u64 {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

/// Restart-point signed delta model. The first value of each restart block is absolute.
pub(crate) fn delta_restart_bytes(sequence: &[u32], restart_interval: usize) -> Option<u64> {
    if sequence.is_empty() || restart_interval == 0 {
        return None;
    }
    let restart_count = sequence.len().div_ceil(restart_interval) as u64;
    let mut bytes = SEQUENCE_HEADER_BYTES.checked_add(restart_count.checked_mul(4)?)?;
    for (index, &value) in sequence.iter().enumerate() {
        let encoded = if index % restart_interval == 0 {
            u64::from(value)
        } else {
            let previous = i64::from(sequence[index - 1]);
            let delta = i64::from(value) - previous;
            ((delta << 1) ^ (delta >> 63)) as u64
        };
        bytes = bytes.checked_add(varint_bytes(encoded))?;
    }
    Some(bytes)
}

/// Reconstruct one rank from the nearest restart point.
///
/// This is a bounded-access model for the candidate, not a decoder for a production blob. It
/// intentionally accepts the canonical sequence as the source of encoded deltas so the test can
/// measure the restart window and verify exact parity without introducing a wire contract.
pub(crate) fn delta_restart_reconstruct_at(
    sequence: &[u32],
    restart_interval: usize,
    index: usize,
) -> Option<u32> {
    if sequence.is_empty() || restart_interval == 0 || index >= sequence.len() {
        return None;
    }
    let restart = index / restart_interval * restart_interval;
    let mut value = sequence[restart];
    for position in (restart + 1)..=index {
        let delta = i64::from(sequence[position]) - i64::from(sequence[position - 1]);
        value = u32::try_from(i64::from(value).checked_add(delta)?).ok()?;
    }
    Some(value)
}

/// Elias–Fano logical size model for a non-decreasing sequence.
///
/// The estimate includes the low-bit stream, unary high-bit stream, and one 32-bit sequence
/// header. Non-monotone input is rejected rather than silently sorted, because sorting would
/// change rank semantics.
pub(crate) fn monotone_elias_fano_bytes(sequence: &[u32]) -> Option<u64> {
    let &maximum = sequence.last()?;
    if sequence.windows(2).any(|pair| pair[0] > pair[1]) {
        return None;
    }
    let n = u64::try_from(sequence.len()).ok()?;
    let universe = u64::from(maximum).checked_add(1)?;
    let ratio = universe / n.max(1);
    let low_bits = if ratio <= 1 {
        0
    } else {
        63 - ratio.leading_zeros() as u64
    };
    let low_stream_bits = n.checked_mul(low_bits)?;
    let high_stream_bits = (universe >> low_bits).checked_add(n)?;
    let payload_bits = low_stream_bits.checked_add(high_stream_bits)?;
    Some(SEQUENCE_HEADER_BYTES + payload_bits.div_ceil(8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_restart_is_bounded_and_restart_interval_changes_overhead() {
        let sequence = [10, 11, 12, 20, 21, 22];
        let dense = delta_restart_bytes(&sequence, 2).expect("dense restart");
        let sparse = delta_restart_bytes(&sequence, 8).expect("sparse restart");
        assert!(dense > sparse);
        assert_eq!(delta_restart_bytes(&sequence, 0), None);
    }

    #[test]
    fn elias_fano_requires_monotone_rank_sequence() {
        assert!(monotone_elias_fano_bytes(&[1, 2, 2, 9]).is_some());
        assert_eq!(monotone_elias_fano_bytes(&[1, 4, 3]), None);
    }

    #[test]
    fn non_monotone_delta_remains_available_without_reordering_ranks() {
        let bytes = delta_restart_bytes(&[20, 3, 19, 4], 2).expect("delta");
        assert!(bytes >= SEQUENCE_HEADER_BYTES + 8);
    }

    #[test]
    fn delta_restart_reconstructs_exact_values_with_bounded_window() {
        let sequence = [20, 3, 19, 4, 100, 101, 2];
        for index in 0..sequence.len() {
            assert_eq!(
                delta_restart_reconstruct_at(&sequence, 3, index),
                Some(sequence[index])
            );
        }
        assert_eq!(
            delta_restart_reconstruct_at(&sequence, 3, sequence.len()),
            None
        );
        assert_eq!(delta_restart_reconstruct_at(&sequence, 0, 0), None);
    }

    #[test]
    fn delta_restart_reconstruct_handles_u32_boundaries() {
        let sequence = [u32::MAX, 0];
        assert_eq!(delta_restart_reconstruct_at(&sequence, 2, 1), Some(0));
        assert_eq!(
            delta_restart_reconstruct_at(&[0, u32::MAX], 2, 1),
            Some(u32::MAX)
        );
    }

    #[test]
    fn shared_orientation_requires_directed_pairs() {
        assert!(shared_orientation_bytes(&[], true).is_err());
    }

    #[test]
    fn shared_orientation_rejects_unpaired_directed_rows() {
        let identities = [ic_stable_lara::adoption_fixture::PhysicalIdentity {
            owner: 1,
            target: 2,
            orientation: 0,
            slot: 0,
        }];
        assert!(shared_orientation_bytes(&identities, false).is_err());
        assert!(SharedOrientationLookup::build(&identities, false).is_err());
    }

    #[test]
    fn shared_orientation_lookup_preserves_rank_parity() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 4,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 8,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 3,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 9,
            },
        ];
        let lookup = SharedOrientationLookup::build(&identities, false).expect("paired lookup");
        let encoded = lookup.encode().expect("shared encode");
        let lookup = SharedOrientationLookup::decode(&encoded).expect("shared decode");
        assert_eq!(lookup.lookup(1, 2, 0), Some(3));
        assert_eq!(lookup.lookup(1, 2, 1), Some(9));
        assert_eq!(lookup.rank_for(1, 2, 8), Some(1));
        assert_eq!(lookup.lookup(1, 2, 2), None);
    }

    #[test]
    fn shared_orientation_decode_rejects_truncated_and_trailing_bytes() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 4,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 3,
            },
        ];
        let encoded = SharedOrientationLookup::build(&identities, false)
            .expect("paired lookup")
            .encode()
            .expect("shared encode");
        assert!(SharedOrientationLookup::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(SharedOrientationLookup::decode(&trailing).is_err());
    }

    #[test]
    fn sampled_paired_residual_preserves_both_directions_and_block_boundaries() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 4,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 8,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 12,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 3,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 9,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 15,
            },
        ];
        let lookup = SampledPairedResidualLookup::build(&identities, 2).expect("sampled lookup");
        assert!(lookup.logical_bytes().is_some());
        assert_eq!(lookup.lookup(1, 2, 0, 4), Some(3));
        assert_eq!(lookup.lookup(1, 2, 2, 12), Some(15));
        assert_eq!(lookup.lookup(2, 1, 1, 9), Some(8));
        assert_eq!(lookup.lookup(1, 2, 3, 16), None);
    }

    #[test]
    fn sampled_paired_residual_round_trip_rejects_malformed_bytes() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 4,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 8,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 3,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 9,
            },
        ];
        let lookup = SampledPairedResidualLookup::build(&identities, 2).expect("sampled lookup");
        let encoded = lookup.encode().expect("sampled encode");
        let decoded = SampledPairedResidualLookup::decode(&encoded, 2).expect("sampled decode");
        assert_eq!(decoded.lookup(1, 2, 0, 4), Some(3));
        assert!(SampledPairedResidualLookup::decode(&encoded[..encoded.len() - 1], 2).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(SampledPairedResidualLookup::decode(&trailing, 2).is_err());
        assert!(SampledPairedResidualLookup::decode(&trailing, 0).is_err());
    }

    #[test]
    fn sampled_paired_residual_rejects_invalid_block_size() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 0,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 0,
            },
        ];
        assert!(SampledPairedResidualLookup::build(&identities, 0).is_err());
        assert!(SampledPairedResidualLookup::build(&identities, 257).is_err());
    }

    #[test]
    fn sampled_paired_residual_round_trips_raw_fallback_with_reverse_stream() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 0,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 1,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 100_000,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 1,
                slot: 100_001,
            },
        ];
        let lookup = SampledPairedResidualLookup::build(&identities, 2).expect("raw lookup");
        let bytes = lookup.logical_bytes().expect("raw logical bytes");
        let encoded = lookup.encode().expect("raw encode");
        assert_eq!(u64::try_from(encoded.len()).expect("encoded bytes"), bytes);
        let decoded = SampledPairedResidualLookup::decode(&encoded, 2).expect("raw decode");
        assert_eq!(decoded.lookup(1, 2, 0, 0), Some(100_000));
        assert_eq!(decoded.lookup(2, 1, 0, 100_000), Some(0));
    }

    #[test]
    fn undirected_pair_rank_uses_unordered_key_and_handles_self_loop() {
        let identities = [
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 4,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 1,
                target: 2,
                orientation: 0,
                slot: 8,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 0,
                slot: 3,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 2,
                target: 1,
                orientation: 0,
                slot: 9,
            },
            ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: 3,
                target: 3,
                orientation: 0,
                slot: 7,
            },
        ];
        let lookup = UndirectedPairRankLookup::build(&identities).expect("pair-rank lookup");
        assert_eq!(lookup.lookup(1, 2, 0), Some(3));
        assert_eq!(lookup.lookup(2, 1, 1), Some(8));
        assert_eq!(lookup.lookup(3, 3, 0), Some(7));
        assert_eq!(lookup.lookup(1, 2, 2), None);
        assert_eq!(lookup.logical_bytes(), Some(20));
    }

    #[test]
    fn undirected_pair_rank_rejects_missing_oriented_counterpart() {
        let identities = [ic_stable_lara::adoption_fixture::PhysicalIdentity {
            owner: 1,
            target: 2,
            orientation: 0,
            slot: 4,
        }];
        assert!(UndirectedPairRankLookup::build(&identities).is_err());
    }

    #[test]
    fn undirected_pair_rank_exception_keeps_only_mismatched_pair_slots() {
        let lookup = UndirectedPairRankExceptionLookup::from_ordered_pairs(
            1,
            2,
            &[(10, 30), (20, 20), (30, 10)],
        )
        .expect("exception lookup");
        assert_eq!(lookup.lookup(1, 2, 0), Some(10));
        assert_eq!(lookup.lookup(2, 1, 2), Some(10));
        assert_eq!(lookup.lookup(1, 2, 3), None);
        assert_eq!(lookup.lookup(2, 3, 0), None);
        assert_eq!(lookup.logical_bytes(), Some(44));
        assert_eq!(lookup.mismatch_count(), Some(2));
        assert_eq!(lookup.within_mismatch_budget(2), Some(true));
        assert_eq!(lookup.within_mismatch_budget(1), Some(false));
    }

    #[test]
    fn undirected_pair_rank_exception_rejects_invalid_pairs() {
        assert!(UndirectedPairRankExceptionLookup::from_ordered_pairs(2, 1, &[(1, 2)]).is_err());
        assert!(UndirectedPairRankExceptionLookup::from_ordered_pairs(1, 2, &[]).is_err());
        assert!(
            UndirectedPairRankExceptionLookup::from_ordered_pairs(1, 2, &[(1, 2), (1, 3)]).is_err()
        );
    }

    #[test]
    fn undirected_pair_rank_exception_accepts_aligned_order_without_mismatch() {
        let lookup = UndirectedPairRankExceptionLookup::from_ordered_pairs(
            1,
            2,
            &[(10, 20), (20, 30), (30, 40)],
        )
        .expect("aligned exception lookup");
        assert_eq!(lookup.mismatch_count(), Some(0));
        assert_eq!(lookup.within_mismatch_budget(0), Some(true));
    }

    #[test]
    fn undirected_block_rank_permutation_round_trips_both_directions() {
        let lookup = UndirectedBlockRankPermutationLookup::from_ordered_pairs(
            1,
            2,
            &[(10, 30), (30, 10), (20, 20)],
            2,
        )
        .expect("block permutation lookup");
        assert_eq!(lookup.lookup(1, 2, 0), Some(30));
        assert_eq!(lookup.lookup(1, 2, 1), Some(20));
        assert_eq!(lookup.lookup(1, 2, 2), Some(10));
        assert_eq!(lookup.lookup(2, 1, 0), Some(30));
        assert_eq!(lookup.lookup(2, 1, 2), Some(10));
        assert_eq!(lookup.lookup(1, 3, 0), None);
        assert_eq!(lookup.logical_bytes(), Some(31));
        assert_eq!(lookup.block_count(), 2);
    }

    #[test]
    fn undirected_block_rank_permutation_rejects_invalid_shape() {
        assert!(
            UndirectedBlockRankPermutationLookup::from_ordered_pairs(1, 2, &[(1, 2)], 0,).is_err()
        );
        assert!(
            UndirectedBlockRankPermutationLookup::from_ordered_pairs(1, 2, &[(1, 2), (1, 3)], 2,)
                .is_err()
        );
        assert!(
            UndirectedBlockRankPermutationLookup::from_ordered_pairs(2, 1, &[(1, 2)], 2,).is_err()
        );
        assert!(
            UndirectedBlockRankPermutationLookup::from_ordered_pairs(2, 2, &[(1, 1)], 2,).is_err()
        );
    }

    #[test]
    fn undirected_block_rank_permutation_amortizes_headers_with_larger_blocks() {
        let pairs = (0..128u32)
            .map(|rank| (rank, 127u32.saturating_sub(rank)))
            .collect::<Vec<_>>();
        for (block_size, expected_bytes) in [(8, 212), (16, 180), (32, 164), (64, 156)] {
            let lookup =
                UndirectedBlockRankPermutationLookup::from_ordered_pairs(1, 2, &pairs, block_size)
                    .expect("block permutation lookup");
            assert_eq!(lookup.logical_bytes(), Some(expected_bytes));
        }
    }
}
