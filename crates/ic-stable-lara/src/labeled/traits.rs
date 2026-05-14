//! Traits for labeled CSR vertex rows.

use crate::traits::CsrVertex;

/// Extension of [`CsrVertex`] for rows in the labeled multi-level CSR layout.
pub trait LabeledCsrVertex: CsrVertex {
    /// Returns `true` when this vertex points directly into the edge CSR.
    fn is_default_edge_labeled(&self) -> bool;

    /// Returns a copy with the default-label bypass flag changed.
    fn with_default_edge_labeled(self, enabled: bool) -> Self;
}

impl LabeledCsrVertex for super::record::LabeledVertex {
    fn is_default_edge_labeled(&self) -> bool {
        super::record::LabeledVertex::is_default_edge_labeled(*self)
    }

    fn with_default_edge_labeled(self, enabled: bool) -> Self {
        super::record::LabeledVertex::with_default_edge_labeled(self, enabled)
    }
}
