//! The FIS-B text products page.
//!
//! METARs, TAFs, PIREPs and the rest, as received. Deliberately shown as raw text rather than
//! decoded into prose: pilots read raw METARs, the encoding is compact enough to fit a small panel,
//! and any decoder would be one more thing that can be subtly wrong about weather.
//!
//! Each entry carries an age, because FIS-B delivery is opportunistic — a station's observation
//! can be twenty minutes stale while the one next to it is current, and nothing on the wire warns
//! you about that.

use std::time::Instant;

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::domain::{WeatherProduct, WeatherText};
use stratux_client::AppState;

use crate::{metar, Layout, Ui, ViewState};

/// Sort priority: the products a pilot reaches for first come first.
fn priority(product: &WeatherProduct) -> u8 {
    match product {
        WeatherProduct::Metar => 0,
        WeatherProduct::Taf => 1,
        WeatherProduct::Sigmet => 2,
        WeatherProduct::Airmet => 3,
        WeatherProduct::Pirep => 4,
        WeatherProduct::Winds => 5,
        WeatherProduct::Notam => 6,
        WeatherProduct::Other(_) => 7,
    }
}

/// Ordered view of everything held, newest-relevant first within each product.
pub fn ordered(state: &AppState) -> Vec<&WeatherText> {
    let mut items: Vec<&WeatherText> = state.weather.values().collect();
    items.sort_by(|a, b| {
        priority(&a.product)
            .cmp(&priority(&b.product))
            .then_with(|| a.location.cmp(&b.location))
            .then_with(|| a.body.cmp(&b.body))
    });
    items
}

