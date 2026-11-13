//! Natural-language ask client: the Router's `interpret`/`ask` contract and slot binding.
//!
//! The Router builds the per-graph context and the inference canister chooses an operation; this
//! module owns the client half of that contract for the CLI, services and browser bindings.
//! `interpret` is plan-only, `ask` composes it with read-only prepared execution, and slots are
//! typed and fail-closed. Callers never need a local tokenizer or model pack.

pub mod client;
pub mod slots;
pub mod transport;

pub use client::AskClient;
pub use gleaph_router_wire::{
    Answer, AskStatus, DebugInterpretation, DebugSlot, Interpretation, InterpretationStatus,
    ReadMode,
};
pub use gleaph_routing_wire::{
    GeneralDomainValue, GeneralSlot, GeneralSlotLiteral, MAX_GENERAL_BATCH_QUESTIONS,
};
pub use slots::{EntityVocab, SlotBindings, SlotError, SpannedTag};
pub use transport::{AskTransport, RouterTransport};

/// Failure while asking a question or binding its slots.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
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
