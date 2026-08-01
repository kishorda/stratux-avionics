//! Drawing the own-ship-centred traffic display.

use std::time::Instant;

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::domain::Target;
use stratux_client::AppState;

use crate::projection::{Orientation, Projection};
use crate::reckon::{reckon, reckon_ownship, Reckoned};
use crate::symbols;
use crate::theme::faded;
use crate::threat::{assess, format_relative_altitude, Assessment, ThreatLevel};
use crate::{FrameStats, Layout, Ui, ViewState};

/// One target resolved to screen coordinates, ready to draw.
struct Plotted<'a> {
    target: &'a Target,
    reckoned: Reckoned,
    assessment: Assessment,
    screen: (f32, f32),
}

/// Build the projection for this frame, or `None` when there is no usable own-ship position.
///
/// Separate from [`draw`] so the weather underlay and the traffic can share one projection.
pub fn make_projection(
    ui: &Ui,
    state: &AppState,
    view: &ViewState,
    now: Instant,
    layout: &Layout,
) -> Option<Projection> {
    let own_position = state.ownship.usable_position()?;
    let own_drawn = reckon_ownship(
        own_position,
        state.ownship.track_deg,
        state.ownship.ground_speed_kt,
        state.ownship.received,
        now,
        &ui.reckon,
    );
    Some(Projection::new(
        own_drawn,
        layout.center,
        layout.outer_radius / view.range_nm,
        view.orientation,
        state.ownship.track_deg,
    ))
}

pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    view: &ViewState,
    now: Instant,
    layout: &Layout,
    projection: Option<Projection>,
) -> FrameStats {
    let mut stats = FrameStats {
        targets_no_position: state.non_positional_count(),
        ..Default::default()
    };

    let Some(projection) = projection else {
        // Rings are still drawn, so the display looks alive and the range selection is visible,
        // but nothing is plotted: without own-ship there is no origin and every relative position
        // would be a fabrication.
        let _ = draw_rings(ui, canvas, view, layout, None);
        draw_no_position_notice(ui, canvas, state, layout);
        // Draw the footer here too, so the layout stays intact and the selected range is still
        // visible. A screen that loses a whole row of chrome looks broken rather than degraded.
        draw_footer(ui, canvas, layout, state, view);
        return stats;
    };

    let ring_chrome = draw_rings(ui, canvas, view, layout, Some(&projection));

    // Resolve every target first so alerts can be drawn last and therefore on top.
    let own_altitude = state.ownship.comparison_altitude_ft();
    let mut plotted: Vec<Plotted> = Vec::with_capacity(state.targets.len());

    for target in state.positional_targets() {
        let Some(reckoned) = reckon(target, now, &ui.reckon) else {
            continue;
        };
        let (range_nm, _) = projection.range_bearing(reckoned.position);
        if range_nm > view.range_nm {
            stats.targets_outside_range += 1;
            continue;
        }
        let assessment = assess(target, range_nm, own_altitude, &ui.threat);
        plotted.push(Plotted {
            target,
            reckoned,
            assessment,
            screen: projection.project(reckoned.position),
        });
    }

    // Ascending threat: alerts are drawn last and therefore on top of everything else.
    plotted.sort_by_key(|p| p.assessment.level);

    for item in &plotted {
        match item.assessment.level {
            ThreatLevel::Alert => stats.alerts += 1,
            ThreatLevel::Advisory => stats.advisories += 1,
            ThreatLevel::Normal => {}
        }
        if item.reckoned.coasting {
            stats.targets_coasting += 1;
        }
        draw_symbol(ui, canvas, &projection, item);
        stats.targets_drawn += 1;
    }

    draw_ownship(ui, canvas, layout, state, view);

    // Tags are a separate pass in *descending* threat order so that when labels compete for space,
    // the most important target keeps its preferred position and a distant one loses its label.
    // Overlapping tags on a traffic display are not a cosmetic problem: two unreadable labels are
    // worse than one readable label and one bare symbol.
    plotted.reverse();
    stats.tags_suppressed = draw_tags(ui, canvas, layout, &plotted, ring_chrome);

    draw_footer(ui, canvas, layout, state, view);

    stats
}

