//! The attitude page: artificial horizon, slip/skid, G-load.
//!
//! # Read this before changing anything here
//!
//! Stratux's AHRS is an uncalibrated MEMS sensor with no redundancy and no independent failure
//! monitor. It drifts, it can be wrong without saying so, and it is **not** a primary attitude
//! reference. This page therefore has one rule that outranks every aesthetic consideration:
//!
//! > **Never draw a horizon that is not backed by a live reading.**
//!
//! A level blue-over-brown drawn from missing data is the single most dangerous thing this
//! program could put on a screen. It looks exactly like a working attitude indicator reporting
//! wings level, which is precisely the picture a disoriented pilot most wants to believe. When
//! pitch or roll is absent or stale, [`draw`] paints the failure flag and nothing else — no
//! horizon, no pitch ladder, no roll pointer.
//!
//! The same reasoning drives the permanent `AHRS — NOT FOR PRIMARY REFERENCE` banner. It costs
//! one line of screen and removes any ambiguity about what this instrument is.
//!
//! # Sign conventions
//!
//! Stratux reports pitch positive nose-up and roll positive right-wing-down. On screen the
//! horizon moves *opposite* to the aircraft: nose-up pushes the horizon down, right bank rotates
//! it counter-clockwise, putting more ground on the right.
//!
//! **Confirmed by tilting the panel, 2026-07-31.** That check is the only one that counts here:
//! the maths is self-consistent whichever way round the signs go, so a unit test can prove the
//! horizon lands where the code intends and still not notice the code intends the wrong thing.
//! An inverted attitude indicator is confidently wrong exactly when someone leans on it. If these
//! signs are ever touched, re-verify the same way — on hardware, by eye.

use std::time::{Duration, Instant};

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path, Solidity};
use avionics_gfx::Canvas;
use stratux_client::AppState;

use crate::{Layout, Ui};

/// Attitude older than this is treated as no attitude at all.
///
/// `/situation` publishes at 10 Hz, so a full second of silence already means something upstream
/// has stopped. Erring short is deliberate: a stale horizon is worse than an absent one.
pub const STALE_AFTER: Duration = Duration::from_millis(1000);

/// Degrees of pitch shown from the centre to the top of the instrument.
const PITCH_RANGE_DEG: f32 = 25.0;

/// Roll-scale tick positions, in degrees either side of vertical.
const ROLL_TICKS: [f32; 5] = [10.0, 20.0, 30.0, 45.0, 60.0];

/// What the page decided to show. Returned so tests can assert the failure path without a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AhrsRender {
    /// A live attitude was drawn.
    Attitude,
    /// No usable attitude: the failure flag was drawn instead.
    Failed(AhrsFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AhrsFailure {
    /// No sensor has ever reported, or pitch/roll are the 3276.7 sentinel.
    NoData,
    /// Readings arrived once but have stopped.
    Stale,
}

impl AhrsFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoData => "AHRS UNAVAILABLE",
            Self::Stale => "AHRS DATA STALE",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::NoData => "no attitude sensor reporting",
            Self::Stale => "sensor stopped updating",
        }
    }
}

/// Decide what to draw. Pure, so the safety-critical branch is testable without a canvas.
pub fn classify(state: &AppState, now: Instant) -> AhrsRender {
    let ahrs = &state.ownship.ahrs;
    match ahrs.attitude() {
        None => AhrsRender::Failed(AhrsFailure::NoData),
        Some(_) if ahrs.is_stale(now, STALE_AFTER) => AhrsRender::Failed(AhrsFailure::Stale),
        Some(_) => AhrsRender::Attitude,
    }
}

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    now: Instant,
    layout: &Layout,
) -> AhrsRender {
    let decision = classify(state, now);

    // Instrument area: between the status bar and the footer, left of the soft keys.
    let top = layout.status_bar_height;
    let bottom = layout.height - layout.footer_height;
    let cx = layout.content_width * 0.5;
    let cy = (top + bottom) * 0.5;
    let radius = ((bottom - top) * 0.5).min(layout.content_width * 0.5) - layout.margin;

    match decision {
        AhrsRender::Attitude => {
            let (pitch, roll) = state
                .ownship
                .ahrs
                .attitude()
                .expect("classify returned Attitude");
            draw_horizon(ui, canvas, cx, cy, radius, pitch as f32, roll as f32);
            draw_roll_scale(ui, canvas, cx, cy, radius, roll as f32);
            draw_aircraft_symbol(ui, canvas, cx, cy, radius);
        }
        AhrsRender::Failed(failure) => draw_failure(ui, canvas, cx, cy, radius, failure),
    }

    draw_slip_skid(ui, canvas, state, cx, cy, radius);
    draw_side_readouts(ui, canvas, state, layout, cx, cy, radius);
    draw_readouts(ui, canvas, state, layout, top);
    draw_banner(ui, canvas, layout, bottom);

    decision
}

