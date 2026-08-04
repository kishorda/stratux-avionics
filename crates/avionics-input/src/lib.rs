//! Touch input, read straight from evdev.
//!
//! No libinput and no display server: there is nothing here but a kernel device and a small state
//! machine. The panel is fixed, single-orientation and has exactly two gestures to recognise, so
//! libinput's device-quirk database and configuration surface would be cost without benefit.
//!
//! # Hardware status
//!
//! Confirmed on the target (Pi 3B v1.2, Hysong 7" DSI, `vc4-kms-dsi-7inch`, 2026-07-31):
//!
//! ```text
//! device = "10-0038 generic ft5x06 (79)"   path = /dev/input/event2
//! x = (0.0, 799.0)                         y = (0.0, 479.0)
//! ```
//!
//! Note the driver names it `ft5x06`, **not** `ft5406` — the overlay's driver is `edt_ft5x06`
//! and it labels the device by i2c address and chip family. Searching `/proc/bus/input/devices`
//! for "ft5406" or "touch" finds nothing on this panel, which looks exactly like a missing
//! device. `NAME_HINTS` below covers both spellings for that reason.
//!
//! The trailing `(79)` is **not stable**: the same panel reported `(00)` on a later boot. It is
//! a firmware value the driver reads back over i2c and folds into the name, so never match on
//! the full string. Substring matching, which is what `NAME_HINTS` does, is what makes this
//! survive — an exact-name match would have worked in testing and failed in the aircraft.
//!
//! The axis ranges are 0..=799 by 0..=479, i.e. the controller reports in panel pixels and the
//! scaling into screen coordinates is the identity here. Do not simplify the scaling away: it is
//! a property of this panel, not of the protocol, and the 1024x600 variants of this class of
//! panel do not share it.
//!
//! Single-finger taps are confirmed working on the panel. Still unconfirmed: two-finger tap,
//! and therefore whether multi-slot `ABS_MT_TRACKING_ID` lifetimes behave as decoded below.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode};

/// A recognised gesture, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// A single finger touched and released without moving far.
    Tap { x: f32, y: f32 },
    /// Two fingers touched and released together.
    TwoFingerTap,
}

/// Substrings matched against device names, lowercased.
///
/// The panel's controller is exposed by the `edt_ft5x06` driver bundled into the
/// `vc4-kms-dsi-7inch` overlay, and on the target it names itself
/// `"10-0038 generic ft5x06 (79)"` — matched here by `"ft5x06"`.
///
/// Matching on the name rather than on an `eventN` number matters: event numbers reorder across
/// boots depending on probe order, and the two SDRs plus a USB keyboard make that ordering
/// genuinely unstable. On the target the touchscreen currently lands on `event2`, behind the
/// two `vc4-hdmi` ALSA/CEC inputs, so hardcoding `event0` would find an HDMI jack.
///
/// These are alternatives, not preferences: the search below takes the first *candidate device*
/// matching any of them, so the order of this list has no effect. Keep `"touch"` last anyway —
/// it is the loosest and would be the one to drop first if it ever mismatched.
const NAME_HINTS: &[&str] = &["ft5406", "ft5x06", "edt-ft5x06", "generic ft5x06", "touch"];

/// Ignore a touch as a tap if the finger moved further than this, in device units scaled to
/// screen pixels. Vibration and a bumpy panel mount produce a few pixels of movement even on a
/// deliberate press.
const TAP_MOVE_TOLERANCE_PX: f32 = 24.0;

/// A press held longer than this is not a tap. Prevents a resting hand from cycling the range.
const TAP_MAX_DURATION: Duration = Duration::from_millis(600);

