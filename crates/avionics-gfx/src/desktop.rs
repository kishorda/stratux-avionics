//! Windowed backend for the dev machine.
//!
//! DRM/KMS needs DRM master, which a running desktop session already holds, so UI code cannot be
//! iterated against a real KMS surface without a Pi. This backend puts the identical [`Canvas`]
//! in a window instead, so everything above [`Presenter`] is written once and developed at full
//! speed.
//!
//! # It asks for GLES 2.0, but usually does not get it
//!
//! The context is requested with `ContextApi::Gles(Some(2.0))` to match the Pi 3's `vc4` driver,
//! which is GLES 2.0 only. **This is best-effort and on Mesa it does not work**: EGL treats the
//! requested version as a *minimum*, so a desktop driver returns the highest compatible context —
//! measured as "OpenGL ES 3.2" on Mesa 26 / Intel here.
//!
//! The consequence is worth being blunt about: **this harness does not catch ES2-incompatible
//! rendering.** Code that works in the window can still fail on the panel. A warning is logged at
//! startup and the negotiated version goes in the window title, so the situation is visible rather
//! than assumed, but the only real check is M1's spike on the Pi.
//!
//! The request is kept anyway because it costs nothing and does the right thing on drivers that
//! honour it — and because dropping it would remove the signal that tells you when it did not.
//!
//! # Why it pumps rather than being driven
//!
//! winit 0.30 is callback-driven (`run_app` takes ownership and calls you back), which is the
//! opposite of [`Presenter`]'s pull model. `pump_app_events` inverts it back: each call drains
//! whatever is pending and returns, so the render loop in `avionics` stays a plain `while` loop
//! and is structurally the same on both backends.

use std::ffi::CString;
use std::num::NonZeroU32;

use anyhow::{anyhow, Context as _, Result};
use femtovg::renderer::OpenGl;
use glutin::config::{Config, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext,
    PossiblyCurrentGlContext, Version,
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use glutin_winit::DisplayBuilder;
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window, WindowId};

use crate::presenter::{query_gl_info, Canvas, GlInfo, Presenter, Pump};

/// Something the developer did with the mouse or keyboard.
///
/// Deliberately *not* [`avionics_input::Gesture`]: this crate is about rendering and must not
/// depend on the input crate. The binary maps these onto the same `avionics_ui::interact` calls
/// that real touch gestures use, so the desktop harness exercises the actual interaction code
/// rather than a parallel implementation of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DesktopInput {
    /// Left click, in physical pixels. Stands in for a single-finger tap.
    Click { x: f32, y: f32 },
    /// Right click. Stands in for a two-finger tap.
    SecondaryClick,
    /// A character key was pressed. The binary decides what each one does.
    Key(char),
    /// The drawable was resized.
    Resized { width: u32, height: u32 },
}

#[derive(Debug, Clone)]
pub struct DesktopConfig {
    pub title: String,
    /// Initial size in logical pixels. Defaults to the 7" panel's likely native mode, so what you
    /// see on the desk is laid out the same as what the panel will show.
    pub width: u32,
    pub height: u32,
    /// Wait for vblank. On by default so the harness is paced roughly like the panel is.
    pub vsync: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            title: "avionics".into(),
            width: 800,
            height: 480,
            vsync: true,
        }
    }
}

pub struct DesktopPresenter {
    event_loop: EventLoop<()>,
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    canvas: Canvas,
    gl_info: GlInfo,
    size: (u32, u32),

    pending: Vec<DesktopInput>,
    cursor: (f32, f32),
    exit: bool,
}

impl DesktopPresenter {
    pub fn new(config: &DesktopConfig) -> Result<Self> {
        let event_loop = EventLoop::new().context("creating the winit event loop")?;

        let window_attributes = Window::default_attributes()
            .with_title(&config.title)
            .with_inner_size(winit::dpi::LogicalSize::new(config.width, config.height));

        // Stencil is not optional: femtovg fills paths through the stencil buffer, and a config
        // without one renders nothing at all, silently. Same requirement as the KMS backend.
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(0)
            .with_stencil_size(8);

        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes))
            .build(&event_loop, template, pick_config)
            .map_err(|e| anyhow!("creating a window and GL config: {e}"))?;

        let window = window.ok_or_else(|| anyhow!("no window was created"))?;
        let raw_window_handle = window
            .window_handle()
            .context("getting the raw window handle")?
            .as_raw();

        // Ask for exactly what the Pi has. See the module note.
        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(2, 0))))
            .build(Some(raw_window_handle));

        let gl_display = gl_config.display();
        let not_current = unsafe { gl_display.create_context(&gl_config, &context_attributes) }
            .context("creating a GLES 2.0 context (does this driver offer GLES?)")?;

        let physical = window.inner_size();
        let (width, height) = (physical.width.max(1), physical.height.max(1));
        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        );

        let surface = unsafe { gl_display.create_window_surface(&gl_config, &surface_attributes) }
            .context("creating the window surface")?;

        let context = not_current
            .make_current(&surface)
            .context("making the GL context current")?;

        if config.vsync {
            // Best effort: a driver that refuses vsync is not a reason to fail, it just means the
            // harness runs unthrottled.
            if let Err(e) =
                surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
            {
                tracing::warn!(error = %e, "could not enable vsync");
            }
        }

        let gl_info = unsafe {
            query_gl_info(|name| match CString::new(name) {
                Ok(name) => gl_display.get_proc_address(&name),
                Err(_) => std::ptr::null(),
            })
        };
        tracing::info!(%gl_info, "GL context ready (desktop)");
        if !gl_info.is_gles2 {
            // Worth saying out loud: the whole point of this backend is to reproduce the Pi's
            // constraints, and a context that came back newer will happily accept calls that vc4
            // would reject.
            tracing::warn!(
                version = %gl_info.version,
                "asked for GLES 2.0 but got something else; this harness will NOT catch \
                 ES2-incompatible rendering"
            );
        }

        let renderer =
            unsafe { OpenGl::new_from_function_cstr(|name| gl_display.get_proc_address(name)) }
                .map_err(|e| anyhow!("initialising the femtovg OpenGl renderer: {e}"))?;

        let mut canvas =
            Canvas::new(renderer).map_err(|e| anyhow!("creating the femtovg canvas: {e}"))?;
        canvas.set_size(width, height, window.scale_factor() as f32);

        window.set_title(&format!("{} — {}", config.title, gl_info.version));

        Ok(Self {
            event_loop,
            window,
            surface,
            context,
            canvas,
            gl_info,
            size: (width, height),
            pending: Vec::new(),
            cursor: (0.0, 0.0),
            exit: false,
        })
    }

    /// Take everything the developer has done since the last call.
    pub fn drain_input(&mut self) -> Vec<DesktopInput> {
        std::mem::take(&mut self.pending)
    }

    pub fn window(&self) -> &Window {
        &self.window
    }
}

