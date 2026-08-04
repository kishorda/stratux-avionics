//! The NEXRAD precipitation underlay.
//!
//! # Why one texture
//!
//! A full picture is on the order of a hundred blocks of 128 bins each — over ten thousand little
//! rectangles. Drawing those as paths would be ten thousand draw calls per frame, which the Pi 3's
//! `vc4` will not do at 30 Hz. Instead every block is composited into a single RGBA texture in
//! CPU memory, uploaded once, and drawn as **one** textured quad.
//!
//! The texture is laid out in latitude/longitude, not screen space. That matters: if it were
//! screen-aligned, every heading change in track-up would invalidate it and force a re-composite
//! several times a second. In lat/lon it stays valid until the blocks themselves change, and
//! rotation becomes a transform on the single quad.
//!
//! The longitude span is divided by cos(latitude) so the texture covers a *square patch of
//! ground*. Since the projection scales longitude by the same factor, the quad stays square on
//! screen and one texel is one constant ground distance.
//!
//! # Cache invalidation
//!
//! Re-composite when any of these change:
//!
//! * the block set ([`AppState::nexrad_revision`]),
//! * own-ship has moved far enough that the patch no longer covers the view,
//! * a periodic refresh, so age-based fading actually progresses.
//!
//! # Intensity scales differ between the two products
//!
//! This is the part most likely to be silently wrong. Product 63 (regional) and product 64
//! (CONUS) do not share an intensity scale. Upstream's block decoder fills an *empty* regional
//! block with 0 and an empty CONUS block with 1, which is the tell: on the regional product 0
//! means "looked, below 5 dBZ", whereas on CONUS that state is 1 and 0 means "no data at all".
//! So the CONUS scale is effectively offset by one, and treating them alike paints either phantom
//! precipitation everywhere or holes through real coverage.
//!
//! Getting this right is exactly what the M5 verification step checks: replay a recorded FIS-B
//! session and compare the mosaic against an independent archived NWS mosaic for the same period.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use avionics_gfx::femtovg::rgb::RGBA8;
use avionics_gfx::femtovg::{imgref::ImgRef, ImageFlags, ImageId, Paint, Path, PixelFormat};
use avionics_gfx::Canvas;
use stratux_client::domain::{LatLon, NexradKind};
use stratux_client::AppState;

use crate::projection::{Projection, NM_PER_DEG_LAT};

/// Visible intensity colours, lightest to heaviest.
///
/// The conventional green / yellow / orange / red / magenta radar ramp, saturated a little harder
/// than a desktop palette would be because it has to survive daylight on a 7" panel.
const RAMP: [(u8, u8, u8); 7] = [
    (16, 150, 24),  // light
    (10, 120, 16),  // moderate green
    (235, 215, 60), // yellow
    (240, 160, 35), // orange
    (235, 60, 45),  // red
    (185, 28, 28),  // dark red
    (210, 45, 195), // magenta / extreme
];

/// Base opacity of the underlay. Weather must never obscure a traffic symbol.
const BASE_ALPHA: f32 = 0.72;

/// Colour for an intensity on a given product, or `None` for "draw nothing here".
///
/// See the module note: the CONUS scale is offset by one relative to regional.
pub fn colour(kind: NexradKind, intensity: u8) -> Option<(u8, u8, u8)> {
    let level = match kind {
        // 0 = valid data below 5 dBZ, i.e. nothing worth drawing.
        NexradKind::Regional => match intensity {
            0 => return None,
            other => other,
        },
        // 0 = no data, 1 = valid but no precipitation. Both are blank.
        NexradKind::Conus => match intensity {
            0 | 1 => return None,
            other => other - 1,
        },
    };
    RAMP.get((level as usize).saturating_sub(1)).copied()
}

/// Which fade step a block is in: 0 fresh, 1 aging, 2 stale.
///
/// Coarse on purpose. Fading is what drives re-compositing, and a continuous fade would mean
/// rebuilding the whole texture every frame. Three steps over the fifteen minutes a block lives
/// means at most two rebuilds per block for fade reasons.
pub fn fade_bucket(age: Duration) -> u8 {
    const FRESH: Duration = Duration::from_secs(5 * 60);
    const AGING: Duration = Duration::from_secs(10 * 60);
    if age <= FRESH {
        0
    } else if age <= AGING {
        1
    } else {
        2
    }
}

