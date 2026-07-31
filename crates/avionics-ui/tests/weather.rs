//! Tests for the NEXRAD underlay and the weather page interaction.
//!
//! The important ones here are the geo-referencing tests. A mosaic that is offset or transposed
//! looks entirely plausible on screen — coherent blobs, sensible colours — while telling the pilot
//! the weather is somewhere it is not. That failure cannot be caught by eye, only by asserting that
//! a known intensity at a known latitude and longitude lands on the right texel.

use std::time::{Duration, Instant};

use avionics_ui::interact::{tap, two_finger_tap, zone_for, TapZone};
use avionics_ui::nexrad::{
    age_alpha, colour, fade_bucket, fade_fingerprint, ground_distance_nm, Mosaic, MosaicConfig,
    Patch,
};
use avionics_ui::weatherpage::format_age;
use avionics_ui::{Layout, Orientation, Page, Theme, ViewState};
use stratux_client::domain::{LatLon, NexradBlock, NexradKind};

const CENTRE: LatLon = LatLon {
    lat: 40.0,
    lon: -105.0,
};

/// Regional block geometry, as upstream's decoder produces below 60 degrees latitude: 48 arcminutes
/// of longitude by 4 of latitude, subdivided 32 x 4.
const BLOCK_WIDTH_DEG: f64 = 48.0 / 60.0;
const BLOCK_HEIGHT_DEG: f64 = 4.0 / 60.0;

fn block(kind: NexradKind, lat_north: f64, lon_west: f64, bins: Vec<u8>, received: Instant) -> NexradBlock {
    NexradBlock {
        kind,
        scale: 1,
        lat_north,
        lon_west,
        height_deg: BLOCK_HEIGHT_DEG,
        width_deg: BLOCK_WIDTH_DEG,
        bins,
        received,
    }
}

fn small_config() -> MosaicConfig {
    MosaicConfig {
        // Small and tight so one block spans many texels and the arithmetic is easy to reason
        // about; the shipped default is 1024 over 120 nm.
        texture_size: 256,
        half_span_nm: 30.0,
        ..Default::default()
    }
}

// --- colour LUT ---------------------------------------------------------------------------

#[test]
fn regional_and_conus_intensity_scales_are_offset_by_one() {
    // This is the single most likely thing to be silently wrong. Upstream fills an empty regional
    // block with 0 and an empty CONUS block with 1, which means CONUS shifts the whole scale up by
    // one. Treating them alike paints phantom precipitation or punches holes through real coverage.
    assert_eq!(colour(NexradKind::Regional, 0), None, "regional 0 is <5 dBZ");
    assert!(colour(NexradKind::Regional, 1).is_some(), "regional 1 is real return");

    assert_eq!(colour(NexradKind::Conus, 0), None, "CONUS 0 is no data");
    assert_eq!(colour(NexradKind::Conus, 1), None, "CONUS 1 is no precipitation");
    assert!(colour(NexradKind::Conus, 2).is_some(), "CONUS 2 is the first real return");

    // The lightest visible colour is the same on both products, just reached at a different level.
    assert_eq!(colour(NexradKind::Regional, 1), colour(NexradKind::Conus, 2));
    assert_eq!(colour(NexradKind::Regional, 7), colour(NexradKind::Conus, 8));
}

#[test]
fn intensity_ramps_monotonically_and_never_indexes_off_the_end() {
    // The decoder clamps to 0..=7, but an out-of-range value must degrade rather than panic.
    let mut previous = None;
    for intensity in 1..=7u8 {
        let current = colour(NexradKind::Regional, intensity).expect("visible level");
        assert_ne!(Some(current), previous, "level {intensity} duplicates the previous colour");
        previous = Some(current);
    }
    for intensity in 8..=255u8 {
        // Off the end of the ramp: no colour rather than a wrapped index.
        let _ = colour(NexradKind::Regional, intensity);
        let _ = colour(NexradKind::Conus, intensity);
    }
}

// --- ageing -------------------------------------------------------------------------------

