//! A console prototype that draws its graph panel through `graph-view-core`.
//!
//! The panel is the adapter: the core decides what is visible, which labels survive and what a hit
//! is. This app owns camera input, the chrome, the theme and the frame loop.
//!
//! ```sh
//! GRAPH_NODES=10000 GRAPH_EDGES=30000 graph-console
//! CONSOLE_THEME=light graph-console     # snapshot override; otherwise the OS appearance decides
//! ```

use gleaph_result_table::ResultTable;
use graph_view_rune::{panel, theme};

use std::cell::RefCell;

mod schema;

use panel::GraphPanel;
use rune::Align;
use rune::{
    AppSpec, Cx, El, KeyEvent, Shape, TextEdit, TextStyle, Transition, Tx, canvas, col, el, row,
    text,
};
use rune_kit::{Cell, SortDir, Table, TableColumn, TableState};
use theme::{Palette, Theme, ThemeChoice};

const W: f32 = 900.0;
const H: f32 = 560.0;
const OUTER: f32 = 16.0;
const DEFAULT_QUERY: &str = "MATCH (n)-[e]->(m) RETURN n, e, m LIMIT 100";

/// The window size: the graph's nominal canvas, the outer margin, and the inspector column.
/// `main` and every test build the runtime with this, so they cannot drift apart.
fn window_size() -> (f32, f32) {
    (W + 2.0 * OUTER + INSPECTOR_WIDTH, H + 540.0)
}
/// The sidebar's default width; the window opens wide enough to keep the main pane comfortable.
const INSPECTOR_WIDTH: f32 = 260.0;
/// One corner radius for every top-level card. A card is a card: the graph's surface is not a
/// different species with a different corner. Inner wells and controls keep the kit's radii.
const CARD_RADIUS: f32 = 12.0;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// The mutable graph state: the adapter and the frame built from it.
///
/// It lives behind a `RefCell` because the theme can change from the view itself (an `Auto` choice
/// re-resolves when the OS appearance changes) and applying a palette means rebuilding the frame,
/// which a `&State` view cannot otherwise do. Every other mutation happens in a handler.
struct Graph {
    panel: GraphPanel,
    frame: Shape,
    labels: Vec<panel::PlacedLabel>,
    splats: Vec<graph_view_core::PaintEdgeSplat>,
}

impl Graph {
    fn refresh(&mut self) -> bool {
        if !self.panel.sync() {
            return false;
        }
        self.frame = self.panel.shape();
        self.labels = self.panel.labels().to_vec();
        self.splats = self.panel.splats().to_vec();
        true
    }
}

/// What the last run produced. The results area shows the matching state: the table, a spinner or
/// the empty state while running, or the failure with its message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum RunOutcome {
    #[default]
    Rows,
    Empty,
    Failed(String),
}

/// A canvas context menu: where it opened, and what was under the press.
#[derive(Clone, Debug)]
struct GraphMenu {
    /// The anchor in window points (where the menu opens).
    point: rune::PointerPress,
    /// The press in canvas points (the menu's actions use it).
    local: [f32; 2],
    /// What was under the press, if anything: "node N12" or "edge E4".
    target: Option<String>,
}

struct State {
    graph: RefCell<Graph>,
    /// What the user asked for (may be `Auto`).
    choice: ThemeChoice,
    /// The palette actually in use, and the theme it came from.
    theme: std::cell::Cell<Theme>,
    palette: RefCell<Palette>,
    /// Last system appearance the view saw, so handlers can resolve `Auto` without a `Cx`.
    system_dark: std::cell::Cell<bool>,
    pointer: [f32; 2],
    /// Pointer travel at the previous drag phase: `Drag::dx` is cumulative for the gesture.
    drag: [f32; 2],
    selected: Option<String>,
    query: TextEdit,
    status: String,
    since_rebuild: u32,
    /// Mock query result rows until the SDK is wired: vertex, edge, latency.
    results: ResultTable,
    /// The graph schema the labels, legend and result columns come from.
    schema: schema::Schema,
    /// rune-kit's table owns selection, sort and column widths.
    table: TableState,
    /// The Run button's in-flight state: the prototype answers after a short delay so the button's
    /// running affordance and the latency readout are real interactions.
    running_since: Option<std::time::Instant>,
    last_run_ms: Option<f32>,
    /// What the last completed run produced.
    outcome: RunOutcome,
    /// Snapshot only: keep a run in flight instead of completing it, so the spinner can be shot.
    stuck_running: bool,
    /// Display options, mirrored onto the core style when they change.
    /// Springs for this app's motion: colours follow the kit's control spring, and a section's
    /// height follows the kit's disclosure spring (the one the kit names for collapsibles).
    control_spring: rune::Spring,
    disclosure_spring: rune::Spring,
    /// Settings dialog and the completion toast (both live in the kit's overlay layer).
    settings_open: bool,
    toast: Option<(String, String)>,
    /// Whether the layout keeps stepping; switching it off freezes the graph.
    live_layout: bool,
    /// What colours encode: vertex labels, relationship types, or nothing categorical.
    color_by: panel::ColorBy,
    /// Whether the inspector column is shown (cmd-I toggles it).
    inspector_open: bool,
    /// The canvas context menu, opened by a secondary press at that point.
    menu: Option<GraphMenu>,
    /// The canvas size the layout last reported, applied to the panel by `poll` (a view cannot
    /// resize the panel it is reading from).
    pending_canvas: std::cell::Cell<Option<[f32; 2]>>,
    /// Collapsible sections (rune-kit `Collapsible` owns the trigger and disclosure arrow).
    results_open: bool,
    options_open: bool,
    legend_open: bool,
    show_labels: bool,
    curved_edges: bool,
    aggregate_edges: bool,
    pinned: bool,
}

impl State {
    fn table_state(&self) -> &TableState {
        &self.table
    }

    fn table_state_mut(&mut self) -> &mut TableState {
        &mut self.table
    }

    fn result_rows(&self) -> usize {
        self.results.row_count()
    }

    /// Reorder the rows to match the sort the table just applied.
    fn sort_results(&mut self) {
        let Some((column, dir)) = self.table.sort().map(|(c, d)| (c.to_owned(), d)) else {
            return;
        };
        let Some(index) = self
            .schema
            .result_columns
            .iter()
            .position(|declared| declared.header() == column || declared.name == column)
        else {
            return;
        };
        let numeric = self.schema.result_columns[index].unit.is_some();
        self.results.sort_rows_by(|a, b| {
            let (Some(left), Some(right)) = (a.get(index), b.get(index)) else {
                return std::cmp::Ordering::Equal;
            };
            if numeric {
                let left = left.parse::<f64>().unwrap_or_default();
                let right = right.parse::<f64>().unwrap_or_default();
                left.total_cmp(&right)
            } else {
                left.cmp(right)
            }
        });
        if dir == SortDir::Descending {
            self.results.reverse_rows();
        }
    }

    /// Complete the simulated run: record the latency, publish fresh rows and report it.
    ///
    /// The query is really parsed first, so a run fails where a real one would: a query that does not
    /// parse reports the parser's message, an empty result set reports the empty state, and only a
    /// successful run replaces the rows. `CONSOLE_RESULTS_COUNT` overrides the row count for
    /// snapshots of the empty state.
    fn finish_run(&mut self) {
        let elapsed = self
            .running_since
            .take()
            .map(|started| started.elapsed().as_secs_f32() * 1000.0)
            .unwrap_or_default();
        self.last_run_ms = Some(elapsed);
        self.table.clear_selection();
        let parsed = gleaph_gql::parser::parse(&self.query.text);
        let Ok(_program) = parsed else {
            let message = match parsed {
                Err(error) => error.to_string(),
                Ok(_) => unreachable!(),
            };
            self.outcome = RunOutcome::Failed(message.clone());
            self.results = ResultTable::default();
            self.status = "run failed".to_owned();
            self.toast = Some(("Query failed".to_owned(), message));
            return;
        };
        let count = std::env::var("CONSOLE_RESULTS_COUNT")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .or_else(|| limit_of(&self.query.text))
            .unwrap_or(2000);
        self.results = fixture_results(&self.schema, count);
        self.outcome = if count == 0 {
            RunOutcome::Empty
        } else {
            RunOutcome::Rows
        };
        self.status = format!(
            "{} rows in {elapsed:.1} ms",
            grouped(self.results.row_count())
        );
        self.toast = Some((
            "Query finished".to_owned(),
            format!(
                "{} rows in {elapsed:.1} ms",
                grouped(self.results.row_count())
            ),
        ));
    }

