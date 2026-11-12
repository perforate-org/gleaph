//! Natural-language routing client: question tokenisation, the inference-canister wire contract,
//! and slot-value binding.
//!
//! The inference canister deliberately receives token ids (not text) and returns routing
//! probabilities plus BIO slot spans; this module owns the client half of that contract so the
//! CLI, services and browser bindings share one implementation.
//!
//! One setup per client ([`client::RoutingClient::new`]) loads the tokenizer; one call per
//! question ([`client::RoutingClient::route_and_extract`]) reaches the inference path — normally
//! the Router, which owns the deployment inference canister ([`transport::RouterTransport`]); slot
//! values are then bound locally ([`client::RoutingClient::bind_slots`]).

pub mod client;
pub mod slots;
pub mod tokenizer;
pub mod transport;
pub mod wire;

pub use client::{Extraction, GeneralRoute, RoutingClient, TextExtraction};
pub use gleaph_routing_wire::{
    GeneralSlot, GeneralSlotLiteral, TextRouteAndExtract, TextSlotSpan, TokenizerFingerprint,
};
pub use slots::{EntityVocab, SlotBindings, SlotError, SpannedTag};
pub use tokenizer::{RoutingTokenizer, TokenizedQuestion};
pub use transport::{CanisterTransport, RouterTransport, RoutingTransport};
pub use wire::{RouteAndExtractWire, SlotSpanWire};

/// Failure while routing a question or binding its slots.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// Loading or running the tokenizer failed.
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    /// The transport to the inference canister failed.
    #[error("transport: {0}")]
    Transport(String),
    /// The canister reply did not match the wire contract.
    #[error("wire: {0}")]
    Wire(String),
    /// Slot binding failed (missing/ambiguous/out-of-range value).
    #[error("slots: {0}")]
    Slots(#[from] SlotError),
}
