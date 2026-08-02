//! The simulated sky, and the clock that moves it.
//!
//! A snapshot is one instant. Serving it unchanged would give the display a frozen picture: every
//! target would sit still, the dead-reckoner would extrapolate for three seconds and then mark the
//! lot as coasting, and the whole thing would go grey. So targets are flown forward along the
//! track and speed they were heard on, which is the same assumption the display's own reckoner
//! makes — and that is deliberate, because it means a disagreement between the two is a bug in one
//! of them rather than an artefact of the mock inventing motion the display cannot predict.
//!
//! Own-ship is stationary at the snapshot's origin by default. That matches the real outdoor
//! capture — a Pi on a battery in a field — and it is the case that exposes the most: no track, no
//! ground speed, `NO ALT REF` in the status bar, and the vertical filter admitting everything
//! because there is no altitude to compare against.

use std::time::Duration;

use stratux_client::wire;

/// Nautical miles per degree of latitude. The projection in `avionics-ui` uses the same figure.
const NM_PER_DEGREE: f64 = 60.0;

/// How own-ship is flying, if at all.
#[derive(Debug, Clone, Copy)]
pub struct OwnShip {
    pub lat: f64,
    pub lon: f64,
    pub altitude_ft: f64,
    /// Degrees true. `None` leaves the display with no track reference, as on the ground.
    pub track_deg: Option<f64>,
    pub ground_speed_kt: f64,
    pub satellites: u16,
}

impl OwnShip {
    pub fn stationary(lat: f64, lon: f64) -> Self {
        Self {
            lat,
            lon,
            // Roughly Morristown's field elevation. Any figure will do; what matters is that it is
            // present, so relative altitudes and the vertical filter have something to work with.
            altitude_ft: 190.0,
            track_deg: None,
            ground_speed_kt: 0.0,
            satellites: 13,
        }
    }

    fn advance(&mut self, dt: Duration) {
        let Some(track) = self.track_deg else { return };
        let (lat, lon) = advance_position(
            self.lat,
            self.lon,
            track,
            self.ground_speed_kt,
            dt.as_secs_f64(),
        );
        self.lat = lat;
        self.lon = lon;
    }
}

/// Move a position along a track. Flat-earth, which is accurate far past any ADS-B range.
pub fn advance_position(
    lat: f64,
    lon: f64,
    track_deg: f64,
    speed_kt: f64,
    seconds: f64,
) -> (f64, f64) {
    let distance_nm = speed_kt * seconds / 3600.0;
    let (sin, cos) = track_deg.to_radians().sin_cos();
    let d_lat = distance_nm * cos / NM_PER_DEGREE;
    // Longitude degrees shrink with latitude; without this a target on an easterly track drifts
    // visibly off its own heading barb at mid latitudes.
    let cos_lat = lat.to_radians().cos().abs().max(1e-6);
    let d_lon = distance_nm * sin / (NM_PER_DEGREE * cos_lat);
    (lat + d_lat, lon + d_lon)
}

/// Everything the mock is currently pretending to hear.
pub struct World {
    pub ownship: OwnShip,
    pub targets: Vec<wire::TrafficInfo>,
    pub weather: Vec<wire::WeatherMessage>,
    /// Seconds of simulated time since the server started.
    pub elapsed_s: f64,
    /// How much of the weather list has been published. FIS-B arrives a product at a time, and a
    /// display that receives the whole set in one frame never exercises the incremental path.
    pub weather_published: usize,
}

impl World {
    pub fn new(
        ownship: OwnShip,
        targets: Vec<wire::TrafficInfo>,
        weather: Vec<wire::WeatherMessage>,
    ) -> Self {
        Self {
            ownship,
            targets,
            weather,
            elapsed_s: 0.0,
            weather_published: 0,
        }
    }