    /// Push the display options onto the panel and rebuild the frame.
    fn apply_options(&mut self) {
        let mut graph = self.graph.borrow_mut();
        graph.panel.set_labels(self.show_labels);
        graph.panel.set_curved(self.curved_edges);
        graph.panel.set_aggregate(self.aggregate_edges);
        graph.refresh();
    }

    /// Apply the palette in `self.palette` to the graph and rebuild once.
    fn apply_palette(&mut self) {
        let palette = *self.palette.borrow();
        let mut graph = self.graph.borrow_mut();
        graph.panel.apply_theme(palette.graph_colors());
        graph.refresh();
    }

    /// Set the user's choice, resolve it against the last seen system appearance, and apply.
    ///
    /// The commit animates its colour changes: swapping palettes is a colour change, so the chrome
    /// and the kit's controls cross-fade instead of flashing.
    fn choose_theme(&mut self, choice: ThemeChoice, tx: &mut Tx) {
        self.choice = choice;
        let resolved = choice.resolve(self.system_dark.get());
        self.theme.set(resolved);
        *self.palette.borrow_mut() = resolved.palette();
        self.apply_palette();
        theme::save_choice(choice);
        tx.color_motion(self.control_spring);
    }
}

fn build() -> State {
    let nodes = env_usize("GRAPH_NODES", 10_000);
    let edges = env_usize("GRAPH_EDGES", 30_000);
    let schema = schema::Schema::fixture();
    let labels = panel::LabelTables {
        node: schema.node_labels.clone(),
        edge: schema.edge_labels.clone(),
    };
    let mut panel = GraphPanel::new(nodes, edges, W, H, labels);

    // The environment is a snapshot override; otherwise the saved choice; otherwise follow the OS.
    let choice = ThemeChoice::from_env()
        .or_else(theme::load_choice)
        .unwrap_or_default();
    // The OS appearance is only readable from a `Cx`, so the first frame resolves it; until then
    // assume dark, which the view corrects before anything is painted twice.
    let resolved = choice.resolve(true);
    let palette = resolved.palette();
    let audit = palette.audit();
    if audit.is_empty() {
        eprintln!("APCA: every foreground/background pair meets its role");
    } else {
        for (pair, measured, required) in audit {
            eprintln!("APCA: {pair} is {measured:.1} Lc, needs {required:.1}");
        }
    }
    let motion = palette.kit().motion;
    let color_by = match std::env::var("CONSOLE_COLOR_BY").as_deref() {
        Ok("edges") => panel::ColorBy::EdgeLabel,
        Ok("none") => panel::ColorBy::None,
        _ => panel::ColorBy::NodeLabel,
    };
    panel.set_color_by(color_by);
    panel.apply_theme(palette.graph_colors());
    if let Ok(key) = std::env::var("CONSOLE_FOCUS_NODE")
        && let Ok(key) = key.parse::<u32>()
    {
        panel.focus_key(key, env_usize_f32("CONSOLE_ZOOM", 3.0));
    }
    if let Ok(spec) = std::env::var("CONSOLE_EMPHASIS")
        && let Some((dimension, index)) = spec.split_once(':')
        && let Ok(index) = index.parse::<usize>()
    {
        panel.emphasise(if dimension == "edge" {
            panel::Emphasis::EdgeLabel(index)
        } else {
            panel::Emphasis::NodeLabel(index)
        });
    }
    if std::env::var("CONSOLE_SELECT").is_ok() {
        panel.select_for_snapshot();
    }
    if let Ok(zoom) = std::env::var("CONSOLE_ZOOM")
        && let Ok(zoom) = zoom.parse::<f32>()
        && zoom > 0.0
    {
        panel.zoom_at([W * 0.5, H * 0.5], zoom / panel.zoom().max(f32::EPSILON));
    }
    panel.sync();
    let graph = Graph {
        frame: panel.shape(),
        labels: panel.labels().to_vec(),
        splats: panel.splats().to_vec(),
        panel,
    };
    let mut state = State {
        graph: RefCell::new(graph),
        choice,
        theme: std::cell::Cell::new(resolved),
        palette: RefCell::new(palette),
        system_dark: std::cell::Cell::new(true),
        pointer: [0.0; 2],
        drag: [0.0; 2],
        selected: None,
        query: TextEdit::new(
            std::env::var("CONSOLE_QUERY").unwrap_or_else(|_| DEFAULT_QUERY.to_owned()),
        ),
        status: format!("{nodes} nodes · {edges} edges"),
        since_rebuild: 0,
        results: fixture_results(&schema, limit_of(DEFAULT_QUERY).unwrap_or(2000)),
        schema,
        table: TableState::new(),
        running_since: None,
        last_run_ms: None,
        outcome: RunOutcome::Rows,
        stuck_running: std::env::var("CONSOLE_RUNNING").is_ok(),
        control_spring: motion.control,
        disclosure_spring: motion.disclosure,
        settings_open: std::env::var("CONSOLE_SETTINGS").is_ok(),
        toast: std::env::var("CONSOLE_TOAST").is_ok().then(|| {
            (
                "Query finished".to_owned(),
                "2000 rows in 148 ms".to_owned(),
            )
        }),
        live_layout: std::env::var("CONSOLE_NO_LIVE_LAYOUT").is_err(),
        results_open: std::env::var("CONSOLE_RESULTS_CLOSED").is_err(),
        options_open: std::env::var("CONSOLE_OPTIONS_CLOSED").is_err(),
        legend_open: std::env::var("CONSOLE_LEGEND_CLOSED").is_err(),
        color_by,
        inspector_open: true,
        menu: None,
        pending_canvas: std::cell::Cell::new(None),
        show_labels: true,
        curved_edges: true,
        aggregate_edges: false,
        pinned: false,
    };
    // Snapshot hook: park the keyboard cursor on a node, as the arrow keys would.
    if std::env::var("CONSOLE_CURSOR").is_ok() {
        let mut graph = state.graph.borrow_mut();
        graph.panel.move_cursor(1.0, 0.0);
        graph.panel.move_cursor(1.0, 0.0);
        graph.refresh();
    }
    // Snapshot hooks: a completed run shows its outcome, one left in flight shows the spinner.
    if std::env::var("CONSOLE_MENU").is_ok() {
        let local = [
            state.graph.borrow().panel.size().x * 0.5,
            state.graph.borrow().panel.size().y * 0.5,
        ];
        let target = state
            .graph
            .borrow()
            .panel
            .pick_label(local)
            .map(|(kind, name)| format!("{kind} {name}"));
        state.menu = Some(GraphMenu {
            point: rune::PointerPress { x: 200.0, y: 300.0 },
            local,
            target,
        });
    }
    if std::env::var("CONSOLE_RUN").is_ok() {
        state.running_since = Some(std::time::Instant::now());
        state.finish_run();
    }
    if std::env::var("CONSOLE_RUNNING").is_ok() {
        state.running_since = Some(std::time::Instant::now());
    }
    state
}

