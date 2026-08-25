//! Platform-independent core of the postMessage worker pipe (§18.2).
//!
//! The wasm glue in [`web_transport`](super::web_transport) wires this state
//! machine to browser events; every rule here is observable without a browser,
//! so the replay and envelope contracts are unit-tested like the rest of the
//! worker protocol rather than trusted to a runtime.
//!
//! Contract summary:
//!
//! - Every main→worker message starts with one [`envelope`] tag byte.
//!   Tag [`envelope::LIB_REQUEST`] carries exactly a library-encoded
//!   [`ToWorker`] request ([`ToWorker::encode_wire_bytes`]). Tag
//!   [`envelope::APP_MUTATION`] carries an application-owned byte form for one
//!   [`SceneMutation`], produced and parsed by a [`PayloadCodec`] — the
//!   library deliberately defines no byte form for application-typed payloads
//!   ([`WireFormatError::PayloadCodecRequired`]).
//! - Worker→main messages are untagged: the only response kind is one
//!   [`PaintFrameWire`](crate::frame_source::PaintFrameWire) transfer form.
//! - The bootstrap script posts the plain string [`READY`] once its Rust
//!   handler is registered; sends made before readiness are queued and
//!   replayed in posting order ([`ReplayQueue`]).

#[cfg(any(target_family = "wasm", test))]
use std::collections::VecDeque;

use crate::frame_source::WireFormatError;

use super::{SceneMutation, ToWorker};

/// Envelope tags for main→worker messages (first byte of every transfer).
pub mod envelope {
    /// The rest of the message is a library-encoded
    /// [`ToWorker`](super::ToWorker) request.
    pub const LIB_REQUEST: u8 = 1;
    /// The rest of the message is one [`SceneMutation`] in an
    /// application-owned form produced by a [`PayloadCodec`].
    pub const APP_MUTATION: u8 = 2;
}

/// The readiness signal posted by the bootstrap script once its Rust message
/// handler is registered.
pub const READY: &str = "ready";

/// Application-owned byte form for scene mutations that carry
/// application-typed payloads ([`SceneMutation::Merge`],
/// [`SceneMutation::Apply`]).
///
/// This trait is the formal seam behind
/// [`WireFormatError::PayloadCodecRequired`]: the library wire covers only
/// library-owned content, so anything typed by the application crosses under
/// [`envelope::APP_MUTATION`] in whatever form this codec chooses. Fail-closed
/// decoding (never a partial mutation) remains the caller-visible contract.
pub trait PayloadCodec<NK, EK, N, E> {
    /// Append the wire form of one application-typed mutation (without the
    /// envelope tag).
    fn encode(
        &self,
        mutation: &SceneMutation<NK, EK, N, E>,
        out: &mut Vec<u8>,
    ) -> Result<(), WireFormatError>;

    /// Parse exactly one mutation produced by [`Self::encode`]. Fail-closed:
    /// truncation, unknown discriminants, and trailing bytes are errors.
    fn decode(&self, bytes: &[u8]) -> Result<SceneMutation<NK, EK, N, E>, WireFormatError>;
}

/// Send-side replay queue: holds bytes until readiness, preserving order.
///
/// The main thread may start sending before the worker finished initializing
/// its wasm module; messages posted in that window must not be lost, and their
/// FIFO order must survive the readiness transition.
///
/// Compiled for the wasm glue that consumes it and for the native unit tests
/// that pin its ordering contract; pure-native builds have no sender to
/// replay.
#[cfg(any(target_family = "wasm", test))]
#[derive(Debug, Default)]
pub struct ReplayQueue {
    ready: bool,
    pending: VecDeque<Vec<u8>>,
}

#[cfg(any(target_family = "wasm", test))]
impl ReplayQueue {
    /// A queue that replays everything pushed before [`Self::set_ready`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one outgoing message. Returns `Some(bytes)` to transmit right
    /// away when already ready, or `None` while the message waits queued.
    pub fn push(&mut self, bytes: Vec<u8>) -> Option<Vec<u8>> {
        if self.ready {
            Some(bytes)
        } else {
            self.pending.push_back(bytes);
            None
        }
    }

    /// Transition to ready and drain the pending messages in posting order.
    /// Idempotent: later calls return nothing.
    pub fn set_ready(&mut self) -> Vec<Vec<u8>> {
        if self.ready {
            return Vec::new();
        }
        self.ready = true;
        self.pending.drain(..).collect()
    }
}

