//! Contrast math and the console's design tokens.
//!
//! Contrast here is **APCA** (Accessible Perceptual Contrast Algorithm, 0.1.9), not the WCAG 2
//! ratio: APCA is polarity-aware and lightness-based, which is what a dark console with a
//! luminous graph panel needs, and it is the algorithm the WCAG 3 drafts use. The reference is the
//! published `apca-w3` implementation; this module ports the formula and the calibration
//! constants verbatim, and its tests pin the algorithm's known values so a refactor cannot drift.
//!
//! Tokens are derived, not hand-picked: [`ink_for`] finds the ink at a requested Lc on a surface,
//! so a palette change cannot silently drop a token below its requirement. [`Palette::audit`]
//! re-checks every foreground/background pair the console actually draws, and the app prints the
//! report at startup - the same numbers a test asserts.

use rune::Color;

use rune_kit::Kit;

/// Parse an sRGB hex colour (`#rgb`, `#rrggbb`, `#rrggbbaa`) into Rune's linear colour.
fn hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    let expand = |n: u8| n as f32 / 255.0;
    let decode = |c: f32| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b, a) = match h.len() {
        3 => {
            let nibble = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
            (nibble(0)?, nibble(1)?, nibble(2)?, 255)
        }
        6 => (byte(0)?, byte(2)?, byte(4)?, 255),
        8 => (byte(0)?, byte(2)?, byte(4)?, byte(6)?),
        _ => return None,
    };
    Some(Color {
        r: decode(expand(r)),
        g: decode(expand(g)),
        b: decode(expand(b)),
        a: expand(a),
    })
}

/// APCA's transfer exponent: the algorithm works on sRGB-encoded channels.
const MAIN_TRC: f32 = 2.4;
/// sRGB luminance coefficients (APCA's own, not Rec. 709).
const S_RCO: f32 = 0.212_672_9;
const S_GCO: f32 = 0.715_152_2;
const S_BCO: f32 = 0.072_175_0;
/// Polarity exponents: normal (dark text on light) and reverse (light text on dark).
const NORM_BG: f32 = 0.56;
const NORM_TXT: f32 = 0.57;
const REV_TXT: f32 = 0.62;
const REV_BG: f32 = 0.65;
/// Black soft clamp: APCA does not let a channel collapse to zero weight.
const BLK_THRS: f32 = 0.022;
const BLK_CLMP: f32 = 1.414;
const SCALE_BOW: f32 = 1.14;
const SCALE_WOB: f32 = 1.14;
const LO_BOW_OFFSET: f32 = 0.027;
const LO_WOB_OFFSET: f32 = 0.027;
const DELTA_Y_MIN: f32 = 0.000_5;
const LO_CLIP: f32 = 0.1;

/// APCA lightness contrast between `text` and `background`, in Lc units (`-108..=106`).
///
/// Positive Lc means dark-on-light, negative means light-on-dark; the magnitude is the contrast.
pub fn lc(text: Color, background: Color) -> f32 {
    let (y_txt, y_bg) = (y(text), y(background));
    if (y_bg - y_txt).abs() < DELTA_Y_MIN {
        return 0.0;
    }
    if y_bg > y_txt {
        // Normal polarity: dark text on a light background.
        let sapc = (y_bg.powf(NORM_BG) - y_txt.powf(NORM_TXT)) * SCALE_BOW;
        if sapc < LO_CLIP {
            0.0
        } else {
            (sapc - LO_BOW_OFFSET) * 100.0
        }
    } else {
        // Reverse polarity: light text on a dark background.
        let sapc = (y_bg.powf(REV_BG) - y_txt.powf(REV_TXT)) * SCALE_WOB;
        if sapc > -LO_CLIP {
            0.0
        } else {
            (sapc + LO_WOB_OFFSET) * 100.0
        }
    }
}

/// APCA luminance of a colour: sRGB-encoded channels, `MAIN_TRC`, black soft-clamped.
fn y(color: Color) -> f32 {
    let encode = |c: f32| {
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let y = S_RCO * encode(color.r).powf(MAIN_TRC)
        + S_GCO * encode(color.g).powf(MAIN_TRC)
        + S_BCO * encode(color.b).powf(MAIN_TRC);
    if y > BLK_THRS {
        y
    } else {
        y + (BLK_THRS - y).powf(BLK_CLMP)
    }
}

/// What a foreground is used for, and the APCA level it must reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Fluent body text at typical sizes.
    Body,
    /// Content text that is read in short spans (labels, values, table cells).
    Content,
    /// Large text (roughly 24pt, or 19pt bold) and incidental UI text.
    Large,
    /// Non-text: borders, dividers, icon strokes, focus rings.
    NonText,
    /// The minimum adjacency for two non-text control surfaces (a segmented control's thumb on its
    /// track): enough to read the state, less than a boundary that carries meaning on its own.
    Indicator,
}

