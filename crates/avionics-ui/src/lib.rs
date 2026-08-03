//! The traffic plan view.
//!
//! Own-ship-centred, with range rings, traffic symbols carrying altitude and vertical-speed tags,
//! threat colouring, and a status bar. No basemap: this is a *relative* position display, which
//! is what matters for seeing and avoiding.
//!
//! A frame is determined entirely by [`AppState`], a [`ViewState`] and the current instant, which
//! is what makes the offscreen filmstrip in the `avionics` binary a usable verification tool. The
//! one piece of retained state is the NEXRAD mosaic texture ([`nexrad::Mosaic`]), and that is a
//! pure cache: it is keyed on the block revision and own-ship position, so the same inputs still
//! produce the same picture. Drawing therefore takes `&mut self`.
//!
//! ```text
//!   AppState (what Stratux told us)
//!        +                              -> Ui::draw -> Canvas
//!   ViewState (range, orientation)
//!        +
//!   Instant (for dead reckoning)
//! ```

pub mod ahrspage;
pub mod chart;
pub mod font;
pub mod footerbar;
pub mod glossary;
pub mod interact;
pub mod maplayer;
pub mod metar;
pub mod nexrad;
pub mod pagestrip;
pub mod planview;
pub mod projection;
pub mod reckon;
pub mod softkeys;
pub mod statusbar;
pub mod symbols;
pub mod tapes;
pub mod theme;
pub mod threat;
pub mod weatherpage;

use std::time::{Duration, Instant};

use anyhow::Result;
use avionics_gfx::femtovg::FontId;
use avionics_gfx::Canvas;
use stratux_client::AppState;

pub use chart::Chart;
pub use nexrad::Mosaic;
pub use projection::{Orientation, Projection};
pub use reckon::ReckonConfig;
pub use theme::Theme;
pub use threat::{AltitudeFilter, ThreatConfig, ThreatLevel};

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Own-ship-centred traffic display with the weather underlay.
    PlanView,
    /// FIS-B text products.
    Weather,
    /// Attitude from the AHRS, if a sensor is fitted.
    Ahrs,
}

impl Page {
    /// Every page, in the order the page strip lists them top to bottom.
    ///
    /// Traffic first because it is the one that matters most, and because a strip read top-down
    /// should start where the display starts.
    pub const ALL: [Self; 3] = [Self::PlanView, Self::Weather, Self::Ahrs];

    pub fn next(self) -> Self {
        match self {
            Self::PlanView => Self::Weather,
            Self::Weather => Self::Ahrs,
            // Traffic is the page that matters most, so the cycle always returns there rather
            // than leaving the attitude page as a place you can get stuck one key press from.
            Self::Ahrs => Self::PlanView,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PlanView => "TFC",
            Self::Weather => "WX",
            Self::Ahrs => "AHRS",
        }
    }

    /// How often this page is worth redrawing, or `None` to run at the panel's refresh rate.
    ///
    /// # Why the plan view does not need 60 Hz
    ///
    /// [`Layout::for_size`] gives an outer ring radius of about 187 px on the 800x480 panel, so at
    /// the 40 nm range one pixel is roughly 0.21 nm. A 150 kt target covers 0.042 nm in a second —
    /// **three thousandths of a pixel per frame at 60 Hz**. Even against the 2 nm ring it is under
    /// a tenth of a pixel. The dead-reckoned motion those frames exist to smooth is an order of
    /// magnitude below what the panel can physically show.
    ///
    /// What the frames do cost is a full-screen GPU composite and the memory traffic behind it, on
    /// a board where `dump1090` and `dump978` are competing for the same SDRAM bandwidth and the
    /// render loop holds the [`AppState`] lock for the whole of every draw. The project's own rule
    /// is that a dropped ADS-B message is worse than a dropped frame, and these are frames nobody
    /// can see. So they go.
    ///
    /// # Why the attitude page is the exception
    ///
    /// Roll rate during a roll-in is tens of degrees per second, which makes attitude the one
    /// thing on this display that genuinely moves fast enough for the refresh rate to be visible.
    /// It stays uncapped.
    ///
    /// # Why not redraw only on change
    ///
    /// Damage tracking would save more, and is rejected deliberately. If the "nothing changed"
    /// test is ever wrong, the result is a *frozen* screen showing stale traffic — indistinguishable
    /// at a glance from a live one, which is the worst failure this display has. A fixed lower rate
    /// cannot fail that way, because every frame is still drawn from current state.
    pub fn frame_interval(self) -> Option<Duration> {
        match self {
            Self::PlanView => Some(Duration::from_millis(1000 / 30)),
            // Text products change when a new one is received, on the order of once a minute.
            // Even 8 Hz is far more often than the content moves; it is this high only so the
            // status bar's staleness clocks tick visibly and a soft key feels immediate.
            Self::Weather => Some(Duration::from_millis(1000 / 8)),
            Self::Ahrs => None,
        }
    }
}