fn poll(state: &mut State, _t: f64, _tx: &mut Tx) -> bool {
    if let Some([width, height]) = state.pending_canvas.take() {
        let size = glam::Vec2::new(width, height);
        if width > 1.0 && height > 1.0 && (state.graph.borrow().panel.size() - size).length() > 0.5
        {
            let mut graph = state.graph.borrow_mut();
            graph.panel.resize(width, height);
            graph.refresh();
            return true;
        }
    }
    // Diagnostic: repeat the panel's sync N times in one poll, which reports the steady-state
    // cost directly (the headless frame loop does not re-run the view while at rest).
    if let Ok(count) = std::env::var("CONSOLE_SYNC_REPEAT")
        && let Ok(count) = count.parse::<usize>()
    {
        let mut graph = state.graph.borrow_mut();
        for _ in 0..count {
            graph.panel.pan([0.25, 0.0]);
            graph.panel.sync();
        }
        return true;
    }
    if let Some(started) = state.running_since.filter(|_| !state.stuck_running)
        && started.elapsed() >= std::time::Duration::from_millis(140)
    {
        state.finish_run();
        return true;
    }
    let budget = graph_view_core::LayoutBudget {
        max_iterations: 2,
        max_duration: Some(std::time::Duration::from_millis(2)),
    };
    // Layer B is not available to this host yet: Rune retains the shape, so new positions mean
    // rebuilding it. Publishing a snapshot every `REBUILD_EVERY` frames keeps the frames between
    // them free, and is what a console with live layout has to do until positions and topology are
    // separate buffers.
    state.since_rebuild += 1;
    let mut graph = state.graph.borrow_mut();
    let moved = if state.live_layout {
        graph.panel.step_layout(budget)
    } else {
        false
    };
    if state.since_rebuild >= env_u32("REBUILD_EVERY", 3) {
        state.since_rebuild = 0;
        graph.refresh();
    }
    moved
}

/// Fixture rows in the schema's shape, standing in for the client until it is wired: the columns
/// (and their units) come from the schema, only the values are synthetic.
fn fixture_results(schema: &schema::Schema, count: usize) -> ResultTable {
    let columns = schema
        .result_columns
        .iter()
        .map(schema::ResultColumn::header)
        .collect();
    let rows = (0..count)
        .map(|index| {
            schema
                .result_columns
                .iter()
                .enumerate()
                .map(|(position, column)| match position {
                    0 => format!("N{:04}", (index * 37) % 1000),
                    1 => format!("E{:05}", (index * 41) % 30000),
                    _ if column.unit.is_some() => {
                        format!("{:.2}", 0.4 + (index % 97) as f32 * 0.13)
                    }
                    _ => format!("#{index}"),
                })
                .collect()
        })
        .collect();
    ResultTable::from_rows(columns, rows)
}