impl Role {
    /// The minimum Lc APCA asks for at this role.
    pub fn minimum(self) -> f32 {
        match self {
            Role::Body => 75.0,
            Role::Content => 60.0,
            Role::Large => 45.0,
            Role::NonText => 30.0,
            Role::Indicator => 15.0,
        }
    }
}

/// Whether `foreground` meets `role` on `background`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn meets(foreground: Color, background: Color, role: Role) -> bool {
    lc(foreground, background).abs() >= role.minimum()
}

/// The ink on `background` at exactly `target` Lc, keeping `tint`'s hue and chroma.
#[cfg_attr(not(test), allow(dead_code))]
pub fn ink_for(background: Color, tint: Color, target: f32) -> Color {
    ink_for_surfaces(&[background], tint, target)
}

/// The ink that meets `target` Lc on **every** given surface, keeping `tint`'s hue and chroma.
///
/// A theme draws the same text token on more than one surface (the window and a raised panel), and
/// the two can be on either side of each other: a light theme's panel is lighter than its window,
/// so the window is the harder surface, while a dark theme's panel is lighter and therefore the
/// easier one. Searching the pair keeps one token valid on both instead of deriving it twice.
pub fn ink_for_surfaces(surfaces: &[Color], tint: Color, target: f32) -> Color {
    let reference = surfaces.first().copied().unwrap_or(Color::BLACK);
    let [bg_l, _, _, _] = reference.to_oklab();
    let [_, tint_a, tint_b, _] = tint.to_oklab();
    // Ink goes to whichever side can actually reach the target: a mid-lightness surface (a teal
    // accent, say) may reach Lc 45 with dark ink while white ink only reaches 38, so the side is
    // chosen by measured reach, not by the surface's lightness.
    let reach_to = |far_l: f32| {
        surfaces
            .iter()
            .map(|surface| lc(Color::from_oklab([far_l, tint_a, tint_b, 1.0]), *surface).abs())
            .fold(f32::INFINITY, f32::min)
    };
    let far_l = if reach_to(1.0) >= reach_to(0.0) {
        1.0
    } else {
        0.0
    };
    let candidate = |t: f32| Color::from_oklab([bg_l + (far_l - bg_l) * t, tint_a, tint_b, 1.0]);
    let reach = |t: f32| {
        surfaces
            .iter()
            .map(|surface| lc(candidate(t), *surface).abs())
            .fold(f32::INFINITY, f32::min)
    };
    if reach(1.0) < target {
        return candidate(1.0);
    }
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (low + high) * 0.5;
        if reach(mid) >= target {
            high = mid;
        } else {
            low = mid;
        }
    }
    candidate(high)
}

/// The well behind a segmented control: the window one subtle step toward the ink.
fn track_for(bg: Color) -> Color {
    ink_for_surfaces(
        &[bg],
        hex("#808080").expect("valid hex"),
        Role::Indicator.minimum(),
    )
}

/// The selected segment: the raised surface when it is distinguishable from the track, otherwise a
/// step off the track so the state still reads (a dark theme's panel sits too close to the well).
fn thumb_for(bg: Color, panel: Color) -> Color {
    let track = track_for(bg);
    if lc(panel, track).abs() >= Role::Indicator.minimum() {
        panel
    } else {
        ink_for_surfaces(&[track], hex("#808080").expect("valid hex"), 18.0)
    }
}

/// The console's colours, each derived from a surface and a contrast requirement.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Whether this palette is the dark appearance (rune-kit needs the flag for its own parts).
    pub dark: bool,
    /// The window background, the darkest surface.
    pub bg: Color,
    /// A panel raised off the window.
    pub panel: Color,
    /// Primary text on a panel.
    pub text: Color,
    /// Secondary text and labels on a panel.
    pub text_muted: Color,
    /// Ancillary values on a panel.
    pub text_faint: Color,
    /// Borders and dividers on the window background.
    pub border: Color,
    /// The accent, used as fill or as large text on its own surface.
    pub accent: Color,
    /// Ink that sits on `accent`.
    pub on_accent: Color,
    /// The well behind a segmented control, one subtle step from the window.
    pub track: Color,
    /// The selected segment's fill, distinguishable from the track as a control state.
    pub thumb: Color,
}

