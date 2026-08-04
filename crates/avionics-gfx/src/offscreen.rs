//! Headless backend: a GLES2 context on a DRM *render node*, rendering into an off-screen
//! target that can be read back as pixels.
//!
//! Render nodes (`/dev/dri/renderD*`) don't require DRM master, so this works on the dev
//! machine with a desktop session running. Two uses:
//!
//! 1. Verifying drawing code on the dev machine without a Pi.
//! 2. Golden-image tests later, since [`OffscreenPresenter::read_pixels`] gives deterministic
//!    output.
//!
//! It deliberately shares the EGL setup shape with [`crate::kms`] so bugs found here are
//! likely to be the same bugs that would show up on the panel. It does *not* exercise
//! scanout, page flipping, or the `vc4` driver — only the real hardware can retire that risk.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use femtovg::renderer::OpenGl;
use femtovg::{ImageFlags, ImageId, PixelFormat, RenderTarget};
use gbm::AsRaw;

use crate::presenter::{query_gl_info, Canvas, GlInfo, Presenter, Pump};

const EGL_PLATFORM_GBM_KHR: khronos_egl::Enum = 0x31D7;

type Egl = khronos_egl::DynamicInstance<khronos_egl::EGL1_5>;

#[derive(Debug, Clone)]
pub struct OffscreenConfig {
    /// Render node to use. Defaults to `/dev/dri/renderD128`.
    pub device: PathBuf,
    pub width: u32,
    pub height: u32,
}

impl Default for OffscreenConfig {
    fn default() -> Self {
        Self {
            device: PathBuf::from("/dev/dri/renderD128"),
            width: 800,
            height: 480,
        }
    }
}

pub struct OffscreenPresenter {
    _gbm: Box<gbm::Device<std::fs::File>>,
    egl: Egl,
    egl_display: khronos_egl::Display,
    egl_context: khronos_egl::Context,
    canvas: Canvas,
    gl_info: GlInfo,
    target: ImageId,
    size: (u32, u32),
}

impl OffscreenPresenter {
    pub fn new(config: &OffscreenConfig) -> Result<Self> {
        let (w, h) = (config.width, config.height);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&config.device)
            .with_context(|| format!("opening render node {}", config.device.display()))?;

        let gbm = Box::new(gbm::Device::new(file).context("creating GBM device")?);

        let egl = unsafe { Egl::load_required() }.map_err(|e| anyhow!("loading libEGL: {e}"))?;

