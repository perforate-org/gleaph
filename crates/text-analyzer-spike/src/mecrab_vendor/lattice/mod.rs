//! Lattice module for building the word graph
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//!
//! The lattice represents all possible segmentations of the input text
//! as a directed acyclic graph (DAG). Each node represents a potential
//! word, and edges connect adjacent words.



use crate::mecrab_vendor::error::Result;
use crate::mecrab_vendor::dict::{CharCategory, Dictionary, DictionaryEntry};

/// A node in the lattice representing a potential word/token
#[derive(Debug, Clone)]
pub struct LatticeNode<'a> {
    /// Surface form (slice into original text)
    pub surface: &'a str,
    /// Start position in bytes
    pub start: usize,
    /// End position in bytes
    pub end: usize,
    /// Word ID (token index in dictionary, used for embeddings)
    pub word_id: u32,
    /// Left context ID for connection matrix
    pub left_id: u16,
    /// Right context ID for connection matrix
    pub right_id: u16,
    /// Part-of-speech ID
    pub pos_id: u16,
    /// Word cost from dictionary
    pub wcost: i16,
    /// Feature string (lazily loaded)
    pub feature: String,
    /// Whether this is an unknown word
    pub is_unknown: bool,
}

impl<'a> LatticeNode<'a> {
    /// Create a new lattice node from a dictionary entry (borrowing feature)
    pub fn from_entry(
        text: &'a str,
        start: usize,
        entry: &DictionaryEntry,
        feature: String,
    ) -> Self {
        Self {
            surface: &text[start..start + entry.length],
            start,
            end: start + entry.length,
            word_id: entry.word_id,
            left_id: entry.left_id,
            right_id: entry.right_id,
            pos_id: entry.pos_id,
            wcost: entry.wcost,
            feature,
            is_unknown: false,
        }
    }

    /// Create a new lattice node from an owned dictionary entry (moves feature)
    #[inline]
    pub fn from_entry_owned(text: &'a str, start: usize, entry: DictionaryEntry) -> Self {
        let end = start + entry.length;
        Self {
            surface: &text[start..end],
            start,
            end,
            word_id: entry.word_id,
            left_id: entry.left_id,
            right_id: entry.right_id,
            pos_id: entry.pos_id,
            wcost: entry.wcost,
            feature: entry.feature, // Move, don't clone
            is_unknown: false,
        }
    }

    /// Create a BOS (Beginning of Sentence) node
    pub fn bos() -> Self {
        Self {
            surface: "",
            start: 0,
            end: 0,
            word_id: u32::MAX, // BOS/EOS don't have word_id
            left_id: 0,
            right_id: 0,
            pos_id: 0,
            wcost: 0,
            feature: "BOS/EOS".to_string(),
            is_unknown: false,
        }
    }

    /// Create an EOS (End of Sentence) node
    pub fn eos(position: usize) -> Self {
        Self {
            surface: "",
            start: position,
            end: position,
            word_id: u32::MAX, // BOS/EOS don't have word_id
            left_id: 0,
            right_id: 0,
            pos_id: 0,
            wcost: 0,
            feature: "BOS/EOS".to_string(),
            is_unknown: false,
        }
    }

    /// Create an unknown word node
    pub fn unknown(
        text: &'a str,
        start: usize,
        length: usize,
        entry: &DictionaryEntry,
        feature: String,
    ) -> Self {
        Self {
            surface: &text[start..start + length],
            start,
            end: start + length,
            word_id: entry.word_id,
            left_id: entry.left_id,
            right_id: entry.right_id,
            pos_id: entry.pos_id,
            wcost: entry.wcost,
            feature,
            is_unknown: true,
        }
    }
}

/// The lattice structure representing all possible segmentations
#[derive(Debug)]
pub struct Lattice<'a> {
    /// Original input text
    pub text: &'a str,
    /// Nodes at each byte position
    /// Index 0 contains BOS, last index contains EOS
    pub nodes_at: Vec<Vec<LatticeNode<'a>>>,
}