/// Progress of a "the aircraft is straight and level" request.
///
/// # Why this is a state machine and not a button
///
/// Caging re-references the sensor, and the corrected attitude flows to everything downstream —
/// this display, the GDL90 feed to a tablet, the logs. Caging while *not* level teaches the
/// sensor that a banked, pitched attitude is level, and nothing announces that it happened.
///
/// So one press is not enough. The first press arms and the key relabels to CONFIRM; only a
/// second press sends the request. The arm lapses on its own after [`CAGE_ARM_TIMEOUT`], so a
/// stray press followed by a real one seconds later cannot combine into a cage the pilot never
/// intended. This is the same reasoning that made the plan-view body inert: in turbulence a hand
/// finds the panel, and the cost of an accidental press here is an instrument that lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CageState {
    Idle,
    /// First press seen; waiting for confirmation.
    Armed,
    /// Confirmed. The app should issue the request and move to [`CageState::InFlight`].
    Requested,
    InFlight,
    /// Finished, showing the outcome briefly before returning to idle.
    Done { ok: bool },
}

/// How much of the map layer is drawn.
///
/// # Why airspace is a separate step and not simply "the map"
///
/// Airports and airspace look like one feature and are not. An airport symbol fifty metres out
/// costs nothing; the worst it can do is clutter. An airspace boundary is something a pilot may
/// fly *relative to*, and one drawn wide, or one AIRAC cycle stale, invites exactly the violation
/// it appears to prevent. Traffic is cross-checked out of the window and a Class B shelf is not.
///
/// So the two are separable, and turning airspace on is what raises the `NOT FOR NAVIGATION`
/// banner in [`crate::footerbar`]. Airports alone make no claim a pilot would fly against, so they
/// raise nothing — which is also why they are the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapLayers {
    Off,
    /// Airports only. The default: useful, and it asserts nothing about airspace.
    Airports,
    /// Airports and Class B/C/D boundaries.
    Full,
}

impl MapLayers {
    pub const ALL: [Self; 3] = [Self::Off, Self::Airports, Self::Full];

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Airports,
            Self::Airports => Self::Full,
            Self::Full => Self::Off,
        }
    }

    pub fn shows_airports(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn shows_airspace(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Shares its text with the soft key, so the key and the page cannot disagree.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "MAP OFF",
            Self::Airports => "MAP APT",
            Self::Full => "MAP ALL",
        }
    }
}

/// How long an armed cage waits for its confirming press before lapsing.
pub const CAGE_ARM_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the outcome stays on the key before it returns to idle.
pub const CAGE_RESULT_DWELL: Duration = Duration::from_secs(3);

/// What the pilot has selected. Persisted across frames by the app; mutated by touch.
#[derive(Debug, Clone)]
pub struct ViewState {
    pub page: Page,
    pub range_nm: f32,
    /// Which slice of the vertical world to draw. The horizontal equivalent of `range_nm`.
    pub altitude_filter: AltitudeFilter,
    pub orientation: Orientation,
    /// Index into the weather list. Its meaning depends on `weather_decode`: the first entry
    /// shown when browsing, the *selected* entry when decoding. Clamped at draw time, since
    /// pruning can shrink the list between frames.
    pub weather_scroll: usize,
    /// Show the selected report's abbreviations expanded, instead of the list.
    pub weather_decode: bool,
    /// Draw the NEXRAD underlay on the plan view.
    pub show_weather_underlay: bool,
    /// How much of the airport and airspace layer to draw. See [`MapLayers`].
    pub map_layers: MapLayers,
    /// Progress of an AHRS cage request. See [`CageState`].
    pub cage: CageState,
    /// When `cage` last changed, for the arm timeout and result dwell.
    pub cage_changed: Option<Instant>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            page: Page::PlanView,
            // 10 nm is the useful default in a light aircraft: far enough to see converging
            // traffic with time to act, close enough that the circuit isn't a single blob.
            range_nm: 10.0,
            // The vertical equivalent of that reasoning, and the same default a Garmin traffic
            // page comes up in. The band is always named in the footer, never merely implied,
            // because this is the one selection that removes traffic from the screen.
            altitude_filter: AltitudeFilter::Normal,
            orientation: Orientation::NorthUp,
            weather_scroll: 0,
            weather_decode: false,
            show_weather_underlay: true,
            // Airports on, airspace off. Airports are the half that carries no navigation claim,
            // so they cost nothing to have up; airspace is opt-in and says so on the panel.
            map_layers: MapLayers::Airports,
            cage: CageState::Idle,
            cage_changed: None,
        }
    }
}

