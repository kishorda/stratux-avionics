//! Tests for the plan view's pure logic: projection, dead reckoning, threat tiers, view state.
//!
//! Nothing here touches a GPU. Drawing is verified by eye through the `avionics --offscreen`
//! filmstrip; what is tested here is the arithmetic that decides *where* things go and *what
//! colour* they are, because those are the parts that can be quietly wrong while still producing
//! a plausible-looking picture.

use std::time::{Duration, Instant};

use avionics_ui::projection::{advance, Orientation, Projection};
use avionics_ui::reckon::{reckon, reckon_ownship, ReckonConfig};
use avionics_ui::threat::{assess, format_relative_altitude, ThreatConfig, ThreatLevel};
use avionics_ui::ViewState;
use stratux_client::domain::{LatLon, Target, TargetType, TrafficSource};

const ORIGIN: LatLon = LatLon {
    lat: 39.9088,
    lon: -105.1172,
};
const CENTER: (f32, f32) = (400.0, 250.0);
/// 10 px per nautical mile: a 20 nm ring in 200 px.
const PX_PER_NM: f32 = 10.0;

fn target(position: Option<LatLon>) -> Target {
    Target {
        icao: 0xABCDEF,
        identity: Some("N123AB".into()),
        position,
        altitude_ft: Some(8000),
        altitude_is_gnss: false,
        on_ground: false,
        track_deg: Some(90.0),
        ground_speed_kt: Some(120),
        vertical_speed_fpm: Some(0),
        emitter_category: 1,
        target_type: TargetType::Adsb,
        source: TrafficSource::Es1090,
        signal_level_db: -20.0,
        squawk: None,
        extrapolated: false,
        age_s: 0.5,
        age_last_alt_s: 0.5,
        reported_bearing_deg: None,
        reported_distance_m: None,
        received: Instant::now(),
    }
}

/// A position `range_nm` away from ORIGIN on the given true bearing.
fn at_bearing(bearing_deg: f64, range_nm: f64) -> LatLon {
    advance(ORIGIN, bearing_deg, range_nm * 3600.0, 1.0)
}

// --- projection ---------------------------------------------------------------------------

#[test]
fn north_up_puts_north_at_the_top_and_east_to_the_right() {
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::NorthUp, Some(43.0));

    let (nx, ny) = p.project(at_bearing(0.0, 5.0));
    assert!((nx - CENTER.0).abs() < 1.0, "due north should be directly above");
    assert!(ny < CENTER.1 - 40.0, "due north should be above centre");

    let (ex, ey) = p.project(at_bearing(90.0, 5.0));
    assert!(ex > CENTER.0 + 40.0, "due east should be to the right");
    assert!((ey - CENTER.1).abs() < 1.0, "due east should be level");

    // In north-up the own-ship track must not rotate the world.
    assert_eq!(p.rotation_deg(), 0.0);
}

#[test]
fn track_up_puts_whatever_is_ahead_at_the_top() {
    // Own-ship tracking 043; a target dead ahead is on bearing 043 and must appear straight up.
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::TrackUp, Some(43.0));
    let (x, y) = p.project(at_bearing(43.0, 5.0));

    assert!((x - CENTER.0).abs() < 1.5, "target ahead should be directly above, x={x}");
    assert!(y < CENTER.1 - 40.0, "target ahead should be above centre");

    // And something behind must appear below.
    let (_, behind_y) = p.project(at_bearing(43.0 + 180.0, 5.0));
    assert!(behind_y > CENTER.1 + 40.0);
}

#[test]
fn track_up_falls_back_to_north_up_without_a_track() {
    // Stationary on the ramp there is no usable track. Rotating by a stale or zero heading would
    // silently mislabel every bearing on screen, so the projection must decline to rotate.
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::TrackUp, None);
    assert_eq!(p.rotation_deg(), 0.0);

    let (x, y) = p.project(at_bearing(0.0, 5.0));
    assert!((x - CENTER.0).abs() < 1.0);
    assert!(y < CENTER.1);
}

#[test]
fn range_and_bearing_round_trip() {
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::NorthUp, None);
    for (bearing, range) in [(0.0, 1.0), (90.0, 7.5), (217.0, 18.0), (359.0, 40.0)] {
        let (got_range, got_bearing) = p.range_bearing(at_bearing(bearing, range));
        assert!(
            (got_range - range as f32).abs() < 0.02,
            "range {range} -> {got_range}"
        );
        let error = ((got_bearing - bearing as f32 + 540.0) % 360.0 - 180.0).abs();
        assert!(error < 0.2, "bearing {bearing} -> {got_bearing}");
    }
}

