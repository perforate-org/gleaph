//! gpui-graph web example — main-thread GPUI application (wasm32).
//!
//! Demonstrates the frame-source contract (DESIGN.md §18.2, ADR 0076) with the
//! same ~100-vertex demo graph rendered two ways:
//!
//! - `InProcess` (the library default): the view builds each frame
//!   synchronously from the shared scene, stepping ForceAtlas2 locally.
//! - `Worker` (strictly opt-in): the app spawns its own `worker.js` module
//!   worker, connects it via `connect_worker_channel`, selects
//!   `set_frame_source(FrameSource::Worker)`, injects the scene as an
//!   application-encoded merge batch, and paints delivered `PaintFrameWire`
//!   frames.
//!
//! The mode is a URL parameter (`?mode=worker` / `?mode=inprocess`); the
//! default mirrors the library: `InProcess`. See README.md for the three
//! wiring layers an application owns.

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use futures::{StreamExt, channel::mpsc::UnboundedSender};
    use gpui::{
        App, Context, Entity, Render, TextStyle, Window, WindowBounds, WindowOptions, div, hsla,
        prelude::*, px, rems, size, white,
    };
    use gpui_graph::worker::web_transport;
    use gpui_graph::{
        FixedLayout, ForceAtlas2, FrameSource, GraphBatch, GraphScene, GraphView, GraphViewState,
        LayoutBudget, LayoutProgress, PaintFrameWire, ToWorker, WorkerChannel,
    };
    use gpui_graph_web_example_worker::batch_codec::{
        ENVELOPE_LIB_REQUEST, ENVELOPE_MERGE_BATCH, encode_merge_batch,
    };
    use gpui_graph_web_example_worker::{DEMO_NODE_COUNT, demo_batch, initial_position};
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use web_sys::{ErrorEvent, MessageEvent, Worker, WorkerOptions, WorkerType};

    const WORKER_SCRIPT_URL: &str = "worker.js";

    /// Per-frame layout work for the InProcess source (§11.8). One iteration
    /// per frame paces the visible relaxation; the wall-clock cap keeps an
    /// oversized iteration from blowing the frame.
    const FRAME_LAYOUT_BUDGET: LayoutBudget = LayoutBudget {
        max_iterations: 1,
        max_duration: Some(core::time::Duration::from_millis(6)),
    };

    /// Which frame source the page runs. `InProcess` is the library default;
    /// `Worker` is opt-in — exactly the seam this example demonstrates.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FrameMode {
        InProcess,
        Worker,
    }

    impl FrameMode {
        fn label(self) -> &'static str {
            match self {
                FrameMode::InProcess => "InProcess (library default)",
                FrameMode::Worker => "Worker (opt-in)",
            }
        }
    }

    thread_local! {
        /// Keeps the GPUI application alive for the page's lifetime (web has
        /// no blocking run loop to hold it — hence `run_embedded`).
        static APP_HANDLE: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
    }

    /// Boot the GPUI application. Called from `main`; the wasm-bindgen
    /// `--target web` glue for a binary crate runs `main` once `init()`
    /// resolves (verified by `build.sh` output).
    pub fn boot() -> Result<(), JsValue> {
        gpui_platform::web_init();
        let mode = requested_mode();
        log(&format!(
            "[example-app] booting (gpui_web) — frame source: {}",
            mode.label()
        ));

        // Web is an embedded guest: `Platform::run` invokes the launch callback
        // and RETURNS instead of blocking like a native run loop. `run_embedded`
        // hands back an ApplicationHandle that keeps the app alive; park it for
        // the page's lifetime.
        let handle =
            gpui_platform::application_with_web_backend(gpui_platform::WebBackendPreference::Auto)
                .run_embedded(move |cx: &mut App| {
                    let bounds = gpui::Bounds::centered(None, size(px(1024.), px(700.)), cx);
                    cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            ..Default::default()
                        },
                        |_window, cx| cx.new(|cx| ExampleApp::new(mode, cx)),
                    )
                    .expect("failed to open example window");
                    cx.activate(true);
                });
        APP_HANDLE.with(|slot| *slot.borrow_mut() = Some(handle));
        Ok(())
    }

    /// `?mode=worker` opts into the worker frame source; everything else runs
    /// the library-default synchronous path.
    fn requested_mode() -> FrameMode {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        if search.contains("mode=worker") {
            FrameMode::Worker
        } else {
            FrameMode::InProcess
        }
    }

    /// Main-thread half of the worker connection (layer ③ in README terms):
    /// a [`WorkerChannel`] implementation that forwards library-encoded
    /// requests through [`web_transport`] and routes delivered wire bytes back
    /// into GPUI.
    struct WebWorkerChannel {
        worker: Worker,
        /// Requests encoded before the worker signalled readiness, replayed in
        /// posting order once it does (wasm init is asynchronous; messages
        /// posted before its handler exists would be lost).
        pending: Rc<RefCell<Vec<Vec<u8>>>>,
        ready: Rc<Cell<bool>>,
    }

    impl WebWorkerChannel {
        /// Spawn the application-owned `worker.js` as an ES-module worker
        /// (layer ②: the script is ours; the library ships no bundle).
        fn spawn() -> Result<Self, JsValue> {
            let options = WorkerOptions::new();
            options.set_type(WorkerType::Module);
            let worker = Worker::new_with_options(WORKER_SCRIPT_URL, &options)?;
            Ok(Self {
                worker,
                pending: Rc::new(RefCell::new(Vec::new())),
                ready: Rc::new(Cell::new(false)),
            })
        }

        /// Send one request now if the worker is ready, else queue it for the
        /// readiness replay.
        fn send(&self, bytes: Vec<u8>) {
            if self.ready.get() {
                self.post_bytes(bytes);
            } else {
                self.pending.borrow_mut().push(bytes);
            }
        }

        fn post_bytes(&self, bytes: Vec<u8>) {
            if let Err(error) = web_transport::send_request(&self.worker, bytes) {
                log(&format!("[example-app] request post failed: {error:?}"));
            }
        }

        /// Inject the demo scene into the worker replica under the
        /// application's merge-batch envelope.
        fn send_merge_batch(&self, batch: &GraphBatch<String, String, String, String>) {
            log(&format!(
                "[example-app] queuing scene injection: {} nodes / {} edges",
                batch.nodes.len(),
                batch.edges.len(),
            ));
            let mut bytes = vec![ENVELOPE_MERGE_BATCH];
            encode_merge_batch(batch, &mut bytes);
            self.send(bytes);
        }

        /// Route every worker response into GPUI through `deliveries`.
        fn listen(&self, deliveries: UnboundedSender<PaintFrameWire>) {
            let on_error = Closure::<dyn FnMut(ErrorEvent)>::new(move |event: ErrorEvent| {
                log(&format!(
                    "[example-app] worker error: {} ({})",
                    event.message(),
                    event.filename()
                ));
            });
            self.worker
                .set_onerror(Some(on_error.as_ref().unchecked_ref()));
            on_error.forget();

            let ready = self.ready.clone();
            let pending = self.pending.clone();
            let worker = self.worker.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                // The bootstrap script posts the plain string "ready" once its
                // Rust handler is registered; replay everything queued before.
                if event.data().as_string().as_deref() == Some("ready") {
                    ready.set(true);
                    for bytes in pending.borrow_mut().drain(..) {
                        post_bytes(&worker, bytes);
                    }
                    log("[example-app] worker ready — pending requests flushed");
                    return;
                }
                let Some(bytes) = web_transport::message_bytes(&event) else {
                    return;
                };
                match PaintFrameWire::from_wire_bytes(&bytes) {
                    Ok(wire) => {
                        let _ = deliveries.unbounded_send(wire);
                    }
                    Err(error) => log(&format!("[example-app] corrupt frame dropped: {error}")),
                }
            });
            self.worker
                .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            on_message.forget();
        }
    }

    fn post_bytes(worker: &Worker, bytes: Vec<u8>) {
        if let Err(error) = web_transport::send_request(worker, bytes) {
            log(&format!("[example-app] request post failed: {error:?}"));
        }
    }

    impl WorkerChannel<String, String, String, String> for WebWorkerChannel {
        fn post(&mut self, request: ToWorker<String, String, String, String>) {
            // Library-owned request content crosses verbatim under the
            // envelope tag; the library deliberately defines no byte form for
            // merge batches (that is why `send_merge_batch` exists above).
            let mut bytes = vec![ENVELOPE_LIB_REQUEST];
            match request.encode_wire_bytes(&mut bytes) {
                Ok(()) => self.send(bytes),
                Err(error) => log(&format!(
                    "[example-app] request not encodable, dropped: {error}"
                )),
            }
        }
    }

    struct ExampleApp {
        mode: FrameMode,
        scene: Entity<GraphScene<String, String, String, String>>,
        view: Entity<GraphViewState<String, String, String, String>>,
        /// InProcess only: whether ForceAtlas2 settled and rendering can idle.
        settled: bool,
        frames_delivered: usize,
        last_counts: Option<(usize, usize, usize)>,
        draws: usize,
    }

    impl ExampleApp {
        fn new(mode: FrameMode, cx: &mut Context<Self>) -> Self {
            match mode {
                FrameMode::InProcess => Self::in_process(mode, cx),
                FrameMode::Worker => Self::worker_mode(mode, cx),
            }
        }

        /// The library default: one local scene under ForceAtlas2, stepped by
        /// the render loop; the view builds every frame synchronously.
        fn in_process(mode: FrameMode, cx: &mut Context<Self>) -> Self {
            let scene = cx.new(|_cx| {
                GraphScene::new()
                    .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)))
            });
            populate(&scene, cx);
            let view = styled_view(scene.clone(), cx);
            log("[example-app] frame source: InProcess");
            Self {
                mode,
                scene,
                view,
                settled: false,
                frames_delivered: 0,
                last_counts: None,
                draws: 0,
            }
        }

        /// The opt-in path: spawn + connect the worker first so scene
        /// injection lands before the first prepaint snapshot (the inbox is
        /// FIFO, preserving that order across messages), then switch the view
        /// to `FrameSource::Worker`.
        fn worker_mode(mode: FrameMode, cx: &mut Context<Self>) -> Self {
            let (delivery_tx, mut delivery_rx) =
                futures::channel::mpsc::unbounded::<PaintFrameWire>();
            let channel = WebWorkerChannel::spawn().expect("failed to spawn the example worker.js");
            channel.listen(delivery_tx);
            channel.send_merge_batch(&demo_batch());

            // Main-thread scene: fixed seed placement, holding content for the
            // view's one-time initial camera fit while the worker replica
            // animates its own copy.
            let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(FixedLayout)));
            populate(&scene, cx);

            let view = styled_view(scene.clone(), cx);
            view.update(cx, |view, cx| {
                view.connect_worker_channel(Box::new(channel));
                view.set_frame_source(FrameSource::Worker);
                cx.notify();
            });

            // Drain worker deliveries into the entity on the GPUI task.
            cx.spawn(async move |this, cx| {
                while let Some(wire) = delivery_rx.next().await {
                    if this
                        .update(cx, |app, cx| app.on_frame_delivered(wire, cx))
                        .is_err()
                    {
                        log("[example-app] entity update failed (app gone?)");
                    }
                }
            })
            .detach();

            log("[example-app] frame source: Worker");
            Self {
                mode,
                scene,
                view,
                settled: true,
                frames_delivered: 0,
                last_counts: None,
                draws: 0,
            }
        }

        fn on_frame_delivered(&mut self, wire: PaintFrameWire, cx: &mut Context<Self>) {
            self.frames_delivered += 1;
            let counts = {
                let frame = wire.decode();
                (frame.nodes.len(), frame.edges.len(), frame.labels.len())
            };
            self.last_counts = Some(counts);
            let first = self.frames_delivered == 1;
            if first || self.frames_delivered.is_multiple_of(120) {
                log(&format!(
                    "[example-app] frame #{} delivered: {} nodes / {} edges / {} labels",
                    self.frames_delivered, counts.0, counts.1, counts.2,
                ));
            }

            self.view
                .update(cx, |view, cx| view.deliver_worker_frame(wire, cx));
            cx.notify();
        }

        fn status_line(&self) -> String {
            match self.mode {
                FrameMode::InProcess => format!(
                    "frame source: {} · {} vertices · add ?mode=worker to route frames through the worker",
                    self.mode.label(),
                    DEMO_NODE_COUNT,
                ),
                FrameMode::Worker => {
                    let counts = self
                        .last_counts
                        .map(|(n, e, l)| format!("{n} nodes / {e} edges / {l} labels"))
                        .unwrap_or_else(|| "waiting for first frame…".into());
                    format!(
                        "frame source: {} · frames delivered: {} · last frame: {counts}",
                        self.mode.label(),
                        self.frames_delivered,
                    )
                }
            }
        }
    }

    /// Merge the demo graph into `scene` at the circular seed placement.
    fn populate(
        scene: &Entity<GraphScene<String, String, String, String>>,
        cx: &mut Context<ExampleApp>,
    ) {
        let batch = demo_batch();
        scene.update(cx, |scene, cx| {
            scene.merge(batch);
            for index in 0..DEMO_NODE_COUNT {
                let key = format!("n{index}");
                if let Some(id) = scene.node_id(&key) {
                    scene.set_position(id, initial_position(index, DEMO_NODE_COUNT));
                }
            }
            cx.notify();
        });
    }

    /// View state over `scene` with demo styling applied.
    fn styled_view(
        scene: Entity<GraphScene<String, String, String, String>>,
        cx: &mut Context<ExampleApp>,
    ) -> Entity<GraphViewState<String, String, String, String>> {
        let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));
        view.update(cx, |view, cx| {
            view.style_mut().label_style = TextStyle {
                color: white(),
                font_size: rems(0.75).into(),
                ..TextStyle::default()
            };
            cx.notify();
        });
        view
    }

    impl Render for ExampleApp {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.draws += 1;
            match self.mode {
                FrameMode::InProcess => {
                    // Advance the force model by one frame; stop requesting
                    // animation frames once it settles.
                    if !self.settled {
                        let progress = self.scene.update(cx, |scene, cx| {
                            let progress = scene.step_layout_async(FRAME_LAYOUT_BUDGET, cx);
                            cx.notify();
                            progress
                        });
                        if progress == LayoutProgress::Settled {
                            self.settled = true;
                        } else {
                            window.request_animation_frame();
                        }
                    }
                }
                FrameMode::Worker => {
                    // Deliveries drive repaints; keep drawing anyway so late
                    // deliveries keep flowing.
                    if self.draws.is_multiple_of(300) {
                        cx.notify();
                    }
                }
            }

            div()
                .size_full()
                .relative()
                .bg(hsla(0.58, 0.35, 0.09, 1.0))
                .child(GraphView::new(self.view.clone()).size_full())
                .child(
                    div()
                        .absolute()
                        .top_2()
                        .left_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(hsla(0.0, 0.0, 0.0, 0.65))
                        .text_color(white())
                        .text_size(px(12.))
                        .child(self.status_line()),
                )
        }
    }

    fn log(message: &str) {
        web_sys::console::log_1(&JsValue::from_str(message));
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    imp::boot().expect("example app boot failed");
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("gpui-graph web example app targets wasm32-unknown-unknown.");
    eprintln!("Build it with ./build.sh from crates/gpui-graph/examples/web (see README.md).");
    std::process::exit(1);
}