/// How opaque a block should be, given how long ago it arrived.
///
/// Fades rather than hides: precipitation that was there five minutes ago is still useful
/// information, provided the pilot can see that it is not current.
pub fn age_alpha(age: Duration) -> f32 {
    match fade_bucket(age) {
        0 => 1.0,
        1 => 0.7,
        _ => 0.45,
    }
}

/// A cheap fingerprint of every block's fade step.
///
/// Compositing is the one expensive thing this module does — measured at roughly 14 ms for a
/// 1024x1024 patch on a desktop, so several times that on a Pi 3, which is a visible multi-frame
/// hitch. Driving fade updates off an actual change in this fingerprint rather than off a wall-clock
/// timer cuts fade-driven rebuilds from one every thirty seconds to at most two per block lifetime.
pub fn fade_fingerprint<'a>(
    blocks: impl Iterator<Item = &'a stratux_client::domain::NexradBlock>,
    now: Instant,
) -> u64 {
    blocks.fold(0u64, |acc, block| {
        let bucket = fade_bucket(now.saturating_duration_since(block.received)) as u64;
        // Order-independent so a HashMap's iteration order cannot cause spurious rebuilds.
        acc.wrapping_add(bucket.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    })
}

#[derive(Debug, Clone)]
pub struct MosaicConfig {
    /// Texture edge length in pixels. Kept a power of two: GLES 2.0 only guarantees
    /// non-power-of-two textures with `CLAMP_TO_EDGE` and no mipmaps, and there is no reason to
    /// depend on that here.
    pub texture_size: usize,
    /// Half the ground extent covered, in nautical miles. Must exceed the largest range ring.
    pub half_span_nm: f64,
    /// Re-centre and re-composite once own-ship has moved this far from the patch centre.
    pub recentre_after_nm: f64,
    /// Backstop rebuild interval.
    ///
    /// Fading is handled by [`fade_fingerprint`], so this exists only to catch anything the
    /// invalidation checks miss. Deliberately long: a rebuild is expensive.
    pub refresh_interval: Duration,
}

impl Default for MosaicConfig {
    fn default() -> Self {
        Self {
            // 1024 x 1024 RGBA is 4 MB, and at 120 nm across gives ~8.5 px per nautical mile —
            // about 8 px per NEXRAD bin, which is more than the data actually resolves.
            texture_size: 1024,
            // The largest range ring is 40 nm; 60 leaves margin so the patch does not need
            // re-centring for every small manoeuvre.
            half_span_nm: 60.0,
            recentre_after_nm: 10.0,
            refresh_interval: Duration::from_secs(5 * 60),
        }
    }
}

/// Statistics from the last composite, for the status bar and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MosaicStats {
    pub blocks_composited: usize,
    pub blocks_skipped_outside: usize,
    pub bins_painted: usize,
    pub composites: u64,
}

/// A composited square patch of ground, in RGBA, ready to upload.
///
/// Split out from [`Mosaic`] so the geo-referencing can be tested without a GPU: build a patch from
/// known blocks and ask [`Patch::texel_at`] what colour landed at a given latitude and longitude.
/// Getting this wrong produces a picture that looks entirely plausible while being in the wrong
/// place, which is the single worst failure mode for a weather overlay.
pub struct Patch {
    pub pixels: Vec<RGBA8>,
    pub size: usize,
    pub centre: LatLon,
    pub half_span_lat_deg: f64,
    pub half_span_lon_deg: f64,
    pub stats: MosaicStats,
}

const TRANSPARENT: RGBA8 = RGBA8 {
    r: 0,
    g: 0,
    b: 0,
    a: 0,
};