impl Palette {
    /// Derive every token from a background, a panel, an ink tint and an accent.
    ///
    /// The palette is a seed, not a list: each foreground is the ink that *just* meets its role's
    /// APCA level on its surface, so a theme cannot ship an unreadable token by accident and the
    /// two themes cannot drift apart in structure.
    pub fn from_seed(bg: &str, panel: &str, tint: &str, accent: &str) -> Self {
        let bg = hex(bg).expect("valid hex");
        let panel = hex(panel).expect("valid hex");
        let tint = hex(tint).expect("valid hex");
        let accent = hex(accent).expect("valid hex");
        // The segmented control's surfaces come before the inks, because the inks must also read on
        // them: a dark theme's thumb sits above the panel, so a label derived only against the
        // panel would fall below the content level there.
        let track = track_for(bg);
        let thumb = thumb_for(bg, panel);
        let raised = panel.mix(tint, 0.05);
        let ink_surfaces = [bg, panel, track, thumb, raised];
        let dark = bg.to_oklab()[0] < 0.5;
        Self {
            dark,
            bg,
            panel,
            text: ink_for_surfaces(&ink_surfaces, tint, Role::Body.minimum()),
            text_muted: ink_for_surfaces(&ink_surfaces, tint, Role::Content.minimum()),
            text_faint: ink_for_surfaces(&ink_surfaces, tint, Role::Large.minimum()),
            border: ink_for_surfaces(&[bg], tint, Role::NonText.minimum()),
            accent,
            // A filled button reads best with crisp white ink. The test below measures that white
            // actually meets the content level on this accent instead of trusting the choice.
            on_accent: Color::WHITE,
            track,
            thumb,
        }
    }

    /// The console's dark palette.
    ///
    /// The accent is Apple's dark-appearance system blue (`#0A84FF`), the brighter twin of the
    /// light theme's `#007AFF`, so the console keeps one recognisable brand primary across
    /// appearances. Both are measured: white ink reaches Lc 68.8 / 72.0 on them (body text wants
    /// 75, content 60), and the fill itself reads 37.2 / 59.8 against the surfaces it sits on.
    pub fn dark() -> Self {
        Self::from_seed("#0b0d12", "#151922", "#93a4c4", "#0a84ff")
    }

    /// The console's light palette.
    ///
    /// APCA is polarity-aware, so the light theme is the same derivation on inverted seeds: the
    /// surfaces are light, the ink is dark, and the accent is darkened so its own ink can reach the
    /// large-text level.
    pub fn light() -> Self {
        Self::from_seed("#f2f4f7", "#ffffff", "#2b3444", "#007aff")
    }

    /// A rune-kit [`Kit`] whose colours are this palette's APCA tokens.
    ///
    /// rune-kit's components read semantic tokens (`layer_01`, `bg_base`, `text_base`, ...) and own
    /// their structure, focus handling and accessibility. The console keeps its own APCA palette as
    /// the source of truth and hands the kit the same colours, instead of re-implementing components
    /// or letting the kit derive a second, WCAG-ratio palette next to it.
    /// The kit's raised surface (dialogs, segmented wells): one step off the panel toward the ink.
    pub fn raised(&self) -> Color {
        self.panel.mix(self.text, 0.05)
    }

    pub fn kit(&self) -> Kit {
        let mut kit = Kit::neutral(self.dark);
        kit.dark = self.dark;
        let c = &mut kit.c;
        // The segmented control paints its well with `layer_01`, its selected indicator and the
        // `Fill` variant with `bg_base`, and its labels with the text ladder.
        c.page = self.bg;
        c.bg_base = self.thumb;
        c.bg_deep = self.bg;
        c.layer_01 = self.raised();
        c.layer_02 = self.thumb;
        c.text_base = self.text;
        c.text_muted = self.text_muted;
        c.text_faint = self.text_faint;
        c.border_base = self.border;
        c.border_strong = self.track;
        c.border_focus = self.accent;
        c.border_muted = self.border;
        c.bg_accent = self.accent;
        c.text_accent = self.accent;
        c.submit_fg = self.on_accent;
        c.bg_icon_button_contrast = self.accent;
        c.bg_contrast = self.accent;
        c.text_contrast = self.on_accent;
        c.submit_highlight = self.accent.mix(Color::WHITE, 0.18);
        c.submit_hover = self.accent.mix(Color::WHITE, 0.12);
        c.contrast_pressed = self.accent.mix(Color::BLACK, 0.18);
        c.hover = self.thumb;
        c.pressed = self.track;
        c.knob = self.thumb;
        // Form controls (checkbox, switch, radio) read the surface/border/ink ladder.
        c.surface_weak = self.thumb;
        c.surface_hover = self.track;
        c.surface_raised = self.panel;
        c.surface_disabled = self.track;
        c.std_border = self.border;
        c.border_weak = self.border;
        c.border_hover = self.border;
        c.border_active = self.accent;
        c.border_disabled = self.track;
        c.std_icon = self.accent;
        c.std_text = self.text;
        // Overlay scrim and status colours: the kit's dialog/error paths read these, and the neutral
        // theme's values would be another palette's. Derive them like every other token.
        c.scrim = self
            .bg
            .mix(Color::BLACK, 0.75)
            .alpha(if self.dark { 0.6 } else { 0.35 });
        let status = |lightness: f32, hue: f32| {
            lift_for_contrast(
                Color::from_oklab([
                    lightness,
                    0.16 * hue.to_radians().cos(),
                    0.16 * hue.to_radians().sin(),
                    1.0,
                ]),
                self.panel,
                Role::Content.minimum(),
            )
        };
        let l = self.text.to_oklab()[0];
        let danger = status(l, 25.0);
        let success = status(l, 150.0);
        c.danger = rune_kit::StateColors {
            bg: danger.alpha(0.16),
            fg: danger,
            border: danger.alpha(0.45),
            icon: danger,
        };
        c.success = rune_kit::StateColors {
            bg: success.alpha(0.16),
            fg: success,
            border: success.alpha(0.45),
            icon: success,
        };
        kit
    }

