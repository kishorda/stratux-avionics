//! Mapping gestures onto the view.
//!
//! This lives in the UI crate rather than in the binary because deciding what a tap means requires
//! knowing the layout — where the status bar ends, how many weather rows fit. Keeping it here also
//! makes the whole interaction model testable without a GPU or a touchscreen.
//!
//! The gesture vocabulary is deliberately tiny. In turbulence a hand steadies itself against the
//! panel, and every additional gesture is another way for the display to silently wander off the
//! range or heading reference the pilot selected. Two fingers and a tap is the whole language.

use crate::{Layout, Page, Ui, ViewState};

/// Where a tap landed. Exposed so tests can assert on zones without a canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapZone {
    /// The status bar strip along the top: switches page.
    StatusBar,
    /// The upper part of the body.
    BodyUpper,
    /// The lower part of the body.
    BodyLower,
    /// The footer strip along the bottom.
    Footer,
}

pub fn zone_for(layout: &Layout, _x: f32, y: f32) -> TapZone {
    if y <= layout.status_bar_height {
        TapZone::StatusBar
    } else if y >= layout.height - layout.footer_height {
        TapZone::Footer
    } else {
        let body_middle = layout.status_bar_height
            + (layout.height - layout.status_bar_height - layout.footer_height) * 0.5;
        if y < body_middle {
            TapZone::BodyUpper
        } else {
            TapZone::BodyLower
        }
    }
}

/// Apply a single-finger tap.
///
/// `weather_rows` is how many text entries currently fit, needed to page the weather list; pass
/// [`crate::weatherpage::rows_per_page`].
pub fn tap(view: &mut ViewState, layout: &Layout, x: f32, y: f32, weather_rows: usize, weather_total: usize) {
    match zone_for(layout, x, y) {
        // The status bar is present and identical on every page, which makes it the one reliable
        // place to put navigation.
        TapZone::StatusBar => view.page = view.page.next(),
        TapZone::Footer => {}
        zone => match view.page {
            Page::PlanView => view.cycle_range(),
            Page::Weather => scroll_weather(view, zone, weather_rows, weather_total),
        },
    }
}

/// Apply a two-finger tap.
pub fn two_finger_tap(view: &mut ViewState) {
    match view.page {
        Page::PlanView => view.toggle_orientation(),
        // Nothing sensible to toggle on the text page; do nothing rather than invent a behaviour.
        Page::Weather => {}
    }
}

fn scroll_weather(view: &mut ViewState, zone: TapZone, rows: usize, total: usize) {
    let max_offset = total.saturating_sub(rows);
    match zone {
        TapZone::BodyLower => {
            // Wrap at the end rather than sticking: with no scrollbar to drag, a dead tap looks
            // like the display has frozen.
            view.weather_scroll = if view.weather_scroll >= max_offset {
                0
            } else {
                (view.weather_scroll + rows).min(max_offset)
            };
        }
        TapZone::BodyUpper => {
            view.weather_scroll = view.weather_scroll.saturating_sub(rows);
        }
        _ => {}
    }
}

/// Convenience for the binary: resolve the row count and dispatch.
pub fn handle_tap(
    ui: &Ui,
    layout: &Layout,
    view: &mut ViewState,
    state: &stratux_client::AppState,
    x: f32,
    y: f32,
) {
    let rows = crate::weatherpage::rows_per_page(ui, layout);
    let total = state.weather.len();
    tap(view, layout, x, y, rows, total);
}
