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

use avionics_gfx::femtovg::{Align, Baseline, Color, Paint, Path};
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

/// Widths of the three tapes, in pixels on the 800-wide target.
const GS_TAPE_W: f32 = 62.0;
const ALT_TAPE_W: f32 = 70.0;
const VSI_W: f32 = 34.0;

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    now: Instant,
    layout: &Layout,
) -> AhrsRender {
    let decision = classify(state, now);

    let top = layout.status_bar_height;
    let bottom = layout.height - layout.footer_height;

    // The attitude fills the whole area and the tapes overlay it translucently, rather than the
    // attitude being a round gauge boxed in beside them. On an 800x480 panel that is the
    // difference between a horizon you can read at a glance and three instruments competing for
    // a strip each — and it matches how every glass panel lays this out.
    // The horizon runs the full width and the tapes sit translucently on top of it, exactly as
    // on the reference panel. Stopping it at the tape edge instead makes the tapes read as walls
    // boxing in a small picture, and wastes the widest part of an already small screen.
    let horizon_left = layout.content_x0;
    let horizon_right = layout.content_x1;
    let cx = layout.content_x0 + layout.content_width() * 0.5;
    // Leave a band at the bottom for the heading readout.
    let heading_band = ui.theme.font_size_large * 1.7;
    let cy = (top + bottom - heading_band) * 0.5;
    let half_h = (bottom - heading_band - top) * 0.5;

    match decision {
        AhrsRender::Attitude => {
            let (pitch, roll) = state
                .ownship
                .ahrs
                .attitude()
                .expect("classify returned Attitude");
            draw_horizon(
                ui,
                canvas,
                cx,
                cy,
                half_h,
                horizon_left,
                horizon_right,
                top,
                bottom - heading_band,
                pitch as f32,
                roll as f32,
            );
            draw_roll_scale(ui, canvas, cx, cy, half_h, roll as f32);
            draw_aircraft_symbol(ui, canvas, cx, cy, half_h);
            draw_slip_skid(ui, canvas, state, cx, cy, half_h);
        }
        AhrsRender::Failed(failure) => {
            draw_failure(ui, canvas, cx, cy, half_h.min(horizon_right - cx), failure)
        }
    }

    // The tapes are driven by GPS and the pressure sensor, NOT by the AHRS, so they keep working
    // when the attitude fails. Blanking them alongside the horizon would throw away good data
    // because a different sensor died.
    draw_tapes(ui, canvas, state, layout, top, bottom);
    draw_heading(ui, canvas, state, cx, bottom - heading_band, bottom);
    draw_banner(
        ui,
        canvas,
        layout.content_x0 + GS_TAPE_W + 8.0,
        bottom - heading_band - 5.0,
    );

    decision
}

fn draw_tapes(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    layout: &Layout,
    top: f32,
    bottom: f32,
) {
    use crate::tapes::{self, Side, Tape, Vsi};

    // Ground speed. 0.25 kt per pixel puts about 100 kt across the tape, so circuit speeds and
    // cruise both show useful movement.
    tapes::draw(
        ui,
        canvas,
        &Tape {
            x: layout.content_x0,
            width: GS_TAPE_W,
            top,
            bottom,
            units_per_px: 0.25,
            major: 10.0,
            minor: 5.0,
            side: Side::Left,
            label: "GS KT",
        },
        state.ownship.ground_speed_kt,
        |v| format!("{v:.0}"),
    );

    // Altitude, captioned by its source. 2.5 ft per pixel gives roughly 1000 ft across the tape.
    let (label, value) = match altitude_source(state) {
        AltSource::Baro(ft) => ("BARO FT", Some(ft as f64)),
        AltSource::Gps(ft) => ("GPS FT", Some(ft as f64)),
        AltSource::None => ("ALT FT", None),
    };
    tapes::draw(
        ui,
        canvas,
        &Tape {
            x: layout.content_x1 - ALT_TAPE_W - VSI_W,
            width: ALT_TAPE_W,
            top,
            bottom,
            units_per_px: 2.5,
            major: 100.0,
            minor: 20.0,
            side: Side::Right,
            label,
        },
        value,
        |v| format!("{v:.0}"),
    );

    tapes::draw_vsi(
        ui,
        canvas,
        &Vsi {
            x: layout.content_x1 - VSI_W,
            width: VSI_W,
            top,
            bottom,
            full_scale: 2000.0,
        },
        state.ownship.vertical_speed_fpm.map(|v| v as f64),
    );
}

