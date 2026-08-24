//! gpui-graph web example — main-thread entry (wasm32 only).
//!
//! Demonstrates the frame-source contract end to end: the same tiny demo
//! graph rendered through the library-default `InProcess` source or, strictly
//! opt-in, through a worker that owns a backend replica off the main thread.
//! The mode is a URL parameter (`?mode=worker` / `?mode=inprocess`); the
//! default mirrors the library: `InProcess`. See README.md for the three
//! wiring layers an application owns.

use std::cell::RefCell;

use futures::StreamExt;
use gpui::{
    App, Context, Entity, Render, TextStyle, Window, WindowBounds, WindowOptions, div, hsla,
    prelude::*, px, rems, size, white,
};
use gpui_graph::worker::SceneMutation;
use gpui_graph::worker::web_transport::PostMessageChannel;
use gpui_graph::{
    FixedLayout, ForceAtlas2, FrameSource, GraphScene, GraphView, GraphViewState, LayoutBudget,
    LayoutProgress, PaintFrameWire,
};
use gpui_graph_web_example_common::{
    DEMO_NODE_COUNT, DemoBatchCodec, demo_batch, initial_position,
};
use gpui_platform::{WebBackendPreference, application_with_web_backend, web_init};
use wasm_bindgen::JsValue;

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
    web_init();
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
            GraphScene::new().with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)))
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

    /// The opt-in path: spawn + connect the worker first so scene injection
    /// lands before the first prepaint snapshot (the replay queue preserves
    /// posting order across readiness), then switch the view to
    /// `FrameSource::Worker`.
    fn worker_mode(mode: FrameMode, cx: &mut Context<Self>) -> Self {
        let (delivery_tx, mut delivery_rx) = futures::channel::mpsc::unbounded::<PaintFrameWire>();

        let mut channel =
            PostMessageChannel::<String, String, String, String>::spawn(WORKER_SCRIPT_URL)
                .expect("failed to spawn the example worker.js");
        channel.set_payload_codec(Box::new(DemoBatchCodec));
        channel.on_frame(move |wire| {
            let _ = delivery_tx.unbounded_send(wire);
        });
        channel.on_error(|error| log(&format!("[example-app] channel error: {error:?}")));
        channel
            .handle()
            .send_mutation(SceneMutation::Merge(demo_batch()));

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

fn main() {
    boot().expect("example app boot failed");
}
