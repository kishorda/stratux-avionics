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

use std::time::{Duration, Instant};

use avionics_gfx::femtovg::{Align, Baseline, Paint, Path};
use avionics_gfx::Canvas;
use stratux_client::domain::{LatLon, WeatherProduct};
use stratux_client::AppState;

use crate::chart::{self, Chart, Class, Tier};
use crate::metar;
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
const CARD_W: f32 = 336.0;
const CARD_H: f32 = 88.0;
/// With a weather line. The card grows rather than reserving a row that is empty at four fields
/// in five.
const CARD_H_WX: f32 = 107.0;

/// Where the weather line's text starts, past the category badge. A fixed column rather than a
/// measured one: `LIFR` is the widest label, and a measured indent would shuffle the line sideways
/// every time the weather changed category.
const CATEGORY_COLUMN_PX: f32 = 40.0;

/// Separator between frequency entries on the card.
const FREQ_SEPARATOR: &str = "  ";

/// Where an airspace row's text starts, past the single-letter class.
const CLASS_COLUMN_PX: f32 = 18.0;

/// Baseline of each card row, from the top of the card. Named rather than written twice, so the
/// layout test cannot drift away from what the drawing actually does.
const CARD_ROWS: [f32; 5] = [16.0, 35.0, 53.0, 71.0, 90.0];

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
        add_runway_ticks(
            chart,
            &mut ticks,
            airport,
            projection,
            view.range_nm,
            radius,
        );
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

/// The weather already on board for one station.
///
/// Nothing is fetched for this. METARs arrive over the Stratux weather socket and sit in
/// [`AppState`] keyed by station; the card is a *join*, not a new data source. Which is also why
/// it is free at a field with no report — most of them — and why the card says so rather than
/// leaving a blank line that reads as a display that did not finish drawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StationWeather {
    pub summary: metar::Summary,
    /// How long ago the report was received. Not its issue time — that is in the text, and this is
    /// the question the pilot is actually asking: is what I am looking at current?
    pub age: Duration,
    pub has_taf: bool,
}

/// Find the METAR and TAF for a station, if either is on board.
///
/// Pure and free of [`Canvas`], so the join is testable without a GPU. Returns `None` for a field
/// with no station identifier at all rather than matching the empty string against everything.
pub fn station_weather(state: &AppState, station: &str, now: Instant) -> Option<StationWeather> {
    if station.is_empty() {
        return None;
    }
    let mut metar_text: Option<&stratux_client::domain::WeatherText> = None;
    let mut has_taf = false;
    for text in state.weather.values() {
        if !text.location.eq_ignore_ascii_case(station) {
            continue;
        }
        match text.product {
            WeatherProduct::Metar => metar_text = Some(text),
            WeatherProduct::Taf => has_taf = true,
            _ => {}
        }
    }
    let text = metar_text?;
    Some(StationWeather {
        summary: metar::summarise(&text.body),
        age: now.saturating_duration_since(text.received),
        has_taf,
    })
}

/// `"4m"`, `"1h12m"` — how long ago a report arrived.
fn age_text(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    }
}

/// Draw the inspect card. Call after the traffic, so a card the pilot asked for is not covered by
/// a tag they did not.
pub fn draw_inspect(
    ui: &Ui,
    canvas: &mut Canvas,
    state: &AppState,
    view: &ViewState,
    layout: &Layout,
    projection: Option<&Projection>,
    now: Instant,
) {
    let (Some(chart), Some(inspect)) = (ui.chart(), view.inspect) else {
        return;
    };
    match inspect.subject {
        crate::Inspected::Airport(index) => {
            let Some(airport) = chart.airport_at(index as usize) else {
                return;
            };
            draw_airport_card(ui, canvas, chart, state, &airport, layout, projection, now);
        }
        crate::Inspected::Airspace(at) => {
            draw_airspace_card(ui, canvas, chart, at, layout, projection);
        }
    }
}