    /// Advance every target and own-ship by `dt`.
    pub fn tick(&mut self, dt: Duration) {
        self.elapsed_s += dt.as_secs_f64();
        self.ownship.advance(dt);

        for t in &mut self.targets {
            if !t.Position_valid || t.OnGround {
                continue;
            }
            let (lat, lon) = advance_position(
                t.Lat as f64,
                t.Lng as f64,
                t.Track as f64,
                t.Speed as f64,
                dt.as_secs_f64(),
            );
            t.Lat = lat as f32;
            t.Lng = lon as f32;
            if t.Vvel != 0 {
                t.Alt += (t.Vvel as f64 * dt.as_secs_f64() / 60.0).round() as i32;
            }
            // The feed's `seen` was the age when the snapshot was taken. Holding it there would
            // publish a target that is permanently 4.2 seconds old; the mock is claiming to have
            // just heard it, so it says so.
            t.Age = 0.0;
            t.AgeLastAlt = 0.0;
        }
    }

    /// Replace the traffic picture with a freshly polled one.
    ///
    /// Wholesale replacement, not a merge. The feed is authoritative about what is in the sky: a
    /// target it no longer lists has either landed, left the area or stopped being heard, and all
    /// three mean it should stop being published. Merging would accumulate ghosts that never age
    /// out, because nothing here would ever be told they were gone.
    ///
    /// The positions that arrive are the truth and replace the flown-forward estimates, which is
    /// the same snap-on-new-fix the display's own reckoner does.
    pub fn refresh_traffic(&mut self, targets: Vec<wire::TrafficInfo>) {
        self.targets = targets;
    }

    /// Add any weather products not already held.
    ///
    /// Additive, unlike traffic, because FIS-B products are not a picture of the current world —
    /// they are reports that stay valid until superseded, and a station dropping out of one poll
    /// does not retract its last METAR. Keyed on the raw text so a re-poll returning the same
    /// report does not queue it a second time.
    pub fn merge_weather(&mut self, incoming: Vec<wire::WeatherMessage>) -> usize {
        let mut added = 0;
        for item in incoming {
            if self.weather.iter().any(|w| w.Data == item.Data) {
                continue;
            }
            self.weather.push(item);
            added += 1;
        }
        added
    }

    /// The next weather product to broadcast, cycling round the list forever.
    ///
    /// Cycling, not draining. FIS-B ground stations **rebroadcast** their products on a repeating
    /// schedule — that is the whole reason a real receiver accumulates weather over several
    /// minutes rather than getting it all at once, and why the display says exactly that when it
    /// has none yet.
    ///
    /// Publishing each product once was the obvious first implementation and it was wrong in a way
    /// that only showed up later: `/weather` deliberately does not replay on connect, so once the
    /// list was exhausted every *new* client got nothing at all, forever. A display started ten
    /// minutes after the server sat on `NO WEATHER RECEIVED YET` with a healthy server beside it.
    ///
    /// Re-sending a report the display already holds is not waste. It refreshes the age, which is
    /// what a rebroadcast does on the real system.
    pub fn next_weather(&mut self) -> Option<wire::WeatherMessage> {
        if self.weather.is_empty() {
            return None;
        }
        if self.weather_published >= self.weather.len() {
            self.weather_published = 0;
        }
        let item = self.weather[self.weather_published].clone();
        self.weather_published += 1;
        Some(item)
    }

