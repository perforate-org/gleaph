//! Label-level traversal semantics for the labeled CSR layout.

use super::label::LabelId;

/// How a label interprets inline edge payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineValueKind {
    /// Raw `u16` payload with no additional interpretation.
    RawU16,
    /// Inline payload encodes a traversal weight.
    Weight,
    /// Inline payload encodes a traversal rank.
    Rank,
}

/// Directionality and inline-value semantics owned by a label definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelSemantics {
    pub label_id: LabelId,
    pub directed: bool,
    pub undirected: bool,
    pub inline_value_kind: InlineValueKind,
}

impl LabelSemantics {
    /// Default directed label with raw `u16` inline payloads.
    #[inline]
    pub const fn default_directed(label_id: LabelId) -> Self {
        Self {
            label_id,
            directed: true,
            undirected: false,
            inline_value_kind: InlineValueKind::RawU16,
        }
    }

    /// Reserved internal label for unlabeled directed edges.
    #[inline]
    pub fn unlabeled_directed() -> Self {
        Self::default_directed(LabelId::from_raw(1))
    }

    /// Reserved internal label for unlabeled undirected edges.
    #[inline]
    pub fn unlabeled_undirected() -> Self {
        Self {
            label_id: LabelId::from_raw(2),
            directed: false,
            undirected: true,
            inline_value_kind: InlineValueKind::RawU16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_directed_semantics_are_directed_only() {
        let semantics = LabelSemantics::default_directed(LabelId::from_raw(10));
        assert!(semantics.directed);
        assert!(!semantics.undirected);
        assert_eq!(semantics.inline_value_kind, InlineValueKind::RawU16);
    }
}