impl Patch {
    /// Composite every visible bin of every block into a fresh patch centred on `centre`.
    pub fn build<'a>(
        config: &MosaicConfig,
        blocks: impl Iterator<Item = &'a stratux_client::domain::NexradBlock>,
        centre: LatLon,
        now: Instant,
    ) -> Self {
        let size = config.texture_size;

        // Square patch of *ground*: dividing the longitude span by cos(latitude) makes one texel a
        // constant ground distance in both axes, matching what the projection does, so the patch
        // stays square on screen.
        let half_span_lat_deg = config.half_span_nm / NM_PER_DEG_LAT;
        let cos_lat = centre.lat.to_radians().cos().max(1e-6);
        let half_span_lon_deg = half_span_lat_deg / cos_lat;

        let mut patch = Self {
            pixels: vec![TRANSPARENT; size * size],
            size,
            centre,
            half_span_lat_deg,
            half_span_lon_deg,
            stats: MosaicStats::default(),
        };

        let north = centre.lat + half_span_lat_deg;
        let south = centre.lat - half_span_lat_deg;
        let west = centre.lon - half_span_lon_deg;
        let east = centre.lon + half_span_lon_deg;
        let lat_span = half_span_lat_deg * 2.0;
        let lon_span = half_span_lon_deg * 2.0;

        for block in blocks {
            // Cheap reject: does the block overlap the patch at all?
            let block_south = block.lat_north - block.height_deg;
            let block_east = block.lon_west + block.width_deg;
            if block_south > north
                || block.lat_north < south
                || block_east < west
                || block.lon_west > east
            {
                patch.stats.blocks_skipped_outside += 1;
                continue;
            }

            let alpha = age_alpha(now.saturating_duration_since(block.received));
            let mut painted_any = false;

            for by in 0..stratux_client::domain::NexradBlock::BINS_Y {
                for bx in 0..stratux_client::domain::NexradBlock::BINS_X {
                    let Some(intensity) = block.intensity(bx, by) else {
                        continue;
                    };
                    let Some((r, g, b)) = colour(block.kind, intensity) else {
                        continue;
                    };
                    let Some((nw, se)) = block.bin_bounds(bx, by) else {
                        continue;
                    };

                    // Geographic bounds to texel bounds. Row 0 is the north edge.
                    let x0 = ((nw.lon - west) / lon_span * size as f64).floor() as isize;
                    let x1 = ((se.lon - west) / lon_span * size as f64).ceil() as isize;
                    let y0 = ((north - nw.lat) / lat_span * size as f64).floor() as isize;
                    let y1 = ((north - se.lat) / lat_span * size as f64).ceil() as isize;

                    let x0 = x0.clamp(0, size as isize) as usize;
                    let x1 = x1.clamp(0, size as isize) as usize;
                    let y0 = y0.clamp(0, size as isize) as usize;
                    let y1 = y1.clamp(0, size as isize) as usize;
                    if x0 >= x1 || y0 >= y1 {
                        continue;
                    }

                    let texel = RGBA8 {
                        r,
                        g,
                        b,
                        a: (255.0 * BASE_ALPHA * alpha) as u8,
                    };
                    for row in y0..y1 {
                        let base = row * size;
                        patch.pixels[base + x0..base + x1].fill(texel);
                    }
                    patch.stats.bins_painted += 1;
                    painted_any = true;
                }
            }

            if painted_any {
                patch.stats.blocks_composited += 1;
            }
        }

        patch
    }

    pub fn is_empty(&self) -> bool {
        self.stats.bins_painted == 0
    }

    /// Texel index for a position, or `None` if it falls outside the patch.
    pub fn texel_index(&self, position: LatLon) -> Option<usize> {
        let north = self.centre.lat + self.half_span_lat_deg;
        let west = self.centre.lon - self.half_span_lon_deg;
        let lat_span = self.half_span_lat_deg * 2.0;
        let lon_span = self.half_span_lon_deg * 2.0;

        let fx = (position.lon - west) / lon_span;
        let fy = (north - position.lat) / lat_span;
        if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
            return None;
        }
        let x = (fx * self.size as f64) as usize;
        let y = (fy * self.size as f64) as usize;
        Some(y * self.size + x)
    }

    /// The composited colour at a position, for verifying geo-referencing.
    pub fn texel_at(&self, position: LatLon) -> Option<RGBA8> {
        self.texel_index(position).map(|i| self.pixels[i])
    }
}

/// The cached precipitation texture.
pub struct Mosaic {
    config: MosaicConfig,
    image: Option<ImageId>,
    patch: Option<Patch>,
    composited_revision: Option<u64>,
    composited_fade: Option<u64>,
    last_composite: Option<Instant>,
    composites: u64,
}

impl Mosaic {
    pub fn new(config: MosaicConfig) -> Self {
        Self {
            config,
            image: None,
            patch: None,
            composited_revision: None,
            composited_fade: None,
            last_composite: None,
            composites: 0,
        }
    }