/// One decoded main→worker payload, tagged by its envelope.
#[derive(Debug)]
pub enum Inbound<NK, EK, N, E> {
    /// A library-encoded request ([`envelope::LIB_REQUEST`]).
    Library(ToWorker<NK, EK, N, E>),
    /// An application-decoded mutation ([`envelope::APP_MUTATION`]).
    App(SceneMutation<NK, EK, N, E>),
}

/// Encode one outgoing request under its envelope tag.
///
/// Library-owned content rides [`envelope::LIB_REQUEST`] verbatim through
/// [`ToWorker::encode_wire_bytes`]; application-typed mutations ride
/// [`envelope::APP_MUTATION`] through `codec`, and fail closed with
/// [`WireFormatError::PayloadCodecRequired`] when no codec is registered.
pub fn encode_request<NK, EK, N, E>(
    request: &ToWorker<NK, EK, N, E>,
    codec: Option<&dyn PayloadCodec<NK, EK, N, E>>,
    out: &mut Vec<u8>,
) -> Result<(), WireFormatError> {
    match request {
        ToWorker::FrameState(_) | ToWorker::Mutation(SceneMutation::SetPosition { .. }) => {
            out.push(envelope::LIB_REQUEST);
            request.encode_wire_bytes(out)
        }
        ToWorker::Mutation(mutation @ (SceneMutation::Merge(_) | SceneMutation::Apply(_))) => {
            let codec = codec.ok_or(WireFormatError::PayloadCodecRequired)?;
            out.push(envelope::APP_MUTATION);
            codec.encode(mutation, out)
        }
    }
}

/// Route one main→worker payload by its envelope tag (worker side).
///
/// Fail-closed: empty messages, unknown tags, undecodable library requests,
/// and codec failures are errors — never partially applied mutations.
pub fn decode_inbound<NK, EK, N, E>(
    bytes: &[u8],
    codec: Option<&dyn PayloadCodec<NK, EK, N, E>>,
) -> Result<Inbound<NK, EK, N, E>, WireFormatError> {
    let Some((tag, rest)) = bytes.split_first() else {
        return Err(WireFormatError::Truncated {
            needed: 1,
            remaining: 0,
        });
    };
    match *tag {
        envelope::LIB_REQUEST => Ok(Inbound::Library(ToWorker::decode_wire_bytes(rest)?)),
        envelope::APP_MUTATION => {
            let codec = codec.ok_or(WireFormatError::PayloadCodecRequired)?;
            Ok(Inbound::App(codec.decode(rest)?))
        }
        other => Err(WireFormatError::BadDiscriminant {
            field: "envelope tag",
            value: other,
        }),
    }
}

#[cfg(test)]
mod tests {
    use glam::Vec2;
    use slotmap::KeyData;

    use crate::graph::NodeId;
    use crate::interaction::{Hover, Selection};
    use crate::patch::GraphBatch;
    use crate::style::GraphStyle;
    use crate::viewport::Viewport;

    use super::*;

    type Mutation = SceneMutation<String, String, String, String>;
    type Request = ToWorker<String, String, String, String>;

    // Glob imports cover the protocol vocabulary; FrameState is named
    // explicitly because the test constructs it directly.
    use crate::worker::FrameState;

    /// A codec whose entire vocabulary is one marker byte, so tests assert on
    /// routing rather than on any real batch format.
    struct MarkerCodec;

    impl PayloadCodec<String, String, String, String> for MarkerCodec {
        fn encode(&self, mutation: &Mutation, out: &mut Vec<u8>) -> Result<(), WireFormatError> {
            match mutation {
                SceneMutation::Merge(_) => out.extend_from_slice(b"merge"),
                SceneMutation::Apply(_) => out.extend_from_slice(b"apply"),
                SceneMutation::SetPosition { .. } => {
                    return Err(WireFormatError::PayloadCodecRequired);
                }
            }
            Ok(())
        }

        fn decode(&self, bytes: &[u8]) -> Result<Mutation, WireFormatError> {
            match bytes {
                b"merge" => Ok(SceneMutation::Merge(crate::patch::GraphBatch::new())),
                b"apply" => Ok(SceneMutation::Apply(crate::patch::GraphPatch::new())),
                _ => Err(WireFormatError::BadDiscriminant {
                    field: "marker payload",
                    value: bytes.first().copied().unwrap_or(0),
                }),
            }
        }
    }

