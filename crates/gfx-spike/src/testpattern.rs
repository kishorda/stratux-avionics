//! The M1 test pattern.
//!
//! This is not decoration. Each element exercises one femtovg capability that a later
//! milestone depends on, so that a failure on the Pi's `vc4` GLES2 driver shows up here as a
//! specific missing feature rather than as a mysteriously blank screen in M4:
//!
//! | Element              | Proves                                       | Needed by |
//! |----------------------|----------------------------------------------|-----------|
//! | Range rings          | Stencil-based path fill and stroke on arcs    | M4 plan view |
//! | Rotating symbols     | Nested transforms                             | M4 traffic symbols |
//! | Text block           | Glyph atlas upload and text shaping           | M4 tags, status bar |
//! | POT / NPOT mosaics   | `glTexImage2D` upload + image paint           | M5 NEXRAD underlay |
//! | Alpha wedge          | Blending                                      | M5 stale-weather fade |
//! | FPS counter          | Sustained throughput at native resolution      | all |
//!
//! The two mosaics are drawn separately and labelled on purpose. GLES 2.0 permits
//! non-power-of-two textures only with `CLAMP_TO_EDGE` and no mipmaps; if NPOT misbehaves on
//! `vc4` we want to learn that here and pad the NEXRAD mosaic to a power of two in M5, rather
//! than discovering it later.

use std::f32::consts::PI;
use std::path::{Path as FsPath, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use avionics_gfx::femtovg::imgref::ImgRef;
use avionics_gfx::femtovg::rgb::RGBA8;
use avionics_gfx::femtovg::{
    Align, Baseline, Color, FontId, ImageFlags, ImageId, Paint, Path, PixelFormat,
};
use avionics_gfx::{Canvas, GlInfo};

/// Fonts to try, in order. DejaVuSans is present on both Ubuntu and Raspberry Pi OS.
///
/// TODO(M4): ship an embedded font in the binary instead. A cockpit display must not fail to
/// draw text because a package was removed.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

fn find_font() -> Result<PathBuf> {
    FONT_CANDIDATES
        .iter()
        .map(FsPath::new)
        .find(|p| p.exists())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "no usable font found; tried {:?}. Install fonts-dejavu-core.",
                FONT_CANDIDATES
            )
        })
}

/// The ADS-B NEXRAD intensity palette, indexed 0..=7.
///
/// Kept here so the spike's mosaic looks like the real thing; M5 owns the authoritative
/// version along with the regional-vs-CONUS intensity distinction.
const NEXRAD_PALETTE: [(u8, u8, u8, u8); 8] = [
    (0, 0, 0, 0),        // 0: no/negligible return
    (0, 0, 0, 0),        // 1: valid, no precipitation
    (16, 140, 16, 190),  // 2: light
    (24, 200, 24, 205),  // 3
    (225, 210, 40, 215), // 4: moderate
    (235, 150, 30, 225), // 5
    (220, 50, 40, 235),  // 6: heavy
    (200, 40, 190, 245), // 7: extreme
];

pub struct TestPattern {
    font: FontId,
    mosaic_pot: ImageId,
    mosaic_npot: ImageId,
    mosaic_pot_size: (usize, usize),
    mosaic_npot_size: (usize, usize),
    start: Instant,
    frame_count: u64,
    fps: f32,
    fps_window_start: Instant,
    fps_window_frames: u64,
}

