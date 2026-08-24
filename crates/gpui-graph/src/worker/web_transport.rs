//! postMessage transport for the ADR 0076 worker host (web only).
//!
//! This module moves bytes across the Worker boundary and hosts the
//! application channel built on that pipe. Frames ride one transferable
//! `ArrayBuffer` each (sub-millisecond at measured sizes); structured cloning
//! of frame data is explicitly out per ADR 0076 §3.
//!
//! The library cannot ship a worker bundle: an application spawns its own
//! Worker script (which imports this wasm module and drives a concrete
//! [`WorkerBackend`](crate::worker::WorkerBackend) instantiation inside
//! `DedicatedWorkerGlobalScope`), then wires it up through these helpers.
//!
//! What lives here versus above it:
//!
//! - [`PostMessageChannel`] owns the generic plumbing — module-worker spawn,
//!   the readiness handshake, ordered replay of pre-readiness sends, envelope
//!   routing ([`envelope`]), frame delivery, and error reporting — as a
//!   ready-made [`WorkerChannel`](crate::worker::WorkerChannel) implementation.
//! - The application still owns its worker script (`assets/worker.js` in the
//!   example) and its payload bytes through a [`PayloadCodec`]: merge/patch
//!   forms are application-typed, so the library refuses to guess them
//!   ([`WireFormatError::PayloadCodecRequired`]).
//! - [`serve`] is the worker-side mirror: one call registers the message loop
//!   that decodes inbound envelopes into a [`WorkerBackend`], runs one
//!   backend cycle per message, and posts resulting frames back.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, Worker};

use crate::frame_source::{PaintFrameWire, WireFormatError};

use super::pipe_core::{self, ReplayQueue};
use super::{FromWorker, ToWorker, WorkerBackend, WorkerChannel};

pub use super::pipe_core::{READY, envelope};

/// Application-owned byte form for scene mutations carrying application-typed
/// payloads; see [`pipe_core::PayloadCodec`].
pub use super::pipe_core::PayloadCodec;

/// Spawn a worker from an application-supplied script URL.
pub fn spawn_worker(script_url: &str) -> Result<Worker, JsValue> {
    Worker::new(script_url)
}

/// Send one request message to `worker`, transferring the backing buffer.
pub fn send_request(worker: &Worker, bytes: Vec<u8>) -> Result<(), JsValue> {
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::new();
    transfer.push(&array.buffer().into());
    worker.post_message_with_transfer(&array.into(), &transfer.into())
}

/// Extract the request bytes from one main-thread message, or `None` for
/// messages that are not byte arrays.
pub fn message_bytes(event: &MessageEvent) -> Option<Vec<u8>> {
    event
        .data()
        .dyn_into::<js_sys::Uint8Array>()
        .ok()
        .map(|array| array.to_vec())
}

/// Send one response message to the main thread from inside the worker,
/// transferring the backing buffer.
pub fn post_response_bytes(bytes: Vec<u8>) -> Result<(), JsValue> {
    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::new();
    transfer.push(&array.buffer().into());
    scope.post_message_with_transfer(&array.into(), &transfer.into())
}

/// Why one channel operation could not complete. Codec failures carry the
/// underlying wire error; nothing here panics — a corrupt or unroutable
/// message is reported and dropped, never partially applied.
#[derive(Debug)]
pub enum ChannelError {
    /// Encoding an outgoing application mutation failed (or no codec was
    /// registered for it).
    Codec(WireFormatError),
    /// An inbound reply was not a well-formed `PaintFrameWire`.
    CorruptFrame(WireFormatError),
    /// A postMessage or worker-level failure surfaced from the browser.
    Transport(JsValue),
}

type FrameSink = Box<dyn FnMut(PaintFrameWire)>;
type ErrorSink = Box<dyn FnMut(ChannelError)>;

struct SharedCore<NK, EK, N, E> {
    worker: Worker,
    queue: RefCell<ReplayQueue>,
    codec: RefCell<Option<Box<dyn PayloadCodec<NK, EK, N, E>>>>,
    frames: RefCell<Box<dyn FnMut(PaintFrameWire)>>,
    errors: RefCell<Box<dyn FnMut(ChannelError)>>,
    /// Keeps the registered event closures alive for the page's lifetime;
    /// dropping them would detach the worker's message handler.
    closures: RefCell<Vec<ClosureHolder>>,
}

/// Type-erased keep-alive for the closures registered in [`spawn_channel`].
struct ClosureHolder(Closure<dyn FnMut(JsValue)>);