/// Blue-over-brown, translated for pitch and rotated for roll, clipped to the instrument circle.
fn draw_horizon(
    ui: &Ui,
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    radius: f32,
    pitch_deg: f32,
    roll_deg: f32,
) {
    let theme = &ui.theme;
    let px_per_deg = radius / PITCH_RANGE_DEG;

    // Confine the oversized horizon rectangles to the instrument's bounding box first, then mask
    // the corners off below. femtovg's scissor is rectangular, so a round gauge needs both steps;
    // without the mask the horizon bleeds into the corners and reads as the whole screen tilting.
    canvas.save();
    canvas.scissor(cx - radius, cy - radius, radius * 2.0, radius * 2.0);

    canvas.translate(cx, cy);
    // Screen-opposite: right bank (positive roll) rotates the horizon counter-clockwise.
    canvas.rotate(-roll_deg.to_radians());
    // Nose-up (positive pitch) pushes the horizon down the screen.
    canvas.translate(0.0, pitch_deg * px_per_deg);

    // Oversized so the rotated rectangle still covers the circle at any bank angle.
    let span = radius * 3.0;

    let mut sky = Path::new();
    sky.rect(-span, -span, span * 2.0, span);
    canvas.fill_path(&sky, &Paint::color(theme.ahrs_sky));

    let mut ground = Path::new();
    ground.rect(-span, 0.0, span * 2.0, span);
    canvas.fill_path(&ground, &Paint::color(theme.ahrs_ground));

    let mut horizon = Path::new();
    horizon.move_to(-span, 0.0);
    horizon.line_to(span, 0.0);
    canvas.stroke_path(
        &horizon,
        &Paint::color(theme.text_primary).with_line_width(2.0),
    );

    draw_pitch_ladder(ui, canvas, radius, px_per_deg);

    canvas.restore();

    // Mask the square corners back to background: a rectangle with the instrument circle punched
    // out of it, so only the disc survives.
    // The rect runs well off-screen on purpose: its own antialiased edge would otherwise show as
    // a faint box around the instrument, since it is drawn in the same colour as the background it
    // sits on. Everything drawn after this (roll scale, symbol, readouts, status bar, soft keys)
    // paints over the top, so an oversized mask costs nothing.
    let mut mask = Path::new();
    mask.rect(cx - radius * 8.0, cy - radius * 8.0, radius * 16.0, radius * 16.0);
    mask.circle(cx, cy, radius);
    mask.solidity(Solidity::Hole);
    canvas.fill_path(&mask, &Paint::color(theme.background));

    // Ring around the instrument, drawn after so it covers the horizon's edge.
    let mut ring = Path::new();
    ring.circle(cx, cy, radius);
    canvas.stroke_path(&ring, &Paint::color(theme.text_dim).with_line_width(1.5));
}