#[test]
fn scale_is_linear_in_range() {
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::NorthUp, None);
    let near = p.project(at_bearing(0.0, 5.0));
    let far = p.project(at_bearing(0.0, 10.0));
    let near_px = CENTER.1 - near.1;
    let far_px = CENTER.1 - far.1;
    assert!(
        (far_px / near_px - 2.0).abs() < 0.02,
        "doubling range should double pixel offset: {near_px} vs {far_px}"
    );
    assert!((p.nm_to_px(3.0) - 30.0).abs() < 1e-4);
}

#[test]
fn rotation_is_consistent_between_positions_and_screen_angles() {
    // Heading barbs are drawn with screen_angle_rad while positions come from project(); if the
    // two disagree, every barb points somewhere plausible but wrong.
    let p = Projection::new(ORIGIN, CENTER, PX_PER_NM, Orientation::TrackUp, Some(120.0));
    for bearing in [0.0f32, 45.0, 137.0, 250.0, 355.0] {
        let (x, y) = p.project(at_bearing(bearing as f64, 6.0));
        let angle = p.screen_angle_rad(bearing);
        let expected_x = CENTER.0 + angle.sin() * 6.0 * PX_PER_NM;
        let expected_y = CENTER.1 - angle.cos() * 6.0 * PX_PER_NM;
        assert!(
            (x - expected_x).abs() < 1.5 && (y - expected_y).abs() < 1.5,
            "bearing {bearing}: project=({x:.1},{y:.1}) screen_angle=({expected_x:.1},{expected_y:.1})"
        );
    }
}

#[test]
fn advance_moves_the_expected_distance_in_the_expected_direction() {
    // 120 kt for 30 s is 1 nm.
    let north = advance(ORIGIN, 0.0, 120.0, 30.0);
    assert!(north.lat > ORIGIN.lat);
    assert!((north.lon - ORIGIN.lon).abs() < 1e-9);
    assert!(((north.lat - ORIGIN.lat) * 60.0 - 1.0).abs() < 1e-6);

    let east = advance(ORIGIN, 90.0, 120.0, 30.0);
    assert!(east.lon > ORIGIN.lon);
    assert!((east.lat - ORIGIN.lat).abs() < 1e-9);
}

// --- dead reckoning -----------------------------------------------------------------------

#[test]
fn targets_are_extrapolated_along_their_track() {
    let config = ReckonConfig::default();
    let mut t = target(Some(ORIGIN));
    t.track_deg = Some(90.0);
    t.ground_speed_kt = Some(360.0 as u16);

    let now = t.received + Duration::from_secs(1);
    let result = reckon(&t, now, &config).expect("positional target");

    // 360 kt for 1 s is 0.1 nm east.
    assert!(result.position.lon > ORIGIN.lon);
    assert!((result.extrapolated_s - 1.0).abs() < 1e-6);
    assert!(!result.coasting);
}

#[test]
fn extrapolation_is_capped_and_the_target_marked_coasting() {
    // Past the cap the last real position is drawn. A confident symbol miles from the aircraft is
    // worse than an obviously stale one.
    let config = ReckonConfig::default();
    let mut t = target(Some(ORIGIN));
    t.ground_speed_kt = Some(300);

    let now = t.received + config.max_extrapolation + Duration::from_secs(5);
    let result = reckon(&t, now, &config).expect("positional target");

    assert!(
        (result.extrapolated_s - config.max_extrapolation.as_secs_f64()).abs() < 1e-6,
        "extrapolation should stop at the cap, got {}",
        result.extrapolated_s
    );
    assert!(result.coasting, "hitting the cap must mark the target coasting");
}

#[test]
fn targets_without_a_velocity_solution_are_not_extrapolated() {
    let config = ReckonConfig::default();
    let mut t = target(Some(ORIGIN));
    t.track_deg = None;
    t.ground_speed_kt = None;

    let result = reckon(&t, t.received + Duration::from_secs(2), &config).unwrap();
    assert_eq!(result.position, ORIGIN);
    assert_eq!(result.extrapolated_s, 0.0);
}

#[test]
fn a_stale_fix_freezes_rather_than_projecting_forward() {
    let config = ReckonConfig::default();
    let mut t = target(Some(ORIGIN));
    t.age_s = config.max_fix_age_s + 1.0;

    let result = reckon(&t, t.received + Duration::from_secs(1), &config).unwrap();
    assert_eq!(
        result.position, ORIGIN,
        "don't project forward from a position we already distrust"
    );
    assert!(result.coasting);
}

#[test]
fn stratux_side_extrapolation_is_surfaced_as_coasting() {
    let config = ReckonConfig::default();
    let mut t = target(Some(ORIGIN));
    t.extrapolated = true;

    let result = reckon(&t, t.received, &config).unwrap();
    assert!(result.coasting);
}

#[test]
fn targets_without_a_position_are_not_plotted() {
    let config = ReckonConfig::default();
    assert!(reckon(&target(None), Instant::now(), &config).is_none());
}

