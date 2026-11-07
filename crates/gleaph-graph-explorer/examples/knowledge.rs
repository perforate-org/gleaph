//! A native graph explorer for `demo/knowledge`.
//!
//! Connects to the live Router cached by `gleaph network start` with the demo's
//! **owner identity** (the Gleaph CLI `dev` key), loads the whole knowledge
//! graph through the Gleaph Rust SDK, and executes three curated prepared
//! queries whose results become transient overlays over a persistent scene.
//! The scene and node positions are built once and `gpui-graph`'s overlay API
//! relights only the presentation when the active query changes — the
//! roadmap's §4/§11 stable-layout contract, driven live.
//!
//! The owner (developer-console) identity is required because the PUBLIC
//! data-plane grants cover the four prepared ops and node reads, but not raw
//! ad-hoc edge traversal with `element_id` returns; whole-graph edge loading is
//! a developer operation, not a public read.
//!
//! Presets auto-cycle every `CYCLE_SECONDS` so the "same graph viewed through
//! another query" effect is observable without a keyboard. **Run** executes the
//! active preset live against the Router on a background runtime (no UI freeze):
//! the fresh presentation and result table replace the preloaded ones, and the
//! status line reports the call's latency or error. `GLEAPH_LIVE_RUN=1` also
//! runs the preset each time the cycler advances.
//!
//! ```sh
//! # prerequisites: a live network (gleaph network start + migration apply)
//! cargo run -p gleaph-graph-explorer --example knowledge
//! ```
//!
//! Notes:
//! - Identity: `GLEAPH_IDENTITY_PEM`, else `~/.config/gleaph/identity/keys/dev.pem`.
//! - `team-readable-documents` is deliberately not in the native set: its
//!   `$query` parameter is a 768-f32 vector produced by the browser-side
//!   generator, and the native presets are parameter-free. It stays a
//!   browser-demo surface.
//! - The three graph-structural presets (citation trail, variable-length reach,
//!   shortest path) need no parameters and highlight real elements.
//!
//! The right column shows what the session now publishes besides the overlay: the active preset's
//! **result table** (virtualized with `uniform_list`, so a large result costs only the visible rows)
//! and the **parameter definitions** from the Router's prepared manifest. A preset with no
//! parameters says so; the panel is what a parameterised console form will bind against.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gleaph_graph_explorer::graph::{
    EdgeLabelQuery, ExplorerEdge, ExplorerNode, GraphLoadSpec, VertexLabelQuery,
};
use gleaph_graph_explorer::mapping::{EdgeIdentity, VertexIdentity};
use gleaph_graph_explorer::presentation::Presentation;
use gleaph_graph_explorer::query::{
    CITATION_REACH_PRESET, QueryPreset, SHORTEST_PATH_PRESET, VARIABLE_LENGTH_REACH_PRESET,
};
use gleaph_graph_explorer::results::ResultTable;
use gleaph_graph_explorer::session::GraphExplorerSession;
use gleaph_sdk::Parameter;
use gpui::{
    App, Bounds, Context, Entity, Render, SharedString, TextStyle, Window, WindowBounds,
    WindowOptions, div, hsla, prelude::*, px, rems, size, uniform_list, white,
};
use gpui_graph::{
    Color, FontSize, ForceAtlas2, GraphScene, GraphView, GraphViewState, LabelStyle, LayoutBudget,
    LayoutProgress,
};

/// The demo directory, resolved from CARGO_MANIFEST_DIR so this works regardless
/// of the current working directory.
const DEMO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../demo/knowledge");
/// The lazy-issuance Router cache `gleaph network start` + a remote CLI command
/// leave behind (ADR 0068). Hold one JSON string (the principal).
const ROUTER_CACHE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../demo/knowledge/.gleaph/cache/account/local.router.json"
);

/// Per-frame layout work for the ForceAtlas2 relaxation. Small enough not to
/// blow the frame; iteration-bounded so a tiny graph settles in a few frames.
const LAYOUT_BUDGET: LayoutBudget = LayoutBudget {
    max_iterations: 600,
    max_duration: Some(Duration::from_millis(8)),
};