/// Range rings, compass ticks, the north pointer and the range labels.
///
/// Returns the screen rectangles it consumed, so the tag pass can avoid drawing over them.
fn draw_rings(
    ui: &Ui,
    canvas: &mut Canvas,
    view: &ViewState,
    layout: &Layout,
    projection: Option<&Projection>,
) -> Vec<Rect> {
    let theme = &ui.theme;
    let (cx, cy) = layout.center;
    let mut reserved = Vec::new();

    // The north pointer's screen angle drives where the range labels go, so compute it first.
    let north_angle = projection.map(|p| p.screen_angle_rad(0.0));

    // Range labels live on a fixed screen diagonal — they are a distance, not a direction, so they
    // must not rotate with the world in track-up. Which diagonal is chosen dynamically: the north
    // pointer sweeps the full circle as own-ship turns, so any fixed corner eventually collides
    // with it. Pick whichever candidate is furthest from it.
    let label_angle = {
        // Screen angles are clockwise from straight up, so x = sin, y = -cos.
        const CANDIDATES: [f32; 4] = [
            -std::f32::consts::FRAC_PI_4, // upper left
            std::f32::consts::FRAC_PI_4,  // upper right
            std::f32::consts::FRAC_PI_4 * 3.0, // lower right
            -std::f32::consts::FRAC_PI_4 * 3.0, // lower left
        ];
        match north_angle {
            None => CANDIDATES[0],
            Some(north) => *CANDIDATES
                .iter()
                .max_by(|a, b| {
                    angular_distance(**a, north)
                        .partial_cmp(&angular_distance(**b, north))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(&CANDIDATES[0]),
        }
    };

    // Two rings: the selected range and half of it. More than two turns into moiré on a 7" panel.
    for (index, fraction) in [1.0f32, 0.5].iter().enumerate() {
        let radius = layout.outer_radius * fraction;
        let mut ring = Path::new();
        ring.circle(cx, cy, radius);

        // The faint fill gives the rings depth without competing with traffic. Only the outer
        // ring is filled; filling both would double the alpha in the middle.
        if index == 0 {
            canvas.fill_path(&ring, &Paint::color(theme.ring_fill));
        }
        canvas.stroke_path(
            &ring,
            &Paint::color(theme.ring).with_line_width(theme.line_width * 0.9),
        );

        let mut label = Paint::color(theme.ring_label);
        label.set_font(&[ui.font()]);
        label.set_font_size(theme.font_size_small);
        label.set_text_align(Align::Center);
        label.set_text_baseline(Baseline::Middle);

        let inset = radius - theme.font_size_small * 0.9;
        let lx = cx + label_angle.sin() * inset;
        let ly = cy - label_angle.cos() * inset;
        let text = format_range(view.range_nm * fraction);
        let width = canvas
            .measure_text(0.0, 0.0, &text, &label)
            .map(|m| m.width())
            .unwrap_or(theme.font_size_small);
        let _ = canvas.fill_text(lx, ly, &text, &label);
        reserved.push(padded_rect(lx, ly, width, theme.font_size_small));
    }

    // Compass rose: ticks every 30 degrees, longer on the cardinals, each one labelled.
    //
    // Labelling every 30 degrees rather than only north is what makes this a compass rather than
    // a ring with an arrow on it. In track-up especially, "the target is off to the left" is much
    // less useful than "the target is to the south-west", and reading that off an unlabelled ring
    // means counting ticks.
    //
    // Cardinals get letters and the rest get tens of degrees — N, 3, 6, E, 12, 15, S ... — which
    // is the convention on every heading indicator, and keeps each label to one or two glyphs.
    // Spelling out "030" would triple the ink for no extra information.
    for degrees in (0..360).step_by(30) {
        let angle = match projection {
            Some(p) => p.screen_angle_rad(degrees as f32),
            None => (degrees as f32).to_radians(),
        };
        // Screen angle is clockwise from up: up is -y.
        let (sin, cos) = (angle.sin(), angle.cos());
        let cardinal = degrees % 90 == 0;
        let inner = layout.outer_radius * if cardinal { 0.90 } else { 0.95 };
        let outer = layout.outer_radius;

        let mut tick = Path::new();
        tick.move_to(cx + sin * inner, cy - cos * inner);
        tick.line_to(cx + sin * outer, cy - cos * outer);
        canvas.stroke_path(
            &tick,
            &Paint::color(theme.compass_tick)
                .with_line_width(if cardinal { theme.line_width } else { 1.0 }),
        );

        // North is the reference the pilot orients from, so it stays brighter and larger than the
        // rest; the others are a scale, and a scale that shouts competes with the traffic.
        let is_north = degrees == 0;
        let size = if cardinal {
            theme.font_size_small
        } else {
            theme.font_size_tag
        };
        let colour = if is_north {
            theme.ring_label
        } else if cardinal {
            theme.text_secondary
        } else {
            theme.compass_tick
        };

        let mut label = Paint::color(colour);
        label.set_font(&[ui.font()]);
        label.set_font_size(size);
        label.set_text_align(Align::Center);
        label.set_text_baseline(Baseline::Middle);

        let radius = layout.outer_radius + size * 0.85;
        let lx = cx + sin * radius;
        let ly = cy - cos * radius;
        let text = compass_label(degrees as u16);
        let _ = canvas.fill_text(lx, ly, text, &label);
        // Reserved so traffic tags route around the rose instead of overprinting it.
        reserved.push(padded_rect(lx, ly, size * text.len() as f32 * 0.62, size));
    }

    reserved
}

/// Smallest angular separation between two angles, in radians. Always in `0..=PI`.
pub(crate) fn angular_distance(a: f32, b: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let delta = (a - b).rem_euclid(TAU);
    if delta > PI {
        TAU - delta
    } else {
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::{angular_distance, compass_label};
    use std::f32::consts::{PI, TAU};

    #[test]
    fn angular_distance_is_zero_for_identical_angles() {
        // An earlier version of this returned PI here, which inverted the "pick the candidate
        // furthest from the north pointer" search into picking the nearest one.
        assert!(angular_distance(1.0, 1.0).abs() < 1e-6);
        assert!(angular_distance(-0.75, -0.75 + TAU).abs() < 1e-5);
    }

    #[test]
    fn the_compass_rose_labels_every_thirty_degrees() {
        // Letters on the cardinals, tens of degrees elsewhere — the heading-indicator convention.
        let expected = [
            (0, "N"), (30, "3"), (60, "6"), (90, "E"),
            (120, "12"), (150, "15"), (180, "S"), (210, "21"),
            (240, "24"), (270, "W"), (300, "30"), (330, "33"),
        ];
        for (deg, want) in expected {
            assert_eq!(compass_label(deg), want, "{deg} degrees");
        }
        // Every position the rose actually draws must have a label; a blank would leave a tick
        // with nothing against it, which reads as a rendering fault.
        for deg in (0..360).step_by(30) {
            assert!(!compass_label(deg).is_empty(), "{deg} degrees has no label");
        }
    }

    #[test]
    fn compass_labels_wrap_rather_than_going_blank() {
        assert_eq!(compass_label(360), "N");
        assert_eq!(compass_label(450), "E");
    }

    #[test]
    fn angular_distance_is_symmetric_and_bounded() {
        for (a, b) in [(0.0, PI), (0.1, -0.1), (-2.9, 2.9), (0.0, TAU - 0.2)] {
            let forward = angular_distance(a, b);
            let backward = angular_distance(b, a);
            assert!((forward - backward).abs() < 1e-5, "not symmetric for {a},{b}");
            assert!(
                (0.0..=PI + 1e-5).contains(&forward),
                "{forward} out of range for {a},{b}"
            );
        }
    }

    #[test]
    fn angular_distance_takes_the_short_way_round() {
        // 10 degrees apart across the wrap point, not 350.
        let a = (-5.0f32).to_radians();
        let b = (5.0f32).to_radians();
        assert!((angular_distance(a, b) - 10.0f32.to_radians()).abs() < 1e-5);
        assert!((angular_distance(0.0, PI) - PI).abs() < 1e-5);
    }
}

/// A centred rectangle with a little breathing room, for collision purposes.
fn padded_rect(cx: f32, cy: f32, width: f32, height: f32) -> Rect {
    const PAD: f32 = 3.0;
    Rect {
        x0: cx - width * 0.5 - PAD,
        y0: cy - height * 0.5 - PAD,
        x1: cx + width * 0.5 + PAD,
        y1: cy + height * 0.5 + PAD,
    }
}

fn draw_ownship(
    ui: &Ui,
    canvas: &mut Canvas,
    layout: &Layout,
    state: &AppState,
    view: &ViewState,
) {
    let theme = &ui.theme;
    let (cx, cy) = layout.center;

    canvas.save();
    canvas.translate(cx, cy);
    // In north-up the symbol points along the track; in track-up the world is rotated instead so
    // own-ship always points straight up.
    if view.orientation == Orientation::NorthUp {
        if let Some(track) = state.ownship.track_deg {
            canvas.rotate(track.to_radians());
        }
    }

    let symbol = symbols::ownship(theme.symbol_size);
    // Outline first so own-ship stays visible against a filled ring or, later, weather.
    canvas.stroke_path(
        &symbol,
        &Paint::color(theme.target_outline).with_line_width(theme.line_width * 2.0),
    );
    canvas.fill_path(&symbol, &Paint::color(theme.ownship));
    canvas.restore();
}

/// An axis-aligned box in screen space, used only for tag collision.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn overlaps(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    fn shifted(&self, dx: f32, dy: f32) -> Rect {
        Rect {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
        }
    }
}

/// Greedy tag placement. Returns how many tags had to be dropped.
///
/// Each tag gets a small set of candidate positions, tried in order of preference: right of the
/// symbol, then left, then nudged vertically. First non-colliding candidate wins. A target that
/// cannot fit anywhere keeps its symbol and loses its label — the symbol carries the position and
/// threat colour, which is the safety-critical part; the label is detail.
fn draw_tags(
    ui: &Ui,
    canvas: &mut Canvas,
    layout: &Layout,
    plotted: &[Plotted],
    ring_chrome: Vec<Rect>,
) -> usize {
    let theme = &ui.theme;
    let line_height = theme.font_size_tag * 1.05;
    let gap = theme.symbol_size + 5.0;

    // Reserve the chrome so tags never overprint the status bar or the footer readouts.
    let mut occupied = vec![
        Rect {
            x0: 0.0,
            y0: 0.0,
            x1: layout.width,
            y1: layout.status_bar_height + 2.0,
        },
        Rect {
            x0: 0.0,
            y0: layout.height - layout.footer_height,
            x1: layout.width,
            y1: layout.height,
        },
        // The soft-key strip. Reserved like any other chrome: it is drawn after the plan view,
        // so a tag placed under it is not clipped, it is silently covered — the symbol stays
        // visible while its label vanishes, which is the most confusing possible outcome.
        Rect {
            x0: layout.content_width,
            y0: 0.0,
            x1: layout.width,
            y1: layout.height,
        },
    ];

    // Range labels and the north pointer are already on screen; a tag drawn over them makes both
    // unreadable, and the ring label is how the pilot knows what scale they are looking at.
    occupied.extend(ring_chrome);

    let mut suppressed = 0usize;

    for item in plotted {
        let (x, y) = item.screen;
        let (first, second) = tag_lines(item);

        let mut paint = tag_paint(ui, item);
        let width = [first.as_str(), second.as_str()]
            .iter()
            .map(|line| {
                canvas
                    .measure_text(0.0, 0.0, line, &paint)
                    .map(|m| m.width())
                    .unwrap_or(0.0)
            })
            .fold(0.0f32, f32::max);
        let height = line_height * 2.0;

        // Candidates: (anchor x, alignment, vertical offset).
        let right = (x + gap, Align::Left);
        let left = (x - gap - width, Align::Right);
        let prefer_left = x > layout.content_width * 0.62;
        let sides = if prefer_left { [left, right] } else { [right, left] };

        let mut placed = None;
        'search: for dy in [0.0, line_height, -line_height, line_height * 2.2, -line_height * 2.2] {
            for (anchor_x, align) in sides {
                let x0 = match align {
                    Align::Right => anchor_x,
                    _ => anchor_x,
                };
                let candidate = Rect {
                    x0,
                    y0: y - height * 0.5,
                    x1: x0 + width,
                    y1: y + height * 0.5,
                }
                .shifted(0.0, dy);

                // Off the panel edge is as bad as a collision.
                if candidate.x0 < layout.margin || candidate.x1 > layout.content_width - layout.margin {
                    continue;
                }
                if occupied.iter().any(|r| r.overlaps(&candidate)) {
                    continue;
                }
                placed = Some((candidate, align, dy));
                break 'search;
            }
        }

        let Some((rect, align, dy)) = placed else {
            suppressed += 1;
            continue;
        };
        occupied.push(rect);

        // With Align::Right femtovg anchors at the right edge of the text.
        let text_x = match align {
            Align::Right => rect.x1,
            _ => rect.x0,
        };
        paint.set_text_align(align);
        let _ = canvas.fill_text(text_x, y + dy - line_height * 0.5, &first, &paint);
        let _ = canvas.fill_text(text_x, y + dy + line_height * 0.5, &second, &paint);

        // A leader line when the tag had to move away from its symbol, so it stays obvious which
        // label belongs to which target.
        if dy.abs() > 0.1 {
            let mut leader = Path::new();
            let anchor_x = if matches!(align, Align::Right) {
                rect.x1
            } else {
                rect.x0
            };
            leader.move_to(x, y);
            leader.line_to(anchor_x, y + dy);
            canvas.stroke_path(
                &leader,
                &Paint::color(faded(theme.tag_text, 0.45)).with_line_width(1.0),
            );
        }
    }

    suppressed
}