impl<NK, EK, N, E> SharedCore<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
{
    fn report(&self, error: ChannelError) {
        (self.errors.borrow_mut())(error);
    }

    fn queue_or_send(&self, bytes: Vec<u8>) {
        match self.queue.borrow_mut().push(bytes) {
            Some(bytes) => self.transmit(bytes),
            None => {}
        }
    }

    fn transmit(&self, bytes: Vec<u8>) {
        if let Err(error) = send_request(&self.worker, bytes) {
            self.report(ChannelError::Transport(error));
        }
    }
}

fn spawn_channel<NK, EK, N, E>(script_url: &str) -> Result<Rc<SharedCore<NK, EK, N, E>>, JsValue>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
{
    let worker = spawn_worker(script_url)?;
    let shared: Rc<SharedCore<NK, EK, N, E>> = Rc::new(SharedCore {
        worker: worker.clone(),
        queue: RefCell::new(ReplayQueue::new()),
        codec: RefCell::new(None),
        frames: RefCell::new(Box::new(|_| {})),
        errors: RefCell::new(Box::new(|_| {})),
        closures: RefCell::new(Vec::new()),
    });

    // Replies and readiness arrive on this single handler. The bootstrap
    // script posts the plain string READY once its Rust handler is
    // registered; everything before that replays in posting order.
    // Handlers are typed over `JsValue` so both the message and the error
    // registration share one keep-alive storage.
    let on_message_shared = Rc::clone(&shared);
    let on_message = Closure::<dyn FnMut(JsValue)>::new(move |raw: JsValue| {
        let Ok(event) = raw.dyn_into::<MessageEvent>() else {
            return;
        };
        if event.data().as_string().as_deref() == Some(READY) {
            let drained = on_message_shared.queue.borrow_mut().set_ready();
            for bytes in drained {
                on_message_shared.transmit(bytes);
            }
            return;
        }
        let Some(bytes) = message_bytes(&event) else {
            return;
        };
        match PaintFrameWire::from_wire_bytes(&bytes) {
            Ok(wire) => (on_message_shared.frames.borrow_mut())(wire),
            Err(error) => on_message_shared.report(ChannelError::CorruptFrame(error)),
        }
    });
    worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    shared.closures.borrow_mut().push(ClosureHolder(on_message));

    // Worker-level failures (script load errors, uncaught exceptions) surface
    // through onerror carrying an ErrorEvent value; they are reported, never
    // swallowed.
    let on_error_shared = Rc::clone(&shared);
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |raw: JsValue| {
        on_error_shared.report(ChannelError::Transport(raw));
    });
    worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    shared.closures.borrow_mut().push(ClosureHolder(on_error));

    Ok(shared)
}

fn encode_and_queue<NK, EK, N, E>(
    shared: &SharedCore<NK, EK, N, E>,
    request: &ToWorker<NK, EK, N, E>,
) where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
{
    let encoded = {
        let codec = shared.codec.borrow();
        let mut out = Vec::new();
        let result = match codec.as_deref() {
            Some(codec) => pipe_core::encode_request(request, Some(codec), &mut out),
            None => pipe_core::encode_request(request, None, &mut out),
        };
        result.map(|()| out)
    };
    match encoded {
        Ok(bytes) => shared.queue_or_send(bytes),
        Err(error) => shared.report(ChannelError::Codec(error)),
    }
}

/// Main-thread half of the connection to an application-owned worker.
///
/// Implements [`WorkerChannel`], so it plugs straight into
/// [`GraphViewState::connect_worker_channel`](crate::view::GraphViewState::connect_worker_channel).
/// Generic plumbing only: spawn, readiness handshake with ordered replay of
/// pre-readiness sends, envelope routing, frame delivery, and error
/// reporting. Application payload bytes cross under
/// [`envelope::APP_MUTATION`] through whatever [`PayloadCodec`] is
/// registered — without one, merge/apply requests fail closed with
/// [`WireFormatError::PayloadCodecRequired`], exactly as the raw protocol
/// requires.
///
/// Keep using [`Self::handle`] when handing the channel to the view if the
/// application also injects scene data itself: the handle stays usable after
/// the channel has been moved into the view.
pub struct PostMessageChannel<NK, EK, N, E> {
    shared: Rc<SharedCore<NK, EK, N, E>>,
}

impl<NK, EK, N, E> PostMessageChannel<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
{
    /// Spawn the application-owned worker at `script_url` and start listening
    /// for replies. Sends issued before the worker signals readiness are
    /// queued and replayed in posting order.
    pub fn spawn(script_url: &str) -> Result<Self, JsValue> {
        Ok(Self {
            shared: spawn_channel(script_url)?,
        })
    }