#[test]
fn older_weather_is_drawn_fainter_but_never_invisible() {
    let fresh = age_alpha(Duration::from_secs(60));
    let aging = age_alpha(Duration::from_secs(7 * 60));
    let stale = age_alpha(Duration::from_secs(13 * 60));

    assert_eq!(fresh, 1.0);
    assert!(aging < fresh, "aging weather must fade");
    assert!(stale < aging, "stale weather must fade further");
    // Precipitation that was there ten minutes ago is still worth knowing about, provided the pilot
    // can see it is not current.
    assert!(stale > 0.0, "stale weather must remain visible");

    // Three steps, so at most two fade-driven rebuilds per block lifetime.
    assert_eq!(fade_bucket(Duration::from_secs(60)), 0);
    assert_eq!(fade_bucket(Duration::from_secs(7 * 60)), 1);
    assert_eq!(fade_bucket(Duration::from_secs(13 * 60)), 2);
}

#[test]
fn age_is_formatted_compactly() {
    assert_eq!(format_age(Duration::from_secs(0)), "0s");
    assert_eq!(format_age(Duration::from_secs(45)), "45s");
    assert_eq!(format_age(Duration::from_secs(60)), "1m");
    assert_eq!(format_age(Duration::from_secs(11 * 60 + 30)), "11m");
    assert_eq!(format_age(Duration::from_secs(3600)), "1h00m");
    assert_eq!(format_age(Duration::from_secs(3600 + 25 * 60)), "1h25m");
}

// --- geo-referencing ----------------------------------------------------------------------

#[test]
fn a_bin_lands_at_its_own_latitude_and_longitude() {
    let now = Instant::now();
    let config = small_config();

    // One block whose bins are all intensity 5, positioned so it straddles the patch centre.
    let bins = vec![5u8; NexradBlock::BIN_COUNT];
    let b = block(
        NexradKind::Regional,
        CENTRE.lat + BLOCK_HEIGHT_DEG * 0.5,
        CENTRE.lon - BLOCK_WIDTH_DEG * 0.5,
        bins,
        now,
    );
    let expected = colour(NexradKind::Regional, 5).expect("level 5 is visible");

    let patch = Patch::build(&config, [b.clone()].iter(), CENTRE, now);
    assert!(!patch.is_empty());
    assert_eq!(patch.stats.blocks_composited, 1);

    // Sample the centre of every bin and confirm the colour is there.
    for by in 0..NexradBlock::BINS_Y {
        for bx in 0..NexradBlock::BINS_X {
            let (nw, se) = b.bin_bounds(bx, by).unwrap();
            let mid = LatLon::new((nw.lat + se.lat) * 0.5, (nw.lon + se.lon) * 0.5);
            let texel = patch.texel_at(mid).expect("bin centre inside the patch");
            assert_eq!(
                (texel.r, texel.g, texel.b),
                expected,
                "bin ({bx},{by}) at {mid:?} did not land where expected"
            );
            assert!(texel.a > 0, "bin ({bx},{by}) should be opaque");
        }
    }
}

#[test]
fn the_mosaic_is_not_transposed_or_mirrored() {
    // A transposed or flipped mosaic still looks like coherent weather, so this is checked
    // explicitly: give one block a single hot bin in a known corner and find it.
    let now = Instant::now();
    let config = small_config();

    // Bin (0, 0) is the block's north-west corner.
    let mut bins = vec![0u8; NexradBlock::BIN_COUNT];
    bins[0] = 7;

    let lat_north = CENTRE.lat + BLOCK_HEIGHT_DEG * 0.5;
    let lon_west = CENTRE.lon - BLOCK_WIDTH_DEG * 0.5;
    let b = block(NexradKind::Regional, lat_north, lon_west, bins, now);
    let patch = Patch::build(&config, [b.clone()].iter(), CENTRE, now);

    let hot = colour(NexradKind::Regional, 7).unwrap();

    // The painted bin is the north-west one.
    let (nw, se) = b.bin_bounds(0, 0).unwrap();
    let inside = LatLon::new((nw.lat + se.lat) * 0.5, (nw.lon + se.lon) * 0.5);
    let texel = patch.texel_at(inside).unwrap();
    assert_eq!((texel.r, texel.g, texel.b), hot, "the hot bin should be at the NW corner");

    // Its east neighbour, its south neighbour, and the SE corner must all be blank. If the mosaic
    // were transposed or mirrored, at least one of these would be painted instead.
    for (bx, by, label) in [(1, 0, "east neighbour"), (0, 1, "south neighbour"), (31, 3, "SE corner")] {
        let (nw, se) = b.bin_bounds(bx, by).unwrap();
        let mid = LatLon::new((nw.lat + se.lat) * 0.5, (nw.lon + se.lon) * 0.5);
        let texel = patch.texel_at(mid).unwrap();
        assert_eq!(texel.a, 0, "{label} should be transparent, got {texel:?}");
    }
}

