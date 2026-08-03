//! The bottom bar: what the pilot has selected, and what own-ship is doing.
//!
//! Mirrors [`crate::statusbar`] at the other end of the panel. Both run edge to edge, and the two
//! key strips sit between them — so the frame reads as two bars with a working area between,
//! rather than as three columns with text loose at the bottom.
//!
//! # Why the pages do not draw their own footers any more
//!
//! They did, and it could not survive the bar gaining a background. The chrome has to be drawn
//! *after* the page — the NEXRAD underlay is a single quad that reaches the bottom edge of the
//! panel, so anything drawn before it is painted over — but the page's own footer text has to be
//! drawn *after* the chrome, or the chrome covers it. Those two orderings cannot both hold while
//! the text lives inside the page's draw call, so the text moved here instead.
//!
//! The result is one place that knows what the bottom of the screen says, which is also where the
//! status bar's fields live. The pages are left drawing only their own content area.

use avionics_gfx::femtovg::{Align, Baseline, Color, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::AppState;

use crate::{FrameStats, Layout, Page, Ui, ViewState};

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    view: &ViewState,
    layout: &Layout,
    stats: &FrameStats,
) {
    let theme = &ui.theme;

    let mut background = Path::new();
    background.rect(0.0, layout.footer_y0(), layout.width, layout.footer_height);
    canvas.fill_path(&background, &Paint::color(theme.bar_background));

    let mut separator = Path::new();
    separator.move_to(0.0, layout.footer_y0());
    separator.line_to(layout.width, layout.footer_y0());
    canvas.stroke_path(
        &separator,
        &Paint::color(theme.text_dim).with_line_width(1.0),
    );

    match view.page {
        Page::PlanView => plan_view(ui, canvas, state, view, layout, stats),
        Page::Weather => weather(ui, canvas, view, layout, state.weather.len()),
        Page::Ahrs => ahrs(ui, canvas, state, layout),
    }

    if show_navigation_banner(ui, view) {
        navigation_banner(ui, canvas, layout);
    }
}

/// Whether the `NOT FOR NAVIGATION` banner belongs on screen.
///
/// Only the plan view, only with airspace drawn, and only when there is a chart to draw it from.
///
/// # Why airspace and not airports
///
/// An airport symbol slightly out of place costs nothing; the worst it can do is clutter. An
/// airspace boundary is something a pilot may fly *relative to*, and traffic is cross-checked out
/// of the window in a way a Class B shelf is not. The banner marks the moment the display starts
/// making a claim of that kind, so it follows the airspace layer and nothing else.
///
/// # Why not "only when a boundary is actually visible"
///
/// Because it would blink on and off as the aircraft crossed in and out of coverage, and a caveat
/// that comes and going teaches the eye to stop reading it. The selection is what raises it.
pub fn show_navigation_banner(ui: &Ui, view: &ViewState) -> bool {
    view.page == Page::PlanView && view.map_layers.shows_airspace() && ui.chart().is_some()
}

/// Centred in the bar, between the selection readout on the left and own-ship's track and speed on
/// the right — the one part of this bar that is otherwise empty.
///
/// Amber, like the attitude page's standing banner, because it is the same kind of statement: not
/// an alarm, and not something that should read as ordinary chrome either.
fn navigation_banner(ui: &Ui, canvas: &mut Canvas, layout: &Layout) {
    let theme = &ui.theme;
    let mut paint = Paint::color(theme.caution);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Middle);
    let _ = canvas.fill_text(
        layout.width * 0.5,
        baseline(layout),
        "AIRSPACE \u{2014} NOT FOR NAVIGATION",
        &paint,
    );
}

/// Baseline for text in the bar, matching the status bar's vertical centring.
fn baseline(layout: &Layout) -> f32 {
    layout.footer_y0() + layout.footer_height * 0.5
}

fn left_paint(ui: &Ui, colour: Color, size: f32) -> Paint {
    let mut paint = Paint::color(colour);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(size);
    paint.set_text_baseline(Baseline::Middle);
    paint.set_text_align(Align::Left);
    paint
}

fn right_paint(ui: &Ui, colour: Color, size: f32) -> Paint {
    let mut paint = left_paint(ui, colour, size);
    paint.set_text_align(Align::Right);
    paint
}