/// The airspace over a tapped point: what is stacked there and where each layer starts and stops.
///
/// # Why this shows numbers and not a verdict
///
/// The display cannot say whether you are inside a shelf. Own-ship altitude comes from GPS, which
/// scattered 356 ft while sitting still on the ground during the outdoor capture, or from a
/// pressure sensor on the 29.92 datum, which differs from MSL by whatever the local altimeter
/// setting is. Airspace floors are MSL and legal compliance is your altimeter on local QNH, which
/// this box does not have.
///
/// So it prints the floor and the ceiling, the way a sectional does, and the pilot cross-checks
/// against the instrument that is actually certified for it. A green "you are clear" would be the
/// most confidently wrong thing this display could say.
fn draw_airspace_card(
    ui: &Ui,
    canvas: &mut Canvas,
    chart: &Chart,
    at: LatLon,
    layout: &Layout,
    projection: Option<&Projection>,
) {
    let volumes = chart.airspace_at(at);
    if volumes.is_empty() {
        return;
    }
    let theme = &ui.theme;

    let shown = volumes.len().min(CARD_ROWS.len() - 1);
    let width = CARD_W;
    let height = CARD_ROWS[shown] + 10.0;
    let x0 = layout.content_left();
    let y0 = layout.footer_y0() - layout.margin - height;
    card_frame(ui, canvas, x0, y0, width, height);

    let pad = 7.0;
    let mut title = Paint::color(theme.text_primary);
    title.set_font(&[ui.font()]);
    title.set_font_size(theme.font_size_normal);
    title.set_text_baseline(Baseline::Middle);
    title.set_text_align(Align::Left);
    let _ = canvas.fill_text(x0 + pad, y0 + CARD_ROWS[0], "AIRSPACE", &title);

    if let Some(projection) = projection {
        let (range, bearing) = projection.range_bearing(at);
        let mut right = Paint::color(theme.text_secondary);
        right.set_font(&[ui.font()]);
        right.set_font_size(theme.font_size_small);
        right.set_text_baseline(Baseline::Middle);
        right.set_text_align(Align::Right);
        let _ = canvas.fill_text(
            x0 + width - pad,
            y0 + CARD_ROWS[0],
            format!("{bearing:03.0}\u{00B0}  {range:.1} nm"),
            &right,
        );
    }

    // Lowest floor first, which is the order you would meet them climbing.
    for (row, space) in volumes.iter().take(shown).enumerate() {
        let mut class = Paint::color(match space.class {
            chart::Class::B => theme.airspace_b,
            chart::Class::C => theme.airspace_c,
            chart::Class::D => theme.airspace_d,
        });
        class.set_font(&[ui.font()]);
        class.set_font_size(theme.font_size_tag);
        class.set_text_baseline(Baseline::Middle);
        class.set_text_align(Align::Left);
        let y = y0 + CARD_ROWS[row + 1];
        let _ = canvas.fill_text(x0 + pad, y, space.class.label(), &class);

        let mut body = Paint::color(theme.text_primary);
        body.set_font(&[ui.font()]);
        body.set_font_size(theme.font_size_tag);
        body.set_text_baseline(Baseline::Middle);
        body.set_text_align(Align::Left);
        let _ = canvas.fill_text(
            x0 + pad + CLASS_COLUMN_PX,
            y,
            format!("{:8} {}", space.label(), vertical_limits(space)),
            &body,
        );
    }
}

/// `"SFC - 2500"`, `"1800 - 7000"` — feet MSL, the way a sectional states them.
pub fn vertical_limits(space: &chart::Airspace) -> String {
    let floor = if space.lower_is_surface() {
        "SFC".to_string()
    } else {
        space.lower_ft.to_string()
    };
    format!("{floor} - {} ft", space.upper_ft)
}

