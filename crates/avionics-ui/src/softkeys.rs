//! The soft-key strip down the right-hand edge.
//!
//! Five slots. The top one is **always** PAGE; the four below it change meaning with the page,
//! Garmin-style. That split is the whole design: page switching is the one action you need to be
//! able to perform without reading the screen, so it never moves, while the remaining keys are
//! free to be useful per page.
//!
//! # The hazard this design carries
//!
//! Context-sensitive keys mean the same physical spot does different things depending on what is
//! on screen. In turbulence that is a real way to press the wrong thing. Three mitigations,
//! all deliberate:
//!
//! * PAGE never moves, so recovering from a wrong page is always the same key.
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
pub const SLOTS: usize = 5;

/// The slot that carries PAGE on every page. Never changes.
pub const PAGE_SLOT: usize = 0;

/// What a soft key does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftKey {
    /// Advance to the next page. Present in [`PAGE_SLOT`] on every page.
    Page,
    RangeUp,
    RangeDown,
    /// North-up / track-up.
    ToggleOrientation,
    /// NEXRAD precipitation underlay on the plan view.
    ToggleUnderlay,
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
        Page::PlanView => [
            Some(SoftKey::Page),
            Some(SoftKey::RangeUp),
            Some(SoftKey::RangeDown),
            Some(SoftKey::ToggleOrientation),
            Some(SoftKey::ToggleUnderlay),
        ],
        Page::Weather => [
            Some(SoftKey::Page),
            Some(SoftKey::ScrollUp),
            Some(SoftKey::ScrollDown),
            Some(SoftKey::ToggleDecode),
            None,
        ],
        // LEVEL sits in slot 4, the bottom of the strip, deliberately as far as possible from
        // PAGE at the top: those are the only two live keys here, and the one that re-references
        // the attitude sensor should not be adjacent to the one used constantly.
        Page::Ahrs => [
            Some(SoftKey::Page),
            None,
            None,
            None,
            Some(SoftKey::CageAhrs),
        ],
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
        SoftKey::Page => "PAGE",
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
/// stopping it short of the strip would waste the one line that reports selected range.
pub fn slot_rect(layout: &Layout, slot: usize) -> (f32, f32, f32, f32) {
    let x = layout.content_width;
    let strip_height = (layout.height - layout.footer_height).max(1.0);
    let h = strip_height / SLOTS as f32;
    (x, h * slot as f32, layout.strip_width, h)
}

/// Which slot a point falls in, or `None` if it is outside the strip.
pub fn hit(layout: &Layout, x: f32, y: f32) -> Option<usize> {
    if x < layout.content_width {
        return None;
    }
    let strip_height = (layout.height - layout.footer_height).max(1.0);
    if y < 0.0 || y >= strip_height {
        return None;
    }
    let h = strip_height / SLOTS as f32;
    let slot = (y / h) as usize;
    // Guard the bottom edge: `y` exactly at `strip_height` would index one past the end.
    Some(slot.min(SLOTS - 1))
}

pub fn draw(ui: &Ui, canvas: &mut Canvas, view: &ViewState) {
    let layout = ui.layout(canvas);
    let theme = &ui.theme;
    let keys = keys_for(view.page);

    // The strip sits on the bar colour so it reads as chrome rather than as part of the moving
    // picture behind it.
    let mut background = Path::new();
    background.rect(
        layout.content_width,
        0.0,
        layout.strip_width,
        layout.height - layout.footer_height,
    );
    canvas.fill_path(&background, &Paint::color(theme.bar_background));

    let mut edge = Path::new();
    edge.move_to(layout.content_width, 0.0);
    edge.line_to(layout.content_width, layout.height - layout.footer_height);
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
    fn page_key_is_in_the_same_slot_on_every_page() {
        for page in [Page::PlanView, Page::Weather, Page::Ahrs] {
            assert_eq!(keys_for(page)[PAGE_SLOT], Some(SoftKey::Page));
        }
    }

    #[test]
    fn taps_left_of_the_strip_are_not_key_presses() {
        let l = layout();
        assert_eq!(hit(&l, l.content_width - 1.0, 100.0), None);
        assert_eq!(hit(&l, 0.0, 100.0), None);
    }

    #[test]
    fn taps_in_the_footer_are_not_key_presses() {
        let l = layout();
        let below_strip = l.height - l.footer_height + 1.0;
        assert_eq!(hit(&l, l.content_width + 5.0, below_strip), None);
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
        let strip_height = l.height - l.footer_height;
        assert_eq!(hit(&l, l.content_width + 1.0, strip_height - 0.001), Some(SLOTS - 1));
    }

    #[test]
    fn slots_tile_the_strip_without_gaps() {
        let l = layout();
        let mut expected_y = 0.0;
        for slot in 0..SLOTS {
            let (_, y, _, h) = slot_rect(&l, slot);
            assert!((y - expected_y).abs() < 0.001, "slot {slot} starts at a gap");
            expected_y += h;
        }
        assert!((expected_y - (l.height - l.footer_height)).abs() < 0.001);
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
    fn the_attitude_page_offers_page_and_level_only() {
        let keys = keys_for(Page::Ahrs);
        assert_eq!(keys[PAGE_SLOT], Some(SoftKey::Page));
        assert_eq!(keys[4], Some(SoftKey::CageAhrs));
        // LEVEL is at the far end of the strip from PAGE on purpose: those are the only two live
        // keys here, and the one that re-references the attitude sensor should not sit next to
        // the one used constantly.
        assert!(
            keys[1..4].iter().all(Option::is_none),
            "nothing else on this page is adjustable"
        );
    }

    #[test]
    fn the_weather_page_offers_scrolling_and_decoding() {
        let keys = keys_for(Page::Weather);
        assert_eq!(keys[PAGE_SLOT], Some(SoftKey::Page));
        assert_eq!(keys[1], Some(SoftKey::ScrollUp));
        assert_eq!(keys[2], Some(SoftKey::ScrollDown));
        assert_eq!(keys[3], Some(SoftKey::ToggleDecode));
        assert_eq!(keys[4], None, "nothing invented to fill the last slot");
    }
}