/// Heading or track, in a box under the attitude — the position it occupies on a real panel.
fn draw_heading(ui: &Ui, canvas: &mut Canvas, state: &AppState, cx: f32, top: f32, bottom: f32) {
    let theme = &ui.theme;
    let source = heading_source(state);
    let cy = (top + bottom) * 0.5;

    let w = theme.font_size_large * 5.0;
    let h = theme.font_size_large * 1.5;
    let mut boxed = Path::new();
    boxed.rect(cx - w * 0.5, cy - h * 0.5, w, h);
    canvas.fill_path(&boxed, &Paint::color(Color::rgba(0, 0, 0, 235)));
    canvas.stroke_path(
        &boxed,
        &Paint::color(theme.text_primary).with_line_width(1.4),
    );

    let mut value = Paint::color(theme.text_primary);
    value.set_font(&[ui.font()]);
    value.set_font_size(theme.font_size_large);
    value.set_text_align(Align::Center);
    value.set_text_baseline(Baseline::Middle);
    let _ = canvas.fill_text(cx, cy, source.text(), &value);

    // The caption that named the source used to sit just below this box, which put it inside the
    // footer band and underneath the footer bar. It now has a field of its own in that bar, still
    // attached to the same number. See `footerbar::ahrs`.
}

/// Blue-over-brown filling the attitude area, translated for pitch and rotated for roll.
///
/// Full-bleed rather than a round gauge: the horizon is the primary picture on this page and the
/// tapes overlay it. That also removes the circular mask the round version needed, and with it
/// the antialiased seam that mask used to leave around the instrument.
#[allow(clippy::too_many_arguments)]
fn draw_horizon(
    ui: &Ui,
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    half_h: f32,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    pitch_deg: f32,
    roll_deg: f32,
) {
    let theme = &ui.theme;
    let px_per_deg = half_h / PITCH_RANGE_DEG;

    canvas.save();
    canvas.scissor(left, top, right - left, bottom - top);

    canvas.translate(cx, cy);
    // Screen-opposite: right bank (positive roll) rotates the horizon counter-clockwise, putting
    // more ground on the right. Verified by tilting the panel — see the module docs.
    canvas.rotate(-roll_deg.to_radians());
    // Nose-up (positive pitch) pushes the horizon down the screen.
    canvas.translate(0.0, pitch_deg * px_per_deg);

    // Oversized so the rotated rectangles still cover the area at any bank angle.
    let span = (right - left).max(bottom - top) * 2.0;

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

    draw_pitch_ladder(ui, canvas, half_h, px_per_deg);

    canvas.restore();
}

