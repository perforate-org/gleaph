//! Rune adapter for [`graph_view_core`].
//!
//! `panel` turns a core paint frame into a Rune `Shape` (nodes, edges, label rings, aggregate
//! density) and owns the camera, hit testing and keyboard cursor. `theme` is the console's APCA
//! palette: it derives every foreground from a seed and maps the result onto rune-kit's tokens.
//!
//! Nothing here depends on the console's screens; an application composes these two modules.

pub mod panel;
pub mod theme;
