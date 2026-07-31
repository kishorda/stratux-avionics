//! Font loading.
//!
//! Loaded from disk by path, trying a few well-known locations. DejaVuSans is present on both
//! Ubuntu and Raspberry Pi OS.
//!
//! TODO(M6): embed a font in the binary. A cockpit display that cannot draw text because
//! `fonts-dejavu-core` was removed by an unrelated `apt autoremove` is a reliability bug, not a
//! packaging inconvenience. Until then, M6 must pin the font package explicitly.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use avionics_gfx::femtovg::FontId;
use avionics_gfx::Canvas;

const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

/// Environment variable to override the font path, for testing an alternative.
pub const FONT_ENV: &str = "AVIONICS_FONT";

pub fn find() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os(FONT_ENV) {
        let path = PathBuf::from(override_path);
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!(
            "{FONT_ENV} points at {}, which does not exist",
            path.display()
        ));
    }

    CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "no usable font found; tried {CANDIDATES:?}. \
                 Install fonts-dejavu-core, or set {FONT_ENV} to a TTF path."
            )
        })
}

pub fn load(canvas: &mut Canvas) -> Result<FontId> {
    let path = find()?;
    let data = std::fs::read(&path).with_context(|| format!("reading font {}", path.display()))?;
    let id = canvas
        .add_font_mem(&data)
        .map_err(|e| anyhow!("loading font {}: {e}", path.display()))?;
    tracing::debug!(font = %path.display(), "loaded font");
    Ok(id)
}