/// Pitch reference lines every 5 degrees, longer every 10.
fn draw_pitch_ladder(ui: &Ui, canvas: &mut Canvas, half_h: f32, px_per_deg: f32) {
    let theme = &ui.theme;
    let mut paint = Paint::color(theme.text_primary);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Middle);

    // Two paths, one per line width, stroked once each. Individually this was a dozen
    // `stroke_path` calls, and femtovg issues a GL draw for every one of them — a dozen tile
    // binning passes on the vc4 to put a dozen straight lines on screen.
    let mut majors = Path::new();
    let mut minors = Path::new();

    for step in 1..=((PITCH_RANGE_DEG / 5.0) as i32 + 1) {
        let deg = step * 5;
        let major = deg % 10 == 0;
        let half = if major { half_h * 0.30 } else { half_h * 0.15 };

        for sign in [-1.0f32, 1.0] {
            // Negated: a positive pitch reference sits ABOVE the horizon on screen, and screen y
            // grows downward.
            let y = -sign * deg as f32 * px_per_deg;
            if y.abs() > half_h * 1.6 {
                continue;
            }

            let rung = if major { &mut majors } else { &mut minors };
            rung.move_to(-half, y);
            rung.line_to(half, y);
        }
    }

    canvas.stroke_path(
        &minors,
        &Paint::color(theme.text_primary).with_line_width(1.0),
    );
    canvas.stroke_path(
        &majors,
        &Paint::color(theme.text_primary).with_line_width(1.6),
    );

    // Labels last, so a rung can never land on top of a digit. They sit 14 px beyond the ends of
    // the rungs, so in practice the two never meet; ordering it this way keeps that true if the
    // ladder is ever widened.
    for step in 1..=((PITCH_RANGE_DEG / 5.0) as i32 + 1) {
        let deg = step * 5;
        if deg % 10 != 0 {
            continue;
        }
        let half = half_h * 0.30;
        let text = format!("{deg}");

        for sign in [-1.0f32, 1.0] {
            let y = -sign * deg as f32 * px_per_deg;
            if y.abs() > half_h * 1.6 {
                continue;
            }
            let _ = canvas.fill_text(-half - 14.0, y, &text, &paint);
            let _ = canvas.fill_text(half + 14.0, y, &text, &paint);
        }
    }
}

/// Fixed roll scale with a moving pointer at the top of the instrument.
fn draw_roll_scale(ui: &Ui, canvas: &mut Canvas, cx: f32, cy: f32, radius: f32, roll_deg: f32) {
    let theme = &ui.theme;

    // Every tick shares a colour and a width, so they all go in one path and one draw. Only the
    // length varies, and that is geometry rather than paint state.
    let mut ticks = Path::new();
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
            ticks.move_to(cx + cos * radius, cy + sin * radius);
            ticks.line_to(cx + cos * (radius - len), cy + sin * (radius - len));
        }
    }
    canvas.stroke_path(
        &ticks,
        &Paint::color(theme.text_secondary).with_line_width(1.4),
    );

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
fn draw_slip_skid(ui: &Ui, canvas: &mut Canvas, state: &AppState, cx: f32, cy: f32, radius: f32) {
    let theme = &ui.theme;
    let y = cy + radius * 0.78;
    let half = radius * 0.22;
    let ball_r = radius * 0.045;

    let mut cage = Path::new();
    cage.move_to(cx - half, y - ball_r * 1.6);
    cage.line_to(cx - half, y + ball_r * 1.6);
    cage.move_to(cx + half, y - ball_r * 1.6);
    cage.line_to(cx + half, y + ball_r * 1.6);
    canvas.stroke_path(
        &cage,
        &Paint::color(theme.text_secondary).with_line_width(1.5),
    );

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
fn draw_failure(ui: &Ui, canvas: &mut Canvas, cx: f32, cy: f32, radius: f32, failure: AhrsFailure) {
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

/// Where a directional reading came from, and therefore what it actually means.
///
/// These are **not interchangeable**, which is why the caption changes with the source rather
/// than everything being labelled `HDG`:
///
/// * Magnetic heading is where the nose points, referenced to magnetic north.
/// * Gyro heading is where the nose points, but integrated and free to drift.
/// * GPS ground track is where the aircraft is *going*, which in any crosswind is a different
///   number — tens of degrees apart in a strong one. Showing it captioned `HDG` would be a
///   plain lie, and the wind is exactly when someone would be looking.
///
/// On the target both AHRS heading fields read the 3276.7 sentinel, so `Track` is the live case.
pub(crate) enum HeadingSource {
    Mag(f64),
    Gyro(f64),
    Track(f64),
    None,
}

pub(crate) fn heading_source(state: &AppState) -> HeadingSource {
    let ahrs = &state.ownship.ahrs;
    if let Some(v) = ahrs.mag_heading_deg {
        HeadingSource::Mag(v)
    } else if let Some(v) = ahrs.gyro_heading_deg {
        HeadingSource::Gyro(v)
    } else if let Some(v) = state.ownship.track_deg {
        HeadingSource::Track(v as f64)
    } else {
        HeadingSource::None
    }
}

impl HeadingSource {
    pub(crate) fn caption(&self) -> &'static str {
        match self {
            Self::Mag(_) => "HDG mag",
            Self::Gyro(_) => "HDG gyro",
            Self::Track(_) => "TRK gps",
            Self::None => "HDG",
        }
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Mag(v) | Self::Gyro(v) | Self::Track(v) => {
                format!("{:03.0}\u{00B0}", v.rem_euclid(360.0))
            }
            Self::None => "---".into(),
        }
    }
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

/// The standing reminder that this is not a primary instrument.
///
/// Bottom-left of the attitude area: clear of the roll pointer at the top, which it overprinted,
/// and clear of the heading box at the bottom centre, which it also overprinted. Left-aligned
/// rather than centred so it cannot collide with either as the layout changes.
///
/// # Why it has a backing box
///
/// It cannot avoid the pitch ladder. The ladder is drawn at whatever pitch the aircraft is at, so
/// there is no fixed corner of the attitude area that is reliably empty — and when the text grew
/// with the rest of the panel it landed squarely on the -30 rung. A caution annunciator that is
/// sometimes illegible is worse than one that costs a little of the horizon, which is why every
/// EFIS draws these on a filled field.
fn draw_banner(ui: &Ui, canvas: &mut Canvas, x: f32, y: f32) {
    let theme = &ui.theme;
    const TEXT: &str = "AHRS \u{2014} NOT FOR PRIMARY REFERENCE";

    let mut paint = Paint::color(theme.caution);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Left);
    paint.set_text_baseline(Baseline::Bottom);

    let width = canvas
        .measure_text(0.0, 0.0, TEXT, &paint)
        .map(|m| m.width())
        .unwrap_or(0.0);
    let pad = 4.0;
    let height = theme.font_size_tag * 1.35;

    let mut box_path = Path::new();
    box_path.rect(x - pad, y - height, width + pad * 2.0, height + pad * 0.5);
    canvas.fill_path(&box_path, &Paint::color(theme.target_outline));

    let _ = canvas.fill_text(x, y, TEXT, &paint);
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
        assert_eq!(
            classify(&state, now),
            AhrsRender::Failed(AhrsFailure::Stale)
        );
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
}