/// Pitch reference lines every 5 degrees, longer every 10.
fn draw_pitch_ladder(ui: &Ui, canvas: &mut Canvas, radius: f32, px_per_deg: f32) {
    let theme = &ui.theme;
    let mut paint = Paint::color(theme.text_primary);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Middle);

    for step in 1..=((PITCH_RANGE_DEG / 5.0) as i32 + 1) {
        let deg = step * 5;
        let major = deg % 10 == 0;
        let half = if major { radius * 0.28 } else { radius * 0.14 };

        for sign in [-1.0f32, 1.0] {
            // Negated: a positive pitch reference sits ABOVE the horizon on screen, and screen y
            // grows downward.
            let y = -sign * deg as f32 * px_per_deg;
            if y.abs() > radius * 1.6 {
                continue;
            }

            let mut line = Path::new();
            line.move_to(-half, y);
            line.line_to(half, y);
            canvas.stroke_path(
                &line,
                &Paint::color(theme.text_primary).with_line_width(if major { 1.6 } else { 1.0 }),
            );

            if major {
                let text = format!("{deg}");
                let _ = canvas.fill_text(-half - 14.0, y, &text, &paint);
                let _ = canvas.fill_text(half + 14.0, y, &text, &paint);
            }
        }
    }
}

/// Fixed roll scale with a moving pointer at the top of the instrument.
fn draw_roll_scale(ui: &Ui, canvas: &mut Canvas, cx: f32, cy: f32, radius: f32, roll_deg: f32) {
    let theme = &ui.theme;

    for tick in ROLL_TICKS {
        for sign in [-1.0f32, 1.0] {
            // Measured from straight up, hence the -90 degree offset.
            let angle = (sign * tick - 90.0).to_radians();
            let len = if tick == 30.0 || tick == 60.0 {
                radius * 0.12
            } else {
                radius * 0.07
            };
            let (sin, cos) = angle.sin_cos();
            let mut line = Path::new();
            line.move_to(cx + cos * radius, cy + sin * radius);
            line.line_to(cx + cos * (radius - len), cy + sin * (radius - len));
            canvas.stroke_path(
                &line,
                &Paint::color(theme.text_secondary).with_line_width(1.4),
            );
        }
    }

    // Pointer: a triangle that rotates with the aircraft, against the fixed scale above.
    let angle = (-roll_deg - 90.0).to_radians();
    let (sin, cos) = angle.sin_cos();
    let tip = (cx + cos * (radius - 2.0), cy + sin * (radius - 2.0));
    let base = radius * 0.055;
    let perp = (-sin, cos);
    let inner = radius - radius * 0.11;

    let mut pointer = Path::new();
    pointer.move_to(tip.0, tip.1);
    pointer.line_to(
        cx + cos * inner + perp.0 * base,
        cy + sin * inner + perp.1 * base,
    );
    pointer.line_to(
        cx + cos * inner - perp.0 * base,
        cy + sin * inner - perp.1 * base,
    );
    pointer.close();
    // Amber past 30 degrees: unusual attitude for the light aircraft this is fitted to.
    let colour = if roll_deg.abs() > 30.0 {
        theme.caution
    } else {
        theme.text_primary
    };
    canvas.fill_path(&pointer, &Paint::color(colour));
}

/// The fixed aircraft reference: wings and a centre dot.
fn draw_aircraft_symbol(ui: &Ui, canvas: &mut Canvas, cx: f32, cy: f32, radius: f32) {
    let theme = &ui.theme;
    let wing = radius * 0.34;
    let inner = radius * 0.10;

    let mut path = Path::new();
    path.move_to(cx - wing, cy);
    path.line_to(cx - inner, cy);
    path.move_to(cx + inner, cy);
    path.line_to(cx + wing, cy);
    canvas.stroke_path(&path, &Paint::color(theme.ownship).with_line_width(3.0));

    let mut dot = Path::new();
    dot.circle(cx, cy, 3.0);
    canvas.fill_path(&dot, &Paint::color(theme.ownship));
}

/// Slip/skid ball below the instrument.
fn draw_slip_skid(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    cx: f32,
    cy: f32,
    radius: f32,
) {
    let theme = &ui.theme;
    let y = cy + radius * 0.78;
    let half = radius * 0.22;
    let ball_r = radius * 0.045;

    let mut cage = Path::new();
    cage.move_to(cx - half, y - ball_r * 1.6);
    cage.line_to(cx - half, y + ball_r * 1.6);
    cage.move_to(cx + half, y - ball_r * 1.6);
    cage.line_to(cx + half, y + ball_r * 1.6);
    canvas.stroke_path(&cage, &Paint::color(theme.text_secondary).with_line_width(1.5));

    let Some(slip) = state.ownship.ahrs.slip_skid_deg else {
        return;
    };
    // Full deflection at 10 degrees of lateral acceleration, clamped so the ball stays in its cage.
    let offset = (slip as f32 / 10.0).clamp(-1.0, 1.0) * half;
    let mut ball = Path::new();
    ball.circle(cx + offset, y, ball_r);
    canvas.fill_path(&ball, &Paint::color(theme.text_primary));
}