    /// Every foreground/background pair the console draws, with the role it plays.
    pub fn pairs(&self) -> Vec<(&'static str, Color, Color, Role)> {
        vec![
            ("body text on panel", self.text, self.panel, Role::Body),
            (
                "muted text on panel",
                self.text_muted,
                self.panel,
                Role::Content,
            ),
            (
                "faint text on panel",
                self.text_faint,
                self.panel,
                Role::Large,
            ),
            ("body text on window", self.text, self.bg, Role::Body),
            (
                "muted text on window",
                self.text_muted,
                self.bg,
                Role::Content,
            ),
            ("border on window", self.border, self.bg, Role::NonText),
            ("ink on accent", self.on_accent, self.accent, Role::Content),
            ("chip label on thumb", self.text, self.thumb, Role::Content),
            (
                "inactive chip label on track",
                self.text_muted,
                self.track,
                Role::Content,
            ),
            ("thumb on track", self.thumb, self.track, Role::Indicator),
            ("track on window", self.track, self.bg, Role::Indicator),
        ]
    }

    /// Check every pair, returning the ones that miss their role's level.
    /// Code colours for the query field: the kit's conventional syntax hues, each lifted to the
    /// contrast its role needs on this palette's card surface.
    pub fn syntax_theme(&self) -> rune_kit::syntax::SyntaxTheme {
        use rune_kit::syntax::Syntax::*;
        let base = rune_kit::syntax::SyntaxTheme::from_kit(&self.kit());
        let mut theme = base;
        // Code is small: content level for the tokens that carry meaning, large level for comments
        // and punctuation, which are secondary marks rather than content.
        for kind in [
            Keyword, String, Number, Type, Function, Property, Inserted, Deleted,
        ] {
            theme = theme.with(
                kind,
                lift_for_contrast(base.color(kind), self.panel, Role::Content.minimum()),
            );
        }
        theme
            .with(
                Comment,
                lift_for_contrast(base.color(Comment), self.panel, Role::Large.minimum()),
            )
            .with(
                Punctuation,
                lift_for_contrast(base.color(Punctuation), self.panel, Role::Large.minimum()),
            )
    }

    pub fn audit(&self) -> Vec<(&'static str, f32, f32)> {
        self.pairs()
            .into_iter()
            .filter_map(|(name, fg, bg, role)| {
                let value = lc(fg, bg).abs();
                (value < role.minimum()).then_some((name, value, role.minimum()))
            })
            .collect()
    }
}

/// Which palette the console draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn palette(self) -> Palette {
        match self {
            Theme::Dark => Palette::dark(),
            Theme::Light => Palette::light(),
        }
    }
}

/// What the user asked for, which may be "follow the system".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// Follow the OS appearance, re-resolved every frame (`Cx::system_dark`).
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeChoice {
    /// The next choice in the toggle order: Auto → Light → Dark → Auto.
    pub fn next(self) -> Self {
        match self {
            ThemeChoice::Auto => ThemeChoice::Light,
            ThemeChoice::Light => ThemeChoice::Dark,
            ThemeChoice::Dark => ThemeChoice::Auto,
        }
    }

    /// The label the segmented control shows.
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Auto => "Auto",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }

    /// Resolve against the system appearance.
    pub fn resolve(self, system_dark: bool) -> Theme {
        match self {
            ThemeChoice::Auto => {
                if system_dark {
                    Theme::Dark
                } else {
                    Theme::Light
                }
            }
            ThemeChoice::Light => Theme::Light,
            ThemeChoice::Dark => Theme::Dark,
        }
    }

    /// Parse a stored or environment value; `None` for anything unrecognised.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(ThemeChoice::Auto),
            "light" => Some(ThemeChoice::Light),
            "dark" => Some(ThemeChoice::Dark),
            _ => None,
        }
    }

    /// The environment override used by snapshot runs (`CONSOLE_THEME`), if any.
    pub fn from_env() -> Option<Self> {
        std::env::var("CONSOLE_THEME")
            .ok()
            .and_then(|v| Self::parse(&v))
    }
}

/// Where the console remembers the user's choice.
fn preference_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".config/gleaph/console-theme"))
}