#[cfg(test)]
mod side_readout_tests {
    use super::*;
    use stratux_client::AppState;

    #[test]
    fn direction_is_captioned_by_what_it_actually_is() {
        let mut state = AppState::new();

        // Nothing available: dashes, not a north-pointing zero.
        assert!(matches!(heading_source(&state), HeadingSource::None));
        assert_eq!(heading_source(&state).text(), "---");

        // GPS track is the live case on this hardware, and must NOT be captioned HDG: track and
        // heading differ by the wind, which is exactly when someone would be reading it.
        state.ownship.track_deg = Some(57.0);
        assert!(matches!(heading_source(&state), HeadingSource::Track(_)));
        assert_eq!(heading_source(&state).caption(), "TRK gps");
        assert_eq!(heading_source(&state).text(), "057\u{00B0}");

        // A real heading outranks track when the sensor supplies one.
        state.ownship.ahrs.mag_heading_deg = Some(120.0);
        assert!(matches!(heading_source(&state), HeadingSource::Mag(_)));
        assert_eq!(heading_source(&state).caption(), "HDG mag");
    }

    #[test]
    fn headings_wrap_into_zero_to_three_sixty() {
        let mut state = AppState::new();
        state.ownship.ahrs.mag_heading_deg = Some(-10.0);
        assert_eq!(heading_source(&state).text(), "350\u{00B0}");
        state.ownship.ahrs.mag_heading_deg = Some(370.0);
        assert_eq!(heading_source(&state).text(), "010\u{00B0}");
    }

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
