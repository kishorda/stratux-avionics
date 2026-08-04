//! Mapping gestures onto the view.
//!
//! This lives in the UI crate rather than in the binary because deciding what a tap means requires
//! knowing the layout — where the status bar ends, how many weather rows fit. Keeping it here also
//! makes the whole interaction model testable without a GPU or a touchscreen.
//!
//! The gesture vocabulary is deliberately tiny. In turbulence a hand steadies itself against the
//! panel, and every additional gesture is another way for the display to silently wander off the
//! range or heading reference the pilot selected. Two fingers and a tap is the whole language.

use crate::{pagestrip, softkeys, CageState, Layout, Page, Ui, ViewState};

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
    // Both strips win over everything: they overlap the ends of the status bar and the body, and
    // a key press must never also register as whatever is underneath it.
    if let Some(page) = pagestrip::hit(layout, x, y) {
        view.page = page;
        return;
    }
    if let Some(slot) = softkeys::hit(layout, x, y) {
        if let Some(key) = softkeys::key_at(view, slot) {
            apply_key(view, key, weather_rows, weather_total);
        }
        // A press on an inert slot is still a press on the strip. Swallow it rather than letting
        // it fall through to the page underneath.
        return;
    }

    match zone_for(layout, x, y) {
        // Inert, as of the two-strip layout. It used to cycle pages, on the grounds that it was
        // the largest target on screen and the status bar had no other meaning. The page strip is
        // now three keys of 151 px each, so there is nothing left for it to be a fallback for —
        // and this is the same reasoning that made the plan-view body inert. A hand steadying
        // itself against the panel should not change what is on it.
        TapZone::StatusBar => {}
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
        SoftKey::RangeUp => view.cycle_range(),
        SoftKey::RangeDown => view.cycle_range_down(),
        SoftKey::ToggleOrientation => view.toggle_orientation(),
        SoftKey::CycleAltitudeFilter => view.cycle_altitude_filter(),
        SoftKey::ToggleUnderlay => view.show_weather_underlay = !view.show_weather_underlay,
        SoftKey::CycleMapLayers => view.cycle_map_layers(),
        SoftKey::ScrollUp => scroll_weather(view, TapZone::BodyUpper, weather_rows, weather_total),
        SoftKey::ScrollDown => scroll_weather(view, TapZone::BodyLower, weather_rows, weather_total),
        SoftKey::ToggleDecode => {
            view.weather_decode = !view.weather_decode;
            // Entering decode selects the first entry that was on screen, so the report being
            // expanded is one the reader was already looking at rather than a jump to the top.
            view.weather_scroll = view.weather_scroll.min(weather_total.saturating_sub(1));
        }
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
    // Decoding shows one report at a time, so UP/DOWN step by one entry rather than by a page.
    let (step, max_offset) = if view.weather_decode {
        (1, total.saturating_sub(1))
    } else {
        (rows, total.saturating_sub(rows))
    };
    let rows = step;
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
///
/// Also the one place that knows about the chart, because inspecting needs a projection and the
/// projection needs own-ship. The order below is the whole rule:
///
/// 1. Either strip wins, always. A key press must never also be something else.
/// 2. On the plan view with the map on, a tap **on an airport symbol** opens its card.
/// 3. A tap anywhere else on the body dismisses an open card.
/// 4. Otherwise, the ordinary zone behaviour — which on the plan view is still nothing.
///
/// Step 2 is the only thing the plan-view body responds to, and it changes no selection. See
/// [`crate::Inspect`] for why that distinction is the one that matters.
pub fn handle_tap(
    ui: &Ui,
    layout: &Layout,
    view: &mut ViewState,
    state: &stratux_client::AppState,
    now: std::time::Instant,
    x: f32,
    y: f32,
) {
    let rows = crate::weatherpage::rows_per_page(ui, layout);
    let total = state.weather.len();

    let on_strip = pagestrip::hit(layout, x, y).is_some() || softkeys::hit(layout, x, y).is_some();
    if !on_strip && view.page == Page::PlanView {
        if let (Some(chart), Some(projection)) = (
            ui.chart(),
            crate::planview::make_projection(ui, state, view, now, layout),
        ) {
            // Airport first: it is the smaller and more specific target, and a symbol sitting
            // inside a Class D would otherwise be unreachable.
            if let Some(airport) =
                crate::maplayer::hit_airport(chart, view, layout, &projection, x, y)
            {
                view.inspect = Some(crate::Inspect {
                    subject: crate::Inspected::Airport(airport.index),
                    opened: now,
                });
                return;
            }
            // Then the airspace under the tap, but only while it is being drawn — you cannot
            // inspect what is not on screen.
            if view.map_layers.shows_airspace() {
                let at = projection.unproject(x, y);
                if !chart.airspace_at(at).is_empty() {
                    view.inspect = Some(crate::Inspect {
                        subject: crate::Inspected::Airspace(at),
                        opened: now,
                    });
                    return;
                }
            }
        }
        // Missed everything, so a card that is up goes away. This is the dismiss gesture, and it
        // is the same one that would otherwise have done nothing at all.
        if view.inspect.is_some() {
            view.inspect = None;
            return;
        }
    }

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

    /// Centre of a page's key on the right-hand strip.
    fn page_point(layout: &Layout, page: Page) -> (f32, f32) {
        let slot = Page::ALL.iter().position(|p| *p == page).expect("page");
        let (x, y, w, h) = crate::pagestrip::slot_rect(layout, slot);
        (x + w * 0.5, y + h * 0.5)
    }

    /// Centre of whichever slot carries `key` on `page`.
    ///
    /// Resolved rather than hardcoded so that rearranging the strip cannot silently turn a test of
    /// what a key *does* into a test of where it *is*. Adding the sixth slot moved LEVEL, and two
    /// tests that had nothing to do with layout failed for it.
    fn point_of(layout: &Layout, page: Page, key: SoftKey) -> (f32, f32) {
        let slot = softkeys::keys_for(page)
            .iter()
            .position(|k| *k == Some(key))
            .unwrap_or_else(|| panic!("{key:?} is not on {page:?}"));
        key_point(layout, slot)
    }

    #[test]
    fn range_keys_step_both_ways() {
        let l = layout();
        let mut view = ViewState::default();
        assert_eq!(view.range_nm, 10.0);

        let (x, y) = point_of(&l, Page::PlanView, SoftKey::RangeUp);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.range_nm, 20.0, "RNG+ should step up");

        let (x, y) = point_of(&l, Page::PlanView, SoftKey::RangeDown);
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
            tap(&mut view, &l, l.content_x0 + l.content_width() * 0.5, y, 5, 0);
        }
        assert_eq!(view.range_nm, before, "body taps must not change range");
        assert_eq!(view.page, Page::PlanView, "body taps must not change page");
    }

    #[test]
    fn every_page_is_one_press_away_from_every_other() {
        // What direct selection buys over the cycle it replaced: no page is ever two presses
        // away, and pressing the key for the page you are already on is a no-op rather than a
        // move. With a cycle, a press when you were unsure which page you were on was a guess.
        let l = layout();
        for from in Page::ALL {
            for to in Page::ALL {
                let mut view = ViewState {
                    page: from,
                    ..Default::default()
                };
                let (x, y) = page_point(&l, to);
                tap(&mut view, &l, x, y, 5, 0);
                assert_eq!(view.page, to, "{from:?} -> {to:?} took more than one press");
            }
        }
    }

    #[test]
    fn the_status_bar_no_longer_changes_page() {
        // It used to cycle. The page strip is now three keys of 151 px, so the fallback bought
        // nothing and cost an accidental-press path along the whole top of the panel — the same
        // trade that made the plan-view body inert.
        let l = layout();
        let mut view = ViewState::default();
        let x = l.content_x0 + l.content_width() * 0.5;
        tap(&mut view, &l, x, l.status_bar_height * 0.5, 5, 0);
        assert_eq!(view.page, Page::PlanView, "the status bar must be inert");
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
        let (x, y) = point_of(&l, Page::Ahrs, SoftKey::CageAhrs);

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
    fn decoding_toggles_and_switches_up_down_to_single_steps() {
        let l = layout();
        let mut view = ViewState {
            page: Page::Weather,
            ..Default::default()
        };
        let (x, y) = point_of(&l, Page::Weather, SoftKey::ToggleDecode);

        // Browsing: UP/DOWN move a page at a time.
        let down = point_of(&l, Page::Weather, SoftKey::ScrollDown);
        tap(&mut view, &l, down.0, down.1, 5, 20);
        assert_eq!(view.weather_scroll, 5, "a page is five rows here");

        // Decoding shows one report, so the same keys step one entry.
        tap(&mut view, &l, x, y, 5, 20);
        assert!(view.weather_decode);
        let down = point_of(&l, Page::Weather, SoftKey::ScrollDown);
        tap(&mut view, &l, down.0, down.1, 5, 20);
        assert_eq!(view.weather_scroll, 6, "decode mode steps one entry");

        // And back.
        tap(&mut view, &l, x, y, 5, 20);
        assert!(!view.weather_decode);
    }

    #[test]
    fn entering_decode_keeps_the_selection_inside_the_list() {
        let l = layout();
        let mut view = ViewState {
            page: Page::Weather,
            weather_scroll: 99,
            ..Default::default()
        };
        let (x, y) = point_of(&l, Page::Weather, SoftKey::ToggleDecode);
        tap(&mut view, &l, x, y, 5, 3);
        assert_eq!(view.weather_scroll, 2, "clamped to the last entry, not left out of range");
    }

    #[test]
    fn the_decode_key_is_not_reachable_from_the_other_pages() {
        let l = layout();
        let (x, y) = point_of(&l, Page::Weather, SoftKey::ToggleDecode);
        for page in [Page::PlanView, Page::Ahrs] {
            let mut view = ViewState {
                page,
                ..Default::default()
            };
            tap(&mut view, &l, x, y, 5, 20);
            assert!(
                !view.weather_decode,
                "the slot DECODE occupies on the weather page toggled it on {page:?}"
            );
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
            tap(&mut view, &l, l.content_x0 + l.content_width() * 0.5, y, 5, 0);
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
        // This used to probe PAGE against the status bar, on the reasoning that both cycled and a
        // double dispatch would advance twice and land back where it started. Neither is true
        // now — PAGE is gone and the status bar is inert — so that probe could no longer detect
        // anything and has been replaced rather than merely relocated.
        //
        // The weather page still has a zone that does something: a body tap scrolls. DOWN sits in
        // the upper half of the body, whose zone scrolls the *other* way, so a press that
        // dispatched twice would add a page and take one straight back off.
        let l = layout();
        let (x, y) = point_of(&l, Page::Weather, SoftKey::ScrollDown);
        assert_eq!(
            zone_for(&l, x, y),
            TapZone::BodyUpper,
            "test premise: this key overlaps a zone that scrolls the opposite way"
        );

        let mut view = ViewState {
            page: Page::Weather,
            ..Default::default()
        };
        tap(&mut view, &l, x, y, 5, 20);
        assert_eq!(
            view.weather_scroll, 5,
            "double dispatch would have scrolled down then back up to 0"
        );
    }

    #[test]
    fn the_bars_run_edge_to_edge_and_the_strips_sit_between_them() {
        // This replaces a test that pinned the page strip winning a dispatch against the status
        // bar beneath it. That overlap no longer exists — the strips now start below the bar — so
        // the old test could never have failed again, whatever the dispatch did.
        //
        // The property worth holding is the one that removed the overlap: both bars own their full
        // width, and neither strip reaches into either.
        let l = layout();
        for x in [
            l.strip_width * 0.5,
            l.content_x0 + l.content_width() * 0.5,
            l.content_x1 + l.strip_width * 0.5,
        ] {
            for y in [0.0, l.status_bar_height * 0.5, l.footer_y0() + 1.0, l.height - 1.0] {
                assert_eq!(softkeys::hit(&l, x, y), None, "function strip at ({x}, {y})");
                assert_eq!(crate::pagestrip::hit(&l, x, y), None, "page strip at ({x}, {y})");
            }
        }
        // And the strips do cover everything between the bars.
        let mid = l.strip_y0() + l.strip_height() * 0.5;
        assert!(softkeys::hit(&l, l.strip_width * 0.5, mid).is_some());
        assert!(crate::pagestrip::hit(&l, l.content_x1 + 1.0, mid).is_some());
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

        let (x, y) = point_of(&l, Page::PlanView, SoftKey::ToggleOrientation);
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
        let (x, y) = point_of(&l, Page::Weather, SoftKey::ScrollDown);
        tap(&mut view, &l, x, y, 5, 20);
        assert_eq!(view.weather_scroll, 5, "DOWN should advance one page of rows");

        let (x, y) = point_of(&l, Page::Weather, SoftKey::ScrollUp);
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
            assert_eq!(
                by_tap.altitude_filter, direct.altitude_filter,
                "slot {slot}"
            );
        }
    }

    #[test]
    fn the_altitude_key_steps_through_every_band_and_returns() {
        // One key, four states, so the only way back to where you started is all the way round.
        // A cycle that dead-ended would leave a filter engaged with no way to open it up.
        let l = layout();
        let mut view = ViewState::default();
        let start = view.altitude_filter;

        let (x, y) = point_of(&l, Page::PlanView, SoftKey::CycleAltitudeFilter);
        let mut seen = vec![start];
        for _ in 1..crate::AltitudeFilter::ALL.len() {
            tap(&mut view, &l, x, y, 5, 0);
            assert!(
                !seen.contains(&view.altitude_filter),
                "{:?} came round twice before the cycle closed",
                view.altitude_filter
            );
            seen.push(view.altitude_filter);
        }
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.altitude_filter, start, "the cycle must close");
        assert_eq!(seen.len(), crate::AltitudeFilter::ALL.len());
    }

    #[test]
    fn the_altitude_key_does_not_disturb_the_range() {
        // The two culls are independent, and the vertical one arrived late enough that wiring it
        // into the wrong ViewState field would be an easy mistake to make and a quiet one to have.
        let l = layout();
        let mut view = ViewState::default();
        let (x, y) = point_of(&l, Page::PlanView, SoftKey::CycleAltitudeFilter);
        tap(&mut view, &l, x, y, 5, 0);
        assert_eq!(view.range_nm, ViewState::default().range_nm);
        assert_eq!(view.orientation, ViewState::default().orientation);
        assert_ne!(view.altitude_filter, ViewState::default().altitude_filter);
    }
}