/// The saved choice, if a previous run wrote one.
pub fn load_choice() -> Option<ThemeChoice> {
    let path = preference_path()?;
    ThemeChoice::parse(&std::fs::read_to_string(path).ok()?)
}

/// Persist the choice. Best effort: a console that cannot write its preference still runs.
pub fn save_choice(choice: ThemeChoice) {
    let Some(path) = preference_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, choice.label().to_ascii_lowercase());
}

/// Eight categorical hues for vertex labels, plus the fold-to-grey bucket.
///
/// The hues are the accent's own hue rotated in OKLab, so the categories look like members of one
/// family rather than a toy palette; lightness is then lifted until each colour reaches the
/// non-text contrast level against the graph canvas, which is what makes small markers readable.
#[derive(Debug, Clone, Copy)]
pub struct Categories {
    pub colors: [Color; 8],
    /// How many entries of `colors` are in use (relationship colours use six: thin strokes need
    /// more separation than fills).
    pub used: usize,
    /// Everything past the last colour: further labels, and elements with no label.
    pub other: Color,
}

impl Categories {
    pub fn index(self, index: usize) -> Color {
        if index < self.used {
            self.colors[index]
        } else {
            self.other
        }
    }

    pub fn len(self) -> usize {
        self.used
    }

    pub fn is_empty(self) -> bool {
        self.used == 0
    }
}

/// Graph-panel colours derived from the palette.
///
/// The graph is drawn by the core's `GraphStyle`, which is authored in the core's own colour
/// vocabulary, so these are palette roles mapped onto that vocabulary rather than a second palette.
#[derive(Debug, Clone, Copy)]
pub struct GraphColors {
    pub node: Color,
    pub node_stroke: Color,
    pub edge: Color,
    pub label: Color,
    /// Ink for the aggregate density ramp: the faint ink, whose APCA level against the panel is the
    /// large-text/non-text indicator target.
    pub density: Color,
    /// Selection highlight: the brand accent, so the selected element reads as "the one you picked".
    pub selected: Color,
    /// The ring drawn around a selected node: the accent, which stays legible as a *state* on both
    /// themes (an ink fill reads as "disabled" on the light one).
    pub selected_ring: Color,
    /// Categorical colours for vertex labels.
    pub categories: Categories,
    /// Categorical colours for relationship labels.
    pub edge_categories: Categories,
    /// The graph canvas colour: ring-sector gaps are cut in it.
    pub canvas: Color,
    /// Elements outside the emphasised label: its ink, pulled most of the way to the canvas.
    pub muted: Color,
    /// Hover highlight: the accent lifted toward the surface, quieter than selection.
    pub hovered: Color,
}

/// Rotate a colour's hue in OKLab by `turn` of a full turn, keeping its lightness and chroma.
fn rotate_hue(color: Color, turn: f32) -> Color {
    let [l, a, b, alpha] = color.to_oklab();
    let chroma = (a * a + b * b).sqrt();
    let angle = b.atan2(a) + turn * std::f32::consts::TAU;
    Color::from_oklab([l, chroma * angle.cos(), chroma * angle.sin(), alpha])
}

/// Lift `color`'s OKLab lightness until it reaches `target` Lc against `background`.
fn lift_for_contrast(color: Color, background: Color, target: f32) -> Color {
    let [l, a, b, alpha] = color.to_oklab();
    let towards_light = background.to_oklab()[0] < 0.5;
    let reach = |candidate: f32| {
        let candidate = Color::from_oklab([candidate, a, b, alpha]);
        lc(candidate, background).abs()
    };
    // Search away from the surface: on a dark canvas that means lighter, on a light one darker.
    let step = if towards_light { 1.0 } else { -1.0 };
    let mut lightness = l;
    for _ in 0..32 {
        if reach(lightness) >= target {
            break;
        }
        lightness = (lightness + step * 0.02).clamp(0.0, 1.0);
    }
    Color::from_oklab([lightness, a, b, alpha])
}

/// Relationship-label colours.
///
/// Thin strokes need more contrast than fills (a 1.4-point line at Lc 30 disappears), so these are
/// lifted to the large-text level. Chroma is deliberately lower than the node palette's: a graph
/// carries far more edges than nodes, and equally saturated lines would out-shout the markers. The
/// hue family starts a half-turn from the node palette so an edge colour never reads as a node.
impl Palette {
    /// A neutral stand-in used to fill unused palette slots.
    fn other_placeholder(&self) -> Color {
        self.text_faint
    }