/// The failure flag. Deliberately loud, and deliberately instead of — never on top of — a horizon.
fn draw_failure(
    ui: &Ui,
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    radius: f32,
    failure: AhrsFailure,
) {
    let theme = &ui.theme;

    let mut disc = Path::new();
    disc.circle(cx, cy, radius);
    canvas.fill_path(&disc, &Paint::color(theme.background));
    canvas.stroke_path(&disc, &Paint::color(theme.warning).with_line_width(2.0));

    // A big X across the instrument: the conventional "this instrument is not to be used" mark,
    // and unmistakable at a glance even without reading the text.
    let d = radius * 0.62;
    let mut cross = Path::new();
    cross.move_to(cx - d, cy - d);
    cross.line_to(cx + d, cy + d);
    cross.move_to(cx + d, cy - d);
    cross.line_to(cx - d, cy + d);
    canvas.stroke_path(&cross, &Paint::color(theme.warning).with_line_width(4.0));

    let mut paint = Paint::color(theme.warning);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_large);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Middle);
    let _ = canvas.fill_text(cx, cy - radius * 0.22, failure.label(), &paint);

    paint.set_font_size(theme.font_size_small);
    paint.set_color(theme.text_secondary);
    let _ = canvas.fill_text(cx, cy + radius * 0.22, failure.detail(), &paint);
}

/// Which altitude source a reading came from.
///
/// The label follows the source rather than the other way round. Falling back from baro to GPS
/// under a fixed "ALT" caption would silently change what the number means — GPS MSL and pressure
/// altitude disagree by the local altimeter setting, which is exactly the error you would not
/// want to discover by reading it off a display that did not mention the swap.
enum AltSource {
    Baro(f32),
    Gps(f32),
    None,
}

fn altitude_source(state: &AppState) -> AltSource {
    match (
        state.ownship.pressure_altitude_ft,
        state.ownship.altitude_msl_ft,
    ) {
        (Some(baro), _) => AltSource::Baro(baro),
        (None, Some(gps)) => AltSource::Gps(gps),
        (None, None) => AltSource::None,
    }
}

/// Ground speed on the left, altitude on the right — speed-left/altitude-right is the layout
/// every glass panel uses, so it costs nothing to learn and is where the eye already goes.
///
/// Both are placed outside the instrument circle rather than over it: an attitude indicator with
/// numbers painted across the horizon is harder to read at a glance in turbulence, and the whole
/// point of the round gauge is that its picture reads pre-attentively.
fn draw_side_readouts(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    layout: &Layout,
    cx: f32,
    cy: f32,
    radius: f32,
) {
    let theme = &ui.theme;

    let mut caption = Paint::color(theme.text_dim);
    caption.set_font(&[ui.font()]);
    caption.set_font_size(theme.font_size_tag);
    caption.set_text_baseline(Baseline::Bottom);

    let mut value = Paint::color(theme.text_primary);
    value.set_font(&[ui.font()]);
    value.set_font_size(theme.font_size_large);
    value.set_text_baseline(Baseline::Top);

    // Ground speed, left. GPS-derived, so it is present whenever there is a fix and absent
    // otherwise — never a zero standing in for "unknown".
    caption.set_text_align(Align::Left);
    value.set_text_align(Align::Left);
    let left = layout.margin;
    let gs = state
        .ownship
        .ground_speed_kt
        .map(|v| format!("{v:.0}"))
        .unwrap_or_else(|| "---".into());
    let _ = canvas.fill_text(left, cy - 6.0, "GS kt", &caption);
    let _ = canvas.fill_text(left, cy - 2.0, &gs, &value);

    // Altitude, right, captioned with the sensor it actually came from.
    caption.set_text_align(Align::Right);
    value.set_text_align(Align::Right);
    let right = layout.content_width - layout.margin;
    let (cap, text) = match altitude_source(state) {
        AltSource::Baro(ft) => ("BARO ft", format!("{ft:.0}")),
        AltSource::Gps(ft) => ("GPS ft", format!("{ft:.0}")),
        AltSource::None => ("ALT ft", "---".to_string()),
    };
    let _ = canvas.fill_text(right, cy - 6.0, cap, &caption);
    let _ = canvas.fill_text(right, cy - 2.0, &text, &value);

    // Keep the numbers clear of the instrument: if the panel is ever narrow enough that they
    // would collide with the circle, the circle is what shrinks, not the readouts.
    debug_assert!(
        cx - radius >= left,
        "instrument overlaps the ground-speed readout"
    );
}