impl ViewState {
    /// Selectable ranges, in nautical miles.
    pub const RANGES: [f32; 5] = [2.0, 5.0, 10.0, 20.0, 40.0];

    /// Step to the next larger range, wrapping back to the smallest.
    pub fn cycle_range(&mut self) {
        let current = Self::RANGES
            .iter()
            .position(|r| (*r - self.range_nm).abs() < f32::EPSILON)
            .unwrap_or(2);
        self.range_nm = Self::RANGES[(current + 1) % Self::RANGES.len()];
    }

    pub fn cycle_range_down(&mut self) {
        let current = Self::RANGES
            .iter()
            .position(|r| (*r - self.range_nm).abs() < f32::EPSILON)
            .unwrap_or(2);
        let next = if current == 0 {
            Self::RANGES.len() - 1
        } else {
            current - 1
        };
        self.range_nm = Self::RANGES[next];
    }

    pub fn toggle_orientation(&mut self) {
        self.orientation = self.orientation.toggled();
    }

    /// Step to the next altitude band, wrapping.
    pub fn cycle_altitude_filter(&mut self) {
        self.altitude_filter = self.altitude_filter.cycle();
    }

    /// Step the map layer through off, airports, airports and airspace.
    pub fn cycle_map_layers(&mut self) {
        self.map_layers = self.map_layers.cycle();
    }

    pub fn set_cage(&mut self, state: CageState, now: Instant) {
        self.cage = state;
        self.cage_changed = Some(now);
    }

    /// Let an armed cage lapse, and retire a finished one. Call once per frame.
    ///
    /// Without this an arm would persist indefinitely, so a press now and an unrelated press
    /// minutes later would combine into a cage nobody asked for.
    pub fn tick_cage(&mut self, now: Instant) {
        let Some(changed) = self.cage_changed else {
            return;
        };
        let elapsed = now.saturating_duration_since(changed);
        match self.cage {
            CageState::Armed if elapsed > CAGE_ARM_TIMEOUT => self.set_cage(CageState::Idle, now),
            CageState::Done { .. } if elapsed > CAGE_RESULT_DWELL => {
                self.set_cage(CageState::Idle, now)
            }
            _ => {}
        }
    }
}

/// What happened while drawing a frame. Feeds the status bar and the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub targets_drawn: usize,
    /// Positional targets beyond the selected range, so culled from the rings.
    pub targets_outside_range: usize,
    /// Targets inside the range ring but outside the selected altitude band.
    ///
    /// Reported separately from `targets_outside_range` and never folded into it: the two are
    /// undone by different keys, and a pilot who cannot tell which selection is hiding something
    /// has to try both. A target outside *both* is counted as out of range only, because range is
    /// tested first.
    pub targets_outside_altitude: usize,
    /// Targets heard without a position: Mode-S only, or ADS-B before its first position report.
    pub targets_no_position: usize,
    /// Targets with a good position that could not be drawn because **own-ship** is missing.
    ///
    /// This is the count that distinguishes "the sky is empty" from "the radios are working and
    /// the GPS is not". Without it the two are identical on screen: the plan view needs an origin
    /// before it can place anything, so a GPS failure blanks the display exactly as a dead
    /// receiver would. That happened on a real outdoor test — 187 ADS-B messages and two targets
    /// were being decoded while the panel showed nothing and `TFC 0`.
    pub targets_unplotted: usize,
    pub targets_coasting: usize,
    pub advisories: usize,
    pub alerts: usize,
    /// Tags dropped because there was nowhere to put them without overlapping. The symbols are
    /// still drawn; only the labels were lost.
    pub tags_suppressed: usize,
    /// Airport symbols drawn from the chart file, after the range tier and the panel cull.
    pub airports_drawn: usize,
    /// Airspace volumes whose boundary reached the screen.
    pub airspace_drawn: usize,
}

