//! Direct-to-display backend: DRM/KMS scanout fed by an EGL-on-GBM GLES2 context.
//!
//! There is no compositor and no window system in this path. We take DRM master, pick the
//! connected panel's preferred mode, render into a GBM surface with femtovg, and page-flip
//! the resulting buffer onto the CRTC.

mod vt;

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use drm::control::{
    connector, crtc, framebuffer, Device as ControlDevice, Event, Mode, ModeTypeFlags,
    PageFlipFlags,
};
use drm::Device as BasicDevice;
use femtovg::renderer::OpenGl;
use gbm::{AsRaw, BufferObject, BufferObjectFlags, Format as GbmFormat};

use crate::presenter::{query_gl_info, Canvas, GlInfo, Presenter, Pump};

pub use vt::Vt;

/// `EGL_PLATFORM_GBM_KHR`, from the `EGL_KHR_platform_gbm` extension. Also accepted by
/// core `eglGetPlatformDisplay` on EGL 1.5, which is what we use.
const EGL_PLATFORM_GBM_KHR: khronos_egl::Enum = 0x31D7;

/// `DRM_FORMAT_XRGB8888` — `fourcc_code('X', 'R', '2', '4')`.
///
/// On the GBM platform, EGL reports a config's `EGL_NATIVE_VISUAL_ID` as the DRM fourcc, so
/// this is how we pick a config whose buffers the display controller can actually scan out.
const FOURCC_XRGB8888: u32 = 0x3432_5258;

type Egl = khronos_egl::DynamicInstance<khronos_egl::EGL1_5>;

/// A DRM device node.
///
/// `Arc<File>` rather than a plain `File` so the same fd can back both the DRM control
/// interface and the GBM device without dup'ing, while staying `Clone` for `gbm::Device`.
#[derive(Debug, Clone)]
pub struct Card(Arc<File>);

impl Card {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening DRM node {}", path.display()))?;
        Ok(Self(Arc::new(file)))
    }
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

/// How to set up the KMS presenter.
#[derive(Debug, Clone)]
pub struct KmsConfig {
    /// DRM node to use. Defaults to `/dev/dri/card0`.
    pub device: PathBuf,
    /// Take the console out of text mode so `fbcon` doesn't draw over us.
    pub take_vt: bool,
    /// Try to become DRM master explicitly. Harmless (and usually redundant) when nothing
    /// else holds it; logged and ignored on failure.
    pub acquire_master: bool,
}

impl Default for KmsConfig {
    fn default() -> Self {
        Self {
            device: PathBuf::from("/dev/dri/card0"),
            take_vt: true,
            acquire_master: true,
        }
    }
}

pub struct KmsPresenter {
    card: Card,
    // The GBM surface borrows the device internally, so the device must outlive it and must
    // not move. Boxed and held for the life of the presenter.
    _gbm: Box<gbm::Device<Card>>,
    surface: gbm::Surface<framebuffer::Handle>,

    egl: Egl,
    egl_display: khronos_egl::Display,
    egl_surface: khronos_egl::Surface,
    egl_context: khronos_egl::Context,

    canvas: Canvas,
    gl_info: GlInfo,

    crtc: crtc::Handle,
    connector: connector::Handle,
    mode: Mode,
    size: (u32, u32),

    /// Buffer currently being scanned out. Held so it isn't released back to GBM while the
    /// display controller is still reading from it.
    front: Option<BufferObject<framebuffer::Handle>>,
    modeset_done: bool,

    _vt: Option<Vt>,
}