    /// Register the codec for application-typed mutations. Without one, merge
    /// and apply requests fail closed instead of guessing a byte form.
    pub fn set_payload_codec(&mut self, codec: Box<dyn PayloadCodec<NK, EK, N, E>>) {
        *self.shared.codec.borrow_mut() = Some(codec);
    }

    /// Register the sink for delivered frames. Called once per decoded
    /// `PaintFrameWire`, in delivery order.
    pub fn on_frame(&mut self, sink: impl FnMut(PaintFrameWire) + 'static) {
        *self.shared.frames.borrow_mut() = Box::new(sink);
    }

    /// Register the sink for transport, codec, and corruption failures.
    /// Unset channels drop errors silently.
    pub fn on_error(&mut self, sink: impl FnMut(ChannelError) + 'static) {
        *self.shared.errors.borrow_mut() = Box::new(sink);
    }

    /// A cheap clonable sending handle that outlives the move of `self` into
    /// the view, for applications that inject their own scene mutations.
    pub fn handle(&self) -> PipeHandle<NK, EK, N, E> {
        PipeHandle {
            shared: Rc::clone(&self.shared),
        }
    }
}

impl<NK, EK, N, E> WorkerChannel<NK, EK, N, E> for PostMessageChannel<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
{
    fn post(&mut self, request: ToWorker<NK, EK, N, E>) {
        encode_and_queue(&self.shared, &request);
    }
}

/// Clonable application-side sending handle over an established
/// [`PostMessageChannel`]. Shares the same readiness replay queue, so
/// app-initiated sends and view-driven requests stay mutually ordered.
pub struct PipeHandle<NK, EK, N, E> {
    shared: Rc<SharedCore<NK, EK, N, E>>,
}

impl<NK, EK, N, E> Clone for PipeHandle<NK, EK, N, E> {
    fn clone(&self) -> Self {
        Self {
            shared: Rc::clone(&self.shared),
        }
    }
}

impl<NK, EK, N, E> PipeHandle<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
{
    /// Send one application-typed scene mutation. Fails closed when no
    /// [`PayloadCodec`] covers it; readiness is never an error here —
    /// pre-readiness sends replay in order.
    pub fn send_mutation(&mut self, mutation: crate::worker::SceneMutation<NK, EK, N, E>) {
        let encoded = {
            let codec = self.shared.codec.borrow();
            let mut out = Vec::new();
            let result = match codec.as_deref() {
                Some(codec) => {
                    out.push(envelope::APP_MUTATION);
                    codec.encode(&mutation, &mut out)
                }
                None => Err(WireFormatError::PayloadCodecRequired),
            };
            result.map(|()| out)
        };
        match encoded {
            Ok(bytes) => self.shared.queue_or_send(bytes),
            Err(error) => self.shared.report(ChannelError::Codec(error)),
        }
    }
}

/// Worker-side counterpart of [`PostMessageChannel`]: register the message
/// loop for `backend` and return. Every inbound message is decoded (library
/// requests verbatim, application mutations through `codec`), applied to the
/// backend inbox, and answered by exactly one backend cycle — one layout step
/// and at most one frame posted back as transferable wire bytes.
///
/// Failures are reported to the console and dropped; the loop keeps serving
/// later messages rather than poisoning the worker.
pub fn serve<NK, EK, N, E>(
    mut backend: WorkerBackend<NK, EK, N, E>,
    codec: Box<dyn PayloadCodec<NK, EK, N, E>>,
) where
    NK: Eq + std::hash::Hash + Sync + 'static,
    EK: Eq + std::hash::Hash + Sync + 'static,
    N: Sync + 'static,
    E: Sync + 'static,
{
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&JsValue::from_str(&format!("{info}")));
    }));

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(bytes) = message_bytes(&event) else {
            return;
        };
        match pipe_core::decode_inbound(&bytes, Some(codec.as_ref())) {
            Ok(pipe_core::Inbound::Library(request)) => backend.receive(request),
            Ok(pipe_core::Inbound::App(mutation)) => {
                backend.receive(ToWorker::Mutation(mutation));
            }
            Err(error) => {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "worker pipe: undecodable request dropped: {error}"
                )));
            }
        }

        if let Some(FromWorker::Frame(wire)) = backend.step() {
            if let Err(error) = post_response_bytes(wire.to_wire_bytes()) {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "worker pipe: response post failed: {error:?}"
                )));
            }
        }
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}