/// How many entries fit on screen, given the layout. Used for scrolling.
pub fn rows_per_page(ui: &Ui, layout: &Layout) -> usize {
    let line = ui.theme.font_size_small * 1.35;
    let body = layout.height - layout.status_bar_height - layout.footer_height - line;
    ((body / (line * 2.0)).floor() as usize).max(1)
}

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    view: &ViewState,
    now: Instant,
    layout: &Layout,
) {
    let theme = &ui.theme;
    let items = ordered(state);
    let line = theme.font_size_small * 1.35;
    let per_page = rows_per_page(ui, layout);

    // Clamp rather than trust the scroll offset: pruning can shrink the list between frames.
    let max_offset = items.len().saturating_sub(per_page);
    let offset = view.weather_scroll.min(max_offset);

    let mut y = layout.status_bar_height + line * 0.6;

    // --- header ---
    let mut header = Paint::color(theme.text_secondary);
    header.set_font(&[ui.font()]);
    header.set_font_size(theme.font_size_small);
    header.set_text_baseline(Baseline::Middle);
    header.set_text_align(Align::Left);

    let nexrad_age = state.nexrad_age(now);
    let nexrad_text = match nexrad_age {
        Some(age) => format!("{} blocks, {}", state.nexrad.len(), format_age(age)),
        None => "none".into(),
    };
    let _ = canvas.fill_text(
        layout.margin,
        y,
        format!("FIS-B TEXT  {} products   |   NEXRAD {nexrad_text}", items.len()),
        &header,
    );

    if items.len() > per_page {
        let mut scroll = Paint::color(theme.text_dim);
        scroll.set_font(&[ui.font()]);
        scroll.set_font_size(theme.font_size_small);
        scroll.set_text_baseline(Baseline::Middle);
        scroll.set_text_align(Align::Right);
        let _ = canvas.fill_text(
            layout.content_width - layout.margin,
            y,
            format!(
                "{}-{} of {}",
                offset + 1,
                (offset + per_page).min(items.len()),
                items.len()
            ),
            &scroll,
        );
    }

    y += line * 1.2;
    let mut separator = Path::new();
    separator.move_to(layout.margin, y - line * 0.4);
    separator.line_to(layout.content_width - layout.margin, y - line * 0.4);
    canvas.stroke_path(
        &separator,
        &Paint::color(theme.text_dim).with_line_width(1.0),
    );

    if items.is_empty() {
        draw_empty_notice(ui, canvas, state, layout);
        return;
    }

    // --- entries ---
    for item in items.iter().skip(offset).take(per_page) {
        let age = now.saturating_duration_since(item.received);

        let mut label = Paint::color(theme.text_primary);
        label.set_font(&[ui.font()]);
        label.set_font_size(theme.font_size_small);
        label.set_text_baseline(Baseline::Middle);
        label.set_text_align(Align::Left);

        let heading = if item.location.is_empty() {
            item.product.label().to_string()
        } else {
            format!("{} {}", item.product.label(), item.location)
        };
        let _ = canvas.fill_text(layout.margin, y, &heading, &label);

        // Flight category badge — METARs and SPECIs only.
        //
        // Derived from two fields with published thresholds, and shown BESIDE the raw text rather
        // than instead of it: the badge answers "can I go?" at a glance, the report remains the
        // authority. Absent rather than guessed when neither field could be read.
        //
        // Deliberately NOT applied to TAFs. A TAF covers many hours in FM/TEMPO/BECMG periods and
        // `summarise` has no notion of periods, so it mixes them: the lowest ceiling anywhere in
        // the forecast against the first visibility. A real one measured in testing badged LIFR
        // off a period eight hours out while its current period was VFR. See the test
        // `a_taf_summarises_to_something_that_describes_no_single_moment`.
        let category = matches!(item.product, WeatherProduct::Metar)
            .then(|| metar::summarise(&item.body).category)
            .flatten();
        if let Some(category) = category {
            let heading_w = canvas
                .measure_text(0.0, 0.0, &heading, &label)
                .map(|m| m.width())
                .unwrap_or(0.0);
            let mut badge = Paint::color(category_colour(ui, category));
            badge.set_font(&[ui.font()]);
            badge.set_font_size(theme.font_size_small);
            badge.set_text_baseline(Baseline::Middle);
            badge.set_text_align(Align::Left);
            let _ = canvas.fill_text(
                layout.margin + heading_w + 10.0,
                y,
                category.label(),
                &badge,
            );
        }

        // Age on the right, coloured once it is old enough to matter.
        let mut age_paint = Paint::color(age_colour(ui, age));
        age_paint.set_font(&[ui.font()]);
        age_paint.set_font_size(theme.font_size_small);
        age_paint.set_text_baseline(Baseline::Middle);
        age_paint.set_text_align(Align::Right);
        let _ = canvas.fill_text(
            layout.content_width - layout.margin,
            y,
            format_age(age),
            &age_paint,
        );

        y += line;

        // Body, indented and truncated to the panel width. Truncated rather than wrapped: a
        // wrapped TAF can run four lines and push everything else off a 480 px panel, and the
        // leading groups are the ones that matter at a glance.
        let mut body = Paint::color(theme.text_secondary);
        body.set_font(&[ui.font()]);
        body.set_font_size(theme.font_size_small);
        body.set_text_baseline(Baseline::Middle);
        body.set_text_align(Align::Left);

        let indent = layout.margin + theme.font_size_small * 1.2;
        let available = layout.content_width - indent - layout.margin;
        draw_body_tokens(ui, canvas, &item.body, &mut body, indent, y, available);

        y += line;
        if y > layout.height - layout.footer_height {
            break;
        }
    }

    draw_footer(ui, canvas, layout, view, items.len(), per_page, offset);
}

fn draw_empty_notice(ui: &Ui, canvas: &mut Canvas, state: &AppState, layout: &Layout) {
    let theme = &ui.theme;
    let (cx, cy) = (layout.content_width * 0.5, layout.height * 0.5);

    // "Nothing yet" is the normal state for minutes after a cold start, because Stratux's
    // /weather socket does not replay its buffer on connect. Saying so avoids it reading as a
    // fault.
    let connected = state
        .streams
        .get(&stratux_client::Stream::Weather)
        .map(|h| h.connected)
        .unwrap_or(false);

    let (headline, detail) = if connected {
        (
            "NO WEATHER RECEIVED YET",
            "FIS-B products arrive over several minutes",
        )
    } else {
        ("WEATHER STREAM OFFLINE", "retrying")
    };

    let mut title = Paint::color(theme.text_secondary);
    title.set_font(&[ui.font()]);
    title.set_font_size(theme.font_size_large);
    title.set_text_align(Align::Center);
    title.set_text_baseline(Baseline::Bottom);
    let _ = canvas.fill_text(cx, cy, headline, &title);

    let mut sub = Paint::color(theme.text_dim);
    sub.set_font(&[ui.font()]);
    sub.set_font_size(theme.font_size_small);
    sub.set_text_align(Align::Center);
    sub.set_text_baseline(Baseline::Top);
    let _ = canvas.fill_text(cx, cy + 4.0, detail, &sub);
}