impl<'a> Lattice<'a> {
    /// Build a lattice from the input text using the dictionary
    ///
    /// # Arguments
    ///
    /// * `text` - The input text to analyze
    /// * `dict` - The dictionary to use for word lookup
    ///
    /// # Errors
    ///
    /// Returns an error if lattice construction fails.
    pub fn build(text: &'a str, dict: &Dictionary) -> Result<Self> {
        let text_len = text.len();

        // Initialize nodes_at with one extra slot for BOS at position 0
        // and one for EOS at position text_len + 1
        let mut nodes_at: Vec<Vec<LatticeNode<'a>>> = vec![Vec::new(); text_len + 2];

        // Add BOS node at position 0
        nodes_at[0].push(LatticeNode::bos());

        // Build lattice by scanning through text
        for (char_idx, c) in text.char_indices() {
            let pos = char_idx;
            let remaining = &text[pos..];

            // Look up all words starting at this position
            let entries = dict.lookup(remaining);

            // Plan 0333 vendored patch (MeCab tokenizer.cpp semantics): unknown
            // candidates are added when the char category has `invoke` set EVEN IF
            // dictionary entries exist (upstream only did entries.is_empty(), which
            // broke katakana-run unknown grouping). `add_unknown_nodes` mirrors
            // MeCab: single-char candidates, the whole same-category run (group flag),
            // and incremental lengths 1..=charinfo.length.
            let invoke = dict.char_def.get_char_info(c).invoke();
            if entries.is_empty() || invoke {
                for entry in entries {
                    let node = LatticeNode::from_entry_owned(text, pos, entry);
                    let end_pos = node.end;
                    if end_pos <= text_len {
                        nodes_at[end_pos + 1].push(node);
                    }
                }
                Self::add_unknown_nodes(text, pos, c, dict, &mut nodes_at);
            } else {
                for entry in entries {
                    let node = LatticeNode::from_entry_owned(text, pos, entry);
                    let end_pos = node.end;

                    // Add node to the end position + 1 (shifted for BOS)
                    if end_pos <= text_len {
                        nodes_at[end_pos + 1].push(node);
                    }
                }
            }
        }

        // Handle case where no nodes reach the end
        // This can happen with unknown characters at the end
        let final_pos = text.len();
        if nodes_at[final_pos + 1].is_empty() && !nodes_at[final_pos].is_empty() {
            // Check if we need to handle trailing characters
        }

        // Add EOS node at the final position
        nodes_at[text_len + 1].push(LatticeNode::eos(text_len));

        Ok(Self { text, nodes_at })
    }

    /// Add MeCab-faithful unknown word candidates at `pos` (vendored patch; see the
    /// call site comment). Mirrors MeCab tokenizer.cpp lookup():
    /// 1. single-char candidate;
    /// 2. whole same-category run candidate (charinfo.group);
    /// 3. incremental candidates of length 1..=charinfo.length.
    fn add_unknown_nodes(
        text: &'a str,
        pos: usize,
        c: char,
        dict: &Dictionary,
        nodes_at: &mut [Vec<LatticeNode<'a>>],
    ) {
        let info = dict.char_def.get_char_info(c);
        let category = info.category();
        let char_len = c.len_utf8();

        // 1) single-char candidate (with unk.dic entries; fallback default node)
        let entries = dict.unknown.generate_entries(category, char_len);
        if entries.is_empty() {
            let end_pos = pos + char_len;
            if end_pos <= text.len() {
                nodes_at[end_pos + 1].push(LatticeNode {
                    surface: &text[pos..end_pos],
                    start: pos,
                    end: end_pos,
                    word_id: u32::MAX,
                    left_id: 0,
                    right_id: 0,
                    pos_id: 0,
                    wcost: 10000,
                    feature: format!("未知語,{category:?}"),
                    is_unknown: true,
                });
            }
        } else {
            Self::add_unk_entries(text, pos, entries, nodes_at);
        }

        // 2) whole-run candidate when the category groups
        if info.group() {
            let mut run_end = pos + char_len;
            for cc in text[pos + char_len..].chars() {
                if dict.char_category(cc) != category {
                    break;
                }
                run_end += cc.len_utf8();
            }
            if run_end > pos + char_len && run_end <= text.len() {
                let entries = dict.unknown.generate_entries(category, run_end - pos);
                Self::add_unk_entries(text, pos, entries, nodes_at);
            }
        }

        // 3) incremental candidates 1..=length
        let mut e = pos + char_len;
        for _ in 1..info.length() as usize {
            match text[e..].chars().next() {
                Some(cc) if dict.char_category(cc) == category => {
                    e += cc.len_utf8();
                    if e > text.len() {
                        break;
                    }
                    let entries = dict.unknown.generate_entries(category, e - pos);
                    Self::add_unk_entries(text, pos, entries, nodes_at);
                }
                _ => break,
            }
        }
    }

    fn add_unk_entries<'b>(
        text: &'b str,
        start: usize,
        entries: Vec<DictionaryEntry>,
        nodes_at: &mut [Vec<LatticeNode<'b>>],
    ) {
        for entry in entries {
            let end = start + entry.length;
            if end <= text.len()
                && !nodes_at[end + 1].iter().any(|n| n.start == start && n.end == end)
            {
                let feature = entry.feature.clone();
                let node = LatticeNode::unknown(text, start, entry.length, &entry, feature);
                nodes_at[end + 1].push(node);
            }
        }
    }

    /// Get the number of byte positions in the lattice
    pub fn len(&self) -> usize {
        self.nodes_at.len()
    }

    /// Check if the lattice is empty
    pub fn is_empty(&self) -> bool {
        self.nodes_at.is_empty()
    }

    /// Get nodes ending at a specific position
    pub fn nodes_ending_at(&self, pos: usize) -> &[LatticeNode<'a>] {
        if pos < self.nodes_at.len() {
            &self.nodes_at[pos]
        } else {
            &[]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bos_eos_nodes() {
        let bos = LatticeNode::bos();
        assert_eq!(bos.surface, "");
        assert_eq!(bos.start, 0);
        assert_eq!(bos.end, 0);

        let eos = LatticeNode::eos(10);
        assert_eq!(eos.surface, "");
        assert_eq!(eos.start, 10);
        assert_eq!(eos.end, 10);
    }
}