impl TestPattern {
    pub fn new(canvas: &mut Canvas) -> Result<Self> {
        let font_path = find_font()?;
        let font_data = std::fs::read(&font_path)
            .with_context(|| format!("reading font {}", font_path.display()))?;
        let font = canvas
            .add_font_mem(&font_data)
            .map_err(|e| anyhow!("loading font {}: {e}", font_path.display()))?;
        tracing::info!(font = %font_path.display(), "loaded font");

        let mosaic_pot_size = (64, 64);
        let mosaic_npot_size = (100, 60);
        let mosaic_pot = upload_mosaic(canvas, mosaic_pot_size.0, mosaic_pot_size.1)?;
        let mosaic_npot = upload_mosaic(canvas, mosaic_npot_size.0, mosaic_npot_size.1)?;

        let now = Instant::now();
        Ok(Self {
            font,
            mosaic_pot,
            mosaic_npot,
            mosaic_pot_size,
            mosaic_npot_size,
            start: now,
            frame_count: 0,
            fps: 0.0,
            fps_window_start: now,
            fps_window_frames: 0,
        })
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub fn fps(&self) -> f32 {
        self.fps
    }

    /// Advance frame/FPS bookkeeping. Call once per rendered frame.
    fn tick(&mut self) {
        self.frame_count += 1;
        self.fps_window_frames += 1;
        let elapsed = self.fps_window_start.elapsed().as_secs_f32();
        if elapsed >= 0.5 {
            self.fps = self.fps_window_frames as f32 / elapsed;
            self.fps_window_frames = 0;
            self.fps_window_start = Instant::now();
        } else if self.fps == 0.0 {
            // Until the first averaging window closes, fall back to the running average.
            // Otherwise a short run (`--frames 60` offscreen, which finishes in well under
            // 0.5 s) reports 0.0 fps and the summary looks like a failure.
            let total = self.start.elapsed().as_secs_f32();
            if total > 0.0 {
                self.fps = self.frame_count as f32 / total;
            }
        }
    }

    pub fn draw(&mut self, canvas: &mut Canvas, gl: &GlInfo) {
        let (w, h) = (canvas.width() as f32, canvas.height() as f32);
        let t = self.start.elapsed().as_secs_f32();

        // Weather underlay sits beneath everything, as it will in the real plan view.
        self.draw_mosaics(canvas, w, h);
        self.draw_range_rings(canvas, w, h);
        self.draw_rotating_symbols(canvas, w, h, t);
        self.draw_alpha_wedge(canvas, w, h);
        self.draw_text_block(canvas, w, h, gl);

        self.tick();
    }

    /// Concentric filled + stroked rings: the stencil path-fill workout.
    fn draw_range_rings(&self, canvas: &mut Canvas, w: f32, h: f32) {
        let (cx, cy) = (w * 0.5, h * 0.55);
        let max_r = (w.min(h) * 0.42).max(20.0);

        for (i, frac) in [1.0f32, 0.66, 0.33].iter().enumerate() {
            let r = max_r * frac;
            let mut path = Path::new();
            path.circle(cx, cy, r);
            let shade = 0.16 - i as f32 * 0.04;
            canvas.fill_path(&path, &Paint::color(Color::rgbaf(0.2, 0.5, 0.9, shade)));
            canvas.stroke_path(
                &path,
                &Paint::color(Color::rgbaf(0.35, 0.75, 1.0, 0.85)).with_line_width(1.5),
            );
        }

        // Cardinal ticks: many short strokes, which is what target tags will look like.
        for deg in (0..360).step_by(30) {
            let a = (deg as f32).to_radians() - PI / 2.0;
            let (inner, outer) = (max_r * 0.94, max_r);
            let mut path = Path::new();
            path.move_to(cx + a.cos() * inner, cy + a.sin() * inner);
            path.line_to(cx + a.cos() * outer, cy + a.sin() * outer);
            canvas.stroke_path(
                &path,
                &Paint::color(Color::rgbaf(0.5, 0.8, 1.0, 0.7)).with_line_width(1.0),
            );
        }
    }

    /// Nested transforms plus a filled non-convex polygon per symbol.
    fn draw_rotating_symbols(&self, canvas: &mut Canvas, w: f32, h: f32, t: f32) {
        let (cx, cy) = (w * 0.5, h * 0.55);
        let orbit = (w.min(h) * 0.42).max(20.0) * 0.72;

        for i in 0..6 {
            let phase = t * 0.35 + i as f32 * PI / 3.0;
            let (x, y) = (cx + phase.cos() * orbit, cy + phase.sin() * orbit);

            canvas.save();
            canvas.translate(x, y);
            canvas.rotate(phase + PI / 2.0);

            // Chevron: non-convex, so it genuinely needs the stencil path.
            let s = 9.0;
            let mut path = Path::new();
            path.move_to(0.0, -s);
            path.line_to(s * 0.8, s * 0.8);
            path.line_to(0.0, s * 0.35);
            path.line_to(-s * 0.8, s * 0.8);
            path.close();

            let colour = if i % 3 == 0 {
                Color::rgb(255, 80, 60) // "alert" tier
            } else if i % 3 == 1 {
                Color::rgb(255, 200, 40) // "advisory" tier
            } else {
                Color::rgb(220, 235, 245)
            };
            canvas.fill_path(&path, &Paint::color(colour));
            canvas.stroke_path(
                &path,
                &Paint::color(Color::rgb(10, 14, 20)).with_line_width(1.0),
            );
            canvas.restore();

            // A tag next to each symbol, as the real display will have.
            let mut tag = Paint::color(Color::rgb(200, 220, 235));
            tag.set_font(&[self.font]);
            tag.set_font_size(11.0);
            tag.set_text_align(Align::Left);
            tag.set_text_baseline(Baseline::Middle);
            let _ = canvas.fill_text(
                x + 14.0,
                y,
                format!("N{:03}TC +{:02}", 100 + i * 7, i * 3),
                &tag,
            );
        }

        // Own-ship at the centre.
        let mut own = Path::new();
        own.move_to(cx, cy - 11.0);
        own.line_to(cx + 8.0, cy + 8.0);
        own.line_to(cx, cy + 4.0);
        own.line_to(cx - 8.0, cy + 8.0);
        own.close();
        canvas.fill_path(&own, &Paint::color(Color::rgb(120, 255, 170)));
    }

    /// Texture upload + image paint, drawn twice at different dimensions.
    fn draw_mosaics(&self, canvas: &mut Canvas, w: f32, h: f32) {
        let tile_w = w * 0.26;
        let tile_h = tile_w * 0.6;
        let y = h - tile_h - 26.0;

        for (i, (id, (iw, ih), label)) in [
            (self.mosaic_pot, self.mosaic_pot_size, "POT 64x64"),
            (self.mosaic_npot, self.mosaic_npot_size, "NPOT 100x60"),
        ]
        .iter()
        .enumerate()
        {
            let x = 12.0 + i as f32 * (tile_w + 12.0);
            let paint = Paint::image(*id, x, y, tile_w, tile_h, 0.0, 1.0);
            let mut rect = Path::new();
            rect.rect(x, y, tile_w, tile_h);
            canvas.fill_path(&rect, &paint);
            canvas.stroke_path(
                &rect,
                &Paint::color(Color::rgbaf(1.0, 1.0, 1.0, 0.35)).with_line_width(1.0),
            );

            let mut text = Paint::color(Color::rgb(190, 205, 220));
            text.set_font(&[self.font]);
            text.set_font_size(11.0);
            text.set_text_baseline(Baseline::Top);
            let _ = canvas.fill_text(x, y + tile_h + 3.0, format!("{label} ({iw}x{ih})"), &text);
        }
    }

    /// A left-to-right alpha ramp: if blending is broken this reads as hard bands.
    fn draw_alpha_wedge(&self, canvas: &mut Canvas, w: f32, h: f32) {
        let steps = 16;
        let bw = w * 0.5 / steps as f32;
        for i in 0..steps {
            let a = i as f32 / (steps - 1) as f32;
            let mut path = Path::new();
            path.rect(w * 0.5 + i as f32 * bw, h - 18.0, bw, 10.0);
            canvas.fill_path(&path, &Paint::color(Color::rgbaf(0.4, 0.9, 1.0, a)));
        }
    }

    fn draw_text_block(&self, canvas: &mut Canvas, w: f32, _h: f32, gl: &GlInfo) {
        // Status bar background.
        let mut bar = Path::new();
        bar.rect(0.0, 0.0, w, 46.0);
        canvas.fill_path(&bar, &Paint::color(Color::rgbaf(0.05, 0.07, 0.10, 0.92)));

        let mut title = Paint::color(Color::rgb(235, 245, 255));
        title.set_font(&[self.font]);
        title.set_font_size(17.0);
        title.set_text_baseline(Baseline::Top);
        let _ = canvas.fill_text(10.0, 5.0, "M1 GFX SPIKE", &title);

        let mut small = Paint::color(Color::rgb(150, 175, 195));
        small.set_font(&[self.font]);
        small.set_font_size(11.0);
        small.set_text_baseline(Baseline::Top);
        let _ = canvas.fill_text(
            10.0,
            26.0,
            format!(
                "{}  |  {}  |  GLES2={}",
                truncate(&gl.renderer, 34),
                truncate(&gl.version, 26),
                gl.is_gles2
            ),
            &small,
        );

        // FPS, right-aligned so it doesn't jitter the layout as digits change.
        let mut fps = Paint::color(if self.fps >= 28.0 {
            Color::rgb(120, 255, 170)
        } else {
            Color::rgb(255, 180, 60)
        });
        fps.set_font(&[self.font]);
        fps.set_font_size(20.0);
        fps.set_text_align(Align::Right);
        fps.set_text_baseline(Baseline::Top);
        let _ = canvas.fill_text(w - 10.0, 6.0, format!("{:.1} fps", self.fps), &fps);

        let mut frames = Paint::color(Color::rgb(140, 160, 180));
        frames.set_font(&[self.font]);
        frames.set_font_size(11.0);
        frames.set_text_align(Align::Right);
        frames.set_text_baseline(Baseline::Top);
        let _ = canvas.fill_text(
            w - 10.0,
            29.0,
            format!("frame {}", self.frame_count),
            &frames,
        );

        // A range of sizes, since the glyph atlas is per-size and small text on a 7" panel is
        // where atlas problems show first.
        for (i, size) in [9.0f32, 11.0, 14.0, 18.0].iter().enumerate() {
            let mut p = Paint::color(Color::rgb(205, 220, 235));
            p.set_font(&[self.font]);
            p.set_font_size(*size);
            p.set_text_baseline(Baseline::Top);
            let _ = canvas.fill_text(
                10.0,
                56.0 + i as f32 * (size + 4.0),
                format!("{size:>4.0}px  METAR KDEN 291853Z 04012KT 10SM FEW120 28/07 A3002"),
                &p,
            );
        }
    }
}

/// Build a plausible-looking precipitation mosaic and upload it as an RGBA8 texture.
fn upload_mosaic(canvas: &mut Canvas, w: usize, h: usize) -> Result<ImageId> {
    let id = canvas
        .create_image_empty(w, h, PixelFormat::Rgba8, ImageFlags::empty())
        .map_err(|e| anyhow!("allocating {w}x{h} mosaic texture: {e}"))?;

    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            // Two offset radial blobs, quantised to the 8 FIS-B intensity levels.
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let d1 = ((fx - 0.35).powi(2) + (fy - 0.45).powi(2)).sqrt();
            let d2 = ((fx - 0.72).powi(2) + (fy - 0.6).powi(2)).sqrt();
            let v = (1.0 - (d1 * 3.2).min(1.0)).max(1.0 - (d2 * 4.0).min(1.0));
            let level = (v * 7.5).clamp(0.0, 7.0) as usize;
            let (r, g, b, a) = NEXRAD_PALETTE[level];
            pixels.push(RGBA8 { r, g, b, a });
        }
    }

    canvas
        .update_image(id, ImgRef::new(&pixels, w, h), 0, 0)
        .map_err(|e| anyhow!("uploading {w}x{h} mosaic texture: {e}"))?;
    Ok(id)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