#[test]
fn own_ship_is_extrapolated_the_same_way_as_traffic() {
    // If traffic is extrapolated and own-ship is not, every relative position is wrong by
    // own-ship's movement during the gap.
    let config = ReckonConfig::default();
    let received = Instant::now();
    let moved = reckon_ownship(
        ORIGIN,
        Some(0.0),
        Some(120.0),
        Some(received),
        received + Duration::from_secs(30),
        &config,
    );
    // Capped at max_extrapolation, so 3 s at 120 kt = 0.1 nm, not 30 s worth.
    let travelled_nm = (moved.lat - ORIGIN.lat) * 60.0;
    let expected = 120.0 * config.max_extrapolation.as_secs_f64() / 3600.0;
    assert!((travelled_nm - expected).abs() < 1e-6, "moved {travelled_nm} nm");

    // With no track or speed it must not move at all.
    let still = reckon_ownship(ORIGIN, None, None, Some(received), received, &config);
    assert_eq!(still, ORIGIN);
}

// --- threat classification ----------------------------------------------------------------

#[test]
fn a_close_co_altitude_target_is_an_alert() {
    let config = ThreatConfig::default();
    let mut t = target(Some(ORIGIN));
    t.altitude_ft = Some(8200);

    let assessment = assess(&t, 2.0, Some(8000.0), &config);
    assert_eq!(assessment.level, ThreatLevel::Alert);
    assert_eq!(assessment.relative_altitude_ft, Some(200.0));
}

#[test]
fn tiers_require_being_inside_both_range_and_altitude() {
    let config = ThreatConfig::default();
    let mut t = target(Some(ORIGIN));

    // Close but well above: not an alert.
    t.altitude_ft = Some(8000 + 5000);
    assert_eq!(assess(&t, 1.0, Some(8000.0), &config).level, ThreatLevel::Normal);

    // Co-altitude but far away: not an alert.
    t.altitude_ft = Some(8000);
    assert_eq!(
        assess(&t, 30.0, Some(8000.0), &config).level,
        ThreatLevel::Normal
    );

    // Inside the advisory box but outside the alert box.
    t.altitude_ft = Some(8000 + 1000);
    assert_eq!(
        assess(&t, 5.0, Some(8000.0), &config).level,
        ThreatLevel::Advisory
    );
}

#[test]
fn without_own_altitude_classification_never_escalates_to_alert() {
    // Range-only alerts fire constantly in the circuit, and a display that cries wolf gets
    // ignored precisely when it matters.
    let config = ThreatConfig::default();
    let t = target(Some(ORIGIN));

    let assessment = assess(&t, 0.5, None, &config);
    assert_eq!(assessment.level, ThreatLevel::Advisory);
    assert!(assessment.relative_altitude_ft.is_none());

    // And a distant one with no altitude reference is still just normal traffic.
    assert_eq!(assess(&t, 20.0, None, &config).level, ThreatLevel::Normal);
}

#[test]
fn targets_on_the_ground_do_not_raise_alerts() {
    // Aircraft taxiing past the hold-short line are legitimately co-altitude and very close.
    let config = ThreatConfig::default();
    let mut t = target(Some(ORIGIN));
    t.on_ground = true;
    t.altitude_ft = Some(5400);

    assert_eq!(assess(&t, 0.2, Some(5400.0), &config).level, ThreatLevel::Normal);
}

#[test]
fn relative_altitude_is_unknown_when_the_target_has_no_altitude() {
    let config = ThreatConfig::default();
    let mut t = target(Some(ORIGIN));
    t.altitude_ft = None;
    assert!(assess(&t, 1.0, Some(8000.0), &config)
        .relative_altitude_ft
        .is_none());
}

#[test]
fn relative_altitude_is_formatted_in_signed_hundreds_of_feet() {
    assert_eq!(format_relative_altitude(Some(1200.0)), "+12");
    assert_eq!(format_relative_altitude(Some(-2500.0)), "-25");
    // Anything that rounds to zero is co-altitude and shown unsigned; the display cannot honestly
    // distinguish 40 ft above from 40 ft below, and "+00"/"-00" reads as a bug.
    assert_eq!(format_relative_altitude(Some(40.0)), "00");
    assert_eq!(format_relative_altitude(Some(-40.0)), "00");
    assert_eq!(format_relative_altitude(Some(0.0)), "00");
    assert_eq!(format_relative_altitude(None), "---");
    // Three digits when needed, rather than truncating.
    assert_eq!(format_relative_altitude(Some(15000.0)), "+150");
}

#[test]
fn threat_levels_order_from_least_to_most_urgent() {
    // The plan view sorts by this to draw alerts on top and to give them first pick of tag space.
    assert!(ThreatLevel::Normal < ThreatLevel::Advisory);
    assert!(ThreatLevel::Advisory < ThreatLevel::Alert);
}

