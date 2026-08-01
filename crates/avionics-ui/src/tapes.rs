//! Moving-scale tapes: the vertical strips either side of the attitude display.
//!
//! A tape shows a value by sliding a numbered scale past a fixed pointer, rather than by
//! printing a number. That is worth the pixels because it encodes **rate and direction** as well
//! as magnitude: a glance shows not just "1770 feet" but "climbing, and quickly". A text readout
//! gives you the first and none of the second, which is why every glass panel uses tapes.
//!
//! # What is fixed and what moves
//!
//! The pointer never moves. The scale slides. This is the opposite of a car speedometer and it
//! matters: the eye learns one screen position for "current value" and never has to hunt for a
//! needle. The digital box sits at that same fixed position so the exact figure is always in the
//! place the eye is already looking.
//!
//! # Missing values
//!
//! A tape with no value draws its frame and its label but **no scale and no box** — see
//! [`draw`]. It must never park the scale at zero: a tape reading zero looks exactly like a
//! working instrument reporting zero, and "no GPS fix" and "stationary" are very different
//! things to a pilot. Same reasoning as the attitude failure flag in [`crate::ahrspage`].

use avionics_gfx::femtovg::{Align, Baseline, Color, Paint, Path};
use avionics_gfx::Canvas;

use crate::{Theme, Ui};

/// Which edge the pointer box projects from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Tape on the left of the screen; the box points right, towards the attitude.
    Left,
    /// Tape on the right; the box points left.
    Right,
}

/// Geometry and scale of one tape.
#[derive(Debug, Clone, Copy)]
pub struct Tape {
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub bottom: f32,
    /// How much of the scale one pixel covers. Smaller means a longer, finer tape.
    pub units_per_px: f32,
    /// Labelled tick interval.
    pub major: f32,
    /// Unlabelled tick interval.
    pub minor: f32,
    pub side: Side,
    /// Caption under the tape.
    pub label: &'static str,
}

impl Tape {
    fn centre_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }

    /// Screen y for a scale value, given what is currently under the pointer.
    fn y_for(&self, value: f64, current: f64) -> f32 {
        // Higher values sit higher on screen, so the offset is negated.
        self.centre_y() - ((value - current) / self.units_per_px as f64) as f32
    }
}

/// Draw a tape. `value` of `None` means no reading: the scale and box are omitted entirely.
pub fn draw(ui: &Ui, canvas: &mut Canvas, tape: &Tape, value: Option<f64>, format: impl Fn(f64) -> String) {
    let theme = &ui.theme;
    frame(canvas, tape, theme);

    if let Some(current) = value {
        // Clip the sliding scale to the tape so numbers do not spill over the attitude, and stop
        // it short of the caption band: a scale label sliding through "BARO FT" makes both
        // unreadable at exactly the moment the altitude is changing.
        let caption_band = ui.theme.font_size_tag * 1.7;
        canvas.save();
        canvas.scissor(
            tape.x,
            tape.top,
            tape.width,
            (tape.bottom - tape.top - caption_band).max(1.0),
        );
        scale(ui, canvas, tape, current);
        canvas.restore();
    }

    caption(ui, canvas, tape, theme);

    match value {
        Some(current) => value_box(ui, canvas, tape, current, &format),
        // No box at all rather than a box full of dashes: the pointer is what says "the value is
        // here", and there is no value.
        None => no_data(ui, canvas, tape, theme),
    }
}

fn frame(canvas: &mut Canvas, tape: &Tape, theme: &Theme) {
    let mut panel = Path::new();
    panel.rect(tape.x, tape.top, tape.width, tape.bottom - tape.top);
    // Translucent so the horizon stays readable behind the tape — the attitude is the primary
    // picture and the tapes are an overlay on it, not a wall beside it.
    canvas.fill_path(&panel, &Paint::color(crate::theme::faded(theme.background, 0.55)));

    // A rule down the edge nearest the attitude, so the tape reads as a distinct instrument.
    let edge_x = match tape.side {
        Side::Left => tape.x + tape.width,
        Side::Right => tape.x,
    };
    let mut edge = Path::new();
    edge.move_to(edge_x, tape.top);
    edge.line_to(edge_x, tape.bottom);
    canvas.stroke_path(&edge, &Paint::color(theme.text_dim).with_line_width(1.0));
}