/// One decoded input event, stripped of everything evdev-specific.
///
/// This exists so the gesture state machine can be driven from a recorded event log as well as
/// from a real panel. The decoding below is where the interesting bugs live, and it is not worth
/// needing hardware — and a finger — to test it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    Slot(i32),
    /// Non-negative starts a contact; -1 ends it.
    TrackingId(i32),
    PositionX(i32),
    PositionY(i32),
    /// `false` is release. Some controllers report a single-touch release only this way.
    BtnTouch(bool),
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    start: (f32, f32),
    latest: (f32, f32),
    moved: f32,
    /// Whether `start`/`latest` hold a real reading yet.
    ///
    /// Was previously inferred from `start == (0.0, 0.0)`, which is ambiguous: the top-left
    /// corner of the panel is a perfectly good place to press, and reports exactly that.
    positioned: bool,
    /// Which axes have been seen for this contact. Positions arrive one axis per event, so a
    /// contact is only usable once BOTH have landed — see `TouchState::apply`.
    have_x: bool,
    have_y: bool,
}

/// The gesture state machine, with no knowledge of evdev or of any device.
///
/// Split out from [`TouchReader`] so it can be driven from a recorded event log. The device I/O
/// is trivial; this is where the decoding — and the bugs — live.
#[derive(Debug, Clone)]
pub struct TouchState {
    /// Device coordinate range, used to scale into screen pixels.
    x_range: (f32, f32),
    y_range: (f32, f32),
    screen: (f32, f32),

    /// Currently addressed slot, set by `ABS_MT_SLOT`.
    current_slot: i32,
    active: HashMap<i32, Slot>,
    /// Highest number of simultaneous fingers seen during this touch sequence.
    peak_fingers: usize,
    sequence_started: Option<Instant>,
    /// Where the last finger was, so a tap can be reported at a position after release.
    last_release: (f32, f32),
    /// Whether `last_release` has ever been set from a real reading.
    have_release: bool,
    max_moved: f32,
}

/// Reads a touchscreen and turns its events into [`Gesture`]s.
pub struct TouchReader {
    device: Device,
    path: PathBuf,
    state: TouchState,
}

impl TouchReader {
    /// Find and open the touchscreen, scaling its coordinates to `screen`.
    pub fn open_auto(screen: (f32, f32)) -> Result<Self> {
        let mut candidates: Vec<(PathBuf, Device)> = evdev::enumerate()
            .filter(|(_, device)| has_multitouch(device))
            .collect();

        if candidates.is_empty() {
            return Err(anyhow!(
                "no multitouch device found. Check that dtoverlay=vc4-kms-dsi-7inch is set and \
                 that dtoverlay=rpi-ft5406 is NOT (they conflict). Without the KMS overlay the \
                 firmware touch path presents as `raspberrypi-ts` instead, which this does not \
                 use. Confirm with: grep -i ft5x06 /proc/bus/input/devices"
            ));
        }

        // Prefer a name that looks like the panel's controller; otherwise take the first
        // multitouch device and say which one, so a wrong guess is visible in the log.
        let index = candidates
            .iter()
            .position(|(_, device)| {
                let name = device.name().unwrap_or_default().to_ascii_lowercase();
                NAME_HINTS.iter().any(|hint| name.contains(hint))
            })
            .unwrap_or(0);

        let (path, device) = candidates.swap_remove(index);
        Self::from_device(device, path, screen)
    }

    pub fn open_path(path: PathBuf, screen: (f32, f32)) -> Result<Self> {
        let device = Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Self::from_device(device, path, screen)
    }