fn draw_footer(
    ui: &Ui,
    canvas: &mut Canvas,
    layout: &Layout,
    _view: &ViewState,
    total: usize,
    per_page: usize,
    offset: usize,
) {
    let theme = &ui.theme;
    let baseline = layout.height - layout.footer_height * 0.35;

    let mut hint = Paint::color(theme.text_dim);
    hint.set_font(&[ui.font()]);
    hint.set_font_size(theme.font_size_small);
    hint.set_text_baseline(Baseline::Alphabetic);
    hint.set_text_align(Align::Left);

    // Name the soft keys, not the touch zones. The keys are the primary controls now, and a hint
    // that teaches tapping the body would teach the habit the strip exists to remove.
    let text = if total > per_page {
        if offset + per_page >= total {
            "DOWN wraps to the top"
        } else {
            "UP / DOWN to scroll"
        }
    } else {
        "PAGE for the traffic view"
    };
    let _ = canvas.fill_text(layout.margin, baseline, text, &hint);
}

fn age_colour(ui: &Ui, age: std::time::Duration) -> avionics_gfx::femtovg::Color {
    let minutes = age.as_secs() / 60;
    if minutes >= 60 {
        ui.theme.warning
    } else if minutes >= 30 {
        ui.theme.caution
    } else {
        ui.theme.text_dim
    }
}

/// Compact age: seconds under a minute, then minutes, then hours.
pub fn format_age(age: std::time::Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}


/// Colour for a flight category. Green is deliberate for VFR: it is the one state that needs no
/// action, and colouring it like everything else would waste the strongest signal on screen.
fn category_colour(ui: &Ui, category: metar::FlightCategory) -> avionics_gfx::femtovg::Color {
    match category {
        metar::FlightCategory::Vfr => ui.theme.good,
        metar::FlightCategory::Mvfr => ui.theme.caution,
        metar::FlightCategory::Ifr | metar::FlightCategory::Lifr => ui.theme.warning,
    }
}

/// Draw the report body token by token, colouring the ones that carry a hazard.
///
/// The text is otherwise unchanged — this highlights, it does not translate. Tokens are measured
/// and placed individually so the line still truncates at the panel edge exactly as before.
fn draw_body_tokens(
    ui: &Ui,
    canvas: &mut Canvas,
    body: &str,
    paint: &mut Paint,
    x0: f32,
    y: f32,
    available: f32,
) {
    let theme = &ui.theme;
    let space = canvas
        .measure_text(0.0, 0.0, " ", paint)
        .map(|m| m.width())
        .unwrap_or(4.0);

    let mut x = x0;
    let mut in_remarks = false;
    for token in body.split_whitespace() {
        // Everything past RMK is free-form and full of things that merely look like fields, so it
        // is dimmed rather than scanned. See `metar::summarise`.
        if token == "RMK" {
            in_remarks = true;
        }

        let width = canvas
            .measure_text(0.0, 0.0, token, paint)
            .map(|m| m.width())
            .unwrap_or(0.0);
        if x - x0 + width > available {
            // Out of room: mark the truncation rather than stopping silently, so a clipped report
            // cannot be mistaken for a short one.
            paint.set_color(theme.text_dim);
            let _ = canvas.fill_text(x, y, "\u{2026}", paint);
            return;
        }

        let colour = if in_remarks {
            theme.text_dim
        } else {
            match metar::token_hazard(token) {
                metar::Hazard::Warning => theme.warning,
                metar::Hazard::Caution => theme.caution,
                metar::Hazard::None => theme.text_secondary,
            }
        };
        paint.set_color(colour);
        let _ = canvas.fill_text(x, y, token, paint);
        x += width + space;
    }
}