        let egl_display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_raw() as *mut c_void,
                &[khronos_egl::ATTRIB_NONE],
            )
        }
        .context("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR)")?;

        let (major, minor) = egl.initialize(egl_display).context("eglInitialize")?;
        tracing::info!("EGL {major}.{minor} (offscreen)");

        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .context("eglBindAPI(EGL_OPENGL_ES_API)")?;

        // No SURFACE_TYPE constraint: we never create an EGL surface, we render into an FBO.
        // Stencil is requested for the same reason as in the KMS path — femtovg fills paths
        // through the stencil buffer.
        let attribs = [
            khronos_egl::RENDERABLE_TYPE,
            khronos_egl::OPENGL_ES2_BIT,
            khronos_egl::RED_SIZE,
            8,
            khronos_egl::GREEN_SIZE,
            8,
            khronos_egl::BLUE_SIZE,
            8,
            khronos_egl::ALPHA_SIZE,
            8,
            khronos_egl::STENCIL_SIZE,
            8,
            khronos_egl::NONE,
        ];
        let egl_config = egl
            .choose_first_config(egl_display, &attribs)
            .context("eglChooseConfig")?
            .ok_or_else(|| anyhow!("no EGL config with GLES2 + 8-bit stencil"))?;

        let context_attribs = [khronos_egl::CONTEXT_CLIENT_VERSION, 2, khronos_egl::NONE];
        let egl_context = egl
            .create_context(egl_display, egl_config, None, &context_attribs)
            .context("eglCreateContext (GLES 2)")?;

        // Surfaceless: needs EGL_KHR_surfaceless_context, which Mesa provides.
        egl.make_current(egl_display, None, None, Some(egl_context))
            .context("eglMakeCurrent (surfaceless; needs EGL_KHR_surfaceless_context)")?;

        let gl_info = unsafe {
            query_gl_info(|name| {
                egl.get_proc_address(name)
                    .map(|p| p as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        tracing::info!(%gl_info, "GL context ready (offscreen)");

        let renderer = unsafe {
            OpenGl::new_from_function_cstr(|name| {
                egl.get_proc_address(name.to_str().unwrap_or_default())
                    .map(|p| p as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        }
        .map_err(|e| anyhow!("initialising femtovg OpenGl renderer: {e}"))?;

        let mut canvas =
            Canvas::new(renderer).map_err(|e| anyhow!("creating femtovg canvas: {e}"))?;
        canvas.set_size(w, h, 1.0);

        let target = canvas
            .create_image_empty(
                w as usize,
                h as usize,
                PixelFormat::Rgba8,
                ImageFlags::empty(),
            )
            .map_err(|e| anyhow!("creating offscreen render target: {e}"))?;
        bind_target(&mut canvas, target);

        Ok(Self {
            _gbm: gbm,
            egl,
            egl_display,
            egl_context,
            canvas,
            gl_info,
            target,
            size: (w, h),
        })
    }

    /// Read the last rendered frame back as tightly packed RGBA8, top row first.
    pub fn read_pixels(&mut self) -> Result<Vec<u8>> {
        let image = self
            .canvas
            .screenshot()
            .map_err(|e| anyhow!("reading back the offscreen target: {e}"))?;
        let mut out = Vec::with_capacity(image.width() * image.height() * 4);
        for pixel in image.pixels() {
            out.extend_from_slice(&[pixel.r, pixel.g, pixel.b, pixel.a]);
        }
        Ok(out)
    }

    /// Write the last rendered frame as a binary PPM (P6).
    ///
    /// PPM rather than PNG purely to avoid pulling an image encoder into a crate that ships
    /// on the aircraft; every common viewer and ImageMagick reads it.
    pub fn write_ppm(&mut self, path: &Path) -> Result<()> {
        let (w, h) = self.size;
        let rgba = self.read_pixels()?;
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        out.reserve(rgba.len() / 4 * 3);
        for chunk in rgba.chunks_exact(4) {
            out.extend_from_slice(&chunk[..3]);
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

impl Presenter for OffscreenPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn gl_info(&self) -> &GlInfo {
        &self.gl_info
    }

    fn pump(&mut self) -> Result<Pump> {
        Ok(Pump::Continue)
    }

    fn begin_frame(&mut self, clear: femtovg::Color) -> Result<&mut Canvas> {
        let (w, h) = self.size;
        // Deliberately no `set_size` here: it is fixed at construction, and calling it would
        // redirect drawing to the screen framebuffer. See `bind_target`.
        bind_target(&mut self.canvas, self.target);
        self.canvas.clear_rect(0, 0, w, h, clear);
        Ok(&mut self.canvas)
    }

    fn end_frame(&mut self) -> Result<()> {
        self.canvas.flush();
        Ok(())
    }
}

/// Point all subsequent drawing at `target`.
///
/// The detour through `RenderTarget::Screen` is load-bearing, not redundant. femtovg's
/// `Canvas::set_render_target` is a no-op when its cached `current_render_target` already
/// equals the requested one, but `Canvas::set_size` emits a `SetRenderTarget(Screen)` command
/// *without* updating that cache. Together those mean a plain `set_render_target(Image(..))`
/// can be silently dropped, leaving draws aimed at the default framebuffer — which in a
/// surfaceless context is incomplete, so every draw fails with
/// `GL_INVALID_FRAMEBUFFER_OPERATION` and the output comes back empty. Forcing the cache
/// through `Screen` guarantees the `Image` command is actually emitted.
fn bind_target(canvas: &mut Canvas, target: ImageId) {
    canvas.set_render_target(RenderTarget::Screen);
    canvas.set_render_target(RenderTarget::Image(target));
}

impl Drop for OffscreenPresenter {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.egl_display, None, None, None);
        let _ = self.egl.destroy_context(self.egl_display, self.egl_context);
        let _ = self.egl.terminate(self.egl_display);
    }
}