/// Holds the loaded font and the tuning constants. Cheap to keep for the process lifetime.
pub struct Ui {
    font: FontId,
    pub theme: Theme,
    pub reckon: ReckonConfig,
    pub threat: ThreatConfig,
    mosaic: Mosaic,
    chart: Option<Chart>,
}

impl Ui {
    pub fn new(canvas: &mut Canvas, theme: Theme) -> Result<Self> {
        let font = font::load(canvas)?;
        Ok(Self {
            font,
            theme,
            reckon: ReckonConfig::default(),
            threat: ThreatConfig::default(),
            mosaic: Mosaic::new(nexrad::MosaicConfig::default()),
            chart: None,
        })
    }

    /// Attach the airport and airspace file, replacing any already loaded.
    ///
    /// Separate from [`Ui::new`] and fallible at the call site on purpose: a missing or corrupt
    /// chart means "no map layer", never a startup failure. Traffic is why the panel exists.
    pub fn set_chart(&mut self, chart: Option<Chart>) {
        self.chart = chart;
    }

    pub fn chart(&self) -> Option<&Chart> {
        self.chart.as_ref()
    }

    /// Replace the mosaic configuration, e.g. to shrink the texture on a memory-tight board.
    pub fn set_mosaic_config(&mut self, config: nexrad::MosaicConfig) {
        self.mosaic = Mosaic::new(config);
    }

    pub fn mosaic_stats(&self) -> nexrad::MosaicStats {
        self.mosaic.stats()
    }

    /// Compute the layout for the current canvas size.
    pub fn layout(&self, canvas: &Canvas) -> Layout {
        Layout::for_size(canvas.width() as f32, canvas.height() as f32, &self.theme)
    }

    pub fn font(&self) -> FontId {
        self.font
    }

    /// Draw one complete frame.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        state: &AppState,
        view: &ViewState,
        now: Instant,
    ) -> FrameStats {
        let layout = self.layout(canvas);

        let stats = match view.page {
            Page::PlanView => self.draw_plan_view(canvas, state, view, now, &layout),
            Page::Weather => {
                weatherpage::draw(self, canvas, state, view, now, &layout);
                FrameStats {
                    targets_no_position: state.non_positional_count(),
                    ..Default::default()
                }
            }
            Page::Ahrs => {
                ahrspage::draw(self, canvas, state, now, &layout);
                FrameStats {
                    targets_no_position: state.non_positional_count(),
                    ..Default::default()
                }
            }
        };

        statusbar::draw(self, canvas, state, now, &layout, &stats);
        // After the page, never before: the NEXRAD underlay is one quad that reaches the bottom
        // edge of the panel, so a footer drawn first would be painted over by the weather.
        footerbar::draw(self, canvas, state, view, &layout, &stats);
        // Both strips last, so neither is overdrawn by page content that ran long. The page strip
        // especially: it is the only way to change pages, so it must survive a drawing bug
        // elsewhere.
        softkeys::draw(self, canvas, view, &stats);
        pagestrip::draw(self, canvas, view);
        stats
    }

    fn draw_plan_view(
        &mut self,
        canvas: &mut Canvas,
        state: &AppState,
        view: &ViewState,
        now: Instant,
        layout: &Layout,
    ) -> FrameStats {
        // The projection is built here rather than inside the plan view so the weather underlay and
        // the traffic share exactly one projection. Two independently-derived projections would
        // drift apart under rotation and put the weather somewhere other than where it is.
        let projection = planview::make_projection(self, state, view, now, layout);

        // Underlay first: beneath the rings, beneath the traffic, above the background.
        if let (Some(projection), true) = (projection.as_ref(), view.show_weather_underlay) {
            match self.mosaic.update(canvas, state, projection.origin(), now) {
                Ok(true) => self.mosaic.draw(canvas, projection),
                Ok(false) => {}
                // Weather is the least important thing on screen; never let it take down traffic.
                Err(e) => tracing::warn!(error = %e, "could not build the NEXRAD underlay"),
            }
        }

        // The map layer sits between the weather and the rings: over the precipitation, which is
        // the only thing on screen it should ever obscure, and under everything that moves.
        let map = match projection.as_ref() {
            Some(projection) => maplayer::draw(self, canvas, view, layout, projection),
            None => maplayer::Drawn::default(),
        };

        let mut stats = planview::draw(self, canvas, state, view, now, layout, projection);
        stats.airports_drawn = map.airports;
        stats.airspace_drawn = map.airspace;
        stats
    }
}