fn scale(ui: &Ui, canvas: &mut Canvas, tape: &Tape, current: f64) {
    let theme = &ui.theme;
    let half_span = (tape.bottom - tape.top) * 0.5 * tape.units_per_px;
    // A tick beyond the ends is still drawn: its label may be partly visible, and the scissor
    // handles the rest. Losing it would make the scale flicker at the edges as it slides.
    let lo = current - half_span as f64 - tape.major as f64;
    let hi = current + half_span as f64 + tape.major as f64;

    let mut text = Paint::color(theme.text_primary);
    text.set_font(&[ui.font()]);
    text.set_font_size(theme.font_size_small);
    text.set_text_baseline(Baseline::Middle);

    let (tick_from, tick_to, label_x, align) = match tape.side {
        Side::Left => (
            tape.x + tape.width,
            tape.x + tape.width - 8.0,
            tape.x + tape.width - 12.0,
            Align::Right,
        ),
        Side::Right => (tape.x, tape.x + 8.0, tape.x + 12.0, Align::Left),
    };
    text.set_text_align(align);

    // Minor ticks first, so a major tick drawn over one wins.
    let mut minors = Path::new();
    let mut n = (lo / tape.minor as f64).ceil() as i64;
    while (n as f64) * tape.minor as f64 <= hi {
        let v = n as f64 * tape.minor as f64;
        n += 1;
        if (v / tape.major as f64).fract().abs() < 1e-6 {
            continue;
        }
        let y = tape.y_for(v, current);
        minors.move_to(tick_from, y);
        minors.line_to(tick_to + (tick_from - tick_to) * 0.45, y);
    }
    canvas.stroke_path(&minors, &Paint::color(theme.text_dim).with_line_width(1.0));

    let mut majors = Path::new();
    let mut n = (lo / tape.major as f64).ceil() as i64;
    while (n as f64) * tape.major as f64 <= hi {
        let v = n as f64 * tape.major as f64;
        n += 1;
        let y = tape.y_for(v, current);
        majors.move_to(tick_from, y);
        majors.line_to(tick_to, y);
        let _ = canvas.fill_text(label_x, y, format_major(v), &text);
    }
    canvas.stroke_path(&majors, &Paint::color(theme.text_secondary).with_line_width(1.4));
}

/// Major ticks are whole numbers; drop the decimal point that `{}` on an f64 would add.
fn format_major(v: f64) -> String {
    format!("{:.0}", v)
}

fn caption(ui: &Ui, canvas: &mut Canvas, tape: &Tape, theme: &Theme) {
    let mut paint = Paint::color(theme.text_dim);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Bottom);
    let _ = canvas.fill_text(
        tape.x + tape.width * 0.5,
        tape.bottom - 3.0,
        tape.label,
        &paint,
    );
}

/// The fixed digital readout, with a chevron pointing at the scale.
fn value_box(ui: &Ui, canvas: &mut Canvas, tape: &Tape, current: f64, format: &impl Fn(f64) -> String) {
    let theme = &ui.theme;
    let text = format(current);

    let mut paint = Paint::color(theme.text_primary);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_large);
    paint.set_text_baseline(Baseline::Middle);

    let cy = tape.centre_y();
    let h = theme.font_size_large * 1.5;
    let w = tape.width + 6.0;
    let tip = 7.0;

    let (x0, x1, point_x, align, text_x) = match tape.side {
        Side::Left => (
            tape.x - 2.0,
            tape.x + w - 2.0,
            tape.x + w - 2.0 + tip,
            Align::Right,
            tape.x + w - 6.0,
        ),
        Side::Right => (
            tape.x + tape.width + 2.0 - w,
            tape.x + tape.width + 2.0,
            tape.x + tape.width + 2.0 - w - tip,
            Align::Left,
            tape.x + tape.width + 2.0 - w + 4.0,
        ),
    };
    paint.set_text_align(align);

    let mut box_path = Path::new();
    box_path.move_to(x0, cy - h * 0.5);
    box_path.line_to(x1, cy - h * 0.5);
    // The chevron: the pointer proper. It is what makes the box read as "the value here", rather
    // than as a label that happens to sit nearby.
    match tape.side {
        Side::Left => {
            box_path.line_to(point_x, cy);
            box_path.line_to(x1, cy + h * 0.5);
        }
        Side::Right => {
            box_path.line_to(x1, cy + h * 0.5);
            box_path.line_to(x0, cy + h * 0.5);
            box_path.line_to(point_x, cy);
            box_path.line_to(x0, cy - h * 0.5);
        }
    }
    if tape.side == Side::Left {
        box_path.line_to(x0, cy + h * 0.5);
    }
    box_path.close();

    canvas.fill_path(&box_path, &Paint::color(Color::rgba(0, 0, 0, 235)));
    canvas.stroke_path(&box_path, &Paint::color(theme.text_primary).with_line_width(1.6));
    let _ = canvas.fill_text(text_x, cy, &text, &paint);
}