impl KmsPresenter {
    pub fn new(config: &KmsConfig) -> Result<Self> {
        let card = Card::open(&config.device)?;

        if config.acquire_master {
            if let Err(e) = card.acquire_master_lock() {
                // Expected to fail if a compositor is running; the later modeset will give a
                // clearer error, so don't hard-fail here.
                tracing::warn!(error = %e, "could not acquire DRM master (is a compositor running?)");
            }
        }

        let (connector, mode, crtc) = select_output(&card)?;
        let (w, h) = (mode.size().0 as u32, mode.size().1 as u32);
        tracing::info!(
            connector = ?connector_name(&card, connector),
            mode = %format!("{w}x{h}@{}", mode.vrefresh()),
            "selected output"
        );

        // VT is taken only once we know we have a usable output, so a failed probe doesn't
        // leave the console in graphics mode.
        let vt = if config.take_vt {
            Some(Vt::acquire().context("taking over the console")?)
        } else {
            None
        };

        let gbm = Box::new(gbm::Device::new(card.clone()).context("creating GBM device")?);
        let surface = gbm
            .create_surface::<framebuffer::Handle>(
                w,
                h,
                GbmFormat::Xrgb8888,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
            )
            .context("creating GBM scanout surface")?;

        let egl = unsafe { Egl::load_required() }
            .map_err(|e| anyhow!("loading libEGL: {e}"))?;

        let egl_display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_raw() as *mut c_void,
                &[khronos_egl::ATTRIB_NONE],
            )
        }
        .context("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR)")?;

        let (major, minor) = egl.initialize(egl_display).context("eglInitialize")?;
        tracing::info!("EGL {major}.{minor}");

        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .context("eglBindAPI(EGL_OPENGL_ES_API)")?;

        let egl_config = choose_config(&egl, egl_display)?;

        // vc4 on the Pi 3 is GLES 2.0 only, so ask for exactly that. Requesting 3.x here
        // would fail outright rather than silently downgrading.
        let context_attribs = [
            khronos_egl::CONTEXT_CLIENT_VERSION,
            2,
            khronos_egl::NONE,
        ];
        let egl_context = egl
            .create_context(egl_display, egl_config, None, &context_attribs)
            .context("eglCreateContext (GLES 2)")?;

        let egl_surface = unsafe {
            egl.create_window_surface(
                egl_display,
                egl_config,
                surface.as_raw() as khronos_egl::NativeWindowType,
                None,
            )
        }
        .context("eglCreateWindowSurface on the GBM surface")?;

        egl.make_current(
            egl_display,
            Some(egl_surface),
            Some(egl_surface),
            Some(egl_context),
        )
        .context("eglMakeCurrent")?;

        let gl_info = unsafe {
            query_gl_info(|name| {
                egl.get_proc_address(name)
                    .map(|p| p as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        tracing::info!(%gl_info, "GL context ready");

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

        Ok(Self {
            card,
            _gbm: gbm,
            surface,
            egl,
            egl_display,
            egl_surface,
            egl_context,
            canvas,
            gl_info,
            crtc,
            connector,
            mode,
            size: (w, h),
            front: None,
            modeset_done: false,
            _vt: vt,
        })
    }

    /// Look up (or create and cache) the DRM framebuffer for a GBM buffer object.
    ///
    /// GBM recycles a small pool of buffers, so caching the framebuffer handle in the BO's
    /// userdata means `add_framebuffer` runs a couple of times at startup rather than on
    /// every single frame.
    fn framebuffer_for(
        &self,
        bo: &mut BufferObject<framebuffer::Handle>,
    ) -> Result<framebuffer::Handle> {
        if let Some(fb) = bo.userdata() {
            return Ok(*fb);
        }
        let fb = self
            .card
            .add_framebuffer(bo, 24, 32)
            .context("drmModeAddFB for a GBM buffer")?;
        bo.set_userdata(fb);
        tracing::debug!(?fb, "cached framebuffer for new GBM buffer");
        Ok(fb)
    }

    /// Block until the pending page flip has completed.
    fn wait_for_flip(&self) -> Result<()> {
        loop {
            let events = match self.card.receive_events() {
                Ok(events) => events,
                // A signal interrupts this blocking read: our SIGINT/SIGTERM handlers are
                // installed without SA_RESTART, precisely so the process notices them. The page
                // flip is still queued, and the previous buffer must not be released until it
                // lands, so the only correct response is to read again — the event arrives
                // within one refresh and the main loop then sees the exit flag.
                //
                // Treating EINTR as fatal instead made every clean shutdown fail: Ctrl-C and
                // `systemctl stop` both exited with "reading DRM events: Interrupted system
                // call" and skipped the render-timing summary on the way out.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading DRM events"),
            };
            for event in events {
                if matches!(event, Event::PageFlip(_)) {
                    return Ok(());
                }
            }
        }
    }
}

impl Presenter for KmsPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn gl_info(&self) -> &GlInfo {
        &self.gl_info
    }

    fn pump(&mut self) -> Result<Pump> {
        // Nothing to poll: there is no window system, and input arrives separately through
        // evdev. Exit is driven by signals, not by the presenter.
        Ok(Pump::Continue)
    }

    fn begin_frame(&mut self, clear: femtovg::Color) -> Result<&mut Canvas> {
        let (w, h) = self.size;
        // `set_size` is called once at construction, not here. The mode cannot change under
        // us (no hotplug handling on a fixed DSI panel), and femtovg's `set_size` quietly
        // emits a `SetRenderTarget(Screen)` command, so calling it per frame is both wasted
        // work and a footgun if a render target is ever introduced in this path.
        self.canvas.clear_rect(0, 0, w, h, clear);
        Ok(&mut self.canvas)
    }

    fn end_frame(&mut self) -> Result<()> {
        self.canvas.flush();

        self.egl
            .swap_buffers(self.egl_display, self.egl_surface)
            .context("eglSwapBuffers")?;

        let mut bo = unsafe { self.surface.lock_front_buffer() }
            .context("gbm_surface_lock_front_buffer")?;
        let fb = self.framebuffer_for(&mut bo)?;

        if !self.modeset_done {
            self.card
                .set_crtc(
                    self.crtc,
                    Some(fb),
                    (0, 0),
                    &[self.connector],
                    Some(self.mode),
                )
                .context("drmModeSetCrtc (initial modeset)")?;
            self.modeset_done = true;
        } else {
            self.card
                .page_flip(self.crtc, fb, PageFlipFlags::EVENT, None)
                .context("drmModePageFlip")?;
            self.wait_for_flip()?;
        }

        // Only now is the previous buffer definitely no longer being scanned out.
        self.front = Some(bo);
        Ok(())
    }
}

impl Drop for KmsPresenter {
    fn drop(&mut self) {
        // Release GL resources before the VT is restored (Vt is dropped after these fields)
        // so the console comes back to a quiescent device.
        let _ = self.egl.make_current(self.egl_display, None, None, None);
        let _ = self.egl.destroy_surface(self.egl_display, self.egl_surface);
        let _ = self.egl.destroy_context(self.egl_display, self.egl_context);
        let _ = self.egl.terminate(self.egl_display);
        let _ = self.card.release_master_lock();
    }
}

/// What a DRM node looks like, without disturbing it.
#[derive(Debug, Clone)]
pub struct Probe {
    pub device: PathBuf,
    /// Every connector, as `("DSI-1", connected, preferred mode description)`.
    pub connectors: Vec<(String, bool, Option<String>)>,
    /// Whether an output was found that could actually be driven.
    pub usable_output: Option<String>,
}

/// Inspect a DRM node read-only: no DRM master, no modeset, no VT takeover.
///
/// This is what `avionics --check` uses. Constructing a [`KmsPresenter`] would blank the console
/// and take the display over, which is exactly what you do not want from a verification command
/// run over SSH while something else is on screen.
pub fn probe(device: &Path) -> Result<Probe> {
    let card = Card::open(device)?;
    let resources = card
        .resource_handles()
        .context("reading DRM resource handles")?;

    let mut connectors = Vec::new();
    for handle in resources.connectors() {
        let Ok(info) = card.get_connector(*handle, false) else {
            continue;
        };
        let name = format!("{:?}-{}", info.interface(), info.interface_id());
        let connected = info.state() == connector::State::Connected;
        let mode = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().first())
            .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh()));
        connectors.push((name, connected, mode));
    }

    // Reuse the real selection logic so a successful check means the real path will work too.
    let usable_output = select_output(&card).ok().map(|(connector, mode, crtc)| {
        format!(
            "{} {}x{}@{} on {:?}",
            connector_name(&card, connector),
            mode.size().0,
            mode.size().1,
            mode.vrefresh(),
            crtc
        )
    });

    Ok(Probe {
        device: device.to_path_buf(),
        connectors,
        usable_output,
    })
}

