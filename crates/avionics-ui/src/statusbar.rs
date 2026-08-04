//! The top status bar: is the system actually working?
//!
//! Everything here answers a question the pilot would otherwise have to guess at. In particular
//! the per-radio message rates matter more than they look: a 978 MHz receiver that has quietly
//! died shows up as plausible-but-thin traffic, not as an error, and the only visible symptom is
//! that its message count sits at zero.
//!
//! There is deliberately **no bus voltage** here, even though the original mockup showed one.
//! Stratux's status structure has no voltage field and this build has no other power sensor, so
//! displaying a number would mean inventing it. If voltage is wanted, it needs hardware and a
//! source first.

use std::time::Instant;

use avionics_gfx::femtovg::{Align, Baseline, Color, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::{AppState, Stream};

use crate::{FrameStats, Layout, Ui};

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    now: Instant,
    layout: &Layout,
    stats: &FrameStats,
) {
    let theme = &ui.theme;

    let mut background = Path::new();
    background.rect(0.0, 0.0, layout.width, layout.status_bar_height);
    canvas.fill_path(&background, &Paint::color(theme.bar_background));

    let mut separator = Path::new();
    separator.move_to(0.0, layout.status_bar_height);
    separator.line_to(layout.width, layout.status_bar_height);
    canvas.stroke_path(
        &separator,
        &Paint::color(theme.text_dim).with_line_width(1.0),
    );

    let baseline = layout.status_bar_height * 0.5;
    let mut cursor = layout.margin;

    // There is no PAGE field here any more. It named the page you were on, which the page strip
    // now does permanently and far more legibly — and removing it was not merely tidy, it is what
    // made the rest of this bar fit. The two strips cost 96 px of content width, the busiest
    // status line measured 603 px against the 600 now available, and the PAGE field with its
    // trailing gap was worth 63 px. The redundancy paid for the space its own replacement took.

    // --- GPS ---
    let gps_ok = state.ownship.fix.is_usable();
    cursor = field(
        ui,
        canvas,
        cursor,
        baseline,
        "GPS",
        &if gps_ok {
            format!(
                "{}/{}",
                state.ownship.fix.label(),
                state.ownship.satellites_locked
            )
        } else {
            state.ownship.fix.label().to_string()
        },
        if gps_ok { theme.good } else { theme.warning },
    );

    // --- Radios ---
    // Rates, not just a tick: "connected but hearing nothing" is the failure that matters, and it
    // looks identical to "connected and working" unless the count is on screen.
    let es_rate = state.status.es_messages_last_minute;
    let uat_rate = state.status.uat_messages_last_minute;
    let status_fresh = !state.is_stale(Stream::Status, now);

    cursor = field(
        ui,
        canvas,
        cursor,
        baseline,
        "1090",
        &rate_text(es_rate, status_fresh),
        radio_colour(ui, es_rate, status_fresh),
    );
    cursor = field(
        ui,
        canvas,
        cursor,
        baseline,
        "978",
        &rate_text(uat_rate, status_fresh),
        radio_colour(ui, uat_rate, status_fresh),
    );

    // --- Traffic counts ---
    let mut traffic = format!("{}", stats.targets_drawn);
    if stats.targets_outside_range > 0 {
        // Culled, not lost. Without this the pilot cannot tell a quiet sky from a small range ring.
        traffic.push_str(&format!(" +{} out", stats.targets_outside_range));
    }
    // Hidden by the vertical filter, and undone by a different key from `out`. Kept separate for
    // that reason: folding the two together would leave the pilot pressing ALT and RNG in turn to
    // find out which selection is holding traffic back.
    if stats.targets_outside_altitude > 0 {
        traffic.push_str(&format!(" +{} alt", stats.targets_outside_altitude));
    }
    if stats.targets_no_position > 0 {
        traffic.push_str(&format!(" +{} nopos", stats.targets_no_position));
    }
    // Held back for want of an own-ship position, not absent. `TFC 0` next to a working receiver
    // is the exact reading that sent a real outdoor test looking for an antenna fault when the
    // GPS was the thing that had failed.
    if stats.targets_unplotted > 0 {
        traffic.push_str(&format!(" +{} held", stats.targets_unplotted));
    }
    let traffic_colour = if stats.alerts > 0 {
        theme.warning
    } else if stats.advisories > 0 {
        theme.caution
    } else {
        theme.text_primary
    };
    cursor = field(ui, canvas, cursor, baseline, "TFC", &traffic, traffic_colour);

    // --- CPU temperature ---
    // The Pi 3 throttles at 80 C and two SDRs in an enclosure run hot; throttling shows up as a
    // dropped frame rate long before anything else complains.
    if status_fresh && state.status.cpu_temp_c > 0.0 {
        let temp = state.status.cpu_temp_c;
        let colour = if temp >= 78.0 {
            theme.warning
        } else if temp >= 70.0 {
            theme.caution
        } else {
            theme.text_secondary
        };
        cursor = field(
            ui,
            canvas,
            cursor,
            baseline,
            "CPU",
            &format!("{temp:.0}\u{00B0}C"),
            colour,
        );
    }

    // --- Weather age ---
    // FIS-B products cycle roughly every 5 minutes. An age climbing past that means reception has
    // stopped, and the precipitation on screen is history rather than weather.
    if let Some(age) = state.nexrad_age(now) {
        let minutes = age.as_secs() / 60;
        let colour = if minutes >= 10 {
            theme.warning
        } else if minutes >= 5 {
            theme.caution
        } else {
            theme.text_secondary
        };
        cursor = field(
            ui,
            canvas,
            cursor,
            baseline,
            "WX",
            &crate::weatherpage::format_age(age),
            colour,
        );
    }

    // --- Right-hand side: whatever is currently wrong ---
    // `cursor` is where the left-hand fields actually ended, which is the only honest limit for
    // the alarms to stop at. It moves with the font size, and the fields themselves come and go.
    draw_alarms(ui, canvas, state, now, layout, baseline, cursor);
}