fn tag_paint(ui: &Ui, item: &Plotted) -> Paint {
    let theme = &ui.theme;
    let mut paint = Paint::color(if item.reckoned.coasting {
        faded(theme.tag_text, 0.6)
    } else {
        theme.tag_text
    });
    paint.set_font(&[ui.font()]);
    paint.set_font_size(theme.font_size_tag);
    paint.set_text_baseline(Baseline::Middle);
    paint
}

fn tag_lines(item: &Plotted) -> (String, String) {
    let identity = item.target.label();
    let altitude = format_relative_altitude(item.assessment.relative_altitude_ft);
    let trend = vertical_trend(item.target.vertical_speed_fpm);
    (identity, format!("{altitude}{trend}"))
}

fn draw_symbol(ui: &Ui, canvas: &mut Canvas, projection: &Projection, item: &Plotted) {
    let theme = &ui.theme;
    let (x, y) = item.screen;

    let base = theme.colour_for(item.assessment.level);
    // Coasting targets are dimmed rather than hidden: the pilot should know something is there,
    // and also that we are not sure exactly where.
    let colour = if item.reckoned.coasting {
        faded(base, 0.55)
    } else {
        base
    };

    let shape = symbols::shape_for_category(item.target.emitter_category);
    let symbol = symbols::build(shape, theme.symbol_size);

    canvas.save();
    canvas.translate(x, y);
    if let Some(track) = item.target.track_deg {
        canvas.rotate(projection.screen_angle_rad(track));
    }

    // Dark outline under every symbol keeps it legible over the ring fill and, in M5, weather.
    canvas.stroke_path(
        &symbol,
        &Paint::color(theme.target_outline).with_line_width(theme.line_width * 2.2),
    );
    if symbols::is_stroke_only(shape) {
        canvas.stroke_path(&symbol, &Paint::color(colour).with_line_width(theme.line_width));
    } else {
        canvas.fill_path(&symbol, &Paint::color(colour));
    }

    // Heading barb, only when there is a velocity solution to point it with.
    if item.target.track_deg.is_some() {
        let barb = symbols::heading_barb(theme.symbol_size, theme.symbol_size * 0.9);
        canvas.stroke_path(
            &barb,
            &Paint::color(colour).with_line_width(theme.line_width),
        );
    }
    canvas.restore();
}