/// Pick a connected connector, its preferred mode, and a CRTC that can drive it.
fn select_output(card: &Card) -> Result<(connector::Handle, Mode, crtc::Handle)> {
    let resources = card
        .resource_handles()
        .context("reading DRM resource handles")?;

    let connector = resources
        .connectors()
        .iter()
        .filter_map(|&handle| card.get_connector(handle, false).ok())
        .find(|info| info.state() == connector::State::Connected)
        .ok_or_else(|| {
            anyhow!("no connected display found; check the DSI ribbon and the vc4-kms-dsi-7inch overlay")
        })?;

    // Trust the panel's own preferred mode rather than assuming 800x480 — DSI panels in this
    // class also ship as 1024x600.
    let mode = connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
        .ok_or_else(|| anyhow!("connector {:?} reports no modes", connector.interface()))?;

    // Prefer the CRTC the connector is already wired to; otherwise take the first CRTC any
    // of its encoders can use.
    let current = connector
        .current_encoder()
        .and_then(|enc| card.get_encoder(enc).ok())
        .and_then(|enc| enc.crtc());

    let crtc = match current {
        Some(crtc) => crtc,
        None => connector
            .encoders()
            .iter()
            .filter_map(|&enc| card.get_encoder(enc).ok())
            .flat_map(|enc| resources.filter_crtcs(enc.possible_crtcs()))
            .next()
            .ok_or_else(|| anyhow!("no CRTC can drive connector {:?}", connector.interface()))?,
    };

    Ok((connector.handle(), mode, crtc))
}

