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
pub mod font;
pub mod interact;
pub mod nexrad;
pub mod planview;
pub mod projection;
pub mod reckon;
pub mod softkeys;
pub mod statusbar;
pub mod symbols;
pub mod theme;
pub mod threat;
pub mod weatherpage;

use std::time::Instant;

use anyhow::Result;
use avionics_gfx::femtovg::FontId;
use avionics_gfx::Canvas;
use stratux_client::AppState;

pub use nexrad::Mosaic;
pub use projection::{Orientation, Projection};
pub use reckon::ReckonConfig;
pub use theme::Theme;
pub use threat::{ThreatConfig, ThreatLevel};

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
}

/// What the pilot has selected. Persisted across frames by the app; mutated by touch.
#[derive(Debug, Clone)]
pub struct ViewState {
    pub page: Page,
    pub range_nm: f32,
    pub orientation: Orientation,
    /// Index of the first weather entry shown. Clamped at draw time, since pruning can shrink the
    /// list between frames.
    pub weather_scroll: usize,
    /// Draw the NEXRAD underlay on the plan view.
    pub show_weather_underlay: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            page: Page::PlanView,
            // 10 nm is the useful default in a light aircraft: far enough to see converging
            // traffic with time to act, close enough that the circuit isn't a single blob.
            range_nm: 10.0,
            orientation: Orientation::NorthUp,
            weather_scroll: 0,
            show_weather_underlay: true,
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
}

/// What happened while drawing a frame. Feeds the status bar and the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub targets_drawn: usize,
    /// Positional targets beyond the selected range, so culled from the rings.
    pub targets_outside_range: usize,
    /// Targets heard without a position: Mode-S only, or ADS-B before its first position report.
    pub targets_no_position: usize,
    pub targets_coasting: usize,
    pub advisories: usize,
    pub alerts: usize,
    /// Tags dropped because there was nowhere to put them without overlapping. The symbols are
    /// still drawn; only the labels were lost.
    pub tags_suppressed: usize,
}

/// Holds the loaded font and the tuning constants. Cheap to keep for the process lifetime.
pub struct Ui {
    font: FontId,
    pub theme: Theme,
    pub reckon: ReckonConfig,
    pub threat: ThreatConfig,
    mosaic: Mosaic,
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
        })
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

        statusbar::draw(self, canvas, state, view, now, &layout, &stats);
        // Last, so the strip is never overdrawn by page content that ran long. It is the only
        // way to change pages, so it must survive a drawing bug elsewhere.
        softkeys::draw(self, canvas, view);
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

        planview::draw(self, canvas, state, view, now, layout, projection)
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
    /// Width of the soft-key strip down the right-hand edge.
    pub strip_width: f32,
    /// Width available to everything else. **Use this, not `width`, for any content that must
    /// not slide under the soft keys** — the strip is drawn over the right edge of the panel.
    pub content_width: f32,
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
        let content_width = (width - strip_width).max(1.0);

        let plan_top = status_bar_height;
        let plan_height = (height - status_bar_height - footer_height).max(1.0);
        let center = (content_width * 0.5, plan_top + plan_height * 0.5);

        // Leave room outside the ring for its label and the compass ticks.
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
            content_width,
        }
    }
}
