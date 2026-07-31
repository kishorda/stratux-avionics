//! Mapping gestures onto the view.
//!
//! This lives in the UI crate rather than in the binary because deciding what a tap means requires
//! knowing the layout — where the status bar ends, how many weather rows fit. Keeping it here also
//! makes the whole interaction model testable without a GPU or a touchscreen.
//!
//! The gesture vocabulary is deliberately tiny. In turbulence a hand steadies itself against the
//! panel, and every additional gesture is another way for the display to silently wander off the
//! range or heading reference the pilot selected. Two fingers and a tap is the whole language.

use crate::{softkeys, CageState, Layout, Page, Ui, ViewState};

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
    // The soft-key strip wins over everything: it overlaps the right edge of the status bar and
    // the body, and a key press must never also register as whatever is underneath it.
    if let Some(slot) = softkeys::hit(layout, x, y) {
        if let Some(key) = softkeys::key_at(view, slot) {
            apply_key(view, key, weather_rows, weather_total);
        }
        // A press on an inert slot is still a press on the strip. Swallow it rather than letting
        // it fall through to the page underneath.
        return;
    }

    match zone_for(layout, x, y) {
        // Kept as a second way to reach the next page: it is the largest target on screen and
        // costs nothing, since the status bar has no other meaning.
        TapZone::StatusBar => view.page = view.page.next(),
        TapZone::Footer => {}
        zone => match view.page {
            // Deliberately inert. Before the soft keys existed, a tap anywhere in the body cycled
            // the range — which meant a hand steadying itself against the panel in turbulence
            // silently changed the range scale. Now that there is a dedicated RNG key, the body
            // does nothing on the plan view.
            Page::PlanView => {}
            // Nothing on the attitude page is adjustable, and an instrument that reacts to being
            // brushed is worse than one that does not react at all.
            Page::Ahrs => {}
            Page::Weather => scroll_weather(view, zone, weather_rows, weather_total),
        },
    }
}

/// Apply a soft key. Separate from [`tap`] so the mapping is testable without hit-testing.
pub fn apply_key(
    view: &mut ViewState,
    key: softkeys::SoftKey,
    weather_rows: usize,
    weather_total: usize,
) {
    apply_key_at(view, key, weather_rows, weather_total, std::time::Instant::now())
}

/// As [`apply_key`], with an explicit clock so the cage state machine is testable.
pub fn apply_key_at(
    view: &mut ViewState,
    key: softkeys::SoftKey,
    weather_rows: usize,
    weather_total: usize,
    now: std::time::Instant,
) {
    use softkeys::SoftKey;
    match key {
        SoftKey::Page => view.page = view.page.next(),
        SoftKey::RangeUp => view.cycle_range(),
        SoftKey::RangeDown => view.cycle_range_down(),
        SoftKey::ToggleOrientation => view.toggle_orientation(),
        SoftKey::ToggleUnderlay => view.show_weather_underlay = !view.show_weather_underlay,
        SoftKey::ScrollUp => scroll_weather(view, TapZone::BodyUpper, weather_rows, weather_total),
        SoftKey::ScrollDown => scroll_weather(view, TapZone::BodyLower, weather_rows, weather_total),
        SoftKey::CageAhrs => match view.cage {
            // Arm, then confirm. A single press must never re-reference the sensor.
            CageState::Idle => view.set_cage(CageState::Armed, now),
            CageState::Armed => view.set_cage(CageState::Requested, now),
            // Already in flight, or showing its result: ignore rather than queueing a second
            // request behind the first.
            CageState::Requested | CageState::InFlight | CageState::Done { .. } => {}
        },
    }
}

