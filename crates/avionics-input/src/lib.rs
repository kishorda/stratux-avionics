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
//! The axis ranges are 0..=799 by 0..=479, i.e. the controller reports in panel pixels and the
//! scaling into screen coordinates is the identity here. Do not simplify the scaling away: it is
//! a property of this panel, not of the protocol, and the 1024x600 variants of this class of
//! panel do not share it.
//!
//! Still unconfirmed, because it needs an actual finger rather than an SSH session: whether the
//! controller emits `BTN_TOUCH`, how many slots it reports, and whether `ABS_MT_TRACKING_ID`
//! lifetimes behave as the decoding below assumes. The gesture state machine is therefore still
//! unproven, even though device discovery and scaling are not.

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

#[derive(Debug, Clone, Copy)]
struct Slot {
    start: (f32, f32),
    latest: (f32, f32),
    moved: f32,
}

/// Reads a touchscreen and turns its events into [`Gesture`]s.
pub struct TouchReader {
    device: Device,
    path: PathBuf,
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
    max_moved: f32,
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
        let device =
            Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
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
            x_range,
            y_range,
            screen,
            current_slot: 0,
            active: HashMap::new(),
            peak_fingers: 0,
            sequence_started: None,
            last_release: (0.0, 0.0),
            max_moved: 0.0,
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Call when the drawable size changes so touches keep landing where they look.
    pub fn set_screen(&mut self, screen: (f32, f32)) {
        self.screen = screen;
    }

    fn scale(&self, raw_x: f32, raw_y: f32) -> (f32, f32) {
        scale_point((raw_x, raw_y), self.x_range, self.y_range, self.screen)
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
            match event.destructure() {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_SLOT, value) => {
                    self.current_slot = value;
                }
                // A non-negative tracking id starts a contact; -1 ends it.
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_TRACKING_ID, value) => {
                    if value >= 0 {
                        self.begin_contact();
                    } else {
                        self.end_contact(&mut gestures);
                    }
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_X, value) => {
                    self.update_position(Some(value as f32), None);
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_Y, value) => {
                    self.update_position(None, Some(value as f32));
                }
                // Some controllers report a single-touch release only via BTN_TOUCH.
                EventSummary::Key(_, KeyCode::BTN_TOUCH, 0) if !self.active.is_empty() => {
                    self.end_all_contacts(&mut gestures);
                }
                _ => {}
            }
        }

        Ok(gestures)
    }

    fn begin_contact(&mut self) {
        let slot = self.current_slot;
        self.active.insert(
            slot,
            Slot {
                start: (0.0, 0.0),
                latest: (0.0, 0.0),
                moved: 0.0,
            },
        );
        self.peak_fingers = self.peak_fingers.max(self.active.len());
        if self.sequence_started.is_none() {
            self.sequence_started = Some(Instant::now());
        }
    }

    fn update_position(&mut self, raw_x: Option<f32>, raw_y: Option<f32>) {
        let slot = self.current_slot;
        // A position for a slot we never saw start: treat it as a start rather than dropping it,
        // since some controllers emit position before the tracking id.
        if !self.active.contains_key(&slot) {
            self.begin_contact();
        }
        // Copy these out before taking the mutable borrow on the slot.
        let (screen, x_range, y_range) = (self.screen, self.x_range, self.y_range);
        let Some(entry) = self.active.get_mut(&slot) else {
            return;
        };

        let mut raw = entry.latest;
        // Positions arrive one axis per event, so carry the other axis forward.
        if let Some(x) = raw_x {
            raw.0 = x;
        }
        if let Some(y) = raw_y {
            raw.1 = y;
        }

        let point = scale_point(raw, x_range, y_range, screen);

        if entry.moved == 0.0 && entry.start == (0.0, 0.0) {
            entry.start = point;
        }
        let dx = point.0 - entry.start.0;
        let dy = point.1 - entry.start.1;
        entry.moved = entry.moved.max((dx * dx + dy * dy).sqrt());
        entry.latest = raw;

        self.last_release = point;
        self.max_moved = self.max_moved.max(entry.moved);
    }

    fn end_contact(&mut self, gestures: &mut Vec<Gesture>) {
        let slot = self.current_slot;
        if let Some(finished) = self.active.remove(&slot) {
            let (x, y) = self.scale(finished.latest.0, finished.latest.1);
            self.last_release = (x, y);
        }
        if self.active.is_empty() {
            self.finish_sequence(gestures);
        }
    }

    fn end_all_contacts(&mut self, gestures: &mut Vec<Gesture>) {
        self.active.clear();
        self.finish_sequence(gestures);
    }

    /// All fingers are up: decide what the sequence was.
    fn finish_sequence(&mut self, gestures: &mut Vec<Gesture>) {
        let duration = self
            .sequence_started
            .map(|start| start.elapsed())
            .unwrap_or_default();
        let fingers = self.peak_fingers;
        let moved = self.max_moved;

        self.peak_fingers = 0;
        self.sequence_started = None;
        self.max_moved = 0.0;

        if duration > TAP_MAX_DURATION || moved > TAP_MOVE_TOLERANCE_PX {
            // A drag or a long press. Neither is bound to anything yet; discarding is better than
            // guessing, because an accidental range change in flight is a real annoyance.
            tracing::trace!(?duration, moved, fingers, "ignoring non-tap touch");
            return;
        }

        match fingers {
            0 => {}
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