/// What a tape shows when it has nothing to show.
fn no_data(ui: &Ui, canvas: &mut Canvas, tape: &Tape, theme: &Theme) {
    let mut paint = Paint::color(theme.warning);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_small);
    paint.set_text_align(Align::Center);
    paint.set_text_baseline(Baseline::Middle);
    let _ = canvas.fill_text(
        tape.x + tape.width * 0.5,
        tape.centre_y(),
        "NO DATA",
        &paint,
    );
}

/// The vertical-speed strip: a fixed scale with a moving pointer, unlike the sliding tapes.
///
/// Vertical speed is bounded and symmetric about zero, so a fixed scale puts "level" at a
/// constant screen position — which is the thing being checked most of the time. A sliding tape
/// would move that reference around for no benefit.
pub struct Vsi {
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub bottom: f32,
    /// Full-scale deflection, in feet per minute.
    pub full_scale: f32,
}

/// Vertical padding so the full-scale labels are not clipped by the chrome above and below.
const VSI_INSET: f32 = 22.0;

impl Vsi {
    fn centre_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }

    /// Screen y for a rate. Non-linear would be conventional on a large panel; linear is honest
    /// and legible at this size, where the whole strip is only a few hundred pixels.
    fn y_for(&self, fpm: f32) -> f32 {
        let half = (self.bottom - self.top) * 0.5 - VSI_INSET;
        self.centre_y() - (fpm / self.full_scale).clamp(-1.0, 1.0) * half
    }
}