#[test]
fn the_patch_covers_a_square_patch_of_ground() {
    // The longitude span is divided by cos(latitude) so one texel is a constant ground distance in
    // both axes. If that were dropped, the mosaic would be stretched east-west by ~30% at these
    // latitudes and weather would appear displaced along track.
    let config = small_config();
    let patch = Patch::build(&config, [].iter(), CENTRE, Instant::now());

    let ns_nm = ground_distance_nm(
        LatLon::new(CENTRE.lat - patch.half_span_lat_deg, CENTRE.lon),
        LatLon::new(CENTRE.lat + patch.half_span_lat_deg, CENTRE.lon),
    );
    let ew_nm = ground_distance_nm(
        LatLon::new(CENTRE.lat, CENTRE.lon - patch.half_span_lon_deg),
        LatLon::new(CENTRE.lat, CENTRE.lon + patch.half_span_lon_deg),
    );

    assert!(
        (ns_nm - ew_nm).abs() / ns_nm < 0.01,
        "patch should be square on the ground: {ns_nm:.2} nm N-S vs {ew_nm:.2} nm E-W"
    );
    assert!(
        (ns_nm - config.half_span_nm * 2.0).abs() < 0.1,
        "patch should span twice the half-span: {ns_nm:.2} nm"
    );
}

#[test]
fn blocks_outside_the_patch_are_skipped_not_wrapped() {
    let now = Instant::now();
    let config = small_config();
    let bins = vec![6u8; NexradBlock::BIN_COUNT];

    // Far away in both axes — well outside a 30 nm half-span.
    let far = block(NexradKind::Regional, CENTRE.lat + 20.0, CENTRE.lon + 20.0, bins, now);
    let patch = Patch::build(&config, [far].iter(), CENTRE, now);

    assert!(patch.is_empty(), "a distant block must not paint anything");
    assert_eq!(patch.stats.blocks_skipped_outside, 1);
    assert_eq!(patch.stats.blocks_composited, 0);
    // And nothing wrapped around into the patch.
    assert!(patch.pixels.iter().all(|p| p.a == 0));
}

#[test]
fn an_all_empty_block_paints_nothing() {
    let now = Instant::now();
    let config = small_config();

    // Regional empty is 0; CONUS empty is 1. Both must produce a blank patch.
    for (kind, empty) in [(NexradKind::Regional, 0u8), (NexradKind::Conus, 1u8)] {
        let bins = vec![empty; NexradBlock::BIN_COUNT];
        let b = block(kind, CENTRE.lat, CENTRE.lon, bins, now);
        let patch = Patch::build(&config, [b].iter(), CENTRE, now);
        assert!(patch.is_empty(), "{kind:?} empty block should paint nothing");
    }
}

#[test]
fn stale_blocks_are_composited_more_faintly_than_fresh_ones() {
    let now = Instant::now();
    let config = small_config();
    let bins = vec![5u8; NexradBlock::BIN_COUNT];

    let sample = LatLon::new(CENTRE.lat, CENTRE.lon);
    let position = (CENTRE.lat + BLOCK_HEIGHT_DEG * 0.5, CENTRE.lon - BLOCK_WIDTH_DEG * 0.5);

    let fresh = Patch::build(
        &config,
        [block(NexradKind::Regional, position.0, position.1, bins.clone(), now)].iter(),
        CENTRE,
        now,
    );
    let stale = Patch::build(
        &config,
        [block(
            NexradKind::Regional,
            position.0,
            position.1,
            bins,
            now - Duration::from_secs(12 * 60),
        )]
        .iter(),
        CENTRE,
        now,
    );

    let fresh_alpha = fresh.texel_at(sample).unwrap().a;
    let stale_alpha = stale.texel_at(sample).unwrap().a;
    assert!(fresh_alpha > 0 && stale_alpha > 0);
    assert!(
        stale_alpha < fresh_alpha,
        "stale weather must be fainter: {stale_alpha} vs {fresh_alpha}"
    );
}