    pub fn situation(&self) -> wire::SituationData {
        let o = &self.ownship;
        wire::SituationData {
            GPSLatitude: o.lat as f32,
            GPSLongitude: o.lon as f32,
            // 2 is "3D GPS + SBAS", which is what the real GPYes reports outdoors.
            GPSFixQuality: 2,
            GPSSatellites: o.satellites,
            GPSSatellitesTracked: o.satellites + 3,
            GPSSatellitesSeen: o.satellites + 5,
            GPSHorizontalAccuracy: 3.5,
            GPSVerticalAccuracy: 5.0,
            GPSNACp: 10,
            GPSAltitudeMSL: o.altitude_ft as f32,
            GPSHeightAboveEllipsoid: (o.altitude_ft - 55.0) as f32,
            GPSGeoidSep: -55.0,
            GPSVerticalSpeed: 0.0,
            GPSTrueCourse: o.track_deg.unwrap_or(0.0) as f32,
            GPSGroundSpeed: o.ground_speed_kt,
            GPSPositionSampleRate: 10.0,

            // A gentle roll and pitch so the attitude page has something to show. The heading
            // fields are left at the in-band sentinel 3276.7 on purpose: that is what the real
            // hardware reports for a reading it does not have, and `domain::Ahrs::value` maps it
            // to None. A mock that sent zeroes instead would quietly stop exercising that path.
            AHRSPitch: (self.elapsed_s * 0.35).sin() * 4.0,
            AHRSRoll: (self.elapsed_s * 0.22).sin() * 12.0,
            AHRSGyroHeading: 3276.7,
            AHRSMagHeading: 3276.7,
            AHRSTurnRate: 3276.7,
            AHRSSlipSkid: (self.elapsed_s * 0.3).cos() * 1.5,
            AHRSGLoad: 1.0 + (self.elapsed_s * 0.5).sin() * 0.05,
            AHRSGLoadMin: 0.92,
            AHRSGLoadMax: 1.08,
            AHRSStatus: 7,
            ..Default::default()
        }
    }