    pub fn stats(&self) -> MosaicStats {
        let mut stats = self
            .patch
            .as_ref()
            .map(|p| p.stats.clone())
            .unwrap_or_default();
        stats.composites = self.composites;
        stats
    }

    pub fn config(&self) -> &MosaicConfig {
        &self.config
    }

    /// Whether the cached texture needs rebuilding.
    pub fn needs_composite(&self, revision: u64, fade: u64, own: LatLon, now: Instant) -> bool {
        if self.image.is_none() || self.patch.is_none() {
            return true;
        }
        if self.composited_revision != Some(revision) {
            return true;
        }
        if self.composited_fade != Some(fade) {
            return true;
        }
        // Own-ship has walked too far from the patch centre for it to still cover the view.
        if let Some(patch) = &self.patch {
            if ground_distance_nm(patch.centre, own) > self.config.recentre_after_nm {
                return true;
            }
        }
        match self.last_composite {
            // A periodic rebuild is what makes age-based fading actually progress.
            Some(last) => now.saturating_duration_since(last) >= self.config.refresh_interval,
            None => true,
        }
    }

    /// Rebuild the texture if needed. Returns whether there is anything to draw.
    pub fn update(
        &mut self,
        canvas: &mut Canvas,
        state: &AppState,
        own: LatLon,
        now: Instant,
    ) -> Result<bool> {
        if state.nexrad.is_empty() {
            self.patch = None;
            return Ok(false);
        }
        let fade = fade_fingerprint(state.nexrad.values(), now);
        if !self.needs_composite(state.nexrad_revision, fade, own, now) {
            return Ok(self.patch.as_ref().is_some_and(|p| !p.is_empty()));
        }

        let patch = Patch::build(&self.config, state.nexrad.values(), own, now);
        let has_content = !patch.is_empty();
        if has_content {
            self.upload(canvas, &patch)?;
        }
        self.patch = Some(patch);
        self.composited_revision = Some(state.nexrad_revision);
        self.composited_fade = Some(fade);
        self.last_composite = Some(now);
        self.composites += 1;

        Ok(has_content)
    }

    fn upload(&mut self, canvas: &mut Canvas, patch: &Patch) -> Result<()> {
        let size = self.config.texture_size;

        let id = match self.image {
            Some(id) => id,
            None => {
                // NEAREST: NEXRAD bins are already coarse, and interpolating between intensity
                // levels invents gradients that imply precision the data does not have.
                let id = canvas
                    .create_image_empty(size, size, PixelFormat::Rgba8, ImageFlags::NEAREST)
                    .map_err(|e| anyhow!("allocating the {size}x{size} NEXRAD texture: {e}"))?;
                self.image = Some(id);
                id
            }
        };

        canvas
            .update_image(id, ImgRef::new(&patch.pixels, size, size), 0, 0)
            .map_err(|e| anyhow!("uploading the NEXRAD texture: {e}"))?;
        Ok(())
    }

    /// Draw the underlay as a single quad. Call before the rings and traffic.
    pub fn draw(&self, canvas: &mut Canvas, projection: &Projection) {
        let (Some(image), Some(patch)) = (self.image, self.patch.as_ref()) else {
            return;
        };
        if patch.is_empty() {
            return;
        }

        let (cx, cy) = projection.project(patch.centre);
        let half_px = projection.nm_to_px(self.config.half_span_nm as f32);
        let edge = half_px * 2.0;

        canvas.save();
        canvas.translate(cx, cy);
        // The texture's "up" is true north, so rotate it by north's screen angle. Using the same
        // helper the heading barbs use keeps the underlay and the traffic in agreement — if these
        // ever diverge, weather appears offset from the aircraft causing it.
        canvas.rotate(projection.screen_angle_rad(0.0));

        let paint = Paint::image(image, -half_px, -half_px, edge, edge, 0.0, 1.0);
        let mut quad = Path::new();
        quad.rect(-half_px, -half_px, edge, edge);
        canvas.fill_path(&quad, &paint);
        canvas.restore();
    }
}

/// Straight-line ground distance between two nearby positions, in nautical miles.
pub fn ground_distance_nm(from: LatLon, to: LatLon) -> f64 {
    let d_lat = (to.lat - from.lat) * NM_PER_DEG_LAT;
    let d_lon = (to.lon - from.lon) * NM_PER_DEG_LAT * from.lat.to_radians().cos();
    (d_lat * d_lat + d_lon * d_lon).sqrt()
}