/// Prefer a config with a stencil buffer, then the most samples on offer.
fn pick_config(configs: Box<dyn Iterator<Item = Config> + '_>) -> Config {
    configs
        .reduce(|best, candidate| {
            let usable = |c: &Config| c.stencil_size() >= 8;
            match (usable(&best), usable(&candidate)) {
                (false, true) => candidate,
                (true, false) => best,
                _ => {
                    if candidate.num_samples() > best.num_samples() {
                        candidate
                    } else {
                        best
                    }
                }
            }
        })
        .expect("the driver offered no GL configs at all")
}

/// Borrows the presenter's mutable state for the duration of one pump.
///
/// winit hands the handler back as `&mut A`, so this exists only to give those callbacks somewhere
/// to record what happened; nothing is drawn from here.
struct Harness<'a> {
    pending: &'a mut Vec<DesktopInput>,
    cursor: &'a mut (f32, f32),
    exit: &'a mut bool,
    resized: Option<(u32, u32)>,
}

impl ApplicationHandler for Harness<'_> {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // The window is created up front in `DesktopPresenter::new`, so there is nothing to do
        // here. Required by the trait.
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => *self.exit = true,
            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                self.resized = Some((w, h));
                self.pending.push(DesktopInput::Resized {
                    width: w,
                    height: h,
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                *self.cursor = (position.x as f32, position.y as f32);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => match button {
                MouseButton::Left => self.pending.push(DesktopInput::Click {
                    x: self.cursor.0,
                    y: self.cursor.1,
                }),
                MouseButton::Right => self.pending.push(DesktopInput::SecondaryClick),
                _ => {}
            },
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => *self.exit = true,
                    Key::Character(ref s) => {
                        // Case is preserved: the binary distinguishes 'r' from 'R' for range up
                        // versus down. Normalising here would silently make one of them
                        // unreachable.
                        if let Some(c) = s.chars().next() {
                            self.pending.push(DesktopInput::Key(c));
                        }
                    }
                    Key::Named(NamedKey::Space) => self.pending.push(DesktopInput::Key(' ')),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Presenter for DesktopPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn gl_info(&self) -> &GlInfo {
        &self.gl_info
    }

    fn pump(&mut self) -> Result<Pump> {
        let mut harness = Harness {
            pending: &mut self.pending,
            cursor: &mut self.cursor,
            exit: &mut self.exit,
            resized: None,
        };

        // Timeout zero: drain what is pending and return immediately. The render loop owns pacing.
        let status = self
            .event_loop
            .pump_app_events(Some(std::time::Duration::ZERO), &mut harness);

        let resized = harness.resized;

        if let Some((width, height)) = resized {
            self.surface.resize(
                &self.context,
                NonZeroU32::new(width).unwrap(),
                NonZeroU32::new(height).unwrap(),
            );
            self.size = (width, height);
            self.canvas
                .set_size(width, height, self.window.scale_factor() as f32);
        }

        if self.exit || matches!(status, PumpStatus::Exit(_)) {
            return Ok(Pump::Exit);
        }
        Ok(Pump::Continue)
    }

    fn begin_frame(&mut self, clear: femtovg::Color) -> Result<&mut Canvas> {
        if !self.context.is_current() {
            self.context
                .make_current(&self.surface)
                .context("making the GL context current")?;
        }
        let (width, height) = self.size;
        self.canvas.clear_rect(0, 0, width, height, clear);
        Ok(&mut self.canvas)
    }

    fn end_frame(&mut self) -> Result<()> {
        self.canvas.flush();
        self.surface
            .swap_buffers(&self.context)
            .context("swapping buffers")?;
        // Keeps the window responsive on compositors that only deliver frames on request.
        self.window.request_redraw();
        Ok(())
    }
}