    pub fn status(&self) -> wire::Status {
        let tracking = self.targets.len() as u16;
        wire::Status {
            Version: "mock-1.0".into(),
            Build: "mock-stratux".into(),
            Devices: 2,
            Connected_Users: 1,
            DiskBytesFree: 8_000_000_000,
            // A plausible 1090 rate scaled to how much traffic is in the snapshot, and zero on
            // 978: FIS-B is line-of-sight from ground stations and a receiver at field elevation
            // usually hears nothing, which is exactly what the real outdoor capture recorded.
            ES_messages_last_minute: (tracking as u32) * 16 + 40,
            ES_messages_max: (tracking as u32) * 20 + 60,
            UAT_messages_last_minute: 0,
            ES_traffic_targets_tracking: tracking,
            UAT_traffic_targets_tracking: 0,
            GPS_satellites_locked: self.ownship.satellites,
            GPS_satellites_seen: self.ownship.satellites + 5,
            GPS_satellites_tracked: self.ownship.satellites + 3,
            GPS_position_accuracy: 3.5,
            GPS_connected: true,
            GPS_solution: "3D GPS + SBAS".into(),
            Uptime: (self.elapsed_s * 1000.0) as i64,
            CPUTemp: 54.0 + (self.elapsed_s * 0.05).sin() as f32 * 3.0,
            CPUTempMin: 48.0,
            CPUTempMax: 61.0,
            UAT_METAR_total: self.weather_published as u32,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(track: f32, speed: u16) -> wire::TrafficInfo {
        wire::TrafficInfo {
            Position_valid: true,
            Lat: 40.0,
            Lng: -74.0,
            Alt: 5000,
            Track: track,
            Speed: speed,
            ..Default::default()
        }
    }

    #[test]
    fn a_target_flies_along_its_own_track() {
        // Due north at 60 kt for an hour is one degree of latitude, and no change of longitude.
        let (lat, lon) = advance_position(40.0, -74.0, 0.0, 60.0, 3600.0);
        assert!((lat - 41.0).abs() < 1e-6, "{lat}");
        assert!((lon + 74.0).abs() < 1e-9, "{lon}");

        // Due east travels further in longitude than in latitude, by 1/cos(lat).
        let (lat, lon) = advance_position(40.0, -74.0, 90.0, 60.0, 3600.0);
        assert!((lat - 40.0).abs() < 1e-9);
        let expected = -74.0 + 1.0 / 40.0f64.to_radians().cos();
        assert!((lon - expected).abs() < 1e-6, "{lon} vs {expected}");
    }

    #[test]
    fn longitude_is_scaled_by_latitude() {
        // Without the cos(lat) divisor an easterly target drifts off its own heading barb. At 60
        // degrees north the scaling is exactly a factor of two, which makes this easy to assert.
        let (_, near_equator) = advance_position(0.0, 0.0, 90.0, 60.0, 3600.0);
        let (_, high_latitude) = advance_position(60.0, 0.0, 90.0, 60.0, 3600.0);
        assert!((near_equator - 1.0).abs() < 1e-6, "{near_equator}");
        assert!((high_latitude - 2.0).abs() < 1e-4, "{high_latitude}");
    }

    #[test]
    fn a_stationary_own_ship_does_not_drift() {
        // The default case, and the one the real outdoor capture was: a Pi on a battery in a
        // field. If own-ship wandered, every relative bearing on the plan view would be wrong.
        let mut world = World::new(OwnShip::stationary(40.7784, -74.3343), vec![], vec![]);
        let before = (world.ownship.lat, world.ownship.lon);
        for _ in 0..100 {
            world.tick(Duration::from_millis(100));
        }
        assert_eq!((world.ownship.lat, world.ownship.lon), before);
    }

    #[test]
    fn ground_traffic_stays_put() {
        // An aircraft at a gate reports a stale track and a speed of zero, but taxiing traffic can
        // report both. Flying it forward would drive airport traffic across the countryside.
        let mut on_ground = target(90.0, 15);
        on_ground.OnGround = true;
        let mut world = World::new(OwnShip::stationary(40.0, -74.0), vec![on_ground], vec![]);
        world.tick(Duration::from_secs(60));
        assert_eq!(world.targets[0].Lng, -74.0);
    }

    #[test]
    fn a_climbing_target_gains_altitude_at_its_reported_rate() {
        let mut climbing = target(0.0, 120);
        climbing.Vvel = 600;
        let mut world = World::new(OwnShip::stationary(40.0, -74.0), vec![climbing], vec![]);
        world.tick(Duration::from_secs(60));
        assert_eq!(world.targets[0].Alt, 5600, "600 fpm for one minute");
    }

    #[test]
    fn republished_targets_are_not_stale_on_arrival() {
        // The snapshot's `seen` was the age at capture. Serving it unchanged would publish traffic
        // that is permanently seconds old, and the display would draw the whole sky as coasting.
        let mut aged = target(0.0, 120);
        aged.Age = 4.2;
        aged.AgeLastAlt = 4.2;
        let mut world = World::new(OwnShip::stationary(40.0, -74.0), vec![aged], vec![]);
        world.tick(Duration::from_millis(100));
        assert_eq!(world.targets[0].Age, 0.0);
        assert_eq!(world.targets[0].AgeLastAlt, 0.0);
    }

    #[test]
    fn weather_is_published_one_at_a_time_and_then_rebroadcast() {
        // One product per turn, because FIS-B is opportunistic and a display that receives the
        // whole set in one frame never exercises the incremental path. Then round again, because
        // ground stations rebroadcast — and because `/weather` does not replay on connect, so a
        // list that drained would leave every later client with nothing at all.
        let items = vec![
            wire::WeatherMessage { Type: "METAR".into(), ..Default::default() },
            wire::WeatherMessage { Type: "TAF".into(), ..Default::default() },
        ];
        let mut world = World::new(OwnShip::stationary(0.0, 0.0), vec![], items);
        assert_eq!(world.next_weather().map(|w| w.Type), Some("METAR".into()));
        assert_eq!(world.next_weather().map(|w| w.Type), Some("TAF".into()));
        assert_eq!(
            world.next_weather().map(|w| w.Type),
            Some("METAR".into()),
            "the cycle must come round rather than drying up"
        );
    }

    #[test]
    fn an_empty_weather_list_never_publishes() {
        // Cycling must not divide by zero or hand out a phantom report when there is nothing to
        // say. This is the state on every cold start, before the first weather poll returns.
        let mut world = World::new(OwnShip::stationary(0.0, 0.0), vec![], vec![]);
        assert!(world.next_weather().is_none());
        assert!(world.next_weather().is_none());
    }

    #[test]
    fn a_product_added_after_the_cycle_started_is_reached() {
        // Weather merges in over time as polls return. A product appended after the index has
        // passed its position must still get broadcast, or a station that came into range late
        // would never be heard from.
        let metar = |body: &str| wire::WeatherMessage {
            Type: "METAR".into(),
            Data: body.into(),
            ..Default::default()
        };
        let mut world = World::new(OwnShip::stationary(0.0, 0.0), vec![], vec![metar("A")]);
        world.next_weather();
        world.merge_weather(vec![metar("B")]);

        let seen: Vec<String> = (0..4)
            .filter_map(|_| world.next_weather().map(|w| w.Data))
            .collect();
        assert!(seen.contains(&"B".to_string()), "late arrival never broadcast: {seen:?}");
    }

    #[test]
    fn a_traffic_refresh_replaces_rather_than_accumulating() {
        // The feed is authoritative about what is in the sky. Merging would keep publishing
        // targets that have landed or left, and nothing here would ever be told they were gone —
        // the display would fill with ghosts that never age out.
        let mut world = World::new(
            OwnShip::stationary(40.0, -74.0),
            vec![target(0.0, 100), target(90.0, 200)],
            vec![],
        );
        world.refresh_traffic(vec![target(180.0, 300)]);
        assert_eq!(world.targets.len(), 1);
        assert_eq!(world.targets[0].Track, 180.0);
    }

    #[test]
    fn a_refresh_snaps_positions_back_to_the_feed() {
        // Between polls the target is flown forward on an estimate. The arriving fix is the truth
        // and must win, exactly as a real ADS-B update does for the display's own reckoner.
        let mut world = World::new(OwnShip::stationary(40.0, -74.0), vec![target(0.0, 600)], vec![]);
        world.tick(Duration::from_secs(30));
        assert!(world.targets[0].Lat > 40.0, "fixture should have moved");

        world.refresh_traffic(vec![target(0.0, 600)]);
        assert_eq!(world.targets[0].Lat, 40.0, "the poll is authoritative");
    }

    #[test]
    fn weather_accumulates_and_does_not_duplicate_on_re_poll() {
        // Unlike traffic, a report stays valid until superseded — a station missing from one poll
        // has not retracted its last METAR. But polling every few minutes returns the same text
        // repeatedly, and queueing it each time would publish the same report forever.
        let metar = |body: &str| wire::WeatherMessage {
            Type: "METAR".into(),
            Data: body.into(),
            ..Default::default()
        };
        let mut world = World::new(OwnShip::stationary(0.0, 0.0), vec![], vec![]);

        assert_eq!(world.merge_weather(vec![metar("A"), metar("B")]), 2);
        assert_eq!(world.merge_weather(vec![metar("A"), metar("B")]), 0, "re-poll adds nothing");
        assert_eq!(world.merge_weather(vec![metar("B"), metar("C")]), 1, "only the new one");
        assert_eq!(world.weather.len(), 3);
    }

    #[test]
    fn absent_ahrs_headings_use_the_hardware_sentinel() {
        // 3276.7 is what the real board reports for a reading it does not have. Sending zero
        // instead would look like a valid due-north heading and would quietly stop exercising
        // `domain::Ahrs::value`, which exists precisely to map the sentinel to None.
        let world = World::new(OwnShip::stationary(40.0, -74.0), vec![], vec![]);
        let s = world.situation();
        assert_eq!(s.AHRSMagHeading, 3276.7);
        assert_eq!(s.AHRSGyroHeading, 3276.7);
        assert_eq!(s.AHRSTurnRate, 3276.7);
        // ... while the readings the board does have are live.
        assert!(s.AHRSGLoad > 0.5 && s.AHRSGLoad < 1.5);
    }
}