    fn snapshot_request() -> Request {
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(800.0, 600.0));
        ToWorker::FrameState(Box::new(FrameState {
            viewport,
            style: GraphStyle::default(),
            selection: Selection::new(),
            hover: Hover::default(),
        }))
    }

    #[test]
    fn replay_queue_preserves_posting_order_across_readiness() {
        let mut queue = ReplayQueue::new();

        // Nothing transmits before readiness; order is retained instead.
        assert!(queue.push(b"a".to_vec()).is_none());
        assert!(queue.push(b"b".to_vec()).is_none());

        let drained = queue.set_ready();
        assert_eq!(drained, vec![b"a".to_vec(), b"b".to_vec()]);

        // Readiness is sticky; later messages pass straight through.
        assert!(queue.set_ready().is_empty());
        assert_eq!(queue.push(b"c".to_vec()), Some(b"c".to_vec()));
    }

    #[test]
    fn library_content_rides_the_library_envelope_byte_for_byte() {
        let mut out = Vec::new();
        encode_request(&snapshot_request(), None, &mut out).expect("snapshots are library-owned");

        assert_eq!(out[0], envelope::LIB_REQUEST);
        let reparsed: Request =
            ToWorker::decode_wire_bytes(&out[1..]).expect("library form must re-parse");
        assert!(matches!(reparsed, ToWorker::FrameState(_)));
    }

    #[test]
    fn node_moves_are_library_owned_too() {
        let node = NodeId::from(KeyData::from_ffi(0));
        let request: Request = ToWorker::Mutation(SceneMutation::SetPosition {
            node,
            position: Vec2::new(1.5, -2.5),
        });

        let mut out = Vec::new();
        encode_request(&request, None, &mut out).expect("moves are library-owned");
        assert_eq!(out[0], envelope::LIB_REQUEST);
        assert!(
            matches!(
                ToWorker::<String, String, String, String>::decode_wire_bytes(&out[1..]),
                Ok(ToWorker::Mutation(SceneMutation::SetPosition { .. }))
            ),
            "the move must re-parse through the library form"
        );
    }

    #[test]
    fn merges_fail_closed_without_a_registered_codec() {
        let request: Request = ToWorker::Mutation(SceneMutation::Merge(GraphBatch::<
            String,
            String,
            String,
            String,
        >::new()));

        let mut out = Vec::new();
        let error = encode_request(&request, None, &mut out)
            .expect_err("merges must not be guessed a byte form");
        assert_eq!(error, WireFormatError::PayloadCodecRequired);
        assert!(out.is_empty(), "nothing may leak past a failed encode");
    }

    #[test]
    fn app_mutations_round_trip_through_codec_and_envelope() {
        let batch: GraphBatch<String, String, String, String> = GraphBatch::new();
        let request: Request = ToWorker::Mutation(SceneMutation::Merge(batch));

        let mut out = Vec::new();
        encode_request(&request, Some(&MarkerCodec), &mut out).expect("codec encodes");
        assert_eq!(out[0], envelope::APP_MUTATION);
        assert_eq!(
            &out[1..],
            b"merge",
            "the codec owns the rest of the message"
        );

        match decode_inbound(&out, Some(&MarkerCodec)).expect("inbound decodes") {
            Inbound::App(SceneMutation::Merge(_)) => {}
            other => panic!("expected an app mutation, got {other:?}"),
        }
    }

    #[test]
    fn inbound_routing_rejects_empty_unknown_and_unroutable_messages() {
        let codec = MarkerCodec;

        assert!(
            matches!(
                decode_inbound::<String, String, String, String>(&[], Some(&codec)),
                Err(WireFormatError::Truncated {
                    needed: 1,
                    remaining: 0
                })
            ),
            "an empty transfer is a truncation, not a panic"
        );

        assert!(matches!(
            decode_inbound(&[9], Some(&codec)),
            Err(WireFormatError::BadDiscriminant {
                field: "envelope tag",
                value: 9,
            })
        ));

        // An app-mutation envelope without a codec cannot be routed.
        let mut orphan = vec![envelope::APP_MUTATION];
        orphan.extend_from_slice(b"merge");
        assert!(matches!(
            decode_inbound::<String, String, String, String>(&orphan, None),
            Err(WireFormatError::PayloadCodecRequired)
        ));
    }
}
