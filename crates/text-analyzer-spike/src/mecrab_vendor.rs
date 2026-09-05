//! Landing shim (plan 0334): the vendored MeCrab dict layer moved out of the spike into
//! the publishable `morph-dict` crate (plus the `ic-morph-dict` stable-memory adapter).
//! This module keeps the plan-0333 test/harness call sites (`text_analyzer_spike::
//! mecrab_vendor::…`) working over the landed crate.

pub use morph_dict::*;

/// 0333-era name for [`morph_dict::Analyzer`].
pub type MecrabAnalyzer = morph_dict::Analyzer;