/// Where things go on screen. Derived from the panel size so the same code works on an 800x480
/// and a 1024x600 panel without a second set of constants.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub status_bar_height: f32,
    pub footer_height: f32,
    /// Screen position of own-ship, and therefore the centre of the rings.
    pub center: (f32, f32),
    /// Radius in pixels of the outermost range ring.
    pub outer_radius: f32,
    pub margin: f32,
    /// Width of each vertical strip. Both are the same width, which is what makes the content
    /// area centred on the panel rather than merely left of the keys.
    pub strip_width: f32,
    /// Left edge of the content area — immediately right of the function strip.
    ///
    /// **Nothing may be positioned from zero.** Everything that used to start at the left edge of
    /// the panel now starts here; the function strip is drawn over what used to be page content.
    pub content_x0: f32,
    /// Right edge of the content area — immediately left of the page strip.
    ///
    /// Deliberately a coordinate and not a width. The field this replaced was a width that most
    /// call sites used as if it were the right edge, which was true only while the content area
    /// began at zero. Keeping the name and changing the meaning would have compiled everywhere and
    /// been wrong in about thirty places.
    pub content_x1: f32,
}

impl Layout {
    pub fn for_size(width: f32, height: f32, theme: &Theme) -> Self {
        let status_bar_height = (theme.font_size_small * 2.6).max(28.0);
        let footer_height = theme.font_size_normal * 1.8;
        let margin = 8.0;

        // 12% of the panel, floored at 72 px. The floor is what matters: these are the only
        // controls, they get pressed in turbulence, and a strip that scales below roughly a
        // fingertip stops being hittable long before it stops being readable.
        let strip_width = (width * 0.12).max(72.0).min(width * 0.25);
        let content_x0 = strip_width;
        let content_x1 = (width - strip_width).max(content_x0 + 1.0);
        let content_width = content_x1 - content_x0;

        let plan_top = status_bar_height;
        let plan_height = (height - status_bar_height - footer_height).max(1.0);
        let center = (
            content_x0 + content_width * 0.5,
            plan_top + plan_height * 0.5,
        );

        // Leave room outside the ring for its label and the compass ticks.
        //
        // On the 800x480 panel this is height-bound with room to spare — the rings need 426 px of
        // width and have 608 — which is why a second strip cost no ring radius at all. Width only
        // begins to bind once the two strips together take more than 374 px.
        let radius_limit = (plan_height * 0.5).min(content_width * 0.5) - margin;
        let outer_radius = (radius_limit - theme.font_size_small * 1.6).max(24.0);

        Self {
            width,
            height,
            status_bar_height,
            footer_height,
            center,
            outer_radius,
            margin,
            strip_width,
            content_x0,
            content_x1,
        }
    }

    /// Width of the area between the two strips.
    pub fn content_width(&self) -> f32 {
        self.content_x1 - self.content_x0
    }

    /// Left edge for page content, inside the margin. **Not `margin`** — that was the left edge
    /// only while the content area began at zero.
    pub fn content_left(&self) -> f32 {
        self.content_x0 + self.margin
    }

    /// Right edge for page content, inside the margin.
    pub fn content_right(&self) -> f32 {
        self.content_x1 - self.margin
    }

    /// Top of both strips: immediately below the status bar.
    ///
    /// The bars run edge to edge and the strips sit between them, rather than the strips running
    /// full height and interrupting the bars at each end. A bar that stops short of the panel edge
    /// reads as a panel of three columns; a bar that crosses it reads as a bar.
    pub fn strip_y0(&self) -> f32 {
        self.status_bar_height
    }

    /// Bottom of both strips: the top of the footer bar.
    pub fn strip_y1(&self) -> f32 {
        (self.height - self.footer_height).max(self.strip_y0() + 1.0)
    }

    /// Height of the strips, between the two bars.
    pub fn strip_height(&self) -> f32 {
        self.strip_y1() - self.strip_y0()
    }

    /// Top of the footer bar.
    pub fn footer_y0(&self) -> f32 {
        self.height - self.footer_height
    }
}
