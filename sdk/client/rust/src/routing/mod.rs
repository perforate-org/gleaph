//! Natural-language general-routing client: the Router context/route contract and slot binding.
//!
//! The Router builds the per-graph context and the inference canister executes the frozen general
//! router; this module owns the client half of that contract for the CLI, services and browser
//! bindings. Slots are typed and fail-closed; callers never need a local tokenizer or model pack.

pub mod client;
pub mod slots;
pub mod transport;

pub use client::{GeneralRoute, RoutingClient};
pub use gleaph_routing_wire::{GeneralSlot, GeneralSlotLiteral, MAX_GENERAL_BATCH_QUESTIONS};
pub use slots::{EntityVocab, SlotBindings, SlotError, SpannedTag};
pub use transport::{RouterTransport, RoutingTransport};

/// Failure while routing a question or binding its slots.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// The transport to the Router failed.
    #[error("transport: {0}")]
    Transport(String),
    /// The Router reply did not match the wire contract.
    #[error("wire: {0}")]
    Wire(String),
    /// Slot binding failed (missing/ambiguous/out-of-range value).
    #[error("slots: {0}")]
    Slots(#[from] SlotError),
}