/// Apply a two-finger tap.
pub fn two_finger_tap(view: &mut ViewState) {
    match view.page {
        Page::PlanView => view.toggle_orientation(),
        // Nothing sensible to toggle on these; do nothing rather than invent a behaviour.
        Page::Weather | Page::Ahrs => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::softkeys::{self, SoftKey};
    use crate::{Orientation, Theme};

    fn layout() -> Layout {
        Layout::for_size(800.0, 480.0, &Theme::dark())
    }

    /// Centre of a soft-key slot, in screen pixels.
    fn key_point(layout: &Layout, slot: usize) -> (f32, f32) {
        let (x, y, w, h) = softkeys::slot_rect(layout, slot);
        (x + w * 0.5, y + h * 0.5)
    }

    #[test]
    fn range_keys_step_both_ways() {
        let l = layout();
        let mut view = ViewState::default();
        assert_eq!(view.range_nm, 10.0);

        let (x, y) = key_point(&l, 1);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.range_nm, 20.0, "RNG+ should step up");

        let (x, y) = key_point(&l, 2);
        tap(&mut view, &l, x, y, 5, 0);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.range_nm, 5.0, "RNG- should step down twice");
    }

    #[test]
    fn the_plan_view_body_no_longer_changes_range() {
        // The whole reason the soft keys exist: a hand steadying itself against the panel used to
        // cycle the range scale silently.
        let l = layout();
        let mut view = ViewState::default();
        let before = view.range_nm;
        for y in [l.status_bar_height + 20.0, l.height * 0.5, l.height - l.footer_height - 20.0] {
            tap(&mut view, &l, l.content_width * 0.5, y, 5, 0);
        }
        assert_eq!(view.range_nm, before, "body taps must not change range");
        assert_eq!(view.page, Page::PlanView, "body taps must not change page");
    }

    #[test]
    fn page_key_cycles_every_page_from_one_fixed_spot() {
        let l = layout();
        let mut view = ViewState::default();
        let (x, y) = key_point(&l, softkeys::PAGE_SLOT);

        // The same coordinates must walk the whole cycle and return, or there is no reliable way
        // out of a page you did not mean to be on.
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.page, Page::Weather);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.page, Page::Ahrs);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.page, Page::PlanView, "the cycle must return to traffic");
    }

    #[test]
    fn caging_needs_two_presses_never_one() {
        // The whole point: one press must never re-reference the attitude sensor.
        let l = layout();
        let now = std::time::Instant::now();
        let mut view = ViewState {
            page: Page::Ahrs,
            ..Default::default()
        };
        let (x, y) = key_point(&l, 4);

        assert_eq!(view.cage, CageState::Idle);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.cage, CageState::Armed, "one press must only arm");
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.cage, CageState::Requested, "the second press confirms");

        // Further presses must not queue another request behind the first.
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.cage, CageState::Requested);

        let _ = now;
    }

    #[test]
    fn an_armed_cage_lapses_so_two_unrelated_presses_cannot_combine() {
        let now = std::time::Instant::now();
        let mut view = ViewState::default();
        view.set_cage(CageState::Armed, now);

        // Still armed a moment later.
        view.tick_cage(now + std::time::Duration::from_secs(1));
        assert_eq!(view.cage, CageState::Armed);

        // Lapsed after the timeout: a stray press now and a real one minutes later must not
        // add up to a cage nobody asked for.
        view.tick_cage(now + crate::CAGE_ARM_TIMEOUT + std::time::Duration::from_millis(1));
        assert_eq!(view.cage, CageState::Idle);
    }

    #[test]
    fn a_finished_cage_returns_to_idle_on_its_own() {
        let now = std::time::Instant::now();
        let mut view = ViewState::default();
        view.set_cage(CageState::Done { ok: true }, now);

        view.tick_cage(now + std::time::Duration::from_secs(1));
        assert_eq!(view.cage, CageState::Done { ok: true }, "result should dwell");

        view.tick_cage(now + crate::CAGE_RESULT_DWELL + std::time::Duration::from_millis(1));
        assert_eq!(view.cage, CageState::Idle);
    }

    #[test]
    fn the_level_key_is_not_reachable_from_the_other_pages() {
        // Slot 4 is the underlay toggle on the plan view and inert on weather. Pressing it there
        // must not touch the cage state.
        let l = layout();
        let (x, y) = key_point(&l, 4);
        for page in [Page::PlanView, Page::Weather] {
            let mut view = ViewState {
                page,
                ..Default::default()
            };
            tap(&mut view, &l, x, y, 5, 20);
            assert_eq!(view.cage, CageState::Idle, "slot 4 armed a cage on {page:?}");
        }
    }

    #[test]
    fn the_key_label_tracks_the_cage_state() {
        let mut view = ViewState {
            page: Page::Ahrs,
            ..Default::default()
        };
        let key = softkeys::SoftKey::CageAhrs;
        let now = std::time::Instant::now();

        assert_eq!(softkeys::label(key, &view), "LEVEL");
        view.set_cage(CageState::Armed, now);
        assert_eq!(softkeys::label(key, &view), "CONFIRM");
        view.set_cage(CageState::Done { ok: true }, now);
        assert_eq!(softkeys::label(key, &view), "CAGED");
        view.set_cage(CageState::Done { ok: false }, now);
        assert_eq!(softkeys::label(key, &view), "FAILED");
    }

    #[test]
    fn the_attitude_page_ignores_body_taps_entirely() {
        let l = layout();
        let mut view = ViewState {
            page: Page::Ahrs,
            ..Default::default()
        };
        let before = view.clone();
        for y in [l.height * 0.35, l.height * 0.5, l.height * 0.65] {
            tap(&mut view, &l, l.content_width * 0.5, y, 5, 0);
        }
        assert_eq!(view.page, before.page);
        assert_eq!(view.range_nm, before.range_nm);
        // Two fingers must not toggle orientation from here either.
        two_finger_tap(&mut view);
        assert_eq!(view.orientation, before.orientation);
    }

    #[test]
    fn inert_slots_swallow_the_press() {
        let l = layout();
        let mut view = ViewState {
            page: Page::Weather,
            ..Default::default()
        };
        let before = view.clone();
        // Slots 3 and 4 are unused on the weather page.
        for slot in [3, 4] {
            let (x, y) = key_point(&l, slot);
            tap(&mut view, &l, x, y, 5, 20);
        }
        assert_eq!(view.page, before.page, "an inert key must not fall through to the page");
        assert_eq!(view.weather_scroll, before.weather_scroll);
    }

    #[test]
    fn a_key_press_does_not_also_trigger_the_zone_beneath_it() {
        // Slot 0 starts at y=0, so its upper part overlaps the status bar band. Probe there
        // rather than at the slot centre, which sits below the bar. Without the early return in
        // `tap`, this one press would switch pages twice and appear to do nothing.
        let l = layout();
        let (x, _) = key_point(&l, softkeys::PAGE_SLOT);
        let y = l.status_bar_height * 0.5;

        let (_, slot_y, _, slot_h) = softkeys::slot_rect(&l, softkeys::PAGE_SLOT);
        assert!(
            y >= slot_y && y < slot_y + slot_h,
            "test premise: this point is inside slot 0"
        );
        assert_eq!(
            zone_for(&l, x, y),
            TapZone::StatusBar,
            "test premise: this point is also in the status bar zone"
        );

        let mut view = ViewState::default();
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.page, Page::Weather, "double-dispatch would have returned to PlanView");
    }

    #[test]
    fn underlay_key_toggles_and_is_reachable_only_on_the_plan_view() {
        let l = layout();
        let mut view = ViewState::default();
        assert!(view.show_weather_underlay);

        let (x, y) = key_point(&l, 4);
        tap(&mut view, &l, x, y, 5, 0);
        assert!(!view.show_weather_underlay);

        // Slot 4 is inert on the weather page, so it must not toggle anything there.
        view.page = Page::Weather;
        tap(&mut view, &l, x, y, 5, 0);
        assert!(!view.show_weather_underlay, "slot 4 must be inert on the weather page");
    }

    #[test]
    fn orientation_key_matches_the_two_finger_gesture() {
        let l = layout();
        let mut by_key = ViewState::default();
        let mut by_gesture = ViewState::default();

        let (x, y) = key_point(&l, 3);
        tap(&mut by_key, &l, x, y, 5, 0);
        two_finger_tap(&mut by_gesture);

        assert_eq!(by_key.orientation, by_gesture.orientation);
        assert_eq!(by_key.orientation, Orientation::TrackUp);
    }

    #[test]
    fn scroll_keys_move_the_weather_list() {
        let l = layout();
        let mut view = ViewState {
            page: Page::Weather,
            ..Default::default()
        };
        let (x, y) = key_point(&l, 2);
        tap(&mut view, &l, x, y, 5, 20);
        assert_eq!(view.weather_scroll, 5, "DOWN should advance one page of rows");

        let (x, y) = key_point(&l, 1);
        tap(&mut view, &l, x, y, 5, 20);
        assert_eq!(view.weather_scroll, 0, "UP should come back");
    }

    #[test]
    fn apply_key_and_tap_agree() {
        // The strip's dispatch and the direct mapping must not drift apart.
        let l = layout();
        for (slot, key) in softkeys::keys_for(Page::PlanView).iter().enumerate() {
            let Some(key) = key else { continue };
            let mut by_tap = ViewState::default();
            let mut direct = ViewState::default();
            let (x, y) = key_point(&l, slot);
            tap(&mut by_tap, &l, x, y, 5, 0);
            apply_key(&mut direct, *key, 5, 0);
            assert_eq!(by_tap.page, direct.page, "slot {slot}");
            assert_eq!(by_tap.range_nm, direct.range_nm, "slot {slot}");
            assert_eq!(by_tap.orientation, direct.orientation, "slot {slot}");
            assert_eq!(
                by_tap.show_weather_underlay, direct.show_weather_underlay,
                "slot {slot}"
            );
        }
        let _ = SoftKey::Page;
    }
}