    /// The categorical relationship colours, derived for this palette's canvas.
    pub fn edge_categories(&self) -> Categories {
        let [lightness, a, b, _] = self.accent.to_oklab();
        // Thin strokes need separation in *colour*, so the lower chroma buys contrast and the
        // lightness offsets buy distinguishability: with both, eight relationships stay apart.
        const USED: usize = 6;
        const OFFSETS: [f32; 6] = [0.08, -0.08, 0.16, -0.16, 0.24, 0.0];
        let chroma = (a * a + b * b).sqrt().clamp(0.12, 0.16);
        let mut colors = [self.other_placeholder(); 8];
        for (index, slot) in colors.iter_mut().take(USED).enumerate() {
            let angle = b.atan2(a)
                + std::f32::consts::PI
                + index as f32 / USED as f32 * std::f32::consts::TAU;
            let tilted = (lightness + OFFSETS[index]).clamp(0.0, 1.0);
            let rotated =
                Color::from_oklab([tilted, chroma * angle.cos(), chroma * angle.sin(), 1.0]);
            *slot = lift_for_contrast(rotated, self.bg, Role::Large.minimum());
        }
        let other = lift_for_contrast(
            Color::from_oklab([lightness, 0.0, 0.0, 1.0]),
            self.bg,
            Role::Large.minimum(),
        );
        Categories {
            colors,
            used: USED,
            other,
        }
    }

    /// The categorical label colours, derived for this palette's canvas.
    pub fn categories(&self) -> Categories {
        let [lightness, a, b, _] = self.accent.to_oklab();
        let chroma = (a * a + b * b).sqrt().max(0.18);
        // Hue alone cannot separate eight categories at marker size: a small alternating lightness
        // offset widens the perceptual gaps without breaking the family resemblance.
        const OFFSETS: [f32; 8] = [0.02, -0.02, 0.04, -0.04, 0.06, -0.06, 0.0, 0.08];
        let mut colors = [self.accent; 8];
        for (index, slot) in colors.iter_mut().enumerate() {
            let angle = b.atan2(a) + index as f32 / 8.0 * std::f32::consts::TAU;
            let tilted = (lightness + OFFSETS[index]).clamp(0.0, 1.0);
            let rotated =
                Color::from_oklab([tilted, chroma * angle.cos(), chroma * angle.sin(), 1.0]);
            *slot = lift_for_contrast(rotated, self.bg, Role::NonText.minimum());
        }
        Categories {
            colors,
            used: 8,
            other: lift_for_contrast(
                rotate_hue(self.accent, 0.0).mix(hex("#808080").expect("valid hex"), 0.85),
                self.bg,
                Role::NonText.minimum(),
            ),
        }
    }

    /// The graph panel's colours for this palette.
    ///
    /// Edges take the faint ink's lightness with no chroma: a data edge is structure, not an
    /// accent, and colouring it with the ink's hue reads as a second accent in both themes.
    pub fn graph_colors(&self) -> GraphColors {
        let [lightness, _, _, alpha] = self.text_faint.to_oklab();
        let neutral_edge = Color::from_oklab([lightness, 0.0, 0.0, alpha]);
        GraphColors {
            node: self.accent,
            node_stroke: self.panel,
            edge: neutral_edge,
            label: self.text,
            density: self.text_faint,
            selected: self.text,
            selected_ring: self.accent,
            categories: self.categories(),
            edge_categories: self.edge_categories(),
            canvas: self.bg,
            muted: self.text_faint.mix(self.bg, 0.55),
            hovered: self.accent.mix(self.panel, 0.35),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.5,
            "APCA Lc: {actual:.2} vs expected {expected:.2}"
        );
    }

    /// The algorithm's own published values: black on white is +106 Lc, white on black -108.
    #[test]
    fn the_published_reference_values_hold() {
        approx(lc(hex("#000000").unwrap(), hex("#ffffff").unwrap()), 106.04);
        approx(
            lc(hex("#ffffff").unwrap(), hex("#000000").unwrap()),
            -107.88,
        );
        approx(lc(hex("#888888").unwrap(), hex("#ffffff").unwrap()), 63.06);
    }

    #[test]
    fn polarity_flips_the_sign_and_the_colour_is_symmetric_in_magnitude_only_by_definition() {
        let white = hex("#ffffff").unwrap();
        let black = hex("#000000").unwrap();
        assert!(lc(black, white) > 0.0, "dark on light is positive");
        assert!(lc(white, black) < 0.0, "light on dark is negative");
        assert_eq!(lc(white, white), 0.0, "no difference is no contrast");
    }

    #[test]
    fn roles_carry_the_apca_levels() {
        assert_eq!(Role::Body.minimum(), 75.0);
        assert_eq!(Role::Content.minimum(), 60.0);
        assert_eq!(Role::Large.minimum(), 45.0);
        assert_eq!(Role::NonText.minimum(), 30.0);
    }

    #[test]
    fn ink_for_reaches_the_requested_level_on_a_dark_surface() {
        let bg = hex("#151922").unwrap();
        let tint = hex("#93a4c4").unwrap();
        for target in [
            Role::Large.minimum(),
            Role::Content.minimum(),
            Role::Body.minimum(),
        ] {
            let ink = ink_for(bg, tint, target);
            assert!(
                lc(ink, bg).abs() >= target - 0.5,
                "ink for Lc {target} measured {:.2}",
                lc(ink, bg).abs()
            );
        }
        // A stricter target is a lighter ink (a dark surface takes light ink).
        let faint = ink_for(bg, tint, Role::Large.minimum()).to_oklab()[0];
        let body = ink_for(bg, tint, Role::Body.minimum()).to_oklab()[0];
        assert!(body > faint, "body ink is lighter than large-text ink");
    }

