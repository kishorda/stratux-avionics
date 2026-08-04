//! The page selector down the **right-hand** edge.
//!
//! Every page has a key, all of them are visible at once, and the one you are on is filled. That
//! is the whole of it.
//!
//! # Why this replaced a cycling PAGE key
//!
//! The old strip carried a single PAGE key that advanced through the cycle, justified on the
//! grounds that it never moved, so recovering from a wrong page was always the same press. Direct
//! selection keeps that property and improves on it: *every* page's key never moves, and getting
//! to the attitude page from the weather page is one press rather than two.
//!
//! It also removes a class of error the cycle had. With a cycle, pressing PAGE when you are not
//! sure which page you are on is a guess — you may land where you wanted or one short of it. With
//! a filled key showing the answer, there is nothing to guess.
//!
//! # Sized by the number of pages, not by a slot count
//!
//! [`crate::softkeys`] has a fixed six slots because its contents change per page and a key that
//! moved when a neighbour disappeared would be worse than a dimmed one. Nothing here changes: the
//! page list is the same on every page, so the keys simply divide the strip. Three pages on the
//! 800x480 panel gives 151.6 px each, which makes navigation by a wide margin the easiest thing
//! on the display to hit — appropriate, since it is the way out of anywhere.
//!
//! Adding a page resizes them all rather than stranding a gap. Four would be 113.7 px and five
//! 91.0 px, so there is room for two more before this reaches the height of a function key.

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;

use crate::{Layout, Page, Ui, ViewState};

/// Pixel rectangle `(x, y, w, h)` of a page's key.
pub fn slot_rect(layout: &Layout, slot: usize) -> (f32, f32, f32, f32) {
    let h = layout.strip_height() / Page::ALL.len() as f32;
    (
        layout.content_x1,
        layout.strip_y0() + h * slot as f32,
        layout.strip_width,
        h,
    )
}

/// Which page a point selects, or `None` if it is outside the strip.
pub fn hit(layout: &Layout, x: f32, y: f32) -> Option<Page> {
    if x < layout.content_x1 {
        return None;
    }
    if y < layout.strip_y0() || y >= layout.strip_y1() {
        return None;
    }
    let h = layout.strip_height() / Page::ALL.len() as f32;
    let slot = ((y - layout.strip_y0()) / h) as usize;
    // Guard the bottom edge: `y` exactly at `strip_height` would index one past the end.
    Page::ALL.get(slot.min(Page::ALL.len() - 1)).copied()
}

pub fn draw(ui: &Ui, canvas: &mut Canvas, view: &ViewState) {
    let layout = ui.layout(canvas);
    let theme = &ui.theme;

    let mut background = Path::new();
    background.rect(
        layout.content_x1,
        layout.strip_y0(),
        layout.strip_width,
        layout.strip_height(),
    );
    canvas.fill_path(&background, &Paint::color(theme.bar_background));

    let mut edge = Path::new();
    edge.move_to(layout.content_x1, layout.strip_y0());
    edge.line_to(layout.content_x1, layout.strip_y1());
    canvas.stroke_path(&edge, &Paint::color(theme.text_dim).with_line_width(1.0));

    // Dividers in one path and one draw, as on the function strip: they share a colour and a
    // width, and each separate `stroke_path` costs a GL draw call on a board where those are not
    // free.
    let mut dividers = Path::new();
    for slot in 1..Page::ALL.len() {
        let (x, y, w, _) = slot_rect(&layout, slot);
        dividers.move_to(x + layout.margin, y);
        dividers.line_to(x + w - layout.margin, y);
    }
    canvas.stroke_path(
        &dividers,
        &Paint::color(crate::theme::faded(theme.text_dim, 0.6)).with_line_width(1.0),
    );

    for (slot, page) in Page::ALL.iter().enumerate() {
        let (x, y, w, h) = slot_rect(&layout, slot);
        let active = *page == view.page;

        // The active page is a filled block with background-coloured text, not merely a brighter
        // label. A colour difference alone is the first thing to go in direct sunlight through a
        // windscreen, and "which page am I on" is the question this strip exists to answer.
        if active {
            let mut fill = Path::new();
            fill.rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0);
            canvas.fill_path(&fill, &Paint::color(theme.ring_label));
        }

        let mut paint = Paint::color(if active {
            theme.background
        } else {
            theme.text_secondary
        });
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_normal);
        paint.set_text_align(Align::Center);
        paint.set_text_baseline(Baseline::Middle);
        let _ = canvas.fill_text(x + w * 0.5, y + h * 0.5, page.label(), &paint);
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
    fn every_page_has_a_key_and_each_one_hits_itself() {
        let l = layout();
        for (slot, page) in Page::ALL.iter().enumerate() {
            let (x, y, w, h) = slot_rect(&l, slot);
            assert_eq!(
                hit(&l, x + w * 0.5, y + h * 0.5),
                Some(*page),
                "{page:?} key did not select its own page"
            );
        }
    }

    #[test]
    fn the_content_area_is_not_the_page_strip() {
        // The gap between the two strips must belong to neither. This is the shape of the bug
        // that once made every touch resolve to the top of the screen, so it is worth pinning on
        // both edges rather than assuming.
        let l = layout();
        assert_eq!(hit(&l, l.content_x1 - 1.0, 100.0), None);
        assert_eq!(hit(&l, l.content_x0 + 1.0, 100.0), None);
        assert_eq!(hit(&l, 0.0, 100.0), None, "that is the function strip");
    }

    #[test]
    fn neither_bar_belongs_to_this_strip() {
        let l = layout();
        let x = l.content_x1 + 5.0;
        assert_eq!(hit(&l, x, 0.0), None, "top of the status bar");
        assert_eq!(hit(&l, x, l.status_bar_height - 0.001), None, "status bar");
        assert_eq!(hit(&l, x, l.footer_y0()), None, "top of the footer bar");
        assert_eq!(hit(&l, x, l.height - 1.0), None, "footer bar");

        assert_eq!(hit(&l, x, l.strip_y0()), Page::ALL.first().copied());
        assert_eq!(hit(&l, x, l.strip_y1() - 0.001), Page::ALL.last().copied());
    }

    #[test]
    fn the_bottom_edge_does_not_index_past_the_last_page() {
        let l = layout();
        assert_eq!(
            hit(&l, l.content_x1 + 1.0, l.strip_y1() - 0.001),
            Page::ALL.last().copied()
        );
    }

    #[test]
    fn keys_tile_the_strip_without_gaps() {
        let l = layout();
        let mut expected_y = l.strip_y0();
        for slot in 0..Page::ALL.len() {
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
    fn page_keys_are_at_least_as_hittable_as_function_keys() {
        // Navigation is the way out of anywhere, so it must never be the harder press.
        let l = layout();
        let (_, _, _, page_h) = slot_rect(&l, 0);
        let (_, _, _, fn_h) = crate::softkeys::slot_rect(&l, 0);
        assert!(
            page_h >= fn_h,
            "page keys {page_h} shorter than function keys {fn_h}"
        );
        assert!(
            page_h >= 60.0,
            "page keys too short to hit reliably: {page_h}"
        );
    }

    #[test]
    fn the_two_strips_do_not_overlap() {
        let l = layout();
        let (fn_x, _, fn_w, _) = crate::softkeys::slot_rect(&l, 0);
        let (pg_x, _, _, _) = slot_rect(&l, 0);
        assert!(fn_x + fn_w <= pg_x, "the strips overlap");
        assert!(fn_x + fn_w <= l.content_x0);
        assert!(pg_x >= l.content_x1);
    }
}
