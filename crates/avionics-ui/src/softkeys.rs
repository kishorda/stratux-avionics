//! The function-key strip down the **left-hand** edge.
//!
//! Six slots, all page-specific. Page selection is not here — it lives on the opposite edge, in
//! [`crate::pagestrip`], where all three pages are visible at once and the active one is filled.
//! Splitting the two was what made room to grow: every slot on this strip is now available to the
//! page that is showing, rather than one of six being permanently spent on navigation.
//!
//! On the 800x480 panel six slots are 75.8 px tall, which is why six and not more: seven would be
//! 65.0 px and eight 56.9 px, and the latter is below the floor that
//! `strip_is_wide_enough_to_hit_on_the_target_panel` holds the design to.
//!
//! # The hazard this design carries
//!
//! Context-sensitive keys mean the same physical spot does different things depending on what is
//! on screen. In turbulence that is a real way to press the wrong thing. Three mitigations,
//! all deliberate:
//!
//! * Navigation is never on this strip, so nothing here can move you off the page you are on.
//!   Recovering from a wrong press is always a press on the other edge.
//! * Every key is **labelled with its current action**, and the label is redrawn every frame —
//!   never a fixed legend that can disagree with what the key does.
//! * Unused slots are drawn dimmed and inert rather than left blank. A blank strip region reads
//!   as "screen not finished drawing"; a dimmed key with no label reads as "nothing here".
//!
//! Nothing here mutates anything. [`hit`] answers *which slot*, [`key_at`] answers *what it does
//! right now*, and `interact` applies it — so the whole mapping is testable without a GPU.

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;

use crate::{Layout, Page, Ui, ViewState};

/// Number of key slots in the strip, counted from the top.
pub const SLOTS: usize = 6;

/// What a soft key does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftKey {
    RangeUp,
    RangeDown,
    /// North-up / track-up.
    ToggleOrientation,
    /// NEXRAD precipitation underlay on the plan view.
    ToggleUnderlay,
    /// Step the vertical filter through its bands. The altitude equivalent of RangeUp.
    CycleAltitudeFilter,
    /// Step the map layer through off, airports, airports and airspace.
    CycleMapLayers,
    ScrollUp,
    ScrollDown,
    /// Tell the AHRS the aircraft is straight and level. Two presses: arm, then confirm.
    CageAhrs,
    /// Expand the selected weather report's abbreviations.
    ToggleDecode,
}

/// Which key sits in each slot for the given page.
///
/// `None` means the slot is drawn dimmed and does nothing. Slots are kept rather than compacted
/// so a key never changes position just because a neighbour became unavailable.
pub fn keys_for(page: Page) -> [Option<SoftKey>; SLOTS] {
    match page {
        // RNG and ALT sit together: they are the same idea on two axes, horizontal and vertical,
        // and the pilot reaching for "show me less" should find both without hunting. Freeing the
        // top slot from PAGE is what allowed them to be adjacent.
        Page::PlanView => [
            Some(SoftKey::RangeUp),
            Some(SoftKey::RangeDown),
            Some(SoftKey::CycleAltitudeFilter),
            Some(SoftKey::ToggleOrientation),
            Some(SoftKey::ToggleUnderlay),
            // The last free slot on this page. Growing the strip to seven would give 60.9 px a
            // key against the 60.0 px floor `strip_is_wide_enough_to_hit_on_the_target_panel`
            // holds the design to, which is not headroom — so both map layers share one key
            // rather than taking one each.
            Some(SoftKey::CycleMapLayers),
        ],
        Page::Weather => [
            Some(SoftKey::ScrollUp),
            Some(SoftKey::ScrollDown),
            Some(SoftKey::ToggleDecode),
            None,
            None,
            None,
        ],
        // LEVEL sits at the bottom of the strip, the furthest point on it from where a hand rests
        // reaching for anything else. It is the only key on this display that changes what an
        // instrument reads rather than how it is drawn, and it should take a deliberate reach.
        Page::Ahrs => [None, None, None, None, None, Some(SoftKey::CageAhrs)],
    }
}

/// The key in `slot` for the current page, if any.
pub fn key_at(view: &ViewState, slot: usize) -> Option<SoftKey> {
    keys_for(view.page).get(slot).copied().flatten()
}