/// Numeric readouts along the top of the instrument area.
fn draw_readouts(ui: &Ui, canvas: &mut Canvas, state: &AppState, layout: &Layout, top: f32) {
    let theme = &ui.theme;
    let ahrs = &state.ownship.ahrs;

    let mut label = Paint::color(theme.text_dim);
    label.set_font(&[ui.font()]);
    label.set_font_size(theme.font_size_tag);
    label.set_text_baseline(Baseline::Middle);

    let mut value = Paint::color(theme.text_primary);
    value.set_font(&[ui.font()]);
    value.set_font_size(theme.font_size_small);
    value.set_text_baseline(Baseline::Middle);

    let y = top + theme.font_size_small * 1.1;
    let mut x = layout.margin;

    // Absent readings print as dashes. Never a zero: a zero is a measurement.
    let field = |canvas: &mut Canvas, x: &mut f32, name: &str, text: String| {
        let _ = canvas.fill_text(*x, y, name, &label);
        let width = canvas
            .measure_text(0.0, 0.0, name, &label)
            .map(|m| m.width())
            .unwrap_or(0.0);
        let _ = canvas.fill_text(*x + width + 5.0, y, &text, &value);
        let vw = canvas
            .measure_text(0.0, 0.0, &text, &value)
            .map(|m| m.width())
            .unwrap_or(0.0);
        *x += width + vw + 22.0;
    };

    field(
        canvas,
        &mut x,
        "PITCH",
        fmt_deg(ahrs.pitch_deg),
    );
    field(canvas, &mut x, "ROLL", fmt_deg(ahrs.roll_deg));
    field(
        canvas,
        &mut x,
        "G",
        ahrs.g_load
            .map(|g| format!("{g:.2}"))
            .unwrap_or_else(|| "---".into()),
    );
    field(
        canvas,
        &mut x,
        "HDG",
        ahrs.mag_heading_deg
            .or(ahrs.gyro_heading_deg)
            .map(|h| format!("{:03.0}\u{00B0}", h.rem_euclid(360.0)))
            .unwrap_or_else(|| "---".into()),
    );
}

fn fmt_deg(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:+.1}\u{00B0}"),
        None => "---".into(),
    }
}

