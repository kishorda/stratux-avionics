use std::ffi::{c_char, c_uchar, c_void, CStr};

use femtovg::renderer::OpenGl;

/// The only drawing surface UI code ever sees.
pub type Canvas = femtovg::Canvas<OpenGl>;

/// Result of pumping platform events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pump {
    Continue,
    Exit,
}

/// What the GL driver reports about itself.
///
/// `is_gles2` is computed with exactly the same test femtovg uses internally
/// (`version.starts_with("OpenGL ES 2.")` in `femtovg/src/renderer/opengl.rs`), so what the
/// spike prints is what femtovg will actually decide. On a Pi 3 the `vc4` driver is
/// GLES 2.0 / OpenGL 2.1 only, and this is expected to be `true`.
#[derive(Debug, Clone, Default)]
pub struct GlInfo {
    pub version: String,
    pub renderer: String,
    pub vendor: String,
    pub is_gles2: bool,
}

impl std::fmt::Display for GlInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "vendor={:?} renderer={:?} version={:?} gles2={}",
            self.vendor, self.renderer, self.version, self.is_gles2
        )
    }
}

/// A platform that can hand us a [`Canvas`] and put the result on a screen.
pub trait Presenter {
    /// Drawable size in physical pixels.
    fn size(&self) -> (u32, u32);

    fn gl_info(&self) -> &GlInfo;

    /// Poll for platform events without blocking.
    fn pump(&mut self) -> anyhow::Result<Pump>;

    /// Make the GL context current, resize/clear the canvas, and return it for drawing.
    fn begin_frame(&mut self, clear: femtovg::Color) -> anyhow::Result<&mut Canvas>;

    /// Flush the canvas and put the frame on screen.
    ///
    /// On KMS this blocks until the page flip completes, which is what paces the render loop
    /// to the panel refresh rate. There is deliberately no separate "wait for vblank" call.
    fn end_frame(&mut self) -> anyhow::Result<()>;
}

/// Query GL strings through a raw proc-address loader.
///
/// Done by hand rather than through `glow` so this crate doesn't have to pin a `glow`
/// version separately from the one femtovg resolves.
///
/// # Safety
/// `load` must return either null or a valid `glGetString` pointer for a context that is
/// current on the calling thread.
pub(crate) unsafe fn query_gl_info(load: impl Fn(&str) -> *const c_void) -> GlInfo {
    const GL_VENDOR: u32 = 0x1F00;
    const GL_RENDERER: u32 = 0x1F01;
    const GL_VERSION: u32 = 0x1F02;

    let ptr = load("glGetString");
    if ptr.is_null() {
        tracing::warn!("could not load glGetString; GL info unavailable");
        return GlInfo::default();
    }
    let get_string: unsafe extern "C" fn(u32) -> *const c_uchar = std::mem::transmute(ptr);

    let fetch = |name: u32| -> String {
        let p = get_string(name);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p as *const c_char)
                .to_string_lossy()
                .into_owned()
        }
    };

    let version = fetch(GL_VERSION);
    GlInfo {
        is_gles2: version.starts_with("OpenGL ES 2."),
        vendor: fetch(GL_VENDOR),
        renderer: fetch(GL_RENDERER),
        version,
    }
}
