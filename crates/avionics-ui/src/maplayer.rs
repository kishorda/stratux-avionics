//! Drawing the airport and airspace layer, underneath everything that moves.
//!
//! # Why this needs no texture, unlike the weather
//!
//! [`crate::nexrad`] composites into a single texture because a full mosaic is on the order of ten
//! thousand rectangles, and ten thousand draw calls will not happen at 30 Hz on a `vc4`. This layer
//! is the opposite shape of problem: the worst view anywhere in the country is 33 airspace polygons
//! and 3,855 vertices, because the geometry was simplified at build time. Paths are the right tool,
//! and building a texture for it would be a lot of machinery earning nothing.
//!
//! What it does borrow from the rose in [`crate::planview`] is batching. femtovg emits a GL draw
//! per `stroke_path`, so the polygons go into one path per class — three strokes, not thirty-three.
//!
//! # Colour, and the one collision worth knowing about
//!
//! The sectional convention is Class B blue, Class C magenta, Class D dashed blue, and it is kept
//! because it is what a pilot already reads. It collides in exactly one place: `nexrad.rs` uses
//! magenta for extreme precipitation. The two are told apart by weight rather than hue — weather is
//! a saturated fill, airspace is a thin desaturated stroke — and the airspace colours are dimmed
//! well below the traffic palette so the whole layer stays behind the things that move.
//!
//! Airports are deliberately *not* given the sectional's blue and magenta. Those are spent on
//! airspace here, and a recessive neutral grey says "background feature" more clearly than a fourth
//! saturated hue would.

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::domain::LatLon;

use crate::chart::{self, Chart, Class, Tier};
use crate::projection::Projection;
use crate::{Layout, MapLayers, Ui, ViewState};

/// Airport labels are drawn at this range and below. Beyond it they lose to the traffic tags they
/// would be competing with, and a label that cannot be read is only ink.
const LABEL_RANGE_NM: f32 = 10.0;

/// Dash pattern for Class D, in pixels. femtovg re-tessellates a dashed path, so this costs a
/// little CPU per frame — affordable for the handful of Class D polygons ever on screen.
const CLASS_D_DASH: [f32; 2] = [7.0, 5.0];

/// What the layer put on screen. Folded into [`crate::FrameStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drawn {
    pub airports: usize,
    pub airspace: usize,
}

/// Draw the layer. Call between the weather underlay and the range rings.
pub fn draw(
    ui: &Ui,
    canvas: &mut Canvas,
    view: &ViewState,
    layout: &Layout,
    projection: &Projection,
) -> Drawn {
    let Some(chart) = ui.chart() else {
        return Drawn::default();
    };
    if view.map_layers == MapLayers::Off {
        return Drawn::default();
    }

    // One box covers both queries, sized to the corners of the content area rather than to the
    // selected range. The rings are inscribed, so the corners of the panel are nearly twice the
    // range out — on the 800x480 panel a 10 nm ring has corners at 19.8 nm. Querying to the range
    // would stop the map at an invisible circle and leave the corners bare, which reads as missing
    // data rather than as a choice.
    let bounds = chart::bounds_around(projection.origin(), visible_radius_nm(layout, projection));

    let mut drawn = Drawn::default();
    if view.map_layers.shows_airspace() {
        drawn.airspace = draw_airspace(ui, canvas, chart, &bounds, projection);
    }
    drawn.airports = draw_airports(ui, canvas, chart, view, layout, &bounds, projection);
    drawn
}

fn draw_airspace(
    ui: &Ui,
    canvas: &mut Canvas,
    chart: &Chart,
    bounds: &chart::Bounds,
    projection: &Projection,
) -> usize {
    let theme = &ui.theme;
    let spaces = chart.airspace_in(bounds);
    if spaces.is_empty() {
        return 0;
    }

    // One path per class, so three strokes carry every boundary on screen.
    let mut paths = [Path::new(), Path::new(), Path::new()];
    for space in &spaces {
        let slot = match space.class {
            Class::B => 0,
            Class::C => 1,
            Class::D => 2,
        };
        for ring in 0..space.ring_count() {
            let mut points = chart.ring(space, ring);
            let Some(first) = points.next() else { continue };
            let (x, y) = projection.project(first);
            paths[slot].move_to(x, y);
            for point in points {
                let (x, y) = projection.project(point);
                paths[slot].line_to(x, y);
            }
            // The closing point was dropped at build time; close the path instead of storing it.
            paths[slot].close();
        }
    }

    // Class B widest and Class D narrowest, so the airspace that matters most is the most visible
    // even before the colour is read. Drawn B first, so a Class D inside a Class B sits on top.
    for (slot, colour, width, dashed) in [
        (0usize, theme.airspace_b, theme.line_width * 1.15, false),
        (1, theme.airspace_c, theme.line_width * 1.0, false),
        (2, theme.airspace_d, theme.line_width * 0.85, true),
    ] {
        let mut paint = Paint::color(colour).with_line_width(width);
        if dashed {
            paint.set_line_dash(&CLASS_D_DASH);
        }
        canvas.stroke_path(&paths[slot], &paint);
    }

    spaces.len()
}