/// Draw "LABEL value" and return the x cursor for the next field.
fn field(
    ui: &Ui,
    canvas: &mut Canvas,
    x: f32,
    baseline: f32,
    label: &str,
    value: &str,
    value_colour: Color,
) -> f32 {
    let theme = &ui.theme;

    let mut label_paint = Paint::color(theme.text_dim);
    label_paint.set_font(&[ui.font()]);
    label_paint.set_font_size(theme.font_size_small * 0.88);
    label_paint.set_text_align(Align::Left);
    label_paint.set_text_baseline(Baseline::Middle);

    let mut value_paint = Paint::color(value_colour);
    value_paint.set_font(&[ui.font()]);
    value_paint.set_font_size(theme.font_size_small);
    value_paint.set_text_align(Align::Left);
    value_paint.set_text_baseline(Baseline::Middle);

    let label_width = canvas
        .measure_text(0.0, 0.0, label, &label_paint)
        .map(|m| m.width())
        .unwrap_or(0.0);
    let _ = canvas.fill_text(x, baseline, label, &label_paint);

    let value_x = x + label_width + 4.0;
    let value_width = canvas
        .measure_text(0.0, 0.0, value, &value_paint)
        .map(|m| m.width())
        .unwrap_or(0.0);
    let _ = canvas.fill_text(value_x, baseline, value, &value_paint);

    value_x + value_width + 14.0
}

fn rate_text(rate: u32, fresh: bool) -> String {
    if !fresh {
        "?".into()
    } else {
        format!("{rate}/m")
    }
}

fn radio_colour(ui: &Ui, rate: u32, fresh: bool) -> Color {
    if !fresh {
        ui.theme.text_dim
    } else if rate == 0 {
        // Airborne with a working antenna this is essentially never zero, so treat it as a fault
        // rather than as quiet airspace.
        ui.theme.warning
    } else {
        ui.theme.good
    }
}

/// Clear space kept between the last left-hand field and the first alarm.
const ALARM_GAP: f32 = 14.0;

/// Right-aligned list of current problems, most severe first. Empty when all is well — a status
/// bar that always shows something to worry about trains the pilot to stop reading it.
fn draw_alarms(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    now: Instant,
    layout: &Layout,
    baseline: f32,
    left_end: f32,
) {
    let theme = &ui.theme;
    let mut messages: Vec<(String, Color)> = Vec::new();

    let disconnected: Vec<&'static str> = Stream::ALL
        .into_iter()
        .filter(|s| state.streams.get(s).map(|h| !h.connected).unwrap_or(true))
        .map(|s| s.name())
        .collect();
    if disconnected.len() == Stream::ALL.len() {
        messages.push(("STRATUX OFFLINE".into(), theme.warning));
    } else if !disconnected.is_empty() {
        messages.push((format!("NO {}", disconnected.join(",")), theme.caution));
    }

    // Own-ship at 10 Hz going quiet for seconds is a real fault; weather being quiet is not, which
    // is why staleness timeouts are per stream.
    if state.is_stale(Stream::Situation, now) && state.ever_had_position {
        messages.push(("POSITION STALE".into(), theme.warning));
    }

    if state.ownship.comparison_altitude_ft().is_none() {
        // Relative altitudes read "---" and threat tiers cannot escalate to alert; say so rather
        // than letting the blank tags look like a rendering glitch.
        messages.push(("NO ALT REF".into(), theme.caution));
    }

    if state.decode_errors > 0 {
        messages.push((format!("{} DECODE ERR", state.decode_errors), theme.caution));
    }

    if messages.is_empty() {
        return;
    }

    let mut paint = Paint::color(theme.warning);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(ui.theme.font_size_small);
    paint.set_text_align(Align::Right);
    paint.set_text_baseline(Baseline::Middle);

    // content_width, not width: past that lies the soft-key strip, and a warning drawn
    // under it is a warning the pilot never sees.
    let mut x = layout.width - layout.margin;
    for (text, colour) in messages {
        paint.set_color(colour);
        let width = canvas
            .measure_text(0.0, 0.0, &text, &paint)
            .map(|m| m.width())
            .unwrap_or(0.0);

        // Checked *before* drawing, and against where the left-hand fields actually ended.
        //
        // Both halves of that were wrong. The old guard tested `layout.width * 0.45` — a fixed
        // fraction unrelated to the fields, which the busiest line already passed at the old font
        // size and passes by 237 px at the new one. And it tested *after* `fill_text`, so the
        // message that broke the loop had already been painted over the fields it was avoiding.
        if x - width < left_end + ALARM_GAP {
            break;
        }
        let _ = canvas.fill_text(x, baseline, &text, &paint);
        x -= width + 12.0;
    }
}
