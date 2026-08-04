//! Take the virtual console out of text mode so `fbcon` stops drawing over our scanout
//! buffer, and put it back on the way out.
//!
//! This is deliberately *not* a full VT-switch implementation (`VT_SETMODE` plus
//! acquire/release signal handling). The display is a single-purpose kiosk with nothing to
//! switch to, so taking the console unconditionally and restoring it on exit is both simpler
//! and more predictable than negotiating handover with a session manager we don't have.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

const KDSETMODE: u16 = 0x4B3A;
const KD_TEXT: i32 = 0x00;
const KD_GRAPHICS: i32 = 0x01;

// KDSETMODE predates the _IOC encoding scheme and takes its argument by value, hence the
// "_bad" (unencoded request number) variant.
nix::ioctl_write_int_bad!(kd_set_mode, KDSETMODE);

/// Owns the console's graphics mode for as long as it is alive.
pub struct Vt {
    file: File,
    restored: bool,
}

impl Vt {
    /// Put the current foreground VT into graphics mode.
    ///
    /// `/dev/tty0` always refers to the current foreground console, which is what we want
    /// whether we were started from a getty or over SSH.
    pub fn acquire() -> Result<Self> {
        Self::acquire_path(Path::new("/dev/tty0"))
    }

    pub fn acquire_path(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {} (are we root?)", path.display()))?;

        // SAFETY: fd is a valid open console device for the lifetime of the call.
        unsafe { kd_set_mode(file.as_raw_fd(), KD_GRAPHICS) }
            .with_context(|| format!("KDSETMODE(KD_GRAPHICS) on {}", path.display()))?;

        tracing::debug!(path = %path.display(), "console switched to graphics mode");
        Ok(Self {
            file,
            restored: false,
        })
    }

    /// Put the console back into text mode. Idempotent.
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        // SAFETY: fd is still open and is a console device.
        match unsafe { kd_set_mode(self.file.as_raw_fd(), KD_TEXT) } {
            Ok(_) => tracing::debug!("console restored to text mode"),
            // Nothing useful to do on failure during teardown beyond making it visible;
            // the user can always recover with chvt.
            Err(e) => tracing::warn!(error = %e, "failed to restore console to text mode"),
        }
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        self.restore();
    }
}