/// Auto-cycle period between query overlays.
const CYCLE_SECONDS: u64 = 5;

/// The native presets: the three graph-structural knowledge queries that run
/// without parameters.
const NATIVE_PRESETS: &[&QueryPreset] = &[
    &CITATION_REACH_PRESET,
    &VARIABLE_LENGTH_REACH_PRESET,
    &SHORTEST_PATH_PRESET,
];

type Scene = GraphScene<VertexIdentity, EdgeIdentity, ExplorerNode, ExplorerEdge>;
type View = GraphViewState<VertexIdentity, EdgeIdentity, ExplorerNode, ExplorerEdge>;

/// One executed preset: its presentation, the result table it published, and the parameter
/// definitions the Router manifest declared for it.
#[derive(Clone)]
struct Slot {
    preset: &'static QueryPreset,
    presentation: Presentation,
    emphasized_nodes: usize,
    emphasized_edges: usize,
    table: Option<ResultTable>,
    parameters: Vec<Parameter>,
}

/// The materialized explorer input handed to the GPUI app: a settled scene and
/// the per-preset presentations.
struct Preloaded {
    scene: Scene,
    slots: Vec<Slot>,
    /// The loaded session (client + identity map), handed to the background runner so live runs
    /// reuse the same connection and identity map as the preload.
    session: GraphExplorerSession,
}

