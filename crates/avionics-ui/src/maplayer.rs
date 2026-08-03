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

/// Runway ticks are drawn at this range and below.
///
/// At 40 nm one pixel is 0.85 nm, so a 5,000 ft runway is 4 px — shorter than the symbol it sits
/// inside, and unable to show an angle. Above this range the symbol says "airport" on its own.
const TICK_RANGE_NM: f32 = 20.0;

/// The most orientations drawn at one field. KORD has four; four ticks at this size is a starburst.
const MAX_TICKS: usize = 2;

/// Longest half-tick, in pixels. At 2 nm a 10,000 ft runway is 154 px, which would reach a third of
/// the way across the panel and read as a boundary rather than as a runway.
const TICK_MAX_PX: f32 = 26.0;

/// Inspect card size. Four lines of small text plus padding, and narrow enough that it occupies
/// the lower-left corner rather than a whole edge — 290 of the 608 px of content area.
const CARD_W: f32 = 290.0;
const CARD_H: f32 = 76.0;

/// Frequencies shown on the card. Four fits the width; the file already sorts them so the four
/// that matter come first.
const MAX_CARD_FREQS: usize = 4;

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

    // Symbols batched the same way: one path for the outlines, one for the centre dots, one for
    // the runway ticks.
    let mut outlines = Path::new();
    let mut centres = Path::new();
    let mut ticks = Path::new();
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
        add_runway_ticks(chart, &mut ticks, airport, projection, view.range_nm, radius);
        drawn += 1;
    }

    canvas.stroke_path(
        &outlines,
        &Paint::color(theme.airport).with_line_width(theme.line_width * 0.85),
    );
    canvas.fill_path(&centres, &Paint::color(theme.airport));
    canvas.stroke_path(
        &ticks,
        &Paint::color(theme.airport).with_line_width(theme.line_width * 0.85),
    );

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