/// The label to paint on a key.
///
/// Toggles are labelled with the state they are **currently in**, not the state they would move
/// to. Both conventions exist in the wild and neither is universally right, but "what am I
/// looking at" is the question a pilot glancing down is actually asking, and it matches the
/// footer, which also reports current state.
pub fn label(key: SoftKey, view: &ViewState) -> &'static str {
    match key {
        SoftKey::RangeUp => "RNG +",
        SoftKey::RangeDown => "RNG -",
        SoftKey::ToggleOrientation => view.orientation.label(),
        SoftKey::ToggleUnderlay => {
            if view.show_weather_underlay {
                "WX ON"
            } else {
                "WX OFF"
            }
        }
        // Shares its text with the footer readout, so the key and the page can never disagree
        // about which band is selected.
        SoftKey::CycleAltitudeFilter => view.altitude_filter.label(),
        SoftKey::CycleMapLayers => view.map_layers.label(),
        SoftKey::ScrollUp => "UP",
        SoftKey::ScrollDown => "DOWN",
        // Labelled with what pressing it gives you, unlike the state-labelled toggles above:
        // there is no ambiguity about which view you are looking at, so naming the destination
        // is more useful than naming where you already are.
        SoftKey::ToggleDecode => {
            if view.weather_decode {
                "RAW"
            } else {
                "DECODE"
            }
        }
        SoftKey::CageAhrs => match view.cage {
            crate::CageState::Idle => "LEVEL",
            crate::CageState::Armed => "CONFIRM",
            crate::CageState::Requested | crate::CageState::InFlight => "...",
            crate::CageState::Done { ok: true } => "CAGED",
            crate::CageState::Done { ok: false } => "FAILED",
        },
    }
}

/// Pixel rectangle `(x, y, w, h)` of a slot.
///
/// The strip runs from the top of the screen to the top of the footer, so the first slot sits
/// alongside the status bar. The footer stays full width: it is a readout, not a control, and
/// stopping it short of either strip would waste the one line that reports selected range.
pub fn slot_rect(layout: &Layout, slot: usize) -> (f32, f32, f32, f32) {
    let h = layout.strip_height() / SLOTS as f32;
    (
        0.0,
        layout.strip_y0() + h * slot as f32,
        layout.strip_width,
        h,
    )
}

/// Which slot a point falls in, or `None` if it is outside the strip.
pub fn hit(layout: &Layout, x: f32, y: f32) -> Option<usize> {
    if x >= layout.content_x0 {
        return None;
    }
    // Both bars belong to themselves. The strip used to run the full height and take the top slot
    // out of the status bar's row; now it starts below it, so a press in either bar is not a key.
    if y < layout.strip_y0() || y >= layout.strip_y1() {
        return None;
    }
    let h = layout.strip_height() / SLOTS as f32;
    let slot = ((y - layout.strip_y0()) / h) as usize;
    // Guard the bottom edge: `y` exactly at `strip_height` would index one past the end.
    Some(slot.min(SLOTS - 1))
}