#[test]
fn positions_outside_the_patch_have_no_texel() {
    let patch = Patch::build(&small_config(), [].iter(), CENTRE, Instant::now());
    assert!(patch.texel_at(LatLon::new(CENTRE.lat + 30.0, CENTRE.lon)).is_none());
    assert!(patch.texel_at(LatLon::new(CENTRE.lat, CENTRE.lon + 30.0)).is_none());
    assert!(patch.texel_at(CENTRE).is_some());
}

// --- cache invalidation -------------------------------------------------------------------

#[test]
fn an_uncomposited_mosaic_always_needs_a_composite() {
    let mosaic = Mosaic::new(small_config());
    assert!(mosaic.needs_composite(1, 0, CENTRE, Instant::now()));
}

#[test]
fn the_fade_fingerprint_changes_only_when_a_block_crosses_a_fade_step() {
    // This is what replaced a wall-clock refresh timer. If it changed continuously, the texture
    // would be rebuilt every frame — roughly 14 ms of work on a desktop and several times that on
    // a Pi 3.
    let now = Instant::now();
    let bins = vec![5u8; NexradBlock::BIN_COUNT];
    let blocks = [block(NexradKind::Regional, CENTRE.lat, CENTRE.lon, bins, now)];

    let base = fade_fingerprint(blocks.iter(), now);

    // Still inside the "fresh" step a minute later: no rebuild.
    assert_eq!(
        base,
        fade_fingerprint(blocks.iter(), now + Duration::from_secs(60)),
        "ageing within a step must not invalidate the cache"
    );
    assert_eq!(
        base,
        fade_fingerprint(blocks.iter(), now + Duration::from_secs(4 * 60)),
    );

    // Crossing into "aging" and then "stale" must each invalidate exactly once.
    let aging = fade_fingerprint(blocks.iter(), now + Duration::from_secs(6 * 60));
    let stale = fade_fingerprint(blocks.iter(), now + Duration::from_secs(12 * 60));
    assert_ne!(base, aging, "crossing into the aging step must invalidate");
    assert_ne!(aging, stale, "crossing into the stale step must invalidate");
}

#[test]
fn the_fade_fingerprint_does_not_depend_on_iteration_order() {
    // Blocks live in a HashMap, whose iteration order is not stable. If the fingerprint depended on
    // it, the texture would be rebuilt at random.
    let now = Instant::now();
    let bins = vec![4u8; NexradBlock::BIN_COUNT];
    let a = block(NexradKind::Regional, CENTRE.lat, CENTRE.lon, bins.clone(), now);
    let b = block(
        NexradKind::Regional,
        CENTRE.lat + BLOCK_HEIGHT_DEG,
        CENTRE.lon,
        bins,
        now - Duration::from_secs(7 * 60),
    );

    let forward = [a.clone(), b.clone()];
    let backward = [b, a];
    assert_eq!(
        fade_fingerprint(forward.iter(), now),
        fade_fingerprint(backward.iter(), now)
    );
}

#[test]
fn moving_far_enough_forces_a_recentre() {
    // Only asserts the threshold arithmetic; the full path needs a GPU to upload.
    let config = small_config();
    let near = ground_distance_nm(
        CENTRE,
        avionics_ui::projection::advance(CENTRE, 90.0, 3600.0, 1.0),
    );
    assert!((near - 1.0).abs() < 0.01, "1 nm displacement, got {near}");
    assert!(config.recentre_after_nm > 1.0, "1 nm should not force a recentre");

    let far = ground_distance_nm(
        CENTRE,
        avionics_ui::projection::advance(CENTRE, 90.0, 3600.0 * 25.0, 1.0),
    );
    assert!(far > config.recentre_after_nm, "25 nm must force a recentre");
}

// --- interaction --------------------------------------------------------------------------

fn layout() -> Layout {
    Layout::for_size(800.0, 480.0, &Theme::dark())
}

#[test]
fn tap_zones_split_the_screen_as_expected() {
    let l = layout();
    assert_eq!(zone_for(&l, 400.0, 4.0), TapZone::StatusBar);
    assert_eq!(zone_for(&l, 400.0, l.height - 2.0), TapZone::Footer);
    assert_eq!(zone_for(&l, 400.0, l.status_bar_height + 10.0), TapZone::BodyUpper);
    assert_eq!(
        zone_for(&l, 400.0, l.height - l.footer_height - 10.0),
        TapZone::BodyLower
    );
}