pub fn draw_vsi(ui: &Ui, canvas: &mut Canvas, vsi: &Vsi, fpm: Option<f64>) {
    let theme = &ui.theme;

    let mut panel = Path::new();
    panel.rect(vsi.x, vsi.top, vsi.width, vsi.bottom - vsi.top);
    canvas.fill_path(&panel, &Paint::color(crate::theme::faded(theme.background, 0.55)));

    let mut edge = Path::new();
    edge.move_to(vsi.x, vsi.top);
    edge.line_to(vsi.x, vsi.bottom);
    canvas.stroke_path(&edge, &Paint::color(theme.text_dim).with_line_width(1.0));

    let mut text = Paint::color(theme.text_secondary);
    text.set_font(&[ui.font()]);
    text.set_font_size(theme.font_size_tag);
    text.set_text_align(Align::Left);
    text.set_text_baseline(Baseline::Middle);

    // Ticks every 500 fpm, labelled every 1000 in thousands.
    let mut ticks = Path::new();
    let step = 500.0;
    let mut v = -vsi.full_scale;
    while v <= vsi.full_scale + 1.0 {
        let y = vsi.y_for(v);
        let major = (v / 1000.0).fract().abs() < 1e-6;
        let len = if major { 9.0 } else { 5.0 };
        ticks.move_to(vsi.x, y);
        ticks.line_to(vsi.x + len, y);
        if major && v.abs() > 1.0 {
            let _ = canvas.fill_text(vsi.x + 12.0, y, format!("{:.0}", v.abs() / 1000.0), &text);
        }
        v += step;
    }
    canvas.stroke_path(&ticks, &Paint::color(theme.text_dim).with_line_width(1.2));

    // The zero line, drawn brighter: "not climbing or descending" is the reference the eye needs.
    let mut zero = Path::new();
    zero.move_to(vsi.x, vsi.centre_y());
    zero.line_to(vsi.x + vsi.width, vsi.centre_y());
    canvas.stroke_path(&zero, &Paint::color(theme.text_secondary).with_line_width(1.0));

    let mut caption = Paint::color(theme.text_dim);
    caption.set_font(&[ui.font()]);
    caption.set_font_size(theme.font_size_tag);
    caption.set_text_align(Align::Center);
    caption.set_text_baseline(Baseline::Bottom);
    let _ = canvas.fill_text(vsi.x + vsi.width * 0.5, vsi.bottom - 3.0, "VS", &caption);

    let Some(fpm) = fpm else {
        let mut paint = Paint::color(theme.warning);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_tag);
        paint.set_text_align(Align::Center);
        paint.set_text_baseline(Baseline::Middle);
        let _ = canvas.fill_text(vsi.x + vsi.width * 0.5, vsi.centre_y(), "--", &paint);
        return;
    };

    // A bar from zero to the current rate reads faster than a needle: length and direction are
    // both apparent without resolving where a thin pointer is sitting.
    let y = vsi.y_for(fpm as f32);
    let mut bar = Path::new();
    bar.rect(
        vsi.x + 1.0,
        y.min(vsi.centre_y()),
        4.0,
        (y - vsi.centre_y()).abs(),
    );
    canvas.fill_path(&bar, &Paint::color(theme.good));

    let mut pointer = Path::new();
    pointer.move_to(vsi.x, y);
    pointer.line_to(vsi.x + 9.0, y - 5.0);
    pointer.line_to(vsi.x + 9.0, y + 5.0);
    pointer.close();
    canvas.fill_path(&pointer, &Paint::color(theme.text_primary));

    // The digital figure, rounded to 50 fpm: a MEMS-derived rate jitters, and a last digit that
    // never settles is one you stop reading.
    let rounded = ((fpm / 50.0).round() * 50.0) as i32;
    if rounded != 0 {
        let mut paint = Paint::color(theme.text_primary);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_small);
        paint.set_text_align(Align::Center);
        paint.set_text_baseline(if fpm >= 0.0 { Baseline::Bottom } else { Baseline::Top });
        let text_y = if fpm >= 0.0 { vsi.top + 14.0 } else { vsi.bottom - 16.0 };
        let _ = canvas.fill_text(
            vsi.x + vsi.width * 0.5,
            text_y,
            format!("{rounded:+}"),
            &paint,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tape() -> Tape {
        Tape {
            x: 0.0,
            width: 64.0,
            top: 40.0,
            bottom: 440.0,
            units_per_px: 0.25,
            major: 10.0,
            minor: 5.0,
            side: Side::Left,
            label: "GS KT",
        }
    }

    #[test]
    fn the_current_value_sits_at_the_pointer() {
        let t = tape();
        assert!((t.y_for(104.0, 104.0) - t.centre_y()).abs() < 0.001);
    }

    #[test]
    fn higher_values_are_higher_on_screen() {
        // Screen y grows downward, so a larger value must produce a SMALLER y. Getting this
        // backwards gives a tape that runs the wrong way, which reads as plausible until you
        // accelerate.
        let t = tape();
        let above = t.y_for(114.0, 104.0);
        let below = t.y_for(94.0, 104.0);
        assert!(above < t.centre_y(), "114 kt should be above the pointer");
        assert!(below > t.centre_y(), "94 kt should be below it");
    }

    #[test]
    fn the_scale_moves_at_the_configured_rate() {
        let t = tape();
        // 10 units at 0.25 units per pixel is 40 pixels.
        let delta = t.centre_y() - t.y_for(114.0, 104.0);
        assert!((delta - 40.0).abs() < 0.001, "moved {delta} px, expected 40");
    }

    #[test]
    fn vsi_zero_is_at_the_centre_and_climb_is_up() {
        let v = Vsi {
            x: 0.0,
            width: 36.0,
            top: 40.0,
            bottom: 440.0,
            full_scale: 2000.0,
        };
        assert!((v.y_for(0.0) - v.centre_y()).abs() < 0.001);
        assert!(v.y_for(500.0) < v.centre_y(), "a climb must deflect upward");
        assert!(v.y_for(-500.0) > v.centre_y(), "a descent must deflect downward");
    }

    #[test]
    fn vsi_clamps_beyond_full_scale_rather_than_running_off_the_strip() {
        let v = Vsi {
            x: 0.0,
            width: 36.0,
            top: 40.0,
            bottom: 440.0,
            full_scale: 2000.0,
        };
        assert!((v.y_for(9999.0) - (v.top + VSI_INSET)).abs() < 0.001);
        assert!((v.y_for(-9999.0) - (v.bottom - VSI_INSET)).abs() < 0.001);
    }
}