fn main() {
    let router = load_router_principal().expect("resolve Router principal");
    let gateway = resolve_gateway();
    println!("connecting to Router {router} at {gateway} (owner identity)");
    let preloaded = preload_knowledge(router, gateway).expect("load knowledge graph");
    println!(
        "loaded {} vertices / {} edges; {} presets",
        preloaded.scene.graph().node_count(),
        preloaded.scene.graph().edge_count(),
        preloaded.slots.len()
    );

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| Explorer::new(preloaded, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

/// Resolve the Router principal: `GLEAPH_CANISTER` env, else the lazy-issuance cache.
fn load_router_principal() -> Result<candid::Principal, String> {
    if let Some(text) = std::env::var("GLEAPH_CANISTER")
        .ok()
        .filter(|t| !t.trim().is_empty())
    {
        return parse_principal(&text);
    }
    let raw = std::fs::read_to_string(ROUTER_CACHE).map_err(|e| {
        format!(
            "read {}: {e}; run `gleaph network start` + `migration apply` first",
            ROUTER_CACHE
        )
    })?;
    parse_principal(raw.trim().trim_matches('"'))
}

fn parse_principal(text: &str) -> Result<candid::Principal, String> {
    candid::Principal::from_text(text.trim())
        .map_err(|e| format!("invalid principal {text:?}: {e}"))
}

/// The owner identity used to load the whole graph (see the module docs for
/// why the owner is required). `GLEAPH_IDENTITY_PEM`, else the Gleaph CLI
/// identity store's `dev.pem`.
fn load_identity() -> Result<Box<dyn ic_agent::Identity>, String> {
    let default = std::env::var("HOME")
        .map(|h| format!("{h}/.config/gleaph/identity/keys/dev.pem"))
        .map_err(|_| "HOME not set".to_string())?;
    let path = std::env::var("GLEAPH_IDENTITY_PEM")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(default);
    ic_agent::identity::Secp256k1Identity::from_pem_file(&path)
        .map(|id| Box::new(id) as Box<dyn ic_agent::Identity>)
        .map_err(|e| format!("load identity {path}: {e}"))
}

/// Resolve the IC gateway: `GLEAPH_GATEWAY_URL` env, else `icp network status
/// local`, else the Gleaph launcher status file, else the default port.
fn resolve_gateway() -> String {
    if let Some(url) = std::env::var("GLEAPH_GATEWAY_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        return strip_trailing_slash(url);
    }
    if let Some(url) = icp_cli_gateway() {
        return url;
    }
    if let Some(port) = launcher_gateway_port() {
        return format!("http://localhost:{port}");
    }
    "http://localhost:8000".to_string()
}

/// The icp-cli network's `Api Url:` from `icp network status local`.
fn icp_cli_gateway() -> Option<String> {
    let out = std::process::Command::new("icp")
        .args(["network", "status", "local"])
        .current_dir(DEMO_ROOT)
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.trim().strip_prefix("Api Url:") {
            return Some(strip_trailing_slash(rest.trim().to_string()));
        }
    }
    None
}

/// The Gleaph-owned launcher status file's `gateway_port` in `$TMPDIR`.
fn launcher_gateway_port() -> Option<u32> {
    let status = std::env::temp_dir().join("gleaph-local-status/status.json");
    let text = std::fs::read_to_string(status).ok()?;
    let idx = text.find("gateway_port")?;
    let colon = text[idx..].find(':')?;
    text[idx + colon + 1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

fn strip_trailing_slash(url: String) -> String {
    if url.ends_with('/') {
        url[..url.len() - 1].to_string()
    } else {
        url
    }
}

/// The bounded GQL queries that recover the whole knowledge topology
/// (roadmap §19: one all-vertex probe, one query per vertex label for names,
/// one per edge label for id/src/dst).
fn knowledge_load_spec() -> GraphLoadSpec {
    let vertex = |label: &str, display: &str| VertexLabelQuery {
        label: label.to_string(),
        query: format!("MATCH (n:{label}) RETURN element_id(n) AS id, n.{display} AS display"),
    };
    let edge = |label: &str| EdgeLabelQuery {
        label: label.to_string(),
        query: format!(
            "MATCH (a)-[e:{label}]->(b) RETURN element_id(e) AS id, element_id(a) AS src, element_id(b) AS dst"
        ),
    };
    GraphLoadSpec {
        // The all-vertices probe must be label-constrained to be attributable
        // for anonymous (an unconstrained `MATCH (n)` scan is unattributed and
        // thus tenancy-only Forbidden). The loader ignores this result
        // (`vertex_ids` is only a sizing check); the per-label queries below
        // are authoritative. Concept is MATCH-granted to PUBLIC, so this probes
        // a granted, attributable scope.
        all_vertices_query: "MATCH (n:Concept) RETURN element_id(n) AS id".to_string(),
        vertex_label_queries: vec![
            vertex("Concept", "name"),
            vertex("Document", "title"),
            vertex("Person", "name"),
            vertex("Team", "name"),
        ],
        edge_label_queries: vec![
            edge("RELATED_TO"),
            edge("CITES"),
            edge("ABOUT"),
            edge("AUTHORED_BY"),
            edge("OWNS"),
            edge("BELONGS_TO"),
            edge("ROUTED_VIA"),
        ],
    }
}

/// Load the graph, run every native preset, and materialize the scene + slots.
///
/// The SDK's ic-agent transport needs a Tokio reactor for its HTTP calls, so
/// the connect + load + query phase runs inside a dedicated multi-thread
/// runtime rather than on the GPUI main thread. The agent (and its
/// `fetch_root_key`) is built in-reactor too, since `gleaph_sdk::connect`'s
/// internal pollster bridge has no reactor.
fn preload_knowledge(router: candid::Principal, gateway: String) -> Result<Preloaded, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build tokio runtime: {e}"))?;
    let preloaded = rt.block_on(async {
        let identity = load_identity()?;
        let agent = ic_agent::Agent::builder()
            .with_url(&gateway)
            .with_boxed_identity(identity)
            .build()
            .map_err(|e| format!("build ic-agent: {e}"))?;
        agent
            .fetch_root_key()
            .await
            .map_err(|e| format!("fetch root key: {e}"))?;
        let client = gleaph_sdk::create_gleaph_client(Arc::new(
            gleaph_sdk::transport::IcAgentTransport::from_agent(agent, router),
        ));

        let mut session = GraphExplorerSession::new(client);
        session.load_graph(&knowledge_load_spec()).await?;
        let load = session.graph().expect("graph loaded");

        let mut scene = Scene::new().with_layout(Box::new(ForceAtlas2::default()));
        scene.merge(load.batch.clone());
        session.build_map_from_scene(&scene);

        let mut slots = Vec::with_capacity(NATIVE_PRESETS.len());
        for preset in NATIVE_PRESETS {
            session.set_active_preset(preset);
            session.run_active_query().await?;
            let presentation = session.presentation().clone();
            // The manifest is the source of truth for parameter metadata; a network that does not
            // publish it yet must not break the demo, so a missing manifest degrades to "none".
            let parameters = match session.load_parameter_definitions().await {
                Ok(()) => session.parameter_definitions().to_vec(),
                Err(error) => {
                    eprintln!("parameter manifest unavailable for {}: {error}", preset.id);
                    Vec::new()
                }
            };
            slots.push(Slot {
                preset,
                emphasized_nodes: presentation.emphasized_nodes.len(),
                emphasized_edges: presentation.emphasized_edges.len(),
                presentation,
                table: session.result_table().cloned(),
                parameters,
            });
        }
        Ok::<Preloaded, String>(Preloaded {
            scene,
            slots,
            session,
        })
    })?;
    Ok(preloaded)
}

/// What the status line reports about the last live run.
enum LiveStatus {
    Idle,
    Running,
    Ok(Duration),
    Failed(String),
}

/// One live execution request, answered out of order only if the UI queues several (it does not).
struct LiveOutcome {
    request: u64,
    preset_index: usize,
    result: Result<(Presentation, Option<ResultTable>), String>,
    elapsed: Duration,
}

/// The background executor for live runs.
///
/// The SDK's ic-agent transport needs a Tokio reactor and the GPUI frame thread must not block on a
/// Router call, so one thread owns the session and a multi-thread runtime and answers requests in
/// order. The thread ends when the app drops the command sender.
struct LiveRunner {
    commands: mpsc::Sender<(u64, usize, &'static QueryPreset)>,
    outcomes: mpsc::Receiver<LiveOutcome>,
    next_request: u64,
}

impl LiveRunner {
    fn spawn(mut session: GraphExplorerSession) -> Self {
        let (commands, command_rx) = mpsc::channel::<(u64, usize, &'static QueryPreset)>();
        let (outcome_tx, outcomes) = mpsc::channel::<LiveOutcome>();
        std::thread::Builder::new()
            .name("knowledge-live".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                while let Ok((request, preset_index, preset)) = command_rx.recv() {
                    let started = Instant::now();
                    session.set_active_preset(preset);
                    let result = runtime.block_on(async {
                        session.run_active_query().await?;
                        Ok::<_, String>((
                            session.presentation().clone(),
                            session.result_table().cloned(),
                        ))
                    });
                    if outcome_tx
                        .send(LiveOutcome {
                            request,
                            preset_index,
                            result,
                            elapsed: started.elapsed(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn the live query runner");
        Self {
            commands,
            outcomes,
            next_request: 0,
        }
    }

    /// Queue one run and return its request id.
    fn run(&mut self, preset_index: usize, preset: &'static QueryPreset) -> u64 {
        let request = self.next_request;
        self.next_request += 1;
        let _ = self.commands.send((request, preset_index, preset));
        request
    }

    /// Take the next finished run, if any. Never blocks: the frame loop polls once per frame.
    fn poll(&self) -> Option<LiveOutcome> {
        self.outcomes.try_recv().ok()
    }
}

struct Explorer {
    scene: Entity<Scene>,
    view: Entity<View>,
    /// Slots are mutable now: a live run replaces one preset's presentation and table.
    slots: Rc<RefCell<Vec<Slot>>>,
    active: Rc<Cell<usize>>,
    settled: bool,
    last_cycle: Instant,
    node_count: usize,
    edge_count: usize,
    /// The active preset's table, shared with the virtualized list. Cached because `Slot` holds the
    /// rows and a per-frame clone would copy the whole result (the render loop runs every frame).
    table: Rc<ResultTable>,
    /// Runs presets off the frame thread; the frame loop polls it once per render.
    runner: LiveRunner,
    status: LiveStatus,
    /// The request the status line is waiting for; outcomes for any other request are stale.
    pending: Option<u64>,
}

impl Explorer {
    fn new(preloaded: Preloaded, cx: &mut Context<Self>) -> Self {
        let Preloaded {
            scene,
            slots,
            session,
        } = preloaded;
        let node_count = scene.graph().node_count();
        let edge_count = scene.graph().edge_count();
        let slots: Rc<RefCell<Vec<Slot>>> = Rc::new(RefCell::new(slots));
        let active: Rc<Cell<usize>> = Rc::new(Cell::new(0));

        let scene: Entity<Scene> = cx.new(|_cx| scene);

        let slots_for_overlay = slots.clone();
        let active_for_overlay = active.clone();
        let view: Entity<View> = cx.new(|cx| {
            let mut view = View::new(scene.clone(), cx);
            view.set_node_label(|_id, node: &ExplorerNode| Some(node.display.clone()));
            view.set_edge_label(|_id, edge: &ExplorerEdge| Some(edge.label.clone()));
            let slots = slots_for_overlay.clone();
            let active = active_for_overlay.clone();
            view.set_node_overlay(move |id| {
                slots.borrow()[active.get()].presentation.node_category(id)
            });
            let slots = slots_for_overlay.clone();
            let active = active_for_overlay.clone();
            view.set_edge_overlay(move |id| {
                slots.borrow()[active.get()].presentation.edge_category(id)
            });
            view
        });

        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.node_radius = 6.0;
            style.node_stroke_width = 1.2;
            style.node_stroke_color = Color::WHITE;
            style.node_fill = gpui_graph::hsla(215., 0.18, 0.34, 1.0);
            style.node_fill_muted = gpui_graph::hsla(215., 0.08, 0.17, 1.0);
            style.node_fill_overlay = gpui_graph::hsla(162., 0.8, 0.55, 1.0); // teal = emphasized
            style.node_fill_selected = gpui_graph::hsla(215., 0.5, 0.7, 1.0);
            style.node_fill_hovered = gpui_graph::hsla(215., 0.35, 0.5, 1.0);
            style.edge_color = gpui_graph::hsla(215., 0.14, 0.42, 1.0);
            style.edge_color_muted = gpui_graph::hsla(215., 0.05, 0.22, 1.0);
            style.edge_color_overlay = gpui_graph::hsla(40., 0.95, 0.6, 1.0); // amber = trail edges
            style.edge_width = 1.7;
            style.edge_arrow_enabled = true;
            style.label_style = LabelStyle {
                color: gpui_graph::hsla(215., 0.12, 0.86, 1.0),
                font_size: FontSize::Rem(0.78),
                ..LabelStyle::default()
            };
            style.label_offset = 3.0;
            cx.notify();
        });

        let table = Rc::new(
            slots
                .borrow()
                .first()
                .and_then(|slot| slot.table.clone())
                .unwrap_or_default(),
        );
        Self {
            scene,
            view,
            slots,
            active,
            settled: false,
            last_cycle: Instant::now(),
            node_count,
            edge_count,
            table,
            runner: LiveRunner::spawn(session),
            status: LiveStatus::Idle,
            pending: None,
        }
    }

    /// Execute the active preset live. One run at a time: the status line reflects the request it
    /// is waiting for, so a second click while one is in flight is ignored rather than racing.
    fn run_active(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let index = self.active.get();
        let preset = self.slots.borrow()[index].preset;
        let request = self.runner.run(index, preset);
        self.pending = Some(request);
        self.status = LiveStatus::Running;
        self.last_cycle = Instant::now();
        cx.notify();
    }

    /// Adopt one live outcome: the fresh presentation and table replace the preloaded ones.
    fn apply_live(&mut self, outcome: LiveOutcome, cx: &mut Context<Self>) {
        if self.pending != Some(outcome.request) {
            return;
        }
        self.pending = None;
        match outcome.result {
            Ok((presentation, table)) => {
                {
                    let mut slots = self.slots.borrow_mut();
                    let slot = &mut slots[outcome.preset_index];
                    slot.emphasized_nodes = presentation.emphasized_nodes.len();
                    slot.emphasized_edges = presentation.emphasized_edges.len();
                    slot.presentation = presentation;
                    slot.table = table;
                }
                if self.active.get() == outcome.preset_index {
                    self.table = Rc::new(
                        self.slots.borrow()[outcome.preset_index]
                            .table
                            .clone()
                            .unwrap_or_default(),
                    );
                }
                self.status = LiveStatus::Ok(outcome.elapsed);
            }
            Err(error) => self.status = LiveStatus::Failed(error),
        }
        cx.notify();
    }

    /// The right column: the active preset's result table and parameter definitions.
    ///
    /// The table is virtualized (GPUI builds only the visible rows), which is what lets the console
    /// show a large result without turning a frame into a widget construction loop.
    fn side_panels(&self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let table = self.table.clone();
        let row_count = table.row_count();
        let columns: Vec<SharedString> = table
            .columns()
            .iter()
            .map(|c| c.to_owned().into())
            .collect();
        let header = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                div()
                    .flex_1()
                    .text_size(rems(0.72))
                    .text_color(muted_color())
                    .child(SharedString::from(format!("{}. {column}", index + 1)))
            })
            .collect::<Vec<_>>();

        let table_body = if row_count == 0 {
            div()
                .p_2()
                .child(muted_text("No rows materialized.", 0.75))
                .into_any_element()
        } else {
            uniform_list("result-rows", row_count, move |range, _window, _cx| {
                range
                    .map(|row_index| {
                        let cells: Vec<SharedString> = table
                            .row(row_index)
                            .map(|row| row.iter().map(|c| c.clone().into()).collect())
                            .unwrap_or_default();
                        let mut row = div().flex().flex_row().w_full().gap_2().py_0p5();
                        for cell in cells {
                            row = row.child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_size(rems(0.72))
                                    .text_color(hsla(215., 0.12, 0.84, 1.0))
                                    .child(cell),
                            );
                        }
                        row
                    })
                    .collect()
            })
            .flex_grow(1.0)
            .into_any_element()
        };

        let definitions = self.slots.borrow()[self.active.get()].parameters.clone();
        let parameters = if definitions.is_empty() {
            div()
                .child(muted_text("This preset declares no parameters.", 0.75))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(definitions.iter().map(|parameter| {
                    div()
                        .text_size(rems(0.75))
                        .text_color(hsla(215., 0.12, 0.84, 1.0))
                        .child(SharedString::from(parameter_summary(parameter)))
                }))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .w(px(380.))
            .h_full()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow(1.0)
                    .rounded_md()
                    .bg(hsla(215., 0.06, 0.09, 1.0))
                    .p_2()
                    .gap_1()
                    .child(styled_text("Results", 0.85))
                    .children(header)
                    .child(table_body),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .bg(hsla(215., 0.06, 0.09, 1.0))
                    .p_2()
                    .gap_1()
                    .child(styled_text("Parameters", 0.85))
                    .child(parameters),
            )
    }
}

/// One manifest parameter as the demo shows it: name, requiredness, nullability and semantic type.
fn parameter_summary(parameter: &Parameter) -> String {
    let mut summary = format!("${}", parameter.name);
    if parameter.nullable {
        summary.push_str(" nullable");
    }
    if parameter.required {
        summary.push_str(" required");
    } else {
        summary.push_str(" optional");
    }
    summary.push(' ');
    summary.push_str(&format!("{:?}", parameter.semantic_type));
    summary
}

fn muted_color() -> gpui::Hsla {
    hsla(215., 0.1, 0.62, 1.0)
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drive the ForceAtlas2 relaxation until it settles; the scene is not
        // rebuilt thereafter.
        if !self.settled {
            let progress = self.scene.update(cx, |scene, cx| {
                let p = scene.step_layout(LAYOUT_BUDGET);
                cx.notify();
                p
            });
            if progress == LayoutProgress::Settled {
                self.settled = true;
            }
        }
        // Keep a per-frame loop so the overlay auto-cycles; only the overlay
        // changes, never the scene/topology/layout (§4/§11).
        window.request_animation_frame();
        if let Some(outcome) = self.runner.poll() {
            self.apply_live(outcome, cx);
        }
        let now = Instant::now();
        if now.duration_since(self.last_cycle) >= Duration::from_secs(CYCLE_SECONDS) {
            self.last_cycle = now;
            let next = (self.active.get() + 1) % self.slots.borrow().len();
            self.active.set(next);
            self.table = Rc::new(self.slots.borrow()[next].table.clone().unwrap_or_default());
            if std::env::var("GLEAPH_LIVE_RUN").is_ok_and(|v| v == "1") {
                self.run_active(cx);
            }
        }

        let (title, description, emphasized_nodes, emphasized_edges) = {
            let slots = self.slots.borrow();
            let slot = &slots[self.active.get()];
            (
                slot.preset.title,
                slot.preset.description,
                slot.emphasized_nodes,
                slot.emphasized_edges,
            )
        };
        let title = SharedString::from(title);
        let description = SharedString::from(description);
        let counts = SharedString::from(format!(
            "emphasized: {} nodes / {} edges — auto-cycles every {}s",
            emphasized_nodes, emphasized_edges, CYCLE_SECONDS
        ));
        let status = SharedString::from(match &self.status {
            LiveStatus::Idle => "run: idle".to_owned(),
            LiveStatus::Running => "run: running…".to_owned(),
            LiveStatus::Ok(elapsed) => {
                format!("run: ok in {:.1} ms", elapsed.as_secs_f64() * 1000.0)
            }
            LiveStatus::Failed(error) => format!("run failed: {error}"),
        });
        let graph_stats = SharedString::from(format!(
            "{} vertices / {} edges",
            self.node_count, self.edge_count
        ));

        div()
            .id("knowledge-explorer")
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla(215., 0.06, 0.05, 1.0))
            .p_4()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .justify_between()
                    .child(styled_text("Knowledge graph", 1.0))
                    .child(styled_text(graph_stats.clone(), 0.85)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .items_center()
                    .child(styled_text(title.clone(), 1.25))
                    .child(
                        div()
                            .id("run-preset")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(if self.pending.is_some() {
                                hsla(215., 0.05, 0.24, 1.0)
                            } else {
                                hsla(162., 0.6, 0.45, 1.0)
                            })
                            .text_color(hsla(215., 0.18, 0.1, 1.0))
                            .text_size(rems(0.85))
                            .cursor_pointer()
                            .child(if self.pending.is_some() {
                                "Running…"
                            } else {
                                "Run"
                            })
                            .on_click(cx.listener(|this, _event, _window, cx| this.run_active(cx))),
                    )
                    .child(muted_text(status.clone(), 0.8)),
            )
            .child(muted_text(description.clone(), 0.85))
            .child(muted_text(counts.clone(), 0.8))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .gap_2()
                    .child(GraphView::new(self.view.clone()).flex_grow(1.0).size_full())
                    .child(self.side_panels(window, cx)),
            )
    }
}

fn styled_text(text: impl Into<SharedString>, scale: f32) -> impl IntoElement {
    div()
        .text_size(rems(scale))
        .text_color(hsla(215., 0.15, 0.9, 1.0))
        .child(text.into())
}

fn muted_text(text: impl Into<SharedString>, scale: f32) -> impl IntoElement {
    div()
        .text_size(rems(scale))
        .text_color(hsla(215., 0.1, 0.62, 1.0))
        .child(text.into())
}

use gpui::IntoElement;

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_sdk::SemanticType;

    fn parameter(required: bool, nullable: bool) -> Parameter {
        Parameter {
            name: "depth".to_owned(),
            description: None,
            required,
            nullable,
            semantic_type: SemanticType::Int64,
        }
    }

    #[test]
    fn a_parameter_summary_states_requiredness_and_type() {
        assert_eq!(
            parameter_summary(&parameter(true, false)),
            "$depth required Int64"
        );
        assert_eq!(
            parameter_summary(&parameter(false, true)),
            "$depth nullable optional Int64"
        );
    }
}