/// Runway alignment ticks through the airport symbol.
///
/// Orientation comes from the runway identifier rather than from the survey heading: the heading
/// column is populated for under a third of runways, the identifier for all of them, and a tick a
/// few pixels long cannot show more than the identifier's 10 degrees anyway.
///
/// Ticks are true-scaled where that reads and floored where it does not. At 40 nm a 5,000 ft
/// runway is 4 px, which is a smudge inside the symbol, so below [`TICK_RANGE_NM`] there are no
/// ticks at all — the symbol alone carries the position, and a tick that cannot show an angle is
/// only ink.
fn add_runway_ticks(
    chart: &Chart,
    path: &mut Path,
    airport: &chart::Airport,
    projection: &Projection,
    range_nm: f32,
    symbol_radius: f32,
) {
    if range_nm > TICK_RANGE_NM || airport.runway_count() == 0 {
        return;
    }
    let (x, y) = projection.project(airport.position);

    // At most two. A field with four orientations drawn in full is a starburst at this size, and
    // the file already sorts them longest first.
    for runway in chart.runways(airport).iter().take(MAX_TICKS) {
        // True length where it is legible, with a floor so a short strip still shows its angle.
        let half = (projection.nm_to_px(runway.length_ft as f32 / 6076.0) * 0.5)
            .max(symbol_radius + 2.0)
            .min(TICK_MAX_PX);
        let angle = projection.screen_angle_rad(runway.heading_deg as f32);
        let (sin, cos) = angle.sin_cos();
        path.move_to(x - sin * half, y + cos * half);
        path.line_to(x + sin * half, y - cos * half);
    }
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

/// Which airport a tap landed on, if any.
///
/// Pure, and free of [`Canvas`], so the whole hit-test is testable without a GPU. Returns the
/// nearest symbol within [`crate::INSPECT_HIT_PX`] rather than the first one found: symbols
/// overlap at close range, and picking the first would make which one you get depend on file
/// order.
pub fn hit_airport(
    chart: &Chart,
    view: &ViewState,
    layout: &Layout,
    projection: &Projection,
    x: f32,
    y: f32,
) -> Option<chart::Airport> {
    if !view.map_layers.shows_airports() {
        return None;
    }
    let bounds = chart::bounds_around(projection.origin(), visible_radius_nm(layout, projection));
    let mut best: Option<(f32, chart::Airport)> = None;

    for airport in chart.airports_in(&bounds, chart::max_tier_for_range(view.range_nm)) {
        let (ax, ay) = projection.project(airport.position);
        if !on_panel(layout, ax, ay) {
            continue;
        }
        let distance = (ax - x).hypot(ay - y);
        if distance > crate::INSPECT_HIT_PX {
            continue;
        }
        if best.as_ref().is_none_or(|(d, _)| distance < *d) {
            best = Some((distance, airport));
        }
    }
    best.map(|(_, airport)| airport)
}

/// Draw the inspect card. Call after the traffic, so a card the pilot asked for is not covered by
/// a tag they did not.
pub fn draw_inspect(
    ui: &Ui,
    canvas: &mut Canvas,
    view: &ViewState,
    layout: &Layout,
    projection: Option<&Projection>,
) {
    let (Some(chart), Some(inspect)) = (ui.chart(), view.inspect) else {
        return;
    };
    let Some(airport) = chart.airport_at(inspect.airport as usize) else {
        return;
    };
    let theme = &ui.theme;

    // Lower-left of the content area. Own-ship is at the centre and the nearest threat is most
    // often ahead of it, so the bottom-left corner is the least costly place to spend.
    let width = CARD_W;
    let height = CARD_H;
    let x0 = layout.content_left();
    let y0 = layout.footer_y0() - layout.margin - height;

    let mut background = Path::new();
    background.rounded_rect(x0, y0, width, height, 3.0);
    canvas.fill_path(&background, &Paint::color(theme.bar_background));
    canvas.stroke_path(
        &background,
        &Paint::color(theme.text_dim).with_line_width(1.0),
    );

    let pad = 7.0;
    let line = |canvas: &mut Canvas, row: f32, size: f32, colour, text: &str, right: bool| {
        let mut paint = Paint::color(colour);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(size);
        paint.set_text_baseline(Baseline::Middle);
        paint.set_text_align(if right { Align::Right } else { Align::Left });
        let x = if right { x0 + width - pad } else { x0 + pad };
        let _ = canvas.fill_text(x, y0 + row, text, &paint);
    };

    // Identifier and, to the right, where it is from here.
    line(canvas, 14.0, theme.font_size_normal, theme.text_primary, airport.label(), false);
    if let Some(projection) = projection {
        let (range, bearing) = projection.range_bearing(airport.position);
        line(
            canvas,
            14.0,
            theme.font_size_small,
            theme.text_secondary,
            &format!("{:03.0}\u{00B0}  {:.1} nm", bearing, range),
            true,
        );
    }

    line(
        canvas,
        30.0,
        theme.font_size_tag,
        theme.text_secondary,
        chart.name(&airport),
        false,
    );

    // Elevation and the longest runway, with its designator when there is one.
    let runways = chart.runways(&airport);
    let mut facts = format!("{} ft", airport.elevation_ft);
    if airport.runway_ft > 0 {
        facts.push_str(&format!("   {} ft", airport.runway_ft));
        if let Some(longest) = runways.first() {
            facts.push_str(&format!(" {}", longest.designator()));
        }
        if airport.lighted() {
            facts.push_str("  LIT");
        }
    }
    line(canvas, 46.0, theme.font_size_tag, theme.text_secondary, &facts, false);

    // Frequencies, most useful first. 82% of fields have none, and saying so is better than a
    // blank line that reads as a display that did not finish drawing.
    // Anything the builder could not name is dropped rather than shown bare. A number with no
    // label on an avionics display invites tuning a radio to it without knowing who answers.
    let freqs: Vec<_> = chart
        .frequencies(&airport)
        .into_iter()
        .filter(|f| !f.kind.label().is_empty())
        .collect();
    let text = if freqs.is_empty() {
        "no published frequency".to_string()
    } else {
        freqs
            .iter()
            .take(MAX_CARD_FREQS)
            .map(|f| format!("{} {}", f.kind.label(), f.mhz_text()))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let colour = if freqs.is_empty() {
        theme.text_dim
    } else {
        theme.text_primary
    };
    line(canvas, 62.0, theme.font_size_tag, colour, &text, false);
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
    use crate::{MapLayers, Theme};

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

    /// The shipped file, or `None` on a checkout without it.
    fn conus() -> Option<Chart> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/conus.chart");
        Chart::load(&path).ok()
    }

    fn view_at(range_nm: f32, layers: MapLayers) -> ViewState {
        ViewState { range_nm, map_layers: layers, ..ViewState::default() }
    }

    /// A projection centred on Morristown, the project's reference position.
    fn morristown(range_nm: f32) -> Projection {
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
    fn a_tap_on_a_symbol_finds_that_airport() {
        let Some(chart) = conus() else { return };
        let l = layout();
        let p = morristown(10.0);
        let view = view_at(10.0, MapLayers::Airports);

        let mmu = chart
            .airports_in(
                &chart::bounds_around(p.origin(), 20.0),
                chart::max_tier_for_range(10.0),
            )
            .into_iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU");
        let (x, y) = p.project(mmu.position);

        let hit = hit_airport(&chart, &view, &l, &p, x, y).expect("a tap on the symbol");
        assert_eq!(hit.label(), "MMU");
        assert_eq!(hit.index, mmu.index);

        // A fingertip is not a pixel: near enough still counts.
        let near = hit_airport(&chart, &view, &l, &p, x + 6.0, y - 6.0).expect("near enough");
        assert_eq!(near.label(), "MMU");
    }

    #[test]
    fn a_tap_on_empty_sky_finds_nothing() {
        // The property that keeps the body effectively inert: a hand steadying itself against the
        // panel lands nowhere near a symbol almost every time, and nothing happens.
        let Some(chart) = conus() else { return };
        let l = layout();
        let p = morristown(10.0);
        let view = view_at(10.0, MapLayers::Airports);

        let mmu = chart
            .airports_in(&chart::bounds_around(p.origin(), 20.0), chart::Tier::Minor)
            .into_iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU");
        let (x, y) = p.project(mmu.position);

        assert!(
            hit_airport(&chart, &view, &l, &p, x + crate::INSPECT_HIT_PX + 4.0, y).is_none(),
            "just outside the hit radius should miss"
        );
    }

    #[test]
    fn nothing_is_tappable_while_the_layer_is_off() {
        // You cannot inspect what is not drawn. Otherwise a pilot who turned the map off would
        // still get cards from invisible symbols.
        let Some(chart) = conus() else { return };
        let l = layout();
        let p = morristown(10.0);
        let mmu = chart
            .airports_in(&chart::bounds_around(p.origin(), 20.0), chart::Tier::Minor)
            .into_iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU");
        let (x, y) = p.project(mmu.position);

        let off = view_at(10.0, MapLayers::Off);
        assert!(hit_airport(&chart, &off, &l, &p, x, y).is_none());

        let on = view_at(10.0, MapLayers::Airports);
        assert!(hit_airport(&chart, &on, &l, &p, x, y).is_some());
    }

    #[test]
    fn an_airport_the_range_declutters_away_is_not_tappable() {
        // The hit test uses the same tier the draw uses, so a symbol that is not on screen at
        // 40 nm cannot be found by tapping where it would have been at 5 nm.
        let Some(chart) = conus() else { return };
        let l = layout();

        // A minor field near Morristown, drawn only at 5 nm and in — and one that is actually on
        // the panel at that range, since the hit test culls off-panel symbols too.
        let p5 = morristown(5.0);
        let minor = chart
            .airports_in(&chart::bounds_around(p5.origin(), 10.0), chart::Tier::Minor)
            .into_iter()
            .find(|a| {
                let (x, y) = p5.project(a.position);
                a.tier == chart::Tier::Minor && on_panel(&l, x, y)
            });
        let Some(minor) = minor else { return };

        let (x, y) = p5.project(minor.position);
        assert!(
            hit_airport(&chart, &view_at(5.0, MapLayers::Airports), &l, &p5, x, y).is_some(),
            "{} should be tappable at 5 nm",
            minor.label()
        );

        let p40 = morristown(40.0);
        let (x, y) = p40.project(minor.position);
        assert!(
            hit_airport(&chart, &view_at(40.0, MapLayers::Airports), &l, &p40, x, y).is_none(),
            "{} is decluttered at 40 nm and must not be tappable",
            minor.label()
        );
    }

    #[test]
    fn overlapping_symbols_resolve_to_the_nearest() {
        // At close range symbols crowd. Returning the first match would make the answer depend on
        // where an airport happens to sit in the file.
        let Some(chart) = conus() else { return };
        let l = layout();
        let p = morristown(20.0);
        let view = view_at(20.0, MapLayers::Airports);

        let airports = chart.airports_in(
            &chart::bounds_around(p.origin(), 40.0),
            chart::max_tier_for_range(20.0),
        );
        // Probe every drawn symbol: each must resolve to itself, not to a neighbour.
        let mut probed = 0usize;
        for airport in &airports {
            let (x, y) = p.project(airport.position);
            if !on_panel(&l, x, y) {
                continue;
            }
            if let Some(hit) = hit_airport(&chart, &view, &l, &p, x, y) {
                let (hx, hy) = p.project(hit.position);
                let mine = 0.0f32;
                let theirs = (hx - x).hypot(hy - y);
                assert!(
                    theirs <= mine + 0.001,
                    "{} resolved to {} which is further away",
                    airport.label(),
                    hit.label()
                );
                probed += 1;
            }
        }
        assert!(probed > 3, "expected several airports near Morristown, probed {probed}");
    }

    #[test]
    fn the_card_fits_inside_the_content_area_and_clears_the_footer() {
        let l = layout();
        let x0 = l.content_left();
        let y0 = l.footer_y0() - l.margin - CARD_H;
        assert!(x0 >= l.content_x0, "card starts left of the content area");
        assert!(x0 + CARD_W <= l.content_x1, "card runs past the page strip");
        assert!(y0 > l.strip_y0(), "card starts above the status bar");
        assert!(y0 + CARD_H < l.footer_y0(), "card overlaps the footer bar");
        // And it leaves most of the plan view alone: own-ship is at the centre.
        assert!(x0 + CARD_W < l.center.0 + l.content_width() * 0.5, "card covers the right half");
    }

    #[test]
    fn runway_ticks_are_dropped_at_ranges_where_they_cannot_show_an_angle() {
        // At 40 nm one pixel is 0.85 nm, so a 5,000 ft runway is 4 px — shorter than the symbol
        // it sits inside. A tick that cannot show an angle is only ink.
        for range in crate::ViewState::RANGES {
            let drawn = range <= TICK_RANGE_NM;
            assert_eq!(drawn, range <= 20.0, "range {range} changed sides of the tick threshold");
        }
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