fn connector_name(card: &Card, handle: connector::Handle) -> String {
    card.get_connector(handle, false)
        .map(|c| format!("{:?}-{}", c.interface(), c.interface_id()))
        .unwrap_or_else(|_| "<unknown>".into())
}

/// Find an EGL config that is renderable as GLES2, has a stencil buffer, and whose buffers
/// the display controller can scan out.
///
/// The stencil buffer is not optional: femtovg's OpenGL renderer fills paths with a
/// stencil-based approach, and a config without one renders nothing but silence.
fn choose_config(egl: &Egl, display: khronos_egl::Display) -> Result<khronos_egl::Config> {
    let attribs = [
        khronos_egl::SURFACE_TYPE,
        khronos_egl::WINDOW_BIT,
        khronos_egl::RENDERABLE_TYPE,
        khronos_egl::OPENGL_ES2_BIT,
        khronos_egl::RED_SIZE,
        8,
        khronos_egl::GREEN_SIZE,
        8,
        khronos_egl::BLUE_SIZE,
        8,
        khronos_egl::ALPHA_SIZE,
        0,
        khronos_egl::DEPTH_SIZE,
        0,
        khronos_egl::STENCIL_SIZE,
        8,
        khronos_egl::NONE,
    ];

    let count = egl
        .get_config_count(display)
        .context("eglGetConfigs (count)")?;
    let mut configs = Vec::with_capacity(count);
    egl.choose_config(display, &attribs, &mut configs)
        .context("eglChooseConfig")?;

    if configs.is_empty() {
        return Err(anyhow!(
            "no EGL config with GLES2 + 8-bit stencil; the vc4 driver should offer one"
        ));
    }

    // Match on the DRM fourcc so the buffers are directly scanout-capable.
    for &config in &configs {
        let visual = egl
            .get_config_attrib(display, config, khronos_egl::NATIVE_VISUAL_ID)
            .unwrap_or(0) as u32;
        if visual == FOURCC_XRGB8888 {
            return Ok(config);
        }
    }

    tracing::warn!(
        "no EGL config advertised XRGB8888 as its native visual; falling back to the first \
         match, which may fail at drmModeAddFB"
    );
    Ok(configs[0])
}