/// Draw the strip.
///
/// `stats` is the frame that has just been drawn, and is used for exactly one thing: colouring the
/// ALT key by whether the filter is *currently withholding traffic*, rather than by whether a
/// filter is merely selected. The default band is a narrowing one, so "amber whenever a filter is
/// set" would light the key on every flight from power-on and mean nothing by the time it did
/// matter.
pub fn draw(ui: &Ui, canvas: &mut Canvas, view: &ViewState, stats: &crate::FrameStats) {
    let layout = ui.layout(canvas);
    let theme = &ui.theme;
    let keys = keys_for(view.page);

    // The strip sits on the bar colour so it reads as chrome rather than as part of the moving
    // picture behind it.
    let mut background = Path::new();
    background.rect(
        0.0,
        layout.strip_y0(),
        layout.strip_width,
        layout.strip_height(),
    );
    canvas.fill_path(&background, &Paint::color(theme.bar_background));

    let mut edge = Path::new();
    edge.move_to(layout.content_x0, layout.strip_y0());
    edge.line_to(layout.content_x0, layout.strip_y1());
    canvas.stroke_path(&edge, &Paint::color(theme.text_dim).with_line_width(1.0));

    // All four dividers in one path and one draw: they share a colour and a width, and each
    // separate `stroke_path` costs a GL draw call on a board where those are not free.
    let mut dividers = Path::new();
    for slot in 1..SLOTS {
        let (x, y, w, _) = slot_rect(&layout, slot);
        dividers.move_to(x + layout.margin, y);
        dividers.line_to(x + w - layout.margin, y);
    }
    canvas.stroke_path(
        &dividers,
        &Paint::color(crate::theme::faded(theme.text_dim, 0.6)).with_line_width(1.0),
    );

    for (slot, key) in keys.iter().enumerate() {
        let (x, y, w, h) = slot_rect(&layout, slot);

        let Some(key) = key else { continue };

        // Active toggles get the accent colour so their state is readable without parsing text —
        // useful at a glance, and the label still carries the authoritative answer.
        let colour = match key {
            SoftKey::ToggleUnderlay if view.show_weather_underlay => theme.good,
            // Amber only while traffic is actually being held back by it. See `draw`.
            SoftKey::CycleAltitudeFilter if stats.targets_outside_altitude > 0 => theme.caution,
            // Amber for airspace, not green: it is the state that raises the NOT FOR NAVIGATION
            // banner, and a key that looked like every other "on" toggle would undersell that.
            SoftKey::CycleMapLayers if view.map_layers.shows_airspace() => theme.caution,
            SoftKey::CycleMapLayers if view.map_layers.shows_airports() => theme.good,
            // An armed cage is amber: it is one press from changing the attitude reference, and
            // that state must not look like every other key on the strip.
            SoftKey::ToggleDecode if view.weather_decode => theme.good,
            SoftKey::CageAhrs => match view.cage {
                crate::CageState::Armed => theme.caution,
                crate::CageState::Done { ok: true } => theme.good,
                crate::CageState::Done { ok: false } => theme.warning,
                _ => theme.text_primary,
            },
            _ => theme.text_primary,
        };

        let mut paint = Paint::color(colour);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_small);
        paint.set_text_align(Align::Center);
        paint.set_text_baseline(Baseline::Middle);
        let _ = canvas.fill_text(x + w * 0.5, y + h * 0.5, label(*key, view), &paint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Theme;

    fn layout() -> Layout {
        Layout::for_size(800.0, 480.0, &Theme::dark())
    }

    #[test]
    fn navigation_is_never_on_this_strip() {
        // The property that replaced "PAGE is always in slot 0". Page selection lives on the
        // opposite edge, and nothing here may move the pilot off the page they are on — so a
        // mispress on the function strip is always recoverable without hunting for where PAGE
        // went on the page you landed on.
        for page in Page::ALL {
            for key in keys_for(page).into_iter().flatten() {
                assert!(
                    !matches!(key, SoftKey::ScrollUp | SoftKey::ScrollDown)
                        || page == Page::Weather,
                    "{key:?} does not belong on {page:?}"
                );
            }
        }
        // And every page's keys are drawn from its own page's set, never another's.
        assert!(keys_for(Page::Ahrs)
            .into_iter()
            .flatten()
            .all(|k| k == SoftKey::CageAhrs));
    }

    #[test]
    fn taps_right_of_the_strip_are_not_key_presses() {
        // The strip is on the left now, so everything from the content area rightwards belongs to
        // the page or to the page strip. The old version of this test asserted the mirror image
        // and would have passed unchanged against a strip that had not moved at all.
        let l = layout();
        // Probe the centre of slot 1 rather than a fixed y. The strip's top moves with the status
        // bar height, which moves with the font size — a hardcoded 100.0 silently changed which
        // slot it was testing the first time the text got bigger.
        let (_, y, _, h) = slot_rect(&l, 1);
        let mid = y + h * 0.5;

        assert_eq!(hit(&l, l.content_x0, mid), None, "content area");
        assert_eq!(hit(&l, l.content_x1 + 5.0, mid), None, "page strip");
        assert_eq!(hit(&l, l.width - 1.0, mid), None);
        assert_eq!(
            hit(&l, l.content_x0 - 1.0, mid),
            Some(1),
            "inside the strip"
        );
    }

    #[test]
    fn neither_bar_belongs_to_this_strip() {
        // The strip runs between the two bars, not the full height of the panel. Both ends are
        // pinned: a press in the status bar or the footer bar is not a key press, even though it
        // is inside the strip's columns.
        let l = layout();
        let x = l.strip_width * 0.5;
        assert_eq!(hit(&l, x, 0.0), None, "top of the status bar");
        assert_eq!(hit(&l, x, l.status_bar_height - 0.001), None, "status bar");
        assert_eq!(hit(&l, x, l.footer_y0()), None, "top of the footer bar");
        assert_eq!(hit(&l, x, l.height - 1.0), None, "footer bar");

        assert_eq!(
            hit(&l, x, l.strip_y0()),
            Some(0),
            "first pixel below the bar is slot 0"
        );
        assert_eq!(hit(&l, x, l.strip_y1() - 0.001), Some(SLOTS - 1));
    }

    #[test]
    fn every_slot_is_reachable_and_none_overlap() {
        let l = layout();
        for slot in 0..SLOTS {
            let (x, y, w, h) = slot_rect(&l, slot);
            // Probe the centre of each slot.
            assert_eq!(
                hit(&l, x + w * 0.5, y + h * 0.5),
                Some(slot),
                "slot {slot} centre did not hit itself"
            );
        }
    }

    #[test]
    fn the_bottom_edge_does_not_index_past_the_last_slot() {
        let l = layout();
        assert_eq!(
            hit(&l, l.strip_width * 0.5, l.strip_y1() - 0.001),
            Some(SLOTS - 1)
        );
    }

    #[test]
    fn slots_tile_the_strip_without_gaps() {
        let l = layout();
        let mut expected_y = l.strip_y0();
        for slot in 0..SLOTS {
            let (_, y, _, h) = slot_rect(&l, slot);
            assert!(
                (y - expected_y).abs() < 0.001,
                "slot {slot} starts at a gap"
            );
            expected_y += h;
        }
        assert!((expected_y - l.strip_y1()).abs() < 0.001);
    }

    #[test]
    fn strip_is_wide_enough_to_hit_on_the_target_panel() {
        // 800x480 is the panel this was designed against; a key must stay finger-sized.
        let l = layout();
        let (_, _, w, h) = slot_rect(&l, 0);
        assert!(w >= 72.0, "strip too narrow to hit reliably: {w}");
        assert!(h >= 60.0, "keys too short to hit reliably: {h}");
    }

    #[test]
    fn orientation_key_reports_current_state_not_the_pending_one() {
        let mut view = ViewState::default();
        let before = label(SoftKey::ToggleOrientation, &view);
        view.toggle_orientation();
        assert_ne!(before, label(SoftKey::ToggleOrientation, &view));
    }

    #[test]
    fn the_attitude_page_offers_level_only() {
        let keys = keys_for(Page::Ahrs);
        // LEVEL sits in the last slot, the furthest point on the strip from where a hand rests
        // reaching for anything else. Asserted as "the last slot" rather than as a number, so
        // growing the strip moves it instead of stranding it in the middle.
        assert_eq!(keys[SLOTS - 1], Some(SoftKey::CageAhrs));
        assert!(
            keys[..SLOTS - 1].iter().all(Option::is_none),
            "nothing else on this page is adjustable"
        );
    }

    #[test]
    fn the_weather_page_offers_scrolling_and_decoding() {
        let keys = keys_for(Page::Weather);
        assert_eq!(keys[0], Some(SoftKey::ScrollUp));
        assert_eq!(keys[1], Some(SoftKey::ScrollDown));
        assert_eq!(keys[2], Some(SoftKey::ToggleDecode));
        assert!(
            keys[3..].iter().all(Option::is_none),
            "nothing invented to fill the spare slots"
        );
    }

    #[test]
    fn the_plan_view_keys_are_pinned_in_order() {
        // Every key on this page and where it sits. The point is not the list, it is that moving a
        // key has to be a deliberate edit here: these are pressed by feel in turbulence, and a key
        // that quietly migrates because a neighbour was added is the failure this prevents.
        //
        // MAP took the last free slot. The strip is now full at six, and seven would give 60.9 px
        // a key against the 60.0 px floor the hittability test holds — so the next page-specific
        // control has to displace something rather than being appended.
        let keys = keys_for(Page::PlanView);
        assert_eq!(
            keys,
            [
                Some(SoftKey::RangeUp),
                Some(SoftKey::RangeDown),
                Some(SoftKey::CycleAltitudeFilter),
                Some(SoftKey::ToggleOrientation),
                Some(SoftKey::ToggleUnderlay),
                Some(SoftKey::CycleMapLayers),
            ]
        );
        assert!(keys.iter().all(Option::is_some), "the strip is full");
    }

    #[test]
    fn the_map_key_reports_the_layer_it_is_in() {
        // Same rule as the orientation and altitude keys: labelled with where you are, not with
        // where pressing it would take you.
        let mut view = ViewState::default();
        for _ in 0..crate::MapLayers::ALL.len() {
            assert_eq!(
                label(SoftKey::CycleMapLayers, &view),
                view.map_layers.label()
            );
            view.cycle_map_layers();
        }
        // ... and the cycle returns to where it started rather than stranding a state.
        assert_eq!(view.map_layers, ViewState::default().map_layers);
    }

    #[test]
    fn the_altitude_key_reports_the_band_it_is_in() {
        // Same rule as the orientation key: a toggle is labelled with where you are, not where
        // pressing it would take you.
        let mut view = ViewState::default();
        for _ in 0..crate::AltitudeFilter::ALL.len() {
            assert_eq!(
                label(SoftKey::CycleAltitudeFilter, &view),
                view.altitude_filter.label()
            );
            view.cycle_altitude_filter();
        }
    }
}