/// ⇧⌘L cycles Auto → Light → Dark.
/// `12345` -> `12,345`: counts are read by magnitude, so their digits are grouped.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The `LIMIT` of a query, so the simulated result honours what the text asks for.
fn limit_of(query: &str) -> Option<usize> {
    let upper = query.to_uppercase();
    let at = upper.rfind("LIMIT")?;
    upper[at + "LIMIT".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Start a run: the button and the ⌘↵ shortcut must do exactly the same thing.
fn start_run(state: &mut State, tx: &mut Tx) {
    state.running_since = Some(std::time::Instant::now());
    state.status = "running query…".to_owned();
    // Wake the frame loop so the button leaves its loading state without input.
    tx.wake_after(std::time::Duration::from_millis(160));
}

fn on_key(state: &mut State, key: &KeyEvent, tx: &mut Tx) -> bool {
    // Cmd+Enter runs, as the Run button's shortcut and the query card both promise.
    if key.is_command('\r') || (key.mods.cmd && matches!(key.key, rune::Key::Enter)) {
        start_run(state, tx);
        return true;
    }
    if key.is_command('i') && !key.mods.shift {
        state.inspector_open = !state.inspector_open;
        return true;
    }
    if key.is_command('l') && key.mods.shift {
        let next = state.choice.next();
        state.choose_theme(next, tx);
        return true;
    }
    false
}

/// The theme segmented control. The brand row and the settings dialog both show it, so it is
/// built once here rather than duplicated.
fn theme_control(state: &State, kit: &rune_kit::Kit, width: Option<f32>) -> El {
    let selected = match state.choice {
        ThemeChoice::Auto => 0,
        ThemeChoice::Light => 1,
        ThemeChoice::Dark => 2,
    };
    let control =
        rune_kit::SegmentedControl::new(&["Auto", "Light", "Dark"], Some(selected)).label("Theme");
    // `Fit` in the chrome (equal segments sized by their labels); a fixed width inside a dialog
    // panel, where filling the parent would stretch each segment across the window.
    let control = match width {
        Some(w) => control.width(Some(w)),
        None => control.fit(),
    };
    control
        .on_pick(|s: &mut State, index, tx| {
            let choice = match index {
                0 => ThemeChoice::Auto,
                1 => ThemeChoice::Light,
                _ => ThemeChoice::Dark,
            };
            s.choose_theme(choice, tx);
        })
        .build(kit)
}

/// The legend card: the colour-by list for the current dimension, then the note that explains what
/// a glance cannot (the ring's meaning and where the full label list lives).
fn legend_card(state: &State, p: &Palette, kit: &rune_kit::Kit) -> El {
    let title = match state.color_by {
        panel::ColorBy::NodeLabel => "Labels",
        panel::ColorBy::EdgeLabel => "Relationships",
        panel::ColorBy::None => "Colors",
    };
    let mode = state.color_by;
    let rows: Vec<El> = state
        .graph
        .borrow()
        .panel
        .color_rows()
        .into_iter()
        .enumerate()
        .map(|(index, (name, color, count))| {
            let emphasis = match mode {
                panel::ColorBy::NodeLabel => Some(panel::Emphasis::NodeLabel(index)),
                panel::ColorBy::EdgeLabel => Some(panel::Emphasis::EdgeLabel(index)),
                panel::ColorBy::None => None,
            };
            let filtered = emphasis.is_some_and(|e| state.graph.borrow().panel.is_filtered(e));
            let mut entry = row()
                .w_full()
                .gap(6.0)
                .px(4.0)
                .py(2.0)
                .radius(4.0)
                // Rows are controls: focusable, named, pressed from Enter or Space, and delayed so
                // sweeping the pointer down the list does not rebuild the graph on every row.
                .focusable(true)
                .role("button")
                .label(format!("Filter by {name}"))
                .hover_delays(
                    std::time::Duration::from_millis(80),
                    std::time::Duration::from_millis(120),
                )
                .hover(p.track);
            if filtered {
                entry = entry.fill(p.track).outline(1.0, 0.0, p.accent.alpha(0.45));
            }
            let mut entry = entry
                .w_full()
                .gap(6.0)
                .center_y()
                .child(
                    row()
                        .w(12.0)
                        .center_y()
                        .child(el().size(8.0, 8.0).radius(4.0).fill(color)),
                )
                .child(
                    text(name)
                        .font(TextStyle::new(11.0, 400.0))
                        .color(p.text_muted),
                )
                .child(el().grow(1.0))
                .child(
                    text(count.to_string())
                        .font(TextStyle::new(11.0, 400.0).features("'tnum' 1"))
                        .color(p.text_faint),
                );
            if let Some(emphasis) = emphasis {
                let hover = emphasis;
                entry = entry
                    .on_hover_change(move |s: &mut State, inside, _tx| {
                        let mut graph = s.graph.borrow_mut();
                        graph.panel.set_hover_emphasis(inside.then_some(hover));
                        graph.refresh();
                    })
                    .on_press_tx(move |s: &mut State, _tx| {
                        let mut graph = s.graph.borrow_mut();
                        graph.panel.toggle_filter(emphasis);
                        graph.refresh();
                    })
                    .on_key(move |s: &mut State, key: &KeyEvent, _tx| {
                        let activate = matches!(key.key, rune::Key::Enter)
                            || matches!(&key.key, rune::Key::Text(t) if t == " ");
                        if !activate {
                            return false;
                        }
                        let mut graph = s.graph.borrow_mut();
                        graph.panel.toggle_filter(emphasis);
                        graph.refresh();
                        true
                    });
            }
            entry
        })
        .collect();
    // Wordless: a short label, with the meaning one hover away.
    let note = text("Ring: extra labels")
        .font(TextStyle::new(10.0, 400.0))
        .color(p.text_faint)
        .help(
            "A ring around a node shows its extra labels: up to three sectors, then grey for \
             the rest. Select a node to see every label.",
        );
    let mut card = rune_kit::Collapsible::new(title, state.legend_open)
        .ghost()
        .on_toggle(|s: &mut State| s.legend_open = !s.legend_open)
        .child(col().w_full().gap(2.0).children(rows));
    if state.color_by == panel::ColorBy::None {
        card = card.child(
            text("No categorical colour.")
                .font(TextStyle::new(10.0, 400.0))
                .color(p.text_faint),
        );
    }
    card.child(el().h(8.0)).child(note).build(kit)
}

fn view(state: &State, cx: &mut Cx<State>) -> El {
    // An `Auto` choice follows the OS appearance live; resolve it here, where the `Cx` is available,
    // and rebuild the frame only when the answer actually changed.
    let resolved = state.choice.resolve(cx.system_dark());
    state.system_dark.set(cx.system_dark());
    if resolved != state.theme.get() {
        state.theme.set(resolved);
        *state.palette.borrow_mut() = resolved.palette();
        // The palette borrow is dropped before the graph borrow: `apply_palette` needs `&mut self`.
        let mut graph = state.graph.borrow_mut();
        let palette = *state.palette.borrow();
        graph.panel.apply_theme(palette.graph_colors());
        graph.refresh();
    }

    let p = *state.palette.borrow();
    let graph_state = state.graph.borrow();

    let mut graph = canvas(graph_state.frame.clone())
        .key("graph")
        .w_full()
        .h_full()
        .role("img")
        .label(match graph_state.panel.cursor_label() {
            Some(node) => format!(
                "Graph: {} nodes, {} edges. Cursor on {node}.",
                graph_state.panel.drawn().0,
                graph_state.panel.drawn().1
            ),
            None => format!(
                "Graph: {} nodes, {} edges. Use arrow keys to move a cursor.",
                graph_state.panel.drawn().0,
                graph_state.panel.drawn().1
            ),
        })
        .help("Select a node to inspect it. Hover a legend row to emphasise its members.")
        .on_layout_tx(|s: &mut State, rect: [f32; 4], _tx: &mut Tx| {
            if rect[2] > 2.0 && rect[3] > 2.0 {
                s.pending_canvas.set(Some([rect[2], rect[3]]));
            }
        })
        .on_pointer_move_local(|s: &mut State, e, _tx| {
            s.pointer = [e.x, e.y];
            // A changed hover relights the element; `refresh` rebuilds the frame the view reads.
            let mut graph = s.graph.borrow_mut();
            if graph.panel.hover_at([e.x, e.y]) {
                graph.refresh();
            }
        })
        .focusable(true)
        .on_key(|s: &mut State, key: &KeyEvent, _tx: &mut Tx| {
            let step = match key.key {
                rune::Key::Left => Some([-1.0, 0.0]),
                rune::Key::Right => Some([1.0, 0.0]),
                rune::Key::Up => Some([0.0, -1.0]),
                rune::Key::Down => Some([0.0, 1.0]),
                _ => None,
            };
            let mut graph = s.graph.borrow_mut();
            if let Some([dx, dy]) = step {
                let moved = graph.panel.move_cursor(dx, dy);
                if moved {
                    graph.refresh();
                }
                return moved;
            }
            match key.key {
                rune::Key::Enter => {
                    let changed = graph.panel.select_cursor();
                    if changed {
                        graph.refresh();
                    }
                    changed
                }
                rune::Key::Escape => {
                    let changed = graph.panel.clear_cursor_and_selection();
                    if changed {
                        graph.refresh();
                    }
                    changed
                }
                _ => false,
            }
        })
        .on_secondary_press_local(|s: &mut State, e, _tx| {
            let local = [e.x, e.y];
            let target = {
                let graph = s.graph.borrow();
                graph
                    .panel
                    .pick_label(local)
                    .map(|(kind, name)| format!("{kind} {name}"))
            };
            s.menu = Some(GraphMenu {
                point: e.window(),
                local,
                target,
            });
        })
        .on_press_tx(|s: &mut State, _tx| {
            let pointer = s.pointer;
            let mut graph = s.graph.borrow_mut();
            graph.panel.click_at(pointer);
            s.selected = graph
                .panel
                .pick_label(pointer)
                .map(|(kind, name)| format!("{kind} {name}"));
            graph.refresh();
        })
        .on_drag(|s: &mut State, drag, _tx| match drag.phase {
            rune::Phase::Down => s.drag = [drag.dx, drag.dy],
            rune::Phase::Move => {
                let delta = [drag.dx - s.drag[0], drag.dy - s.drag[1]];
                s.drag = [drag.dx, drag.dy];
                let mut graph = s.graph.borrow_mut();
                graph.panel.pan(delta);
                graph.refresh();
            }
            rune::Phase::Up => s.drag = [0.0; 2],
        })
        .on_wheel(|s: &mut State, e, _tx| {
            let mut graph = s.graph.borrow_mut();
            if e.mods.cmd {
                let factor = (1.0 - e.dy * 0.01).clamp(0.5, 2.0);
                graph.panel.zoom_at([e.x, e.y], factor);
            } else {
                graph.panel.pan([e.dx, e.dy]);
            }
            graph.refresh();
            true
        })
        .on_magnify(|s: &mut State, e, _tx| {
            let mut graph = s.graph.borrow_mut();
            graph.panel.zoom_at([e.x, e.y], 1.0 + e.delta);
            graph.refresh();
            true
        });
    // The aggregate layer: a coarse density image, upscaled bilinearly by the shader, under the
    // nodes and labels. No per-cell geometry is drawn.
    if let Some((image, us)) = graph_state.panel.density() {
        if env_u32("CONSOLE_DENSITY_LOG", 0) == 1 {
            eprintln!("density: image {} us", us);
        }
        graph = graph.child(el().absolute(0.0, 0.0).size(W, H).pixels(image));
    }
    for label in panel::label_elements(&graph_state.labels, p.text, p.accent, p.bg) {
        graph = graph.child(label);
    }
    drop(graph_state);

    let kit = p.kit();
    // The chrome: one row that drags the window and clears the traffic lights. It lives in the
    // card's leading column, so the inspector's divider can run from the card's top edge down -
    // a title bar that stops at the divider instead of crossing it.
    let titlebar_inset = cx.titlebar_inset();
    let toolbar = row()
        .key("toolbar")
        .w_full()
        .h(48.0)
        .pl(if titlebar_inset > 0.0 {
            titlebar_inset + 8.0
        } else {
            12.0
        })
        .pr(12.0)
        .center_y()
        .gap(10.0)
        .window_drag()
        .child(
            text("Gleaph")
                .font(TextStyle::new(15.0, 700.0))
                .color(p.text),
        )
        .child(
            text("graph console")
                .font(TextStyle::new(13.0, 400.0))
                .color(p.text_faint),
        )
        .child(
            text(state.status.clone())
                .font(TextStyle::new(11.5, 400.0).features("'tnum' 1"))
                .color(p.text_muted),
        )
        .child(el().grow(1.0))
        .child(
            rune_kit::Button::new("Settings")
                .variant(rune_kit::ButtonVariant::Ghost)
                .on_press(|s: &mut State| s.settings_open = true)
                .build(&kit),
        )
        .child(theme_control(state, &kit, None));

    let query_panel = col()
        .key("query-card")
        .w_full()
        .gap(6.0)
        .px(12.0)
        .py(12.0)
        .child(
            text("Query")
                .font(TextStyle::new(11.0, 600.0))
                .color(p.text_muted),
        )
        .child(
            // A query is code: a monospaced face and GQL token colours, so keywords, strings,
            // numbers and parameters read apart at a glance.
            rune_kit::TextInput::new(&state.query)
                .on_edit(|s: &mut State| &mut s.query)
                .text_style(TextStyle::new(12.5, 400.0).mono())
                .spans(query_spans(&state.query.text, &p.syntax_theme()))
                .label("Query")
                .width(None)
                .build(&kit),
        )
        .child(el().h(4.0))
        .child(
            row()
                .w_full()
                .gap(10.0)
                .child(
                    row()
                        .gap(4.0)
                        .center_y()
                        .child(
                            rune_kit::Keybind::new(&["⌘", "↵"])
                                .variant(rune_kit::KeybindVariant::Ghost)
                                .build(&kit),
                        )
                        .child(
                            text("run")
                                .font(TextStyle::new(10.0, 400.0))
                                .color(p.text_faint),
                        ),
                )
                .child(
                    text(match (&state.outcome, state.last_run_ms) {
                        (RunOutcome::Failed(_), _) => "last run failed".to_owned(),
                        (_, Some(ms)) => {
                            format!(
                                "last run {} rows · {ms:.1} ms",
                                grouped(state.results.row_count())
                            )
                        }
                        (_, None) => "no run yet".to_owned(),
                    })
                    .font(TextStyle::new(10.0, 400.0))
                    .color(p.text_faint),
                ),
        );

    let running = state.running_since.is_some();
    let run_button = {
        let mut button =
            rune_kit::Button::new(if running { "Running…" } else { "Run" }).shortcut("Cmd+Enter");
        button = if running {
            button.variant(rune_kit::ButtonVariant::Loading)
        } else {
            button.variant(rune_kit::ButtonVariant::Submit)
        };
        button.on_press_tx(start_run).build(&kit)
    };

    let options_panel = col()
        .key("options-group")
        .w_full()
        .gap(4.0)
        .child(
            rune_kit::Collapsible::new("Options", state.options_open)
                .ghost()
                .on_toggle(|s: &mut State| s.options_open = !s.options_open)
                .child(
                    rune_kit::Checkbox::new(state.show_labels)
                        .label("Labels")
                        .on_toggle(|s: &mut State| {
                            s.show_labels = !s.show_labels;
                            s.apply_options();
                        })
                        .build(&kit),
                )
                .child(
                    rune_kit::Checkbox::new(state.curved_edges)
                        .label("Curved edges")
                        .on_toggle(|s: &mut State| {
                            s.curved_edges = !s.curved_edges;
                            s.apply_options();
                        })
                        .build(&kit),
                )
                .child(
                    rune_kit::Checkbox::new(state.aggregate_edges)
                        .label("Aggregate edges")
                        .on_toggle(|s: &mut State| {
                            s.aggregate_edges = !s.aggregate_edges;
                            s.apply_options();
                        })
                        .build(&kit),
                )
                .child(
                    rune_kit::Switch::new(state.live_layout)
                        .label("Live layout")
                        .on_toggle_tx(|s: &mut State, _tx: &mut Tx| {
                            s.live_layout = !s.live_layout;
                        })
                        .build(&kit),
                )
                .child(
                    rune_kit::Checkbox::new(state.pinned)
                        .label("Pin selection")
                        .on_toggle(|s: &mut State| {
                            s.pinned = !s.pinned;
                            let changed = s.graph.borrow_mut().panel.set_selection_pinned(s.pinned);
                            if changed {
                                s.graph.borrow_mut().refresh();
                            }
                        })
                        .build(&kit),
                )
                .build(&kit),
        )
        .child(
            rune_kit::SegmentedControl::new(
                &["Nodes", "Edges", "Off"],
                Some(match state.color_by {
                    panel::ColorBy::NodeLabel => 0,
                    panel::ColorBy::EdgeLabel => 1,
                    panel::ColorBy::None => 2,
                }),
            )
            .label("Color by")
            .fit()
            .on_pick(|s: &mut State, index, _tx| {
                let mode = match index {
                    0 => panel::ColorBy::NodeLabel,
                    1 => panel::ColorBy::EdgeLabel,
                    _ => panel::ColorBy::None,
                };
                s.color_by = mode;
                let mut graph = s.graph.borrow_mut();
                graph.panel.set_color_by(mode);
                graph.refresh();
            })
            .build(&kit)
            .help(
                "What colour encodes: vertex labels, relationship types, or nothing categorical.",
            ),
        )
        .child(el().h(10.0))
        .child(legend_card(state, &p, &kit));

    let inspector_panel = col()
        .key("inspector-group")
        .w_full()
        .gap(4.0)
        .child(
            text("Inspector")
                .font(TextStyle::new(11.0, 600.0))
                .color(p.text_muted),
        )
        .child(
            text(state.selected.clone().unwrap_or_else(|| {
                state
                    .graph
                    .borrow()
                    .panel
                    .cursor_label()
                    .map(|node| format!("cursor {node} · Enter selects"))
                    .unwrap_or_else(|| "click a node or edge".to_owned())
            }))
            .font(TextStyle::new(12.0, 400.0))
            .color(p.text_muted),
        )
        .child(
            text(format!(
                "zoom {:.2} · {}",
                state.graph.borrow().panel.zoom(),
                if state.graph.borrow().panel.layout_settled() {
                    "settled"
                } else {
                    "running"
                }
            ))
            .font(TextStyle::new(11.0, 400.0))
            .color(p.text_faint),
        )
        .child({
            let labels = state.graph.borrow().panel.selected_labels();
            text(if labels.is_empty() {
                "labels —".to_owned()
            } else {
                format!("labels: {}", labels.join(", "))
            })
            .font(TextStyle::new(11.0, 400.0))
            .color(p.text_muted)
        })
        .child(run_button);

    // Results: rune-kit's Table owns the header, virtualization, sorting, selection and column
    // resizing; the schema supplies the columns (and their units, so a column never shows a bare
    // number), the table supplies the cells.
    let row_id = |s: &State, index: usize| {
        let first = s
            .results
            .row(index)
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default();
        format!("{first}-{index}").into()
    };
    let mut results_table = Table::new(
        State::table_state,
        State::table_state_mut,
        State::result_rows,
        row_id,
    );
    for (position, declared) in state.schema.result_columns.iter().enumerate() {
        let header = declared.header();
        let mut column = TableColumn::new(header.clone(), header);
        column = if declared.unit.is_some() || position >= 2 {
            column.width(104.0).align(Align::End).sortable()
        } else {
            column.flex(1.0)
        };
        results_table = results_table.column(column);
    }
    let results_table = results_table
        .zebra(true)
        .name("Results")
        .cell(|s: &State, index: usize, column: &str| {
            let position = s
                .schema
                .result_columns
                .iter()
                .position(|declared| declared.header() == column || declared.name == column)
                .unwrap_or_default();
            let cell = s
                .results
                .row(index)
                .and_then(|row| row.get(position))
                .cloned()
                .unwrap_or_default();
            Cell::text(cell).mono()
        })
        .on_sort(
            |s: &mut State, _column: &str, _direction: SortDir, _tx: &mut Tx| {
                s.sort_results();
            },
        )
        .build(state, &kit)
        // The kit expects the caller to bound the table: it grows into the frame it is given.
        .grow(1.0)
        .min_h(0.0);

    let running = state.running_since.is_some();
    let results_body = col()
        .w_full()
        .h(246.0)
        .transition(Transition::offset(0.0, -6.0).combined(Transition::opacity()));
    // Every state is designed: running, failed, empty, rows. The area keeps its height so the table
    // does not jump when a run lands.
    let results_body = if running {
        results_body.child(
            row()
                .w_full()
                .h_full()
                .center()
                .gap(8.0)
                .child(rune_kit::Spinner::new().size(16.0).build(&kit))
                .child(
                    text("Running query…")
                        .font(TextStyle::new(12.0, 400.0))
                        .color(p.text_muted),
                ),
        )
    } else {
        match &state.outcome {
            RunOutcome::Failed(message) => results_body
                .gap(6.0)
                .child(
                    text("Query failed")
                        .font(TextStyle::new(12.0, 600.0))
                        .color(kit.c.danger.fg),
                )
                .child(
                    text(message.clone())
                        .font(TextStyle::new(11.0, 400.0).mono())
                        .color(p.text_muted)
                        .wrap(),
                ),
            RunOutcome::Empty => results_body
                .gap(6.0)
                .child(
                    text("No rows")
                        .font(TextStyle::new(12.0, 600.0))
                        .color(p.text),
                )
                .child(
                    text("Widen the pattern or raise the limit.")
                        .font(TextStyle::new(11.0, 400.0))
                        .color(p.text_faint),
                ),
            RunOutcome::Rows => results_body.child(results_table),
        }
    };
    let results_panel = col().w_full().h_full().px(12.0).py(12.0).child(
        rune_kit::Collapsible::new(
            if running {
                "Results · running…".to_owned()
            } else {
                match &state.outcome {
                    RunOutcome::Failed(_) => "Results".to_owned(),
                    RunOutcome::Empty => "Results · 0 rows".to_owned(),
                    RunOutcome::Rows => {
                        format!("Results · {} rows", grouped(state.results.row_count()))
                    }
                }
            },
            state.results_open,
        )
        .on_toggle(|s: &mut State| s.results_open = !s.results_open)
        .child(results_body)
        .build(&kit),
    );

    // The graph sits on its own surface with a hairline border: the shell's content canvas.
    let graph_panel = col()
        .key("graph-card")
        .padding(12.0)
        .radius(8.0)
        .clip_rounded(8.0)
        .fill(p.bg)
        .border(1.0, p.border)
        .child(graph);

    // The shell owns the window frame: a toolbar that drags the window and clears the traffic
    // lights, a framed main pane, and a resizable inspector pane. The app owns only the contents.
    // The inspector is not a second rectangle: it is a column of the same main pane, attached to
    // the graph by a 1-point rule. One surface, one frame.
    let mut inspector_column = col()
        .key("inspector-column")
        .w(INSPECTOR_WIDTH)
        .h_full()
        .gap(12.0)
        .px(12.0)
        .pb(12.0)
        .pt(60.0)
        .child(inspector_panel)
        .child(el().w_full().h(1.0).fill(p.border))
        .child(options_panel);
    if !state.inspector_open {
        inspector_column = inspector_column.w(0.0).a11y_hidden();
    }

    let mut results_section = row()
        .key("results-section")
        .w_full()
        .px(12.0)
        .h(if state.results_open { 342.0 } else { 56.0 });
    if !cx.is_reduced_motion() {
        results_section = results_section.fold_glide(state.disclosure_spring);
    }
    let rule = || el().w_full().h(1.0).fill(p.border);
    let graph_column = col()
        .key("main-content")
        .grow(1.0)
        .min_h(0.0)
        .child(query_panel)
        .child(rule())
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(12.0)
                .py(12.0)
                .child(graph_panel.grow(1.0).h_full()),
        )
        .child(rule())
        .child(results_section.child(results_panel));

    let leading = col()
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .child(toolbar)
        .child(graph_column);
    let mut card = row()
        .key("card")
        .w_full()
        .h_full()
        .radius(CARD_RADIUS)
        .clip_rounded(CARD_RADIUS)
        .fill(p.panel)
        .border(1.0, p.border)
        .child(leading);
    if state.inspector_open {
        // From the card's top edge to its bottom edge: the title bar stops at it.
        card = card.child(el().w(1.0).h_full().fill(p.border));
    }
    let page = col()
        .w_full()
        .h_full()
        .fill(p.bg)
        .padding(12.0)
        .child(card.child(inspector_column));
    let page = page;

    // Overlays: the kit renders these above the page and owns their layering, scrim and focus.
    let mut overlays = Vec::new();
    if state.settings_open {
        let (nodes, edges) = state.graph.borrow().panel.drawn();
        let body = col()
            .w_full()
            .gap(12.0)
            .px(16.0)
            .pb(16.0)
            .child(
                text("Appearance")
                    .font(TextStyle::new(11.0, 600.0))
                    .color(p.text_muted),
            )
            .child(theme_control(state, &kit, Some(240.0)))
            .child(
                text("Graph")
                    .font(TextStyle::new(11.0, 600.0))
                    .color(p.text_muted),
            )
            .child(
                text(format!(
                    "{nodes} nodes · {edges} edges drawn · zoom {:.2}",
                    state.graph.borrow().panel.zoom()
                ))
                .font(TextStyle::new(12.0, 400.0))
                .color(p.text_muted),
            )
            .child(
                text("Contrast is measured with APCA; every pair in the palette meets its role.")
                    .font(TextStyle::new(12.0, 400.0))
                    .color(p.text_faint)
                    .wrap(),
            );
        rune_kit::Dialog::new("settings", "Settings")
            .description("Appearance and graph diagnostics")
            .width(460.0)
            .fit()
            .open(true)
            .on_close(|s: &mut State| s.settings_open = false)
            .child(body)
            .build(&kit, &mut overlays);
    }
    if let Some(menu) = state.menu.clone() {
        let local = menu.local;
        let on_element = menu.target.is_some();
        let mut context = rune_kit::Menu::new("graph-menu")
            .at(menu.point.x, menu.point.y)
            .open(true)
            .on_close(|s: &mut State| s.menu = None);
        context = context.item(
            rune_kit::MenuItem::new(if menu.target.is_some() {
                "Select"
            } else {
                "Clear selection"
            })
            .on_select(move |s: &mut State| {
                s.menu = None;
                let mut graph = s.graph.borrow_mut();
                if on_element {
                    graph.panel.click_at(local);
                } else {
                    graph.panel.clear_selection();
                }
                graph.refresh();
            }),
        );
        if let Some(target) = &menu.target {
            let text = target.clone();
            context = context.item(
                rune_kit::MenuItem::new("Copy id")
                    .shortcut("⌘C")
                    .on_select_tx(move |s: &mut State, tx: &mut Tx| {
                        s.menu = None;
                        tx.copy(text.clone());
                    }),
            );
        }
        context = context
            .separator()
            .item(
                rune_kit::MenuItem::new("Zoom in").on_select(move |s: &mut State| {
                    s.menu = None;
                    let mut graph = s.graph.borrow_mut();
                    graph.panel.zoom_at(local, 1.25);
                    graph.refresh();
                }),
            )
            .item(
                rune_kit::MenuItem::new("Zoom out").on_select(move |s: &mut State| {
                    s.menu = None;
                    let mut graph = s.graph.borrow_mut();
                    graph.panel.zoom_at(local, 0.8);
                    graph.refresh();
                }),
            )
            .item(
                rune_kit::MenuItem::new("Fit graph").on_select(|s: &mut State| {
                    s.menu = None;
                    let mut graph = s.graph.borrow_mut();
                    graph.panel.fit();
                    graph.refresh();
                }),
            );
        context.build(&kit, &mut overlays);
    }
    if let Some((title, description)) = &state.toast {
        let toast = rune_kit::Toast::new("run-done", title.clone())
            .description(description.clone())
            .closable()
            .on_close(|s: &mut State| s.toast = None)
            .build(&kit);
        overlays.push(rune_kit::ToastRegion::new().toast(toast).build(&kit));
    }
    rune_kit::root(page, overlays)
}

fn main() {
    rune::run(AppSpec {
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("graph-console", window_size(), build, view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kit needs the appearance flag to pick its own structural defaults, and a light theme must
    /// not report `dark` (a light window with a lighter panel is not a dark theme).
    #[test]
    fn the_palette_reports_its_appearance_to_the_kit() {
        assert!(Palette::dark().dark);
        assert!(!Palette::light().dark);
        assert!(Palette::dark().kit().dark);
        assert_eq!(Palette::light().kit().dark, Palette::light().dark);
    }

    /// A secondary press on the canvas opens the context menu, anchored where it landed.
    #[test]
    fn a_secondary_press_opens_the_canvas_menu() {
        let mut rt = rune::Runtime::new(build(), view, window_size(), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        assert!(rt.state.menu.is_none(), "no menu before a press");
        // A point inside the graph well (the card starts at 12, the toolbar is 48 tall).
        rt.secondary_down(300.0, 400.0);
        rt.secondary_up(300.0, 400.0);
        rt.frame(0.0);
        let menu = rt
            .state
            .menu
            .as_ref()
            .expect("a secondary press opens the menu");
        assert!(
            menu.point.x > 0.0 && menu.point.y > 0.0,
            "the menu anchors at the press"
        );
    }

    /// A query that does not parse must not replace the rows with a guess: it reports the parser's
    /// message and clears the table.
    #[test]
    fn a_failed_query_reports_the_parser_message() {
        let mut state = build();
        state.query = TextEdit::new("MATCH (n RETURN");
        state.running_since = Some(std::time::Instant::now());
        state.finish_run();
        match &state.outcome {
            RunOutcome::Failed(message) => {
                assert!(message.contains("parse error"), "message was {message:?}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(state.results.is_empty(), "a failed run publishes no rows");
        assert_eq!(state.status, "run failed");
    }

    /// A successful run asks the mock for what the query's `LIMIT` says, and `LIMIT 0` is the empty
    /// state - not a failure and not a fake row.
    #[test]
    fn the_run_honours_the_limit_and_reports_the_empty_state() {
        let mut state = build();
        state.query = TextEdit::new("MATCH (n:User)-[e:LIKES]->(m:Post) RETURN n, e, m LIMIT 7");
        state.running_since = Some(std::time::Instant::now());
        state.finish_run();
        assert_eq!(state.outcome, RunOutcome::Rows);
        assert_eq!(state.results.row_count(), 7, "the limit bounds the rows");

        state.query = TextEdit::new("MATCH (n:User)-[e:LIKES]->(m:Post) RETURN n, e, m LIMIT 0");
        state.running_since = Some(std::time::Instant::now());
        state.finish_run();
        assert_eq!(state.outcome, RunOutcome::Empty);
        // A zero-row read still carries the schema's columns: only the rows are empty.
        assert_eq!(state.results.row_count(), 0);
        assert!(!state.results.columns().is_empty());
        assert!(
            state.status.starts_with("0 rows in"),
            "status was {:?}",
            state.status
        );
    }

    /// The query card and the Run button both promise Cmd+Enter: it must start a run.
    #[test]
    fn command_enter_starts_a_run() {
        let mut rt = rune::Runtime::new(build(), view, window_size(), rune::Env::new(true, 1.0));
        // `App::run` installs the app-level key handler; a bare `Runtime` must be told, exactly as
        // `main` does, or the test would exercise a path production never uses.
        rt.on_key = Some(on_key);
        rt.frame(0.0);
        assert!(
            rt.state.status.contains("edges"),
            "idle status should name the graph, got {:?}",
            rt.state.status
        );
        rt.key(KeyEvent {
            key: rune::Key::Enter,
            mods: rune::Mods {
                cmd: true,
                ..rune::Mods::default()
            },
        });
        // Handlers run between frames: the shortcut's effect lands on the next one.
        rt.frame(0.016);
        assert!(
            rt.state.running_since.is_some(),
            "Cmd+Enter must start the run the shortcut advertises"
        );
    }

    /// The Run button's completion publishes rows and a latency, and clears the previous selection
    /// (the rows it named no longer exist).
    #[test]
    fn finishing_a_run_publishes_rows_and_a_latency() {
        let mut state = build();
        state.results = ResultTable::from_rows(
            vec![
                "vertex".to_owned(),
                "edge".to_owned(),
                "latency (ms)".to_owned(),
            ],
            vec![vec!["old".to_owned(), "row".to_owned(), "0".to_owned()]],
        );
        state.running_since = Some(std::time::Instant::now());
        state.finish_run();
        assert!(state.running_since.is_none());
        // The default query says LIMIT 100: the mock answers exactly that, so the readout is honest.
        assert_eq!(
            state.results.row_count(),
            100,
            "a run publishes what the query asked for"
        );
        assert!(state.last_run_ms.is_some_and(|ms| ms >= 0.0));
        assert_eq!(
            state.status,
            format!(
                "100 rows in {:.1} ms",
                state.last_run_ms.unwrap_or_default()
            )
        );
        assert!(
            state.toast.is_some(),
            "a finished run reports itself in a toast"
        );
    }

    /// The option checkboxes drive the core style, and the core's own mechanisms produce the
    /// visible change: the label budget empties the labels, the aggregate threshold swaps edges for
    /// cells. Nothing is filtered in the adapter.
    #[test]
    fn the_display_options_drive_the_core_style() {
        let mut panel = GraphPanel::new(60, 120, 600.0, 400.0, panel::LabelTables::default());
        panel.set_labels(false);
        panel.sync();
        assert!(
            panel.labels().is_empty(),
            "a zero label budget leaves no labels to place"
        );

        panel.set_labels(true);
        panel.set_aggregate(true);
        panel.sync();
        assert!(
            !panel.splats().is_empty(),
            "aggregation renders cells instead of edges"
        );
        assert_eq!(panel.drawn().1, 0, "an aggregate frame carries no edges");

        panel.set_aggregate(false);
        panel.sync();
        assert!(panel.splats().is_empty());
        assert!(panel.drawn().1 > 0, "individual edges return");
    }

    /// Sorting reorders the rows to match what the table already shows in its header, in the
    /// requested direction. This is the contract `on_sort` relies on.
    #[test]
    fn sorting_reorders_rows_to_match_the_table() {
        let mut state = build();
        // The graph is irrelevant here; keep the test cheap.
        state.results = fixture_results(&state.schema, 6);
        state.table.set_sort("latency (ms)", SortDir::Ascending);
        state.sort_results();
        let ascending: Vec<f32> = state
            .results
            .rows()
            .map(|row| row[2].parse().expect("latency is numeric"))
            .collect();
        let mut expected = ascending.clone();
        expected.sort_by(f32::total_cmp);
        assert_eq!(ascending, expected);

        state.table.set_sort("vertex", SortDir::Descending);
        state.sort_results();
        let mut vertices: Vec<String> = state.results.rows().map(|row| row[0].clone()).collect();
        let sorted = {
            let mut v = vertices.clone();
            v.sort();
            v.reverse();
            v
        };
        assert_eq!(vertices, sorted);
        vertices.clear();
    }
}

/// A growth stack row that folds: the framework's disclosure mechanism. Proving it here keeps the
/// claim in the record tied to an executed check rather than to an assumption about `.motion`.
#[cfg(test)]
mod disclosure_motion {
    use super::*;
    use rune::{GrowthSpec, Runtime};

    #[test]
    fn a_fold_glide_row_glides_its_height() {
        struct Demo {
            open: bool,
        }
        fn view(s: &Demo, _cx: &mut Cx<Demo>) -> El {
            col()
                .w_full()
                .h_full()
                .growth(GrowthSpec::default())
                .child(
                    row()
                        .key("header")
                        .h(24.0)
                        .w_full()
                        .child(text("header").font(TextStyle::new(12.0, 400.0))),
                )
                .child(
                    col()
                        .key("section")
                        .w_full()
                        .h(if s.open { 200.0 } else { 40.0 })
                        .fold_glide(rune_motion::Spring::new(0.3, 0.0))
                        .child(text("body").font(TextStyle::new(12.0, 400.0))),
                )
        }

        let mut rt = Runtime::new(
            Demo { open: true },
            view,
            (300.0, 400.0),
            rune::Env::new(true, 1.0),
        );
        rt.frame(0.0);
        let height = |rt: &Runtime<Demo>| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("section"))
                .expect("the section row is in the tree")
                .rect[3]
        };
        assert!((height(&rt) - 200.0).abs() < 1.0, "open at rest");

        rt.update(|s, _| s.open = false);
        let mut seen = Vec::new();
        for _ in 0..40 {
            rt.frame(1.0 / 60.0);
            seen.push(height(&rt));
        }
        assert!(
            seen.iter().any(|h| *h > 42.0 && *h < 198.0),
            "the row must pass through intermediate heights while folding shut: {seen:?}"
        );
        assert!((seen.last().copied().unwrap_or_default() - 40.0).abs() < 1.0);
    }

    /// The console's own results section is a folding growth row: closing it glides its height and
    /// the graph row grows into the freed space. This is the app-level check of the mechanism the
    /// minimal tests above prove in isolation.
    #[test]
    fn closing_the_results_section_glides_in_the_console_layout() {
        let mut rt = rune::Runtime::new(build(), view, window_size(), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        let section_height = |rt: &rune::Runtime<State>| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("results-section"))
                .map(|e| e.rect[3])
                .unwrap_or_default()
        };
        let open = section_height(&rt);
        assert!((open - 342.0).abs() < 2.0, "open section at rest: {open}");
        rt.update(|s, _| s.results_open = false);
        let mut seen = Vec::new();
        for _ in 0..45 {
            rt.frame(1.0 / 60.0);
            seen.push(section_height(&rt));
        }
        assert!(
            seen.iter().any(|h| *h > 58.0 && *h < 340.0),
            "the section must pass through intermediate heights: {seen:?}"
        );
        assert!(
            (seen.last().copied().unwrap_or_default() - 56.0).abs() < 2.0,
            "it settles closed: {:?}",
            seen.last()
        );
    }

    /// A grown row inside a growth stack still absorbs the leftover height, so the shell can keep
    /// its "graph fills the rest" layout while the results row folds.
    #[test]
    fn a_grow_row_shares_a_growth_stack_with_a_fold_glide_row() {
        struct Demo {
            open: bool,
        }
        fn view(s: &Demo, _cx: &mut Cx<Demo>) -> El {
            col()
                .w_full()
                .h_full()
                .growth(GrowthSpec::default())
                .child(row().key("header").h(24.0).w_full())
                .child(col().key("fill").w_full().grow(1.0).min_h(0.0))
                .child(
                    col()
                        .key("section")
                        .w_full()
                        .h(if s.open { 200.0 } else { 40.0 })
                        .fold_glide(rune_motion::Spring::new(0.3, 0.0)),
                )
        }
        let mut rt = Runtime::new(
            Demo { open: true },
            view,
            (300.0, 400.0),
            rune::Env::new(true, 1.0),
        );
        rt.frame(0.0);
        let rect = |rt: &Runtime<Demo>, key: &str| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(key))
                .map(|e| e.rect)
                .unwrap_or_default()
        };
        let fill_open = rect(&rt, "fill")[3];
        assert!(
            fill_open > 100.0,
            "the grown row takes the leftover height: {fill_open}"
        );
        rt.update(|s, _| s.open = false);
        for _ in 0..40 {
            rt.frame(1.0 / 60.0);
        }
        let fill_closed = rect(&rt, "fill")[3];
        assert!(
            fill_closed > fill_open + 100.0,
            "the grown row expands as the section folds: {fill_open} -> {fill_closed}"
        );
    }
}

#[cfg(test)]
mod layout_audit {
    use super::*;
    use rune::Runtime;

    /// Print the rects of the shell's structural elements so margins and gaps are read from the
    /// layout instead of guessed from a screenshot.
    #[test]
    #[ignore]
    fn print_structure_rects() {
        let mut rt = Runtime::new(build(), view, window_size(), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        let rects: Vec<(String, [f32; 4])> = rt
            .elements()
            .into_iter()
            .filter(|e| {
                e.key.as_deref().is_some_and(|k| {
                    [
                        "card",
                        "toolbar",
                        "main-content",
                        "query-card",
                        "graph-card",
                        "inspector-column",
                        "results-section",
                    ]
                    .contains(&k)
                })
            })
            .map(|e| (e.key.unwrap_or_default(), e.rect))
            .collect();
        for (key, rect) in &rects {
            println!(
                "{key:18} x {:7.2} y {:7.2} w {:7.2} h {:7.2}",
                rect[0], rect[1], rect[2], rect[3]
            );
        }
        // Report the vertical gaps between siblings and the horizontal gaps inside the top row.
        let get = |key: &str| rects.iter().find(|(k, _)| k == key).map(|(_, r)| *r);
        if let (Some(main), Some(query), Some(graph), Some(results)) = (
            get("card"),
            get("query-card"),
            get("graph-card"),
            get("results-section"),
        ) {
            println!(
                "insets: pane→query {:.2}, query→graph {:.2}, graph→results {:.2}; pane x {:.2} y {:.2}",
                query[1] - main[1],
                graph[1] - (query[1] + query[3]),
                results[1] - (graph[1] + graph[3]),
                main[0],
                main[1]
            );
        }
        if let (Some(graph), Some(rail)) = (get("graph-card"), get("rail")) {
            println!(
                "graph/rail: gap {:.2}, graph {}x{}, rail {}x{}",
                rail[0] - (graph[0] + graph[2]),
                graph[2],
                graph[3],
                rail[2],
                rail[3]
            );
        }
        if let (Some(inspector), Some(options)) = (get("inspector-group"), get("options-group")) {
            println!(
                "rail gap {:.2}; inspector h {:.2}, options h {:.2}",
                options[1] - (inspector[1] + inspector[3]),
                inspector[3],
                options[3]
            );
        }
    }
}

/// GQL token colours for the query field.
///
/// The token classification comes from the repository's real lexer (`gleaph_gql::highlight`), so the
/// console and the parser cannot disagree about what is a keyword; only the *painting* is the kit's
/// concern: map each kind to a `rune_kit::syntax::Syntax` and let `SyntaxTheme::from_kit` supply the
/// hues, which follow the theme's lightness.
fn query_spans(source: &str, theme: &rune_kit::syntax::SyntaxTheme) -> Vec<rune::Span> {
    use rune_kit::syntax::{Highlight, Paint, Syntax};
    let Ok(found) = gleaph_gql::highlight::highlight(source) else {
        // A half-typed query that does not lex yet stays plain: colouring a guess would flicker.
        return Vec::new();
    };
    let highlights: Vec<Highlight> = found
        .into_iter()
        .filter_map(|(range, kind)| {
            let syntax = match kind {
                gleaph_gql::highlight::HighlightKind::Keyword => Syntax::Keyword,
                gleaph_gql::highlight::HighlightKind::String => Syntax::String,
                gleaph_gql::highlight::HighlightKind::Number => Syntax::Number,
                gleaph_gql::highlight::HighlightKind::Parameter => Syntax::Property,
                gleaph_gql::highlight::HighlightKind::QuotedIdentifier => Syntax::Type,
                gleaph_gql::highlight::HighlightKind::Punctuation => Syntax::Punctuation,
                gleaph_gql::highlight::HighlightKind::Comment => Syntax::Comment,
                // Variables are ordinary ink: colouring every identifier would make the query
                // stripes of colour with no meaning.
                gleaph_gql::highlight::HighlightKind::Identifier => return None,
            };
            Some((range, Paint::Kind(syntax)))
        })
        .collect();
    theme.spans(&highlights, 0)
}