/// Climb/descend arrows. Deliberately insensitive: ADS-B vertical speed is noisy, and a marker
/// that flickers between up and down in level flight is worse than none.
fn vertical_trend(vertical_speed_fpm: Option<i16>) -> &'static str {
    const THRESHOLD_FPM: i16 = 300;
    match vertical_speed_fpm {
        Some(v) if v >= THRESHOLD_FPM => " \u{2191}",
        Some(v) if v <= -THRESHOLD_FPM => " \u{2193}",
        _ => "",
    }
}

/// Own-ship track and ground speed, bottom right; range and orientation, bottom left.
fn draw_footer(
    ui: &Ui,
    canvas: &mut Canvas,
    layout: &Layout,
    state: &AppState,
    view: &ViewState,
) {
    let theme = &ui.theme;
    let baseline = layout.height - layout.footer_height * 0.35;

    let mut value = Paint::color(theme.text_primary);
    value.set_font(&[ui.font()]);
    value.set_font_size(theme.font_size_normal);
    value.set_text_baseline(Baseline::Alphabetic);
    value.set_text_align(Align::Right);

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
        baseline,
        format!("TRK {track}   GS {speed} kt"),
        &value,
    );

    let mut selection = Paint::color(theme.text_secondary);
    selection.set_font(&[ui.font()]);
    selection.set_font_size(theme.font_size_small);
    selection.set_text_baseline(Baseline::Alphabetic);
    selection.set_text_align(Align::Left);
    let _ = canvas.fill_text(
        layout.margin,
        baseline,
        format!(
            "{} nm   {}",
            format_range(view.range_nm),
            view.orientation.label()
        ),
        &selection,
    );
}