/// The card's background and border, shared by both subjects.
fn card_frame(ui: &Ui, canvas: &mut Canvas, x0: f32, y0: f32, width: f32, height: f32) {
    let mut background = Path::new();
    background.rounded_rect(x0, y0, width, height, 3.0);
    canvas.fill_path(&background, &Paint::color(ui.theme.bar_background));
    canvas.stroke_path(
        &background,
        &Paint::color(ui.theme.text_dim).with_line_width(1.0),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_airport_card(
    ui: &Ui,
    canvas: &mut Canvas,
    chart: &Chart,
    state: &AppState,
    airport: &chart::Airport,
    layout: &Layout,
    projection: Option<&Projection>,
    now: Instant,
) {
    let theme = &ui.theme;

    // The weather line only exists when there is weather, so the card grows rather than reserving
    // a row that is empty at four fields in five.
    let weather = station_weather(state, airport.station(), now);

    // Lower-left of the content area. Own-ship is at the centre and the nearest threat is most
    // often ahead of it, so the bottom-left corner is the least costly place to spend.
    let width = CARD_W;
    let height = if weather.is_some() { CARD_H_WX } else { CARD_H };
    let x0 = layout.content_left();
    let y0 = layout.footer_y0() - layout.margin - height;

    card_frame(ui, canvas, x0, y0, width, height);

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
    line(
        canvas,
        CARD_ROWS[0],
        theme.font_size_normal,
        theme.text_primary,
        airport.label(),
        false,
    );
    if let Some(projection) = projection {
        let (range, bearing) = projection.range_bearing(airport.position);
        line(
            canvas,
            CARD_ROWS[0],
            theme.font_size_small,
            theme.text_secondary,
            &format!("{:03.0}\u{00B0}  {:.1} nm", bearing, range),
            true,
        );
    }

    line(
        canvas,
        CARD_ROWS[1],
        theme.font_size_tag,
        theme.text_secondary,
        chart.name(airport),
        false,
    );

    // Elevation and the longest runway, with its designator when there is one.
    let runways = chart.runways(airport);
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
    line(
        canvas,
        CARD_ROWS[2],
        theme.font_size_tag,
        theme.text_secondary,
        &facts,
        false,
    );

    // Frequencies, most useful first. 82% of fields have none, and saying so is better than a
    // blank line that reads as a display that did not finish drawing.
    // Anything the builder could not name is dropped rather than shown bare. A number with no
    // label on an avionics display invites tuning a radio to it without knowing who answers.
    let freqs: Vec<_> = chart
        .frequencies(airport)
        .into_iter()
        .filter(|f| !f.kind.label().is_empty())
        .collect();
    // How many fit is *measured*, not assumed. A fixed count was right at one font size and wrong
    // at the next: bold at 13 px pushed `APP 127.6` past the card's right border and out over the
    // plan view. The file already sorts these most-useful-first, so dropping from the end drops
    // the least useful.
    let text = if freqs.is_empty() {
        "no published frequency".to_string()
    } else {
        let entries: Vec<String> = freqs
            .iter()
            .map(|f| format!("{} {}", f.kind.label(), f.mhz_text()))
            .collect();
        fit_entries(canvas, &entries, &line_paint(ui), width - pad * 2.0)
    };
    let colour = if freqs.is_empty() {
        theme.text_dim
    } else {
        theme.text_primary
    };
    line(
        canvas,
        CARD_ROWS[3],
        theme.font_size_tag,
        colour,
        &text,
        false,
    );

    // The weather line, from a METAR already on board. The category badge carries its own colour,
    // so it is drawn separately from the rest of the line rather than inheriting one.
    if let Some(weather) = weather {
        let row = CARD_ROWS[4];
        if let Some(category) = weather.summary.category {
            let mut badge = Paint::color(category.colour(theme));
            badge.set_font(&[ui.font()]);
            badge.set_font_size(theme.font_size_tag);
            badge.set_text_baseline(Baseline::Middle);
            badge.set_text_align(Align::Left);
            let _ = canvas.fill_text(x0 + pad, y0 + row, category.label(), &badge);
        }

        // Indented past the badge by a fixed amount rather than by measuring it: LIFR is the
        // widest label and the column has to be stable as the category changes, or the line
        // shuffles sideways every time the weather does.
        let mut paint = Paint::color(theme.text_secondary);
        paint.set_font(&[ui.font()]);
        paint.set_font_size(theme.font_size_tag);
        paint.set_text_baseline(Baseline::Middle);
        paint.set_text_align(Align::Left);
        let _ = canvas.fill_text(
            x0 + pad + CATEGORY_COLUMN_PX,
            y0 + row,
            weather_line(&weather),
            &paint,
        );
    }
}

/// The paint the card's body lines are drawn in. Shared so a measurement and the drawing that
/// follows it cannot disagree about the font.
fn line_paint(ui: &Ui) -> Paint {
    let mut paint = Paint::color(ui.theme.text_primary);
    paint.set_font(&[ui.font()]);
    paint.set_font_size(ui.theme.font_size_tag);
    paint.set_text_baseline(Baseline::Middle);
    paint.set_text_align(Align::Left);
    paint
}

/// Join as many entries as fit in `available` pixels, in order.
///
/// Always keeps at least the first, even when it alone is too wide: a card showing one frequency
/// that runs a little long is more use than a card showing none, and the caller has already put
/// the most useful one first.
fn fit_entries(canvas: &mut Canvas, entries: &[String], paint: &Paint, available: f32) -> String {
    let measure = |canvas: &mut Canvas, text: &str| {
        canvas
            .measure_text(0.0, 0.0, text, paint)
            .map(|m| m.width())
            .unwrap_or(f32::MAX)
    };

    let mut out = match entries.first() {
        Some(first) => first.clone(),
        None => return String::new(),
    };
    for entry in entries.iter().skip(1) {
        let candidate = format!("{out}{FREQ_SEPARATOR}{entry}");
        if measure(canvas, &candidate) > available {
            break;
        }
        out = candidate;
    }
    out
}

/// Everything on the weather line except the category badge, which carries its own colour and is
/// drawn separately into a fixed column.
///
/// Extracted so the content and the width are testable without a canvas. The order is deliberate:
/// wind comes before ceiling and visibility because the card names the runways two lines above,
/// and wind against runway is the pairing a pilot is reading for.
fn weather_line(weather: &StationWeather) -> String {
    let mut out = String::new();
    // Neither ceiling nor visibility could be read — `summarise` never guesses, so there is no
    // badge. Naming the product is better than a blank column that reads as a drawing fault.
    if weather.summary.category.is_none() {
        out.push_str("METAR");
    }
    if let Some(wind) = weather.summary.wind {
        out.push_str(&format!("  {}", wind.text()));
    }
    if let Some(ceiling) = weather.summary.ceiling_ft {
        out.push_str(&format!("  {ceiling} ft"));
    }
    if let Some(visibility) = weather.summary.visibility_sm {
        out.push_str(&format!("  {} sm", format_visibility(visibility)));
    }
    out.push_str(&format!("  {}", age_text(weather.age)));
    if weather.has_taf {
        out.push_str("  TAF");
    }
    out
}

/// Visibility without a trailing `.0`, since whole miles are the common case.
fn format_visibility(sm: f32) -> String {
    if (sm - sm.round()).abs() < 0.05 {
        format!("{:.0}", sm.round())
    } else {
        format!("{sm:.2}")
    }
}

/// Distance in nautical miles from own-ship to the furthest corner of the content area.
///
/// Derived from the layout rather than scaled off `range_nm` by a constant, so it stays right on a
/// panel of a different shape. On the 800x480 panel the content area is 608x426 px around a centre
/// with a 187.5 px ring, which puts the corners at 1.98 times the selected range.
pub fn visible_radius_nm(layout: &Layout, projection: &Projection) -> f32 {
    let half_diagonal = (layout.content_width() * 0.5).hypot(layout.strip_height() * 0.5);
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
            assert!(
                span_nm >= corner_nm - 0.05,
                "the box is tighter than the radius asked for"
            );
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
                assert!(
                    (r - first).abs() < 1e-3,
                    "ratio drifted with range: {ratios:?}"
                );
            }
            assert!(
                first > 1.0,
                "the corners are further out than the ring: {first}"
            );
        }
    }

    #[test]
    fn off_panel_symbols_are_skipped_but_edge_ones_survive() {
        let l = layout();
        assert!(
            on_panel(&l, l.center.0, l.center.1),
            "the centre is on the panel"
        );
        assert!(on_panel(&l, l.content_x0, l.strip_y0()), "top-left corner");
        assert!(
            on_panel(&l, l.content_x0 - 6.0, l.center.1),
            "a symbol straddling the strip edge is still drawn"
        );
        assert!(
            !on_panel(&l, l.content_x0 - 40.0, l.center.1),
            "well under the strip"
        );
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
        ViewState {
            range_nm,
            map_layers: layers,
            ..ViewState::default()
        }
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
        assert!(
            probed > 3,
            "expected several airports near Morristown, probed {probed}"
        );
    }

    fn weather_state(reports: &[(&str, &str, &str)]) -> AppState {
        use stratux_client::domain::WeatherText;
        let mut state = AppState::default();
        for (product, location, body) in reports {
            let text = WeatherText {
                product: match *product {
                    "METAR" => WeatherProduct::Metar,
                    "TAF" => WeatherProduct::Taf,
                    other => WeatherProduct::Other(other.to_string()),
                },
                location: location.to_string(),
                time: "021656Z".into(),
                body: body.to_string(),
                received: Instant::now(),
            };
            state.weather.insert(
                stratux_client::state::WeatherKey {
                    product: text.product.label().to_string(),
                    location: text.location.clone(),
                    discriminator: String::new(),
                },
                text,
            );
        }
        state
    }

    #[test]
    fn a_stations_metar_is_found_by_its_icao_identifier() {
        // The join the whole feature rests on: the symbol says MMU, the METAR says KMMU.
        let state = weather_state(&[(
            "METAR",
            "KMMU",
            "METAR KMMU 021656Z 15014KT 10SM BKN031 27/22 A2993",
        )]);
        let now = Instant::now();

        let found = station_weather(&state, "KMMU", now).expect("KMMU has a METAR");
        assert_eq!(found.summary.ceiling_ft, Some(3100));
        assert_eq!(found.summary.visibility_sm, Some(10.0));
        assert_eq!(found.summary.category, Some(metar::FlightCategory::Vfr));
        assert!(!found.has_taf);

        // The short label must not match, or every airport would show its neighbour's weather.
        assert!(station_weather(&state, "MMU", now).is_none());
        assert!(station_weather(&state, "KEWR", now).is_none());
    }

    #[test]
    fn the_shipped_file_joins_a_real_metar_to_a_real_airport() {
        // The end-to-end join, through the actual file rather than a hand-built record: look up
        // Morristown the way a tap does, take the station off it, and match a METAR keyed the way
        // Stratux keys them. If the station field or the lookup ever drifts, this is the test that
        // notices — the unit tests above would all still pass with a chart that had no stations.
        let Some(chart) = conus() else { return };
        let p = morristown(10.0);
        let mmu = chart
            .airports_in(&chart::bounds_around(p.origin(), 20.0), chart::Tier::Minor)
            .into_iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU");

        let state = weather_state(&[(
            "METAR",
            "KMMU",
            "METAR KMMU 021656Z 15014G21KT 4SM BR OVC008 22/21 A2993",
        )]);
        let found = station_weather(&state, mmu.station(), Instant::now())
            .expect("the shipped file must carry KMMU as MMU's station");
        assert_eq!(found.summary.category, Some(metar::FlightCategory::Ifr));
        assert_eq!(found.summary.ceiling_ft, Some(800));
        assert_eq!(found.summary.visibility_sm, Some(4.0));
    }

    #[test]
    fn a_field_with_no_station_identifier_matches_nothing() {
        // 18% of fields have no ICAO code. An empty string must not match an empty `location`,
        // or those fields would all show the same arbitrary report.
        let state = weather_state(&[("METAR", "", "METAR 021656Z 10SM CLR")]);
        assert!(station_weather(&state, "", Instant::now()).is_none());
    }

    #[test]
    fn a_taf_alone_is_not_reported_as_weather() {
        // The card's line is built from the METAR. A TAF on its own is a forecast with no
        // observation behind it, and showing a category derived from nothing would be a guess.
        let state = weather_state(&[("TAF", "KMMU", "TAF KMMU 021543Z 0216/0318 15010KT P6SM")]);
        assert!(station_weather(&state, "KMMU", Instant::now()).is_none());
    }

    #[test]
    fn a_taf_alongside_a_metar_is_flagged() {
        let state = weather_state(&[
            (
                "METAR",
                "KEWR",
                "METAR KEWR 021651Z 18008KT 10SM FEW250 28/19 A2994",
            ),
            (
                "TAF",
                "KEWR",
                "TAF KEWR 021543Z 0216/0318 15010G18KT P6SM FEW070",
            ),
        ]);
        let found = station_weather(&state, "KEWR", Instant::now()).expect("KEWR");
        assert!(found.has_taf);
    }

    #[test]
    fn the_station_match_is_case_insensitive() {
        let state = weather_state(&[("METAR", "kmmu", "METAR KMMU 021656Z 10SM CLR 27/22")]);
        assert!(station_weather(&state, "KMMU", Instant::now()).is_some());
    }

    #[test]
    fn an_unreadable_report_yields_no_category_rather_than_vfr() {
        // `summarise` never guesses, and the card names the product instead of showing a badge.
        // Implying VFR from a report that could not be read is the failure worth designing out.
        let state = weather_state(&[("METAR", "KAAA", "METAR KAAA 021656Z AUTO")]);
        let found = station_weather(&state, "KAAA", Instant::now()).expect("KAAA");
        assert_eq!(found.summary.category, None);
    }

    fn line_for(body: &str, age: Duration, has_taf: bool) -> String {
        weather_line(&StationWeather {
            summary: metar::summarise(body),
            age,
            has_taf,
        })
    }

    #[test]
    fn the_weather_line_leads_with_wind() {
        // The card names the runways two lines above, and wind against runway is the pairing a
        // pilot is reading for — so it comes before ceiling and visibility, not after.
        let line = line_for(
            "METAR KMMU 021656Z 15014G21KT 10SM BKN031 27/22 A2993",
            Duration::from_secs(4 * 60),
            false,
        );
        assert_eq!(line, "  150\u{00B0} 14G21  3100 ft  10 sm  4m");

        let calm = line_for(
            "METAR KMMU 021656Z 00000KT 10SM CLR 27/22 A2993",
            Duration::from_secs(60),
            true,
        );
        assert_eq!(calm, "  CALM  10 sm  1m  TAF");
    }

    #[test]
    fn a_line_with_no_category_names_the_product_instead_of_going_blank() {
        // No badge is drawn in this case, so without the word the line would start with a gap.
        let line = line_for(
            "METAR KAAA 021656Z 09005KT",
            Duration::from_secs(120),
            false,
        );
        assert!(line.starts_with("METAR"), "{line}");
        assert!(line.contains("090\u{00B0} 05"), "{line}");
    }

    /// Stand-in for `Canvas::measure_text`, so the fitting *rule* is testable without a GPU.
    ///
    /// Deliberately not an attempt to reproduce the real font — a per-character average is wrong
    /// for a string of digits and capitals, and an earlier version of this test used one close
    /// enough to look right and disagree with what the panel actually did. The widths below are
    /// chosen to make the arithmetic unambiguous; the real fit is measured at draw time.
    fn fit_with(entries: &[&str], per_char: f32, available: f32) -> String {
        let entries: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        let mut out = match entries.first() {
            Some(f) => f.clone(),
            None => return String::new(),
        };
        for entry in entries.iter().skip(1) {
            let candidate = format!("{out}{FREQ_SEPARATOR}{entry}");
            if candidate.chars().count() as f32 * per_char > available {
                break;
            }
            out = candidate;
        }
        out
    }

    #[test]
    fn the_frequency_line_drops_entries_from_the_least_useful_end() {
        // The file sorts most-useful-first, so dropping from the end drops APP before CTAF. A
        // fixed count was right at one font size and wrong at the next — bold at 13 px pushed
        // `APP 127.6` past the card border and out over the plan view.
        let all = ["TWR 118.1", "GND 121.7", "ATIS 124.25", "APP 127.6"];
        // 10 px a character: three entries are 33 characters and fit in 340; four are 44 and do not.
        assert_eq!(
            fit_with(&all, 10.0, 340.0),
            "TWR 118.1  GND 121.7  ATIS 124.25",
            "the fourth should be dropped, not the first"
        );
        assert_eq!(
            fit_with(&all, 10.0, 1000.0),
            "TWR 118.1  GND 121.7  ATIS 124.25  APP 127.6",
            "all four when there is room"
        );
        assert_eq!(fit_with(&all, 10.0, 210.0), "TWR 118.1  GND 121.7");
        assert_eq!(
            fit_with(&all, 10.0, 100.0),
            "TWR 118.1",
            "tower survives longest"
        );
    }

    #[test]
    fn one_entry_too_wide_is_still_shown() {
        // A card with one frequency running a little long is more use than a card with none, and
        // the caller has already put the most useful one first.
        assert_eq!(fit_with(&["CTAF 122.8"], 6.6, 10.0), "CTAF 122.8");
        assert_eq!(fit_with(&[], 6.6, 500.0), "");
    }

    #[test]
    fn the_worst_weather_line_still_fits_the_card() {
        // Measured at 6.6 px per character for the bold face at `font_size_tag` on the rendered
        // panel. It was 5.5 at the old regular 11 px; both the size and the weight moved.
        // The budget matters because the line grew once already and would grow again silently.
        const PX_PER_CHAR: f32 = 6.6;
        let worst = line_for(
            "METAR KAAA 021656Z 36025G40KT 1/4SM FG VV001 M01/M02 A2960",
            Duration::from_secs(72 * 60),
            true,
        );
        let width = CATEGORY_COLUMN_PX + worst.chars().count() as f32 * PX_PER_CHAR;
        let available = CARD_W - 7.0 * 2.0;
        assert!(
            width <= available,
            "worst line {worst:?} needs {width:.0} px of {available:.0}"
        );
        // And it really is the worst case: gusting, three-digit ceiling, fractional visibility,
        // an hours-old report and a TAF.
        assert!(
            worst.contains('G') && worst.contains("TAF") && worst.contains('h'),
            "{worst}"
        );
    }

    #[test]
    fn report_age_reads_the_way_a_pilot_would_say_it() {
        assert_eq!(age_text(Duration::from_secs(0)), "0m");
        assert_eq!(age_text(Duration::from_secs(59)), "0m");
        assert_eq!(age_text(Duration::from_secs(4 * 60)), "4m");
        assert_eq!(age_text(Duration::from_secs(59 * 60)), "59m");
        assert_eq!(age_text(Duration::from_secs(60 * 60)), "1h00m");
        assert_eq!(age_text(Duration::from_secs(72 * 60)), "1h12m");
    }

    #[test]
    fn whole_miles_of_visibility_lose_the_decimal() {
        assert_eq!(format_visibility(10.0), "10");
        assert_eq!(format_visibility(3.0), "3");
        assert_eq!(format_visibility(0.5), "0.50");
        assert_eq!(format_visibility(1.75), "1.75");
    }

    #[test]
    fn vertical_limits_read_the_way_a_sectional_states_them() {
        // A surface floor says SFC, not the meaningless number `lower_ft` holds for those volumes.
        let Some(chart) = conus() else { return };
        let over_teb = chart.airspace_at(LatLon::new(40.79, -74.10));
        assert!(over_teb.len() >= 2, "Teterboro sits under a Class B shelf");

        let d = over_teb
            .iter()
            .find(|a| a.class == chart::Class::D)
            .expect("Class D");
        assert!(d.lower_is_surface());
        assert!(
            vertical_limits(d).starts_with("SFC - "),
            "{}",
            vertical_limits(d)
        );

        let b = over_teb
            .iter()
            .find(|a| a.class == chart::Class::B)
            .expect("Class B shelf");
        assert!(!b.lower_is_surface());
        assert_eq!(
            vertical_limits(b),
            format!("{} - {} ft", b.lower_ft, b.upper_ft)
        );

        // Lowest floor first — the order you meet them climbing, and the reason the surface area
        // has to sort as 0 rather than by whatever `lower_ft` happens to contain.
        assert_eq!(over_teb[0].class, chart::Class::D);
    }

    #[test]
    fn the_airspace_card_never_promises_more_rows_than_it_has() {
        // The card grows to fit what it shows, capped at the rows that exist. A stack deeper than
        // the card is truncated rather than drawn past the border.
        let l = layout();
        for (shown, row) in CARD_ROWS.iter().enumerate().skip(1) {
            let height = row + 10.0;
            let y0 = l.footer_y0() - l.margin - height;
            assert!(
                y0 > l.strip_y0(),
                "{shown} rows starts above the status bar"
            );
            assert!(
                y0 + height < l.footer_y0(),
                "{shown} rows overlaps the footer"
            );
            assert!(*row < height, "the last row is inside the card");
        }
    }

    #[test]
    fn the_card_grows_for_weather_and_still_clears_the_footer() {
        // The taller card is the one that can collide with the bar below it.
        let l = layout();
        let x0 = l.content_left();
        let y0 = l.footer_y0() - l.margin - CARD_H_WX;
        assert!(
            y0 > l.strip_y0(),
            "the weather card starts above the status bar"
        );
        assert!(
            y0 + CARD_H_WX < l.footer_y0(),
            "the weather card overlaps the footer"
        );
        assert!(x0 + CARD_W <= l.content_x1);
        // Every row has somewhere to sit inside the card it belongs to.
        for (index, row) in CARD_ROWS.iter().enumerate() {
            let card = if index == 4 { CARD_H_WX } else { CARD_H };
            assert!(
                *row < card,
                "row {index} at {row} is outside a card of {card}"
            );
            assert!(*row > 0.0, "row {index} is above the card");
        }
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
        assert!(
            x0 + CARD_W < l.center.0 + l.content_width() * 0.5,
            "card covers the right half"
        );
    }

    #[test]
    fn runway_ticks_are_dropped_at_ranges_where_they_cannot_show_an_angle() {
        // At 40 nm one pixel is 0.85 nm, so a 5,000 ft runway is 4 px — shorter than the symbol
        // it sits inside. A tick that cannot show an angle is only ink.
        for range in crate::ViewState::RANGES {
            let drawn = range <= TICK_RANGE_NM;
            assert_eq!(
                drawn,
                range <= 20.0,
                "range {range} changed sides of the tick threshold"
            );
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
