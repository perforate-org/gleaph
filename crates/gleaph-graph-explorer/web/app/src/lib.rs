//! ADR 0076 S4a — main-thread web entry for the Gleaph graph explorer.
//!
//! Boots GPUI through `gpui_web`, generates a deterministic demo graph, injects
//! it into the application-owned worker backend (the `worker.js` script plus
//! the `gleaph-explorer-web-worker` wasm module) through the library's
//! `PostMessageChannel` (§18.2: spawn, readiness replay, envelope routing, and
//! frame delivery; application batch bytes cross under
//! [`gpui_graph::worker::web_transport::PipeHandle::send_mutation`] via the
//! explorer codec), switches the view to `FrameSource::Worker`, and renders
//! delivered frames through
//! [`gpui_graph::GraphViewState::deliver_worker_frame`].
//!
//! Deliberately absent from this slice: wasm threads, atomics,
//! SharedArrayBuffer, and COOP/COEP/CORP headers (ADR 0076 S4b). Everything
//! here runs from any static file server.

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::Cell;
    use std::rc::Rc;

    use futures::StreamExt;
    use gpui::{
        App, Context, Entity, Render, SharedString, TextStyle, Window, WindowBounds, WindowOptions,
        div, hsla, prelude::*, px, rems, size, white,
    };
    use gpui_graph::worker::web_transport::{ChannelError, PostMessageChannel};
    use gpui_graph::worker::{SceneMutation, ToWorker};
    use gpui_graph::{
        FrameSource, GraphScene, GraphView, GraphViewState, PaintFrameWire, WorkerChannel,
    };
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
    use web_time::Instant;

    use gleaph_explorer_web_common::ExplorerBatchCodec;
    use gleaph_explorer_web_common::fixture::random_fixture;

    const WORKER_SCRIPT_URL: &str = "worker.js";
    /// Demo-graph size when the page URL carries no `?nodes=` parameter.
    const DEFAULT_NODE_COUNT: usize = 1500;
    const MIN_NODE_COUNT: usize = 50;
    const MAX_NODE_COUNT: usize = 20_000;

    #[wasm_bindgen(start)]
    pub fn run() -> Result<(), JsValue> {
        gpui_platform::web_init();
        log("[explorer-web] main-thread app booting (gpui_web)");

        // Web is an embedded guest: `Platform::run` invokes the launch callback
        // and RETURNS instead of blocking like a native run loop, so plain
        // `Application::run` would drop the last strong `AppCell` reference as
        // soon as this call stack unwinds — the app dies right after its first
        // draw. `run_embedded` hands back an ApplicationHandle that keeps the
        // app alive; park it for the page's lifetime.
        let handle =
            gpui_platform::application_with_web_backend(gpui_platform::WebBackendPreference::Auto)
                .run_embedded(|cx: &mut App| {
                    let bounds = gpui::Bounds::centered(None, size(px(1280.), px(800.)), cx);
                    cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            ..Default::default()
                        },
                        |_window, cx| cx.new(ExplorerApp::new),
                    )
                    .expect("failed to open explorer window");
                    cx.activate(true);
                });
        APP_HANDLE.with(|slot| *slot.borrow_mut() = Some(handle));
        Ok(())
    }

    thread_local! {
        /// Keeps the GPUI application alive for the page's lifetime (web has no
        /// blocking run loop to hold it).
        static APP_HANDLE: std::cell::RefCell<Option<gpui::ApplicationHandle>> =
            const { std::cell::RefCell::new(None) };
    }

    /// One frame handed over from the worker's message event to the GPUI task
    /// that owns the view entity.
    struct DeliveredFrame {
        wire: PaintFrameWire,
        /// Time from posting the triggering snapshot to this message, if the
        /// snapshot was posted through the instrumented channel.
        round_trip_ms: Option<f64>,
    }

    /// Round-trip instrumentation over the library channel.
    ///
    /// All transport behavior — spawn, readiness handshake, ordered replay,
    /// envelope routing, frame delivery — is [`PostMessageChannel`] (§18.2);
    /// this wrapper only stamps each view-driven request so the next delivery
    /// can report latency. The one-shot scene injection goes through the
    /// channel's `PipeHandle` under the application codec, sharing the same
    /// replay queue.
    struct TimedChannel {
        inner: PostMessageChannel<String, String, String, String>,
        posted_at: Rc<Cell<Option<Instant>>>,
    }

    impl WorkerChannel<String, String, String, String> for TimedChannel {
        fn post(&mut self, request: ToWorker<String, String, String, String>) {
            self.posted_at.set(Some(Instant::now()));
            self.inner.post(request);
        }
    }

    struct ExplorerApp {
        view: Entity<GraphViewState<String, String, String, String>>,
        frames_delivered: usize,
        draws: usize,
        last_round_trip_ms: Option<f64>,
        last_counts: Option<(usize, usize, usize)>,
        notice: SharedString,
    }

    impl ExplorerApp {
        fn new(cx: &mut Context<Self>) -> Self {
            let node_count = requested_node_count();
            let use_worker = requested_source() == "worker";
            let fixture = random_fixture(node_count);

            // Spawn + connect the worker first so scene injection lands before
            // the first prepaint snapshot (the library replay queue preserves
            // that order across readiness).
            let posted_at: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
            let (delivery_tx, mut delivery_rx) =
                futures::channel::mpsc::unbounded::<DeliveredFrame>();
            if use_worker {
                let nodes = fixture.batch.nodes.len();
                let edges = fixture.batch.edges.len();

                let mut channel =
                    PostMessageChannel::<String, String, String, String>::spawn(WORKER_SCRIPT_URL)
                        .expect("failed to spawn the explorer worker");
                channel.set_payload_codec(Box::new(ExplorerBatchCodec));
                let mut handle = channel.handle();
                let posted_for_frames = posted_at.clone();
                channel.on_frame(move |wire| {
                    let round_trip_ms = posted_for_frames
                        .get()
                        .map(|start| start.elapsed().as_secs_f64() * 1000.0);
                    let _ = delivery_tx.unbounded_send(DeliveredFrame {
                        wire,
                        round_trip_ms,
                    });
                });
                channel.on_error(|error: ChannelError| {
                    log(&format!("[explorer-web] channel error: {error:?}"))
                });

                // Inject the demo scene into the worker replica. Pre-readiness
                // sends queue in the library and replay in posting order.
                handle.send_mutation(SceneMutation::Merge(fixture.batch.clone()));
                log(&format!(
                    "[explorer-web] scene queued for injection: {nodes} nodes / {edges} edges"
                ));

                // Main-thread scene: same content under FixedLayout, holding the
                // initial placement purely for the view's one-time auto-fit
                // camera; the worker replica animates its own copy.
                let scene = cx.new(|_cx| GraphScene::new());
                scene.update(cx, |scene, cx| {
                    scene.merge(fixture.batch.clone());
                    let ids: Vec<_> = scene.graph().nodes().map(|(id, _)| id).collect();
                    for (id, position) in ids.into_iter().zip(&fixture.positions) {
                        scene.set_position(id, *position);
                    }
                    cx.notify();
                });

                let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));
                view.update(cx, |view, cx| {
                    view.style_mut().label_style = TextStyle {
                        color: white(),
                        font_size: rems(0.75).into(),
                        ..TextStyle::default()
                    };
                    view.connect_worker_channel(Box::new(TimedChannel {
                        inner: channel,
                        posted_at,
                    }));
                    view.set_frame_source(FrameSource::Worker);
                    cx.notify();
                });
                log("[explorer-web] frame source: Worker");

                // Drain worker deliveries into the entity on the GPUI task.
                cx.spawn(async move |this, cx| {
                    log("[explorer-web] delivery task started");
                    while let Some(delivery) = delivery_rx.next().await {
                        let result =
                            this.update(cx, |app, cx| app.on_frame_delivered(delivery, cx));
                        if result.is_err() {
                            log("[explorer-web] entity update failed (app gone?)");
                        }
                    }
                    log("[explorer-web] delivery stream ended");
                })
                .detach();

                Self {
                    view,
                    frames_delivered: 0,
                    last_round_trip_ms: None,
                    last_counts: None,
                    notice: "waiting for first worker frame…".into(),
                    draws: 0,
                }
            } else {
                let scene = cx.new(|_cx| GraphScene::new());
                scene.update(cx, |scene, cx| {
                    scene.merge(fixture.batch.clone());
                    let ids: Vec<_> = scene.graph().nodes().map(|(id, _)| id).collect();
                    for (id, position) in ids.into_iter().zip(&fixture.positions) {
                        scene.set_position(id, *position);
                    }
                    cx.notify();
                });
                let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));
                view.update(cx, |view, cx| {
                    view.style_mut().label_style = TextStyle {
                        color: white(),
                        font_size: rems(0.75).into(),
                        ..TextStyle::default()
                    };
                    cx.notify();
                });
                log("[explorer-web] frame source: InProcess");
                Self {
                    view,
                    frames_delivered: 0,
                    last_round_trip_ms: None,
                    last_counts: None,
                    notice: "InProcess mode".into(),
                    draws: 0,
                }
            }
        }

        fn on_frame_delivered(&mut self, delivery: DeliveredFrame, cx: &mut Context<Self>) {
            self.frames_delivered += 1;
            if delivery.round_trip_ms.is_some() {
                self.last_round_trip_ms = delivery.round_trip_ms;
            }
            let counts = {
                let frame = delivery.wire.decode();
                (frame.nodes.len(), frame.edges.len(), frame.labels.len())
            };
            self.last_counts = Some(counts);

            let first = self.frames_delivered == 1;
            if first || self.frames_delivered.is_multiple_of(120) {
                log(&format!(
                    "[explorer-web] frame #{} delivered: {} nodes / {} edges / {} labels{}",
                    self.frames_delivered,
                    counts.0,
                    counts.1,
                    counts.2,
                    self.last_round_trip_ms
                        .map(|ms| format!(" ({ms:.1} ms round trip)"))
                        .unwrap_or_default(),
                ));
            }
            if first {
                self.notice = "worker frames flowing ✓".into();
            }

            self.view
                .update(cx, |view, cx| view.deliver_worker_frame(delivery.wire, cx));
            cx.notify();
        }

        fn status_line(&self) -> String {
            let counts = self
                .last_counts
                .map(|(n, e, l)| format!("{n} nodes / {e} edges / {l} labels"))
                .unwrap_or_else(|| "—".into());
            let round_trip = self
                .last_round_trip_ms
                .map(|ms| format!("{ms:.1} ms"))
                .unwrap_or_else(|| "—".into());
            format!(
                "{} · frame source: Worker · frames: {} · last frame: {} · round trip: {}",
                self.notice, self.frames_delivered, counts, round_trip
            )
        }
    }

    impl Render for ExplorerApp {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.draws += 1;
            if self.draws == 1 || self.draws.is_multiple_of(300) {
                log(&format!("[explorer-web] render #{}", self.draws));
                // Keep drawing so worker deliveries keep flowing.
                cx.notify();
            }
            div()
                .size_full()
                .relative()
                .bg(hsla(0.6, 0.3, 0.07, 1.0))
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

    fn requested_node_count() -> usize {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|parameter| parameter.strip_prefix("nodes="))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_NODE_COUNT)
            .clamp(MIN_NODE_COUNT, MAX_NODE_COUNT)
    }

    /// `?source=inprocess` runs the same page through the synchronous frame
    /// source for comparison.
    fn requested_source() -> &'static str {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        if search.contains("source=inprocess") {
            "inprocess"
        } else {
            "worker"
        }
    }

    fn log(message: &str) {
        web_sys::console::log_1(&JsValue::from_str(message));
    }
}