fn draw_airports(
    ui: &Ui,
    canvas: &mut Canvas,
    chart: &Chart,
    view: &ViewState,
    layout: &Layout,
    bounds: &chart::Bounds,
    projection: &Projection,
) -> usize {
    let theme = &ui.theme;
    let airports = chart.airports_in(bounds, chart::max_tier_for_range(view.range_nm));
    if airports.is_empty() {
        return 0;
    }

    // Symbols batched the same way: one path for the outlines, one for the centre dots.
    let mut outlines = Path::new();
    let mut centres = Path::new();
    let mut drawn = 0usize;

    for airport in &airports {
        let (x, y) = projection.project(airport.position);
        if !on_panel(layout, x, y) {
            continue;
        }
        let radius = match airport.tier {
            Tier::Major => 5.0,
            Tier::Paved => 4.0,
            _ => 3.0,
        };
        outlines.circle(x, y, radius);
        // A filled centre marks a hard runway — the distinction that decides whether a field is
        // usable to most of what this display is fitted to.
        if airport.hard_surface() && airport.tier <= Tier::Paved {
            centres.circle(x, y, 1.6);
        }
        drawn += 1;
    }

    canvas.stroke_path(
        &outlines,
        &Paint::color(theme.airport).with_line_width(theme.line_width * 0.85),
    );
    canvas.fill_path(&centres, &Paint::color(theme.airport));

    // Labels last and only close in. They are deliberately *not* registered with the traffic tag
    // placer: a traffic label must win a contested spot, and teaching the placer to route around
    // airports would invert that.
    if view.range_nm <= LABEL_RANGE_NM {
        let mut paint = Paint::color(theme.airport_label);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_tag);
        paint.set_text_align(Align::Center);
        paint.set_text_baseline(Baseline::Top);

        let (own_x, own_y) = layout.center;
        for airport in &airports {
            if airport.tier > Tier::Paved {
                continue;
            }
            let (x, y) = projection.project(airport.position);
            if !on_panel(layout, x, y) {
                continue;
            }
            // An airport under own-ship gets no label. It would be drawn straight through the
            // chevron and read as neither — which is what happened at Broomfield, where the synth
            // session starts on the field and `BJC` came out overprinted by own-ship.
            if (x - own_x).hypot(y - own_y) < theme.symbol_size * 1.8 {
                continue;
            }
            // Below the symbol, as an identifier sits on a sectional, rather than beside it: the
            // horizontal axis is where the traffic tags are already competing for room.
            let _ = canvas.fill_text(x, y + 6.0, airport.label(), &paint);
        }
    }

    drawn
}

/// Whether a projected point is inside the content area, with a little slack so a symbol that
/// straddles the edge is still drawn rather than popping in.
fn on_panel(layout: &Layout, x: f32, y: f32) -> bool {
    const SLACK: f32 = 12.0;
    x >= layout.content_x0 - SLACK
        && x <= layout.content_x1 + SLACK
        && y >= layout.strip_y0() - SLACK
        && y <= layout.strip_y1() + SLACK
}

/// Distance in nautical miles from own-ship to the furthest corner of the content area.
///
/// Derived from the layout rather than scaled off `range_nm` by a constant, so it stays right on a
/// panel of a different shape. On the 800x480 panel the content area is 608x426 px around a centre
/// with a 187.5 px ring, which puts the corners at 1.98 times the selected range.
pub fn visible_radius_nm(layout: &Layout, projection: &Projection) -> f32 {
    let half_diagonal =
        (layout.content_width() * 0.5).hypot(layout.strip_height() * 0.5);
    half_diagonal / projection.px_per_nm().max(f32::EPSILON)
}