/// The standing reminder that this is not a primary instrument.
fn draw_banner(ui: &Ui, canvas: &mut Canvas, layout: &Layout, bottom: f32) {
    let theme = &ui.theme;
    let mut paint = Paint::color(theme.caution);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Bottom);
    let _ = canvas.fill_text(
        layout.content_width * 0.5,
        bottom - 3.0,
        "AHRS \u{2014} NOT FOR PRIMARY REFERENCE",
        &paint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use stratux_client::domain::Ahrs;

    /// A non-zero status word: what the target reports when a module is present.
    const REPORTING: u8 = 6;

    fn state_with(ahrs: Ahrs) -> AppState {
        let mut state = AppState::new();
        state.ownship.ahrs = ahrs;
        state
    }

    #[test]
    fn a_zero_status_word_means_failure_even_with_plausible_numbers() {
        // The dangerous case: a Stratux with no AHRS reports 0.0 pitch and 0.0 roll, which is
        // indistinguishable from wings-level unless the status word is consulted.
        let now = Instant::now();
        let state = state_with(Ahrs {
            pitch_deg: Some(0.0),
            roll_deg: Some(0.0),
            status: 0,
            received: Some(now),
            ..Default::default()
        });
        assert_eq!(
            classify(&state, now),
            AhrsRender::Failed(AhrsFailure::NoData)
        );
    }

    #[test]
    fn no_sensor_means_failure_not_a_level_horizon() {
        let now = Instant::now();
        let state = state_with(Ahrs::default());
        assert_eq!(
            classify(&state, now),
            AhrsRender::Failed(AhrsFailure::NoData)
        );
    }

    #[test]
    fn the_invalid_sentinel_never_reaches_the_horizon() {
        // 3276.7 must not be mistaken for a very steep attitude.
        assert_eq!(Ahrs::value(3276.7), None);
        assert_eq!(Ahrs::value(3276.68), None);
        assert_eq!(Ahrs::value(f64::NAN), None);
        assert_eq!(Ahrs::value(0.0), Some(0.0));
        assert_eq!(Ahrs::value(-12.5), Some(-12.5));
    }

    #[test]
    fn roll_without_pitch_is_a_failure_not_a_partial_indicator() {
        let now = Instant::now();
        let state = state_with(Ahrs {
            roll_deg: Some(20.0),
            pitch_deg: None,
            status: REPORTING,
            received: Some(now),
            ..Default::default()
        });
        assert_eq!(
            classify(&state, now),
            AhrsRender::Failed(AhrsFailure::NoData)
        );
    }

    #[test]
    fn a_stale_reading_is_flagged_rather_than_frozen_on_screen() {
        let now = Instant::now();
        let state = state_with(Ahrs {
            pitch_deg: Some(2.0),
            roll_deg: Some(3.0),
            status: REPORTING,
            received: Some(now - Duration::from_millis(1500)),
            ..Default::default()
        });
        assert_eq!(classify(&state, now), AhrsRender::Failed(AhrsFailure::Stale));
    }

    #[test]
    fn a_fresh_reading_draws_the_attitude() {
        let now = Instant::now();
        let state = state_with(Ahrs {
            pitch_deg: Some(2.0),
            roll_deg: Some(3.0),
            status: REPORTING,
            received: Some(now - Duration::from_millis(100)),
            ..Default::default()
        });
        assert_eq!(classify(&state, now), AhrsRender::Attitude);
    }

    #[test]
    fn zero_attitude_is_a_reading_not_an_absence() {
        // Exactly level must render, not trip the failure path.
        let now = Instant::now();
        let state = state_with(Ahrs {
            pitch_deg: Some(0.0),
            roll_deg: Some(0.0),
            status: REPORTING,
            received: Some(now),
            ..Default::default()
        });
        assert_eq!(classify(&state, now), AhrsRender::Attitude);
    }

    #[test]
    fn absent_readouts_print_dashes_never_zero() {
        assert_eq!(fmt_deg(None), "---");
        assert_eq!(fmt_deg(Some(0.0)), "+0.0\u{00B0}");
    }
}

#[cfg(test)]
mod side_readout_tests {
    use super::*;
    use stratux_client::AppState;

    #[test]
    fn altitude_is_captioned_with_the_sensor_it_came_from() {
        let mut state = AppState::new();

        // Baro wins when present: it is what traffic reports, so it is the like-for-like number.
        state.ownship.pressure_altitude_ft = Some(4800.0);
        state.ownship.altitude_msl_ft = Some(5000.0);
        assert!(matches!(altitude_source(&state), AltSource::Baro(v) if v == 4800.0));

        // Without a pressure sensor the number is GPS MSL — and the caption must say so rather
        // than silently relabelling it, since the two differ by the altimeter setting.
        state.ownship.pressure_altitude_ft = None;
        assert!(matches!(altitude_source(&state), AltSource::Gps(v) if v == 5000.0));

        // No fix at all: dashes, never a zero.
        state.ownship.altitude_msl_ft = None;
        assert!(matches!(altitude_source(&state), AltSource::None));
    }
}