    /// The toggle cycles in a fixed order, resolves Auto against the system, and parses what it
    /// writes, so a stored preference always round-trips.
    #[test]
    fn the_theme_choice_cycles_resolves_and_round_trips() {
        use super::ThemeChoice;
        assert_eq!(ThemeChoice::Auto.next(), ThemeChoice::Light);
        assert_eq!(ThemeChoice::Light.next(), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::Dark.next(), ThemeChoice::Auto);

        assert_eq!(ThemeChoice::Auto.resolve(true), Theme::Dark);
        assert_eq!(ThemeChoice::Auto.resolve(false), Theme::Light);
        assert_eq!(ThemeChoice::Light.resolve(true), Theme::Light);
        assert_eq!(ThemeChoice::Dark.resolve(false), Theme::Dark);

        for choice in [ThemeChoice::Auto, ThemeChoice::Light, ThemeChoice::Dark] {
            let written = choice.label().to_ascii_lowercase();
            assert_eq!(ThemeChoice::parse(&written), Some(choice));
            assert_eq!(ThemeChoice::parse(&written.to_uppercase()), Some(choice));
        }
        assert_eq!(ThemeChoice::parse("sepia"), None);
    }

    /// Every categorical colour must read against the graph canvas, and stay distinguishable from
    /// its neighbours: contrast handles "can I see it", OKLab distance handles "can I tell them
    /// apart".
    #[test]
    fn the_categorical_colours_are_readable_and_distinct() {
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            let categories = palette.categories();
            for (index, color) in categories.colors.iter().enumerate() {
                let measured = lc(*color, palette.bg).abs();
                assert!(
                    measured >= Role::NonText.minimum() - 0.5,
                    "{name}: category {index} is {measured:.1} Lc on the canvas"
                );
            }
            let mut closest = f32::INFINITY;
            for i in 0..categories.used {
                for j in i + 1..categories.colors.len() {
                    let [li, ai, bi, _] = categories.colors[i].to_oklab();
                    let [lj, aj, bj, _] = categories.colors[j].to_oklab();
                    let delta = ((li - lj).powi(2) + (ai - aj).powi(2) + (bi - bj).powi(2)).sqrt();
                    closest = closest.min(delta);
                }
            }
            assert!(
                closest > 0.10,
                "{name}: categorical colours are too close (min OKLab distance {closest:.3})"
            );
        }
    }

    /// Relationship colours: readable as thin lines, and distinguishable from each other.
    #[test]
    fn the_relationship_colours_are_readable_and_distinct() {
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            let categories = palette.edge_categories();
            let mut closest = f32::INFINITY;
            for (index, color) in categories.colors.iter().take(categories.used).enumerate() {
                let measured = lc(*color, palette.bg).abs();
                assert!(
                    measured >= Role::Large.minimum() - 0.5,
                    "{name}: relationship {index} is {measured:.1} Lc on the canvas"
                );
            }
            for i in 0..categories.used {
                for j in i + 1..categories.used {
                    let [li, ai, bi, _] = categories.colors[i].to_oklab();
                    let [lj, aj, bj, _] = categories.colors[j].to_oklab();
                    let delta = ((li - lj).powi(2) + (ai - aj).powi(2) + (bi - bj).powi(2)).sqrt();
                    closest = closest.min(delta);
                }
            }
            assert!(
                closest > 0.10,
                "{name}: relationship colours are too close (min OKLab distance {closest:.3})"
            );
        }
    }

    /// The overlay scrim must actually darken the window it covers: a pure black veil disappears on
    /// a near-black background, so the token is the window's own colour pulled toward black.
    #[test]
    fn the_scrim_darkens_the_window_in_both_themes() {
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            let scrim = palette.kit().c.scrim;
            let alpha = scrim.a;
            let base = [palette.bg.r, palette.bg.g, palette.bg.b];
            let veil = [scrim.r, scrim.g, scrim.b];
            let composite = Color {
                r: base[0] * (1.0 - alpha) + veil[0] * alpha,
                g: base[1] * (1.0 - alpha) + veil[1] * alpha,
                b: base[2] * (1.0 - alpha) + veil[2] * alpha,
                a: 1.0,
            };
            // APCA's near-black clamp reports 0 for two very dark colours, so measure the veil by
            // the lightness step it produces instead.
            let drop = palette.bg.to_oklab()[0] - composite.to_oklab()[0];
            // A 2% Oklab lightness step is a perceptible veil even on a near-black window; more
            // would wash the content behind the dialog.
            assert!(
                drop >= 0.02,
                "{name}: the scrim only darkens the window by {drop:.3} Oklab L"
            );
            assert!(alpha > 0.0 && alpha < 1.0, "{name}: scrim alpha {alpha}");
        }
    }

    /// The query field paints GQL tokens with the syntax theme: each must meet its role on the card
    /// it sits on, in both themes, or the highlight is decoration rather than information.
    #[test]
    fn the_query_syntax_colours_meet_their_role_on_the_card() {
        use rune_kit::syntax::Syntax;
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            let theme = palette.syntax_theme();
            let surfaces = [palette.panel, palette.kit().c.bg_base];
            for (kind, role) in [
                (Syntax::Keyword, Role::Content),
                (Syntax::String, Role::Content),
                (Syntax::Number, Role::Content),
                (Syntax::Type, Role::Content),
                (Syntax::Property, Role::Content),
                (Syntax::Comment, Role::Large),
                (Syntax::Punctuation, Role::Large),
            ] {
                for surface in surfaces {
                    let measured = lc(theme.color(kind), surface).abs();
                    assert!(
                        measured >= role.minimum() - 0.5,
                        "{name}: {kind:?} is {measured:.1} Lc on {surface:?}, needs {}",
                        role.minimum()
                    );
                }
            }
        }
    }

    #[test]
    fn both_console_palettes_meet_every_pair_they_draw() {
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            assert_eq!(
                palette.audit(),
                Vec::new(),
                "{name}: every token must meet its role; misses are (pair, Lc, required)"
            );
        }
    }

    /// The filled button's white ink must be measured on the accent, not assumed.
    #[test]
    fn white_ink_meets_the_content_level_on_both_accents() {
        for (name, palette) in [("dark", Palette::dark()), ("light", Palette::light())] {
            let measured = lc(Color::WHITE, palette.accent).abs();
            assert!(
                measured >= Role::Content.minimum(),
                "{name}: white on the accent is {measured:.1} Lc"
            );
        }
    }

    /// APCA is polarity-aware: the dark palette's text is lighter than its surface and the light
    /// palette's is darker, and both reach the same Lc levels.
    #[test]
    fn the_two_themes_sit_on_opposite_polarities() {
        let dark = Palette::dark();
        let light = Palette::light();
        assert!(
            lc(dark.text, dark.panel) < 0.0,
            "dark theme draws light ink on dark surfaces"
        );
        assert!(
            lc(light.text, light.panel) > 0.0,
            "light theme draws dark ink on light surfaces"
        );
        for (palette, name) in [(dark, "dark"), (light, "light")] {
            assert!(
                lc(palette.text, palette.panel).abs() >= Role::Body.minimum(),
                "{name} body text"
            );
            assert!(
                lc(palette.text_muted, palette.panel).abs() >= Role::Content.minimum(),
                "{name} content text"
            );
        }
        // The same roles end up at comparable contrast in both themes (within APCA's own curve).
        let difference = lc(dark.text, dark.panel).abs() - lc(light.text, light.panel).abs();
        assert!(
            difference.abs() < 12.0,
            "themes should not diverge in reading contrast: {difference:.1} Lc"
        );
    }
}