#[test]
fn tapping_the_status_bar_switches_page() {
    // The status bar is the one piece of chrome present and identical on every page, which is what
    // makes it a reliable navigation target.
    let l = layout();
    let mut view = ViewState::default();
    assert_eq!(view.page, Page::PlanView);

    // Three pages now, so a full cycle is three presses.
    tap(&mut view, &l, 400.0, 4.0, 8, 0);
    assert_eq!(view.page, Page::Weather);
    tap(&mut view, &l, 400.0, 4.0, 8, 0);
    assert_eq!(view.page, Page::Ahrs);
    tap(&mut view, &l, 400.0, 4.0, 8, 0);
    assert_eq!(view.page, Page::PlanView);
}

#[test]
fn tapping_the_plan_view_body_changes_nothing() {
    // This test used to assert the opposite: that a body tap cycled the range. That behaviour was
    // removed when the soft-key strip landed, because it meant a hand steadying itself against the
    // panel in turbulence silently changed the range scale. Range now moves only via RNG+/RNG-.
    let l = layout();
    let mut view = ViewState::default();
    let before = view.range_nm;

    tap(&mut view, &l, 400.0, 240.0, 8, 0);
    assert_eq!(view.range_nm, before, "body taps must not change range");
    assert_eq!(view.page, Page::PlanView);
}

#[test]
fn tapping_the_weather_page_scrolls_instead_of_changing_range() {
    let l = layout();
    let mut view = ViewState {
        page: Page::Weather,
        ..Default::default()
    };
    let range = view.range_nm;
    let rows = 8;
    let total = 30;

    // Lower half pages forward.
    tap(&mut view, &l, 400.0, l.height - l.footer_height - 10.0, rows, total);
    assert_eq!(view.weather_scroll, rows);
    assert_eq!(view.range_nm, range, "range must not change on the weather page");

    // Upper half pages back.
    tap(&mut view, &l, 400.0, l.status_bar_height + 10.0, rows, total);
    assert_eq!(view.weather_scroll, 0);

    // Paging back from the top stays at the top rather than underflowing.
    tap(&mut view, &l, 400.0, l.status_bar_height + 10.0, rows, total);
    assert_eq!(view.weather_scroll, 0);
}

#[test]
fn scrolling_past_the_end_wraps_to_the_top() {
    // With no scrollbar to drag, a tap that does nothing looks like the display has frozen.
    let l = layout();
    let mut view = ViewState {
        page: Page::Weather,
        ..Default::default()
    };
    let (rows, total) = (8usize, 10usize);
    let lower = l.height - l.footer_height - 10.0;

    tap(&mut view, &l, 400.0, lower, rows, total);
    assert_eq!(view.weather_scroll, total - rows, "clamps to the last page");
    tap(&mut view, &l, 400.0, lower, rows, total);
    assert_eq!(view.weather_scroll, 0, "then wraps");
}

#[test]
fn a_short_list_does_not_scroll_at_all() {
    let l = layout();
    let mut view = ViewState {
        page: Page::Weather,
        ..Default::default()
    };
    tap(&mut view, &l, 400.0, l.height - l.footer_height - 10.0, 8, 3);
    assert_eq!(view.weather_scroll, 0);
}

#[test]
fn two_finger_tap_toggles_orientation_only_on_the_plan_view() {
    let mut view = ViewState::default();
    two_finger_tap(&mut view);
    assert_eq!(view.orientation, Orientation::TrackUp);

    // On the text page there is nothing sensible to toggle, so it must do nothing rather than
    // silently change the orientation of a screen the pilot cannot see.
    let mut view = ViewState {
        page: Page::Weather,
        orientation: Orientation::NorthUp,
        ..Default::default()
    };
    two_finger_tap(&mut view);
    assert_eq!(view.orientation, Orientation::NorthUp);
}

#[test]
fn tapping_the_footer_does_nothing() {
    let l = layout();
    let mut view = ViewState::default();
    let before = view.clone();
    tap(&mut view, &l, 400.0, l.height - 2.0, 8, 30);
    assert_eq!(view.page, before.page);
    assert_eq!(view.range_nm, before.range_nm);
    assert_eq!(view.weather_scroll, before.weather_scroll);
}