/// How far from the position a target airport is, for tests and for future nearest-field logic.
pub fn range_nm(from: LatLon, to: LatLon) -> f32 {
    let north = (to.lat - from.lat) * 60.0;
    let east = (to.lon - from.lon) * 60.0 * from.lat.to_radians().cos().abs();
    ((north * north + east * east) as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::Orientation;
    use crate::Theme;

    fn layout() -> Layout {
        Layout::for_size(800.0, 480.0, &Theme::dark())
    }

    fn projection(range_nm: f32) -> Projection {
        let l = layout();
        Projection::new(
            LatLon::new(40.7784, -74.3343),
            l.center,
            l.outer_radius / range_nm,
            Orientation::NorthUp,
            None,
        )
    }

    #[test]
    fn the_query_box_reaches_the_corners_of_the_screen() {
        // The rings are inscribed, so the corners of the panel are nearly twice the selected range
        // out. A box of `range_nm` — or of `range_nm * sqrt(2)`, which was the first guess — stops
        // the map at an invisible circle and leaves the corners bare, which reads as missing data.
        let l = layout();
        for range in crate::ViewState::RANGES {
            let p = projection(range);
            let radius = visible_radius_nm(&l, &p);
            let bounds = chart::bounds_around(p.origin(), radius);

            let corner_dx = (l.content_x1 - l.center.0).abs();
            let corner_dy = (l.center.1 - l.strip_y0()).abs();
            let corner_nm = corner_dx.hypot(corner_dy) / p.px_per_nm();

            assert!(
                radius >= corner_nm - 0.01,
                "at {range} nm the query reaches {radius:.1} nm, the corner is at {corner_nm:.1}"
            );
            let span_nm = (bounds.lat_max - bounds.lat_min) as f32 / 1e6 * 60.0 / 2.0;
            assert!(span_nm >= corner_nm - 0.05, "the box is tighter than the radius asked for");
        }
    }

    #[test]
    fn the_visible_radius_scales_with_the_range_and_not_with_the_panel_size() {
        // Two panels of different shape must each cover their own corners, and the ratio to the
        // selected range must stay put as the range changes.
        let wide = Layout::for_size(1024.0, 600.0, &Theme::dark());
        let small = layout();
        for l in [wide, small] {
            let mut ratios = Vec::new();
            for range in crate::ViewState::RANGES {
                let p = Projection::new(
                    LatLon::new(40.0, -74.0),
                    l.center,
                    l.outer_radius / range,
                    Orientation::NorthUp,
                    None,
                );
                ratios.push(visible_radius_nm(&l, &p) / range);
            }
            let first = ratios[0];
            for r in &ratios {
                assert!((r - first).abs() < 1e-3, "ratio drifted with range: {ratios:?}");
            }
            assert!(first > 1.0, "the corners are further out than the ring: {first}");
        }
    }

    #[test]
    fn off_panel_symbols_are_skipped_but_edge_ones_survive() {
        let l = layout();
        assert!(on_panel(&l, l.center.0, l.center.1), "the centre is on the panel");
        assert!(on_panel(&l, l.content_x0, l.strip_y0()), "top-left corner");
        assert!(
            on_panel(&l, l.content_x0 - 6.0, l.center.1),
            "a symbol straddling the strip edge is still drawn"
        );
        assert!(!on_panel(&l, l.content_x0 - 40.0, l.center.1), "well under the strip");
        assert!(!on_panel(&l, l.center.0, l.height + 5.0), "below the panel");
    }

    #[test]
    fn range_between_two_known_fields_is_right() {
        // Morristown to Newark is about 17 nm.
        let mmu = LatLon::new(40.799, -74.415);
        let ewr = LatLon::new(40.692, -74.169);
        let d = range_nm(mmu, ewr);
        assert!((d - 13.9).abs() < 1.5, "got {d} nm");
    }

    #[test]
    fn labels_are_confined_to_the_ranges_where_they_can_be_read() {
        // Not a rendering test — a statement of the rule, so a later change to RANGES has to
        // decide deliberately whether the new range gets labels.
        for range in crate::ViewState::RANGES {
            let labelled = range <= LABEL_RANGE_NM;
            assert_eq!(
                labelled,
                range <= 10.0,
                "range {range} nm changed which side of the label threshold it is on"
            );
        }
    }
}