// --- view state ---------------------------------------------------------------------------

#[test]
fn range_cycles_forwards_and_wraps() {
    let mut view = ViewState {
        range_nm: 2.0,
        orientation: Orientation::NorthUp,
        ..Default::default()
    };
    let mut seen = vec![view.range_nm];
    for _ in 0..ViewState::RANGES.len() {
        view.cycle_range();
        seen.push(view.range_nm);
    }
    assert_eq!(seen.first(), seen.last(), "cycling all the way should wrap");
    assert_eq!(&seen[..ViewState::RANGES.len()], &ViewState::RANGES[..]);
}

#[test]
fn range_cycles_backwards_and_wraps() {
    let mut view = ViewState {
        range_nm: 2.0,
        orientation: Orientation::NorthUp,
        ..Default::default()
    };
    view.cycle_range_down();
    assert_eq!(view.range_nm, *ViewState::RANGES.last().unwrap());
    view.cycle_range();
    assert_eq!(view.range_nm, 2.0);
}

#[test]
fn an_unknown_range_recovers_to_a_valid_one() {
    // A config file or a future touch gesture could set something off-list; cycling must not get
    // stuck or panic.
    let mut view = ViewState {
        range_nm: 7.3,
        orientation: Orientation::NorthUp,
        ..Default::default()
    };
    view.cycle_range();
    assert!(ViewState::RANGES.contains(&view.range_nm), "got {}", view.range_nm);
}

#[test]
fn orientation_toggles() {
    let mut view = ViewState::default();
    assert_eq!(view.orientation, Orientation::NorthUp);
    view.toggle_orientation();
    assert_eq!(view.orientation, Orientation::TrackUp);
    assert_eq!(view.orientation.label(), "TRK-UP");
    view.toggle_orientation();
    assert_eq!(view.orientation, Orientation::NorthUp);
}

// --- traffic held back for want of own-ship -------------------------------------------------

/// Build a state holding `targets`, with no own-ship position — the outdoor-test situation.
fn state_without_ownship(targets: Vec<Target>) -> stratux_client::AppState {
    let mut state = stratux_client::AppState::new();
    for (i, mut t) in targets.into_iter().enumerate() {
        t.icao = 0x100000 + i as u32;
        state.targets.insert(t.icao, t);
    }
    assert!(
        state.ownship.usable_position().is_none(),
        "this fixture is only meaningful without an own-ship position"
    );
    state
}

#[test]
fn traffic_received_without_own_ship_is_counted_not_forgotten() {
    // The real failure this exists for: outdoors, 187 ADS-B messages were decoded and two targets
    // tracked while the panel showed nothing and the status bar read `TFC 0`, because the plan
    // view cannot place anything without an origin. A dead receiver looked exactly the same.
    let now = Instant::now();
    let state = state_without_ownship(vec![
        target(Some(at_bearing(0.0, 4.0))),
        target(Some(at_bearing(90.0, 7.0))),
    ]);
    assert_eq!(
        avionics_ui::planview::unplotted_count(&state, now, &ReckonConfig::default()),
        2
    );
}

#[test]
fn targets_without_a_position_are_not_counted_as_held() {
    // Mode-S-only targets have nothing to plot even with own-ship, so counting them here would
    // promise traffic that a GPS fix would not actually reveal. They have their own status-bar
    // field for that reason.
    let now = Instant::now();
    let state = state_without_ownship(vec![target(None), target(None)]);
    assert_eq!(
        avionics_ui::planview::unplotted_count(&state, now, &ReckonConfig::default()),
        0
    );
}

#[test]
fn a_coasting_target_still_counts_as_held() {
    // A fix too old to extrapolate from is frozen and drawn dimmed, not dropped — `reckon` only
    // returns None when there is no position at all. So a coasting target WOULD have appeared had
    // own-ship been available, and must be counted. Getting this backwards would under-report the
    // receiver's health in exactly the situation where reassurance matters most.
    let config = ReckonConfig::default();
    let now = Instant::now();
    let mut coasting = target(Some(at_bearing(0.0, 4.0)));
    coasting.age_s = config.max_fix_age_s * 3.0;
    coasting.received = now - Duration::from_secs(600);

    assert!(
        reckon(&coasting, now, &config).is_some_and(|r| r.coasting),
        "fixture is meant to be a coasting target"
    );
    let state = state_without_ownship(vec![coasting]);
    assert_eq!(avionics_ui::planview::unplotted_count(&state, now, &config), 1);
}

#[test]
fn an_empty_sky_is_still_reported_as_empty() {
    // The counter must not invent reassurance when there genuinely is nothing being received.
    let now = Instant::now();
    let state = state_without_ownship(vec![]);
    assert_eq!(
        avionics_ui::planview::unplotted_count(&state, now, &ReckonConfig::default()),
        0
    );
}