    fn from_device(device: Device, path: PathBuf, screen: (f32, f32)) -> Result<Self> {
        // Non-blocking: the render loop polls once per frame and must never wait on a finger.
        device
            .set_nonblocking(true)
            .context("setting the touch device non-blocking")?;

        let absinfo: HashMap<AbsoluteAxisCode, evdev::AbsInfo> = device
            .get_absinfo()
            .map(|iter| iter.collect())
            .unwrap_or_default();

        let axis_range = |axis: AbsoluteAxisCode, fallback: f32| -> (f32, f32) {
            match absinfo.get(&axis) {
                Some(info) if info.maximum() > info.minimum() => {
                    (info.minimum() as f32, info.maximum() as f32)
                }
                // Without a usable range, assume the axis already reports screen pixels. Better
                // than dividing by zero and putting every touch in one corner.
                _ => (0.0, fallback),
            }
        };

        let x_range = axis_range(AbsoluteAxisCode::ABS_MT_POSITION_X, screen.0);
        let y_range = axis_range(AbsoluteAxisCode::ABS_MT_POSITION_Y, screen.1);

        tracing::info!(
            device = %device.name().unwrap_or("<unnamed>"),
            path = %path.display(),
            x = ?x_range,
            y = ?y_range,
            "opened touch device"
        );

        Ok(Self {
            device,
            path,
            state: TouchState::new(x_range, y_range, screen),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Call when the drawable size changes so touches keep landing where they look.
    pub fn set_screen(&mut self, screen: (f32, f32)) {
        self.state.screen = screen;
    }

    /// Drain pending events and return any completed gestures. Never blocks.
    pub fn poll(&mut self) -> Result<Vec<Gesture>> {
        let mut gestures = Vec::new();

        let events: Vec<_> = match self.device.fetch_events() {
            Ok(iter) => iter.collect(),
            // WouldBlock just means nothing happened this frame.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(gestures),
            Err(e) => {
                return Err(anyhow!(
                    "reading {}: {e}. Was the panel unplugged?",
                    self.path.display()
                ))
            }
        };

        for event in events {
            // Translate to a device-independent event, then let the state machine decide. The
            // translation is deliberately dumb so that everything worth testing lives in
            // `TouchState::apply`, which needs no hardware.
            let decoded = match event.destructure() {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_SLOT, v) => {
                    Some(TouchEvent::Slot(v))
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_TRACKING_ID, v) => {
                    Some(TouchEvent::TrackingId(v))
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_X, v) => {
                    Some(TouchEvent::PositionX(v))
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_Y, v) => {
                    Some(TouchEvent::PositionY(v))
                }
                EventSummary::Key(_, KeyCode::BTN_TOUCH, v) => Some(TouchEvent::BtnTouch(v != 0)),
                _ => None,
            };
            if let Some(decoded) = decoded {
                self.state.apply(decoded, Instant::now(), &mut gestures);
            }
        }

        Ok(gestures)
    }
}

impl TouchState {
    pub fn new(x_range: (f32, f32), y_range: (f32, f32), screen: (f32, f32)) -> Self {
        Self {
            x_range,
            y_range,
            screen,
            current_slot: 0,
            active: HashMap::new(),
            peak_fingers: 0,
            sequence_started: None,
            last_release: (0.0, 0.0),
            have_release: false,
            max_moved: 0.0,
        }
    }

    fn scale(&self, raw_x: f32, raw_y: f32) -> (f32, f32) {
        scale_point((raw_x, raw_y), self.x_range, self.y_range, self.screen)
    }

    /// Feed one event. Appends any completed gestures.
    ///
    /// `now` is passed rather than read so a recorded event log can be replayed with a synthetic
    /// clock — otherwise every replayed tap would look instantaneous, or take however long the
    /// test took to run.
    pub fn apply(&mut self, event: TouchEvent, now: Instant, gestures: &mut Vec<Gesture>) {
        match event {
            TouchEvent::Slot(v) => self.current_slot = v,
            TouchEvent::TrackingId(v) => {
                if v >= 0 {
                    self.begin_contact(now);
                } else {
                    self.end_contact(gestures, now);
                }
            }
            TouchEvent::PositionX(v) => self.update_position(Some(v as f32), None),
            TouchEvent::PositionY(v) => self.update_position(None, Some(v as f32)),
            // A release reported only via BTN_TOUCH. Guarded on there being a live contact so the
            // BTN_TOUCH=0 that trails a tracking-id release is not counted a second time.
            TouchEvent::BtnTouch(false) if !self.active.is_empty() => {
                self.end_all_contacts(gestures, now)
            }
            TouchEvent::BtnTouch(_) => {}
        }
    }

    fn begin_contact(&mut self, now: Instant) {
        let slot = self.current_slot;
        self.active.insert(
            slot,
            Slot {
                start: (0.0, 0.0),
                latest: (0.0, 0.0),
                moved: 0.0,
                positioned: false,
                have_x: false,
                have_y: false,
            },
        );
        self.peak_fingers = self.peak_fingers.max(self.active.len());
        if self.sequence_started.is_none() {
            self.sequence_started = Some(now);
        }
    }

    fn update_position(&mut self, raw_x: Option<f32>, raw_y: Option<f32>) {
        let slot = self.current_slot;
        // A position for a slot we never saw start: treat it as a start rather than dropping it,
        // since some controllers emit position before the tracking id.
        if !self.active.contains_key(&slot) {
            self.begin_contact(Instant::now());
        }
        let (screen, x_range, y_range) = (self.screen, self.x_range, self.y_range);
        let Some(entry) = self.active.get_mut(&slot) else {
            return;
        };

        let mut raw = entry.latest;
        // Positions arrive one axis per event, so carry the other axis forward.
        if let Some(x) = raw_x {
            raw.0 = x;
            entry.have_x = true;
        }
        if let Some(y) = raw_y {
            raw.1 = y;
            entry.have_y = true;
        }
        entry.latest = raw;

        // Do nothing further until BOTH axes have been seen for this contact.
        //
        // The panel sends ABS_MT_POSITION_X then ABS_MT_POSITION_Y as separate events, so after
        // the first of them `raw` is half real and half leftover. Acting on that pinned every tap
        // to whatever y the previous contact ended at — in practice near zero, which is the top
        // of the screen, so every press registered as the topmost soft key.
        if !(entry.have_x && entry.have_y) {
            return;
        }

        let point = scale_point(raw, x_range, y_range, screen);

        if !entry.positioned {
            entry.start = point;
            entry.positioned = true;
        }
        let dx = point.0 - entry.start.0;
        let dy = point.1 - entry.start.1;
        entry.moved = entry.moved.max((dx * dx + dy * dy).sqrt());

        self.last_release = point;
        self.have_release = true;
        self.max_moved = self.max_moved.max(entry.moved);
    }

    fn end_contact(&mut self, gestures: &mut Vec<Gesture>, now: Instant) {
        let slot = self.current_slot;
        if let Some(finished) = self.active.remove(&slot) {
            // Only trust the position if both axes were seen; otherwise keep whatever the last
            // complete reading was rather than overwriting it with a half-formed one.
            if finished.have_x && finished.have_y {
                self.last_release = self.scale(finished.latest.0, finished.latest.1);
                self.have_release = true;
            }
        }
        if self.active.is_empty() {
            self.finish_sequence(gestures, now);
        }
    }

    fn end_all_contacts(&mut self, gestures: &mut Vec<Gesture>, now: Instant) {
        self.active.clear();
        self.finish_sequence(gestures, now);
    }

    /// All fingers are up: decide what the sequence was.
    fn finish_sequence(&mut self, gestures: &mut Vec<Gesture>, now: Instant) {
        let duration = self
            .sequence_started
            .map(|start| now.saturating_duration_since(start))
            .unwrap_or_default();
        let fingers = self.peak_fingers;
        let moved = self.max_moved;
        let positioned = self.have_release;

        self.peak_fingers = 0;
        self.sequence_started = None;
        self.max_moved = 0.0;
        self.have_release = false;

        if duration > TAP_MAX_DURATION || moved > TAP_MOVE_TOLERANCE_PX {
            // A drag or a long press. Neither is bound to anything yet; discarding is better than
            // guessing, because an accidental range change in flight is a real annoyance.
            tracing::trace!(?duration, moved, fingers, "ignoring non-tap touch");
            return;
        }

        match fingers {
            0 => {}
            // Without a complete position this tap would be reported at a coordinate nobody
            // touched. Dropping it is the honest outcome.
            1 if !positioned => tracing::debug!("dropping a tap with no complete position"),
            1 => gestures.push(Gesture::Tap {
                x: self.last_release.0,
                y: self.last_release.1,
            }),
            _ => gestures.push(Gesture::TwoFingerTap),
        }
    }
}

/// Scale a raw device coordinate pair into screen pixels.
fn scale_point(
    raw: (f32, f32),
    x_range: (f32, f32),
    y_range: (f32, f32),
    screen: (f32, f32),
) -> (f32, f32) {
    let span = |value: f32, range: (f32, f32), extent: f32| {
        let width = (range.1 - range.0).max(1.0);
        ((value - range.0) / width * extent).clamp(0.0, extent)
    };
    (
        span(raw.0, x_range, screen.0),
        span(raw.1, y_range, screen.1),
    )
}

/// Whether a device looks like a multitouch screen rather than a keyboard or a mouse.
fn has_multitouch(device: &Device) -> bool {
    device
        .supported_absolute_axes()
        .is_some_and(|axes| axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_X))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel's ranges, read off the target with EVIOCGABS.
    const X_RANGE: (f32, f32) = (0.0, 799.0);
    const Y_RANGE: (f32, f32) = (0.0, 479.0);
    const SCREEN: (f32, f32) = (800.0, 480.0);

    fn state() -> TouchState {
        TouchState::new(X_RANGE, Y_RANGE, SCREEN)
    }

    /// One touch, in exactly the event order the target emits.
    ///
    /// Captured from the real panel (`edt_ft5x06`, `/dev/input/event2`) with a raw evdev reader:
    ///
    /// ```text
    /// ABS_MT_TRACKING_ID = 90
    /// ABS_MT_POSITION_X  = 493
    /// ABS_MT_POSITION_Y  = 129
    /// BTN_TOUCH          = 1
    /// -- SYN_REPORT --
    /// ABS_MT_TRACKING_ID = -1
    /// BTN_TOUCH          = 0
    /// -- SYN_REPORT --
    /// ```
    ///
    /// Note X and Y arrive as **separate events**, and the release carries no position at all.
    fn tap_events(id: i32, x: i32, y: i32) -> Vec<TouchEvent> {
        vec![
            TouchEvent::TrackingId(id),
            TouchEvent::PositionX(x),
            TouchEvent::PositionY(y),
            TouchEvent::BtnTouch(true),
            TouchEvent::TrackingId(-1),
            TouchEvent::BtnTouch(false),
        ]
    }

    fn replay(state: &mut TouchState, events: &[TouchEvent]) -> Vec<Gesture> {
        let mut gestures = Vec::new();
        let now = Instant::now();
        for event in events {
            state.apply(*event, now, &mut gestures);
        }
        gestures
    }

    #[test]
    fn a_captured_tap_lands_where_the_finger_was() {
        // The regression. Because X and Y arrive as separate events, the decoder used to act on
        // the half-updated pair after X, pinning Y to whatever the previous contact left behind —
        // in practice ~0, the top of the screen. Every press therefore registered as the topmost
        // soft key: on the attitude page that is PAGE, which is why pressing LEVEL cycled pages
        // instead, and why no cage request was ever issued.
        let mut s = state();
        let gestures = replay(&mut s, &tap_events(90, 493, 129));

        assert_eq!(gestures.len(), 1, "one finger down and up is one tap");
        match gestures[0] {
            Gesture::Tap { x, y } => {
                assert!((x - 493.6).abs() < 1.0, "x was {x}, expected ~493");
                assert!(
                    (y - 129.3).abs() < 1.0,
                    "y was {y}, expected ~129 (was ~0 before)"
                );
            }
            other => panic!("expected a tap, got {other:?}"),
        }
    }

    #[test]
    fn the_bottom_of_the_screen_is_reachable() {
        // The specific failure the user hit: the LEVEL key sits at y 361..451, and no press there
        // ever registered.
        let mut s = state();
        let gestures = replay(&mut s, &tap_events(1, 752, 406));
        match gestures.as_slice() {
            [Gesture::Tap { x, y }] => {
                assert!(*x > 704.0, "x {x} should be inside the soft-key strip");
                assert!(
                    (361.0..451.0).contains(y),
                    "y {y} should land in the bottom slot, not the top one"
                );
            }
            other => panic!("expected one tap in the bottom slot, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_taps_do_not_inherit_each_others_coordinates() {
        // The mechanism behind the bug: state left over from the previous contact leaking into
        // the next one. Three taps down the screen must report three different positions.
        let mut s = state();
        let mut ys = Vec::new();
        for (i, y) in [40, 240, 440].iter().enumerate() {
            let gestures = replay(&mut s, &tap_events(i as i32 + 1, 750, *y));
            match gestures.as_slice() {
                [Gesture::Tap { y, .. }] => ys.push(*y),
                other => panic!("tap {i} produced {other:?}"),
            }
        }
        assert!(
            ys[0] < ys[1] && ys[1] < ys[2],
            "taps did not track the finger: {ys:?}"
        );
        assert!((ys[0] - 40.0).abs() < 1.5, "first tap y was {}", ys[0]);
        assert!((ys[2] - 440.0).abs() < 1.5, "third tap y was {}", ys[2]);
    }

    #[test]
    fn a_genuine_touch_at_the_top_left_corner_is_not_mistaken_for_no_reading() {
        // (0, 0) used to double as "no position yet", which is ambiguous with a real press in the
        // corner. It must still produce a tap.
        let mut s = state();
        let gestures = replay(&mut s, &tap_events(7, 0, 0));
        assert_eq!(
            gestures.len(),
            1,
            "the top-left corner is a valid place to press"
        );
        assert!(matches!(gestures[0], Gesture::Tap { .. }));
    }

    #[test]
    fn a_contact_with_no_position_is_dropped_rather_than_reported_at_zero() {
        // Seen in a real capture: a tracking id with no position events following it. Reporting
        // that as a tap would invent a press at a coordinate nobody touched.
        let mut s = state();
        let gestures = replay(
            &mut s,
            &[
                TouchEvent::TrackingId(47),
                TouchEvent::BtnTouch(true),
                TouchEvent::TrackingId(-1),
                TouchEvent::BtnTouch(false),
            ],
        );
        assert!(gestures.is_empty(), "expected no gesture, got {gestures:?}");
    }

    #[test]
    fn two_fingers_are_a_two_finger_tap() {
        let mut s = state();
        let mut gestures = Vec::new();
        let now = Instant::now();
        for e in [
            TouchEvent::Slot(0),
            TouchEvent::TrackingId(10),
            TouchEvent::PositionX(200),
            TouchEvent::PositionY(200),
            TouchEvent::Slot(1),
            TouchEvent::TrackingId(11),
            TouchEvent::PositionX(400),
            TouchEvent::PositionY(220),
            TouchEvent::Slot(0),
            TouchEvent::TrackingId(-1),
            TouchEvent::Slot(1),
            TouchEvent::TrackingId(-1),
        ] {
            s.apply(e, now, &mut gestures);
        }
        assert_eq!(gestures, vec![Gesture::TwoFingerTap]);
    }

    #[test]
    fn a_drag_is_not_a_tap() {
        let mut s = state();
        let mut gestures = Vec::new();
        let now = Instant::now();
        for e in [
            TouchEvent::TrackingId(20),
            TouchEvent::PositionX(100),
            TouchEvent::PositionY(100),
            TouchEvent::PositionX(300),
            TouchEvent::PositionY(300),
            TouchEvent::TrackingId(-1),
        ] {
            s.apply(e, now, &mut gestures);
        }
        assert!(gestures.is_empty(), "a drag must not fire a control");
    }

    #[test]
    fn a_long_press_is_not_a_tap() {
        let mut s = state();
        let mut gestures = Vec::new();
        let start = Instant::now();
        s.apply(TouchEvent::TrackingId(30), start, &mut gestures);
        s.apply(TouchEvent::PositionX(400), start, &mut gestures);
        s.apply(TouchEvent::PositionY(240), start, &mut gestures);
        // Released well after the tap window.
        let late = start + TAP_MAX_DURATION + Duration::from_millis(100);
        s.apply(TouchEvent::TrackingId(-1), late, &mut gestures);
        assert!(
            gestures.is_empty(),
            "a resting hand must not fire a control"
        );
    }
}