/// Shown in place of traffic when there is no own-ship position to plot against.
fn draw_no_position_notice(ui: &Ui, canvas: &mut Canvas, state: &AppState, layout: &Layout) {
    let theme = &ui.theme;
    let (cx, cy) = layout.center;

    // Distinguish "waiting for a fix" from "we have never seen a position at all", which is what
    // a renamed upstream field would look like. Both show nothing; only one is a bug.
    let (headline, detail) = if state.ever_had_position {
        ("GPS FIX LOST", "traffic hidden until position returns")
    } else if state.streams.values().any(|h| h.connected) {
        ("WAITING FOR GPS FIX", "connected to Stratux")
    } else {
        ("NO STRATUX CONNECTION", "retrying")
    };

    let mut title = Paint::color(theme.warning);
    title.set_font(&[ui.font()]);
    title.set_font_size(theme.font_size_large);
    title.set_text_align(Align::Center);
    title.set_text_baseline(Baseline::Bottom);
    let _ = canvas.fill_text(cx, cy, headline, &title);

    let mut sub = Paint::color(theme.text_secondary);
    sub.set_font(&[ui.font()]);
    sub.set_font_size(theme.font_size_small);
    sub.set_text_align(Align::Center);
    sub.set_text_baseline(Baseline::Top);
    let _ = canvas.fill_text(cx, cy + 4.0, detail, &sub);
}

/// Ranges below 1 nm would need a decimal; the selectable set is all integers, so this just avoids
/// printing "10.0".
pub fn format_range(range_nm: f32) -> String {
    if (range_nm - range_nm.round()).abs() < 0.05 {
        format!("{:.0}", range_nm.round())
    } else {
        format!("{range_nm:.1}")
    }
}

/// Compass rose label for a bearing, in the convention every heading indicator uses: letters on
/// the cardinals, tens of degrees elsewhere.
pub(crate) fn compass_label(degrees: u16) -> &'static str {
    match degrees % 360 {
        0 => "N",
        30 => "3",
        60 => "6",
        90 => "E",
        120 => "12",
        150 => "15",
        180 => "S",
        210 => "21",
        240 => "24",
        270 => "W",
        300 => "30",
        330 => "33",
        _ => "",
    }
}