/// Selected range, orientation and altitude band on the left; own-ship track and speed right.
fn plan_view(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    view: &ViewState,
    layout: &Layout,
    stats: &FrameStats,
) {
    let theme = &ui.theme;
    let y = baseline(layout);

    let track = state
        .ownship
        .track_deg
        .map(|t| format!("{:03.0}\u{00B0}", t.rem_euclid(360.0)))
        .unwrap_or_else(|| "---\u{00B0}".into());
    let speed = state
        .ownship
        .ground_speed_kt
        .map(|s| format!("{s:.0}"))
        .unwrap_or_else(|| "--".into());
    let _ = canvas.fill_text(
        layout.width - layout.margin,
        y,
        format!("TRK {track}   GS {speed} kt"),
        &right_paint(ui, theme.text_primary, theme.font_size_normal),
    );

    let mut selection = left_paint(ui, theme.text_secondary, theme.font_size_small);
    let selections = format!(
        "{} nm   {}   ",
        crate::planview::format_range(view.range_nm),
        view.orientation.label()
    );
    let _ = canvas.fill_text(layout.margin, y, &selections, &selection);

    // The altitude band, always named — including when it is not filtering anything. The two
    // selections that decide what reaches the screen belong side by side, and a pilot who has
    // deliberately opened the filter up wants to see that they did, not infer it from an absence.
    //
    // A separate `fill_text` only so it can carry its own colour: amber while it is actually
    // withholding traffic, ordinary otherwise. The distinction matters because the default band is
    // a narrowing one, so a colour keyed on "a filter is selected" would be amber from power-on.
    let width = canvas
        .measure_text(0.0, 0.0, &selections, &selection)
        .map(|m| m.width())
        .unwrap_or(0.0);
    if stats.targets_outside_altitude > 0 {
        selection.set_color(theme.caution);
    }
    let _ = canvas.fill_text(
        layout.margin + width,
        y,
        view.altitude_filter.label(),
        &selection,
    );
}

/// How to move through the list, and where in it you are.
fn weather(ui: &Ui, canvas: &mut Canvas, view: &ViewState, layout: &Layout, total: usize) {
    let theme = &ui.theme;
    let y = baseline(layout);
    let per_page = crate::weatherpage::rows_per_page(ui, layout);

    // Name the soft keys, not the touch zones. The keys are the primary controls, and a hint that
    // taught tapping the body would teach the habit the strips exist to remove.
    let hint = if view.weather_decode {
        "UP / DOWN for the next report"
    } else if total > per_page {
        if view.weather_scroll + per_page >= total {
            "DOWN wraps to the top"
        } else {
            "UP / DOWN to scroll"
        }
    } else {
        // Nothing to scroll, and naming TFC would be teaching a hint for a key already visible and
        // filled on the opposite edge.
        ""
    };
    let _ = canvas.fill_text(
        layout.margin,
        y,
        hint,
        &left_paint(ui, theme.text_dim, theme.font_size_small),
    );
}

/// Heading with its source on the left, G-load on the right.
///
/// Both moved here from the attitude area. The heading caption had been drawn just below the
/// heading box, which put it inside the footer band and underneath this bar; giving it a field of
/// its own fixes that and stops the attitude page being the one page with an empty footer.
fn ahrs(ui: &Ui, canvas: &mut Canvas, state: &AppState, layout: &Layout) {
    let theme = &ui.theme;
    let y = baseline(layout);
    let source = crate::ahrspage::heading_source(state);

    // The caption is what stops GPS track being read as a magnetic heading, so it stays attached
    // to the number rather than becoming a separate field that could be read on its own.
    let _ = canvas.fill_text(
        layout.margin,
        y,
        format!("{}  {}", source.caption(), source.text()),
        &left_paint(ui, theme.text_secondary, theme.font_size_small),
    );

    let g = state
        .ownship
        .ahrs
        .g_load
        .map(|g| format!("{g:.2}"))
        .unwrap_or_else(|| "---".into());
    let _ = canvas.fill_text(
        layout.width - layout.margin,
        y,
        format!("G {g}"),
        &right_paint(ui, theme.text_secondary, theme.font_size_small),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Theme;

    fn layout() -> Layout {
        Layout::for_size(800.0, 480.0, &Theme::dark())
    }

    #[test]
    fn the_bar_spans_the_whole_panel_and_clears_both_strips() {
        // The two problems this bar exists to fix: the footer text used to run the full width with
        // no bar under it, so it sat directly beneath the strips and read as part of them.
        let l = layout();
        assert_eq!(l.footer_y0(), l.strip_y1(), "the strips must stop at the bar");
        assert!(l.footer_y0() > l.strip_y0());
        assert!((l.footer_y0() + l.footer_height - l.height).abs() < 0.001);
    }

    #[test]
    fn text_sits_inside_the_bar() {
        let l = layout();
        let y = baseline(&l);
        assert!(y > l.footer_y0() && y < l.height, "baseline {y} outside the bar");
    }
}