#[cfg(test)]
mod accent_probe {
    use super::*;

    /// Print the APCA levels of candidate accents so the choice is measured, not eyeballed.
    #[test]
    #[ignore]
    fn probe_control_tokens() {
        for (name, bg, panel) in [
            ("dark", "#0b0d12", "#151922"),
            ("light", "#f2f4f7", "#ffffff"),
        ] {
            let bg = hex(bg).unwrap();
            let panel = hex(panel).unwrap();
            let tint = hex("#808080").unwrap();
            for step in [5.0f32, 8.0, 10.0, 12.0] {
                let track = ink_for_surfaces(&[bg], tint, step);
                eprintln!(
                    "{name}: track at {step} Lc from bg -> track/bg {:.1}, panel/track {:.1}",
                    lc(track, bg).abs(),
                    lc(panel, track).abs()
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn probe_accent_candidates() {
        let white = hex("#ffffff").unwrap();
        let dark_panel = hex("#151922").unwrap();
        let light_panel = hex("#ffffff").unwrap();
        let light_window = hex("#f2f4f7").unwrap();
        for name in [
            "#007aff", "#0a84ff", "#0066cc", "#0059b2", "#0e6f66", "#2f8f7f",
        ] {
            let accent = hex(name).unwrap();
            eprintln!(
                "{name}: white ink {:.1}, dark ink {:.1} | on dark panel {:.1} | on light window {:.1}",
                lc(white, accent).abs(),
                lc(hex("#0b0d12").unwrap(), accent).abs(),
                lc(accent, dark_panel).abs(),
                lc(accent, light_window).abs(),
            );
        }
        let _ = light_panel;
    }
}
