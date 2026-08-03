//! Colours and sizes.
//!
//! Tuned for a 7" panel in daylight behind a windscreen, which is a harsher environment than a
//! desk monitor: thin lines disappear, mid greys wash out, and anything below about 9 px is
//! unreadable with vibration. Hence heavier strokes and fewer, more saturated colours than a
//! desktop UI would use.
//!
//! The background is deliberately not pure black. On a dim panel, pure black makes "the app
//! crashed" and "the app is running with nothing to draw" look identical.

use avionics_gfx::femtovg::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Color,
    pub bar_background: Color,

    pub ring: Color,
    pub ring_fill: Color,
    pub ring_label: Color,
    pub compass_tick: Color,

    pub ownship: Color,

    /// Artificial-horizon sky and ground. Muted rather than saturated: this sits on a panel read
    /// in daylight, and a vivid blue/brown next to amber traffic symbols costs contrast where it
    /// matters more.
    pub ahrs_sky: Color,
    pub ahrs_ground: Color,

    pub target_normal: Color,
    pub target_advisory: Color,
    pub target_alert: Color,
    /// Outline drawn under every symbol so it stays legible over weather.
    pub target_outline: Color,

    pub tag_text: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_dim: Color,

    pub good: Color,
    pub caution: Color,
    pub warning: Color,

    /// Class B, C and D boundaries, in the sectional convention: B and D share a blue and are told
    /// apart by D being dashed, C is magenta.
    ///
    /// **The blue is indigo, not the rings' cyan.** The first attempt used a blue close to the
    /// sectional's and it read as more range rings on the panel — `ring` is (90, 190, 250), and two
    /// blue curves of similar hue are not distinguishable at a glance in daylight. Pushing the hue
    /// towards violet and dropping the green channel from 155 to 132 separates them.
    ///
    /// Class C is the other one to watch: `nexrad.rs` paints extreme precipitation magenta too.
    /// Those are told apart by weight rather than hue — weather is a saturated fill, airspace a
    /// thin stroke — which is why this stays well short of `RAMP`'s (210, 45, 195).
    pub airspace_b: Color,
    pub airspace_c: Color,
    pub airspace_d: Color,

    /// Airports and their labels. A recessive neutral rather than a fourth saturated hue: the
    /// sectional's blue and magenta are spent on airspace, and a background feature should read as
    /// one.
    pub airport: Color,
    pub airport_label: Color,

    pub font_size_tag: f32,
    pub font_size_small: f32,
    pub font_size_normal: f32,
    pub font_size_large: f32,

    pub symbol_size: f32,
    pub line_width: f32,
}

impl Theme {
    /// The standard dark cockpit theme.
    pub fn dark() -> Self {
        Self {
            background: Color::rgb(8, 10, 14),
            bar_background: Color::rgbaf(0.05, 0.07, 0.10, 0.94),

            ring: Color::rgba(90, 190, 250, 200),
            ring_fill: Color::rgbaf(0.13, 0.35, 0.62, 0.10),
            ring_label: Color::rgb(130, 190, 225),
            compass_tick: Color::rgba(120, 200, 245, 170),

            ownship: Color::rgb(120, 255, 170),
            ahrs_sky: Color::rgb(38, 96, 150),
            ahrs_ground: Color::rgb(112, 74, 38),

            target_normal: Color::rgb(220, 235, 245),
            target_advisory: Color::rgb(255, 200, 40),
            target_alert: Color::rgb(255, 80, 60),
            target_outline: Color::rgb(8, 10, 14),

            tag_text: Color::rgb(205, 222, 235),
            text_primary: Color::rgb(235, 245, 255),
            text_secondary: Color::rgb(160, 185, 205),
            text_dim: Color::rgb(105, 125, 145),

            good: Color::rgb(120, 235, 160),
            caution: Color::rgb(255, 195, 60),
            warning: Color::rgb(255, 90, 70),

            airspace_b: Color::rgba(128, 132, 248, 205),
            airspace_c: Color::rgba(206, 108, 186, 195),
            airspace_d: Color::rgba(128, 132, 248, 165),

            airport: Color::rgb(150, 172, 188),
            airport_label: Color::rgba(162, 184, 200, 220),

            font_size_tag: 11.0,
            font_size_small: 11.0,
            font_size_normal: 14.0,
            font_size_large: 19.0,

            symbol_size: 9.0,
            line_width: 1.6,
        }
    }

    pub fn colour_for(&self, level: crate::threat::ThreatLevel) -> Color {
        match level {
            crate::threat::ThreatLevel::Normal => self.target_normal,
            crate::threat::ThreatLevel::Advisory => self.target_advisory,
            crate::threat::ThreatLevel::Alert => self.target_alert,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Dim a colour by scaling its alpha. Used for coasting targets and stale weather.
pub fn faded(colour: Color, alpha: f32) -> Color {
    Color::rgbaf(colour.r, colour.g, colour.b, colour.a * alpha)
}
