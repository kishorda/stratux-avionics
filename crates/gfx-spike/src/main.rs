//! M1 rendering spike — the go/no-go gate for the whole display stack.
//!
//! Its only job is to answer one question on real hardware: **does femtovg's OpenGL ES 2.0
//! path work on the Raspberry Pi 3's `vc4` driver, rendered straight to DRM/KMS?**
//!
//! femtovg's README claims it needs "OpenGl (ES) 3.0+", but its renderer detects ES2 at
//! runtime (`version.starts_with("OpenGL ES 2.")`) and has ES2-specific code paths. The Pi 3
//! is GLES 2.0 only. That combination is expected to work but had to be verified before any
//! UI was built on top of it.
//!
//! ```text
//! # On the Pi, from a console (not under X/Wayland):
//! sudo ./gfx-spike                      # runs until Ctrl-C
//! sudo ./gfx-spike --frames 600         # then exits and prints a summary
//!
//! # On the dev machine (no DRM master needed, writes an image instead):
//! ./gfx-spike --offscreen --out /tmp/spike.ppm
//! ```
//!
//! Note: the release profile uses `panic = "abort"`, so a panic will *not* run the VT
//! restore. If that leaves the console blank, `chvt 1` brings it back.

mod testpattern;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use avionics_gfx::femtovg::Color;
use avionics_gfx::Presenter;

use testpattern::TestPattern;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Background colour. Near-black but not pure black: on a dim cockpit panel, pure black makes
/// it impossible to tell "app crashed" from "app running with nothing to draw".
const BACKGROUND: Color = Color {
    r: 0.03,
    g: 0.04,
    b: 0.06,
    a: 1.0,
};

#[derive(Debug)]
struct Args {
    offscreen: bool,
    device: Option<String>,
    out: String,
    frames: u64,
    size: (u32, u32),
    take_vt: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            offscreen: false,
            device: None,
            out: "spike.ppm".into(),
            frames: 0,
            size: (800, 480),
            take_vt: true,
        }
    }
}

const USAGE: &str = "\
gfx-spike — M1 rendering go/no-go

  --offscreen         Render headless on a DRM render node and write an image (dev machine)
  --out PATH          Output PPM path for --offscreen        [default: spike.ppm]
  --device PATH       DRM node  [default: /dev/dri/card0, or renderD128 with --offscreen]
  --frames N          Render N frames then exit; 0 = until Ctrl-C  [default: 0, or 60 offscreen]
  --size WxH          Offscreen render size                  [default: 800x480]
  --no-vt             Don't put the console into graphics mode (KMS only)
  -h, --help          Show this help
";

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    let mut frames_set = false;
    let mut argv = std::env::args().skip(1);

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--offscreen" => args.offscreen = true,
            "--no-vt" => args.take_vt = false,
            "--out" => {
                args.out = argv.next().context("--out needs a path")?;
            }
            "--device" => {
                args.device = Some(argv.next().context("--device needs a path")?);
            }
            "--frames" => {
                let v = argv.next().context("--frames needs a number")?;
                args.frames = v
                    .parse()
                    .with_context(|| format!("bad --frames value {v:?}"))?;
                frames_set = true;
            }
            "--size" => {
                let v = argv.next().context("--size needs WxH")?;
                let (w, h) = v
                    .split_once(['x', 'X'])
                    .with_context(|| format!("bad --size value {v:?}, want e.g. 800x480"))?;
                args.size = (w.trim().parse()?, h.trim().parse()?);
            }
            other => bail!("unrecognised argument {other:?}\n\n{USAGE}"),
        }
    }

    // Offscreen renders a fixed short burst by default so the FPS figure and the animation
    // phase are both meaningful in the written image.
    if args.offscreen && !frames_set {
        args.frames = 60;
    }
    Ok(Some(args))
}

fn install_signal_handlers() -> Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    extern "C" fn handler(_: i32) {
        // Only an atomic store: async-signal-safe.
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handler does nothing but an atomic store.
    unsafe {
        sigaction(Signal::SIGINT, &action).context("installing SIGINT handler")?;
        sigaction(Signal::SIGTERM, &action).context("installing SIGTERM handler")?;
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let Some(args) = parse_args()? else {
        print!("{USAGE}");
        return Ok(());
    };

    install_signal_handlers()?;

    if args.offscreen {
        run_offscreen(&args)
    } else {
        run_kms(&args)
    }
}

#[cfg(feature = "offscreen")]
fn run_offscreen(args: &Args) -> Result<()> {
    use avionics_gfx::offscreen::{OffscreenConfig, OffscreenPresenter};

    let mut config = OffscreenConfig {
        width: args.size.0,
        height: args.size.1,
        ..Default::default()
    };
    if let Some(device) = &args.device {
        config.device = device.into();
    }

    let mut presenter = OffscreenPresenter::new(&config)?;
    let mut pattern = TestPattern::new(presenter.begin_frame(BACKGROUND)?)?;

    for _ in 0..args.frames.max(1) {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let gl = presenter.gl_info().clone();
        let canvas = presenter.begin_frame(BACKGROUND)?;
        pattern.draw(canvas, &gl);
        presenter.end_frame()?;
    }

    presenter.write_ppm(std::path::Path::new(&args.out))?;
    report(presenter.gl_info(), presenter.size(), &pattern);
    tracing::info!(path = %args.out, "wrote frame");
    Ok(())
}

#[cfg(not(feature = "offscreen"))]
fn run_offscreen(_args: &Args) -> Result<()> {
    bail!("this binary was built without the `offscreen` feature")
}

#[cfg(feature = "kms")]
fn run_kms(args: &Args) -> Result<()> {
    use avionics_gfx::kms::{KmsConfig, KmsPresenter};
    use avionics_gfx::Pump;

    let mut config = KmsConfig {
        take_vt: args.take_vt,
        ..Default::default()
    };
    if let Some(device) = &args.device {
        config.device = device.into();
    }

    let mut presenter = KmsPresenter::new(&config)?;
    let mut pattern = TestPattern::new(presenter.begin_frame(BACKGROUND)?)?;

    tracing::info!("rendering; Ctrl-C to stop");
    while !SHUTDOWN.load(Ordering::SeqCst) {
        if args.frames != 0 && pattern.frame_count() >= args.frames {
            break;
        }
        if presenter.pump()? == Pump::Exit {
            break;
        }
        let gl = presenter.gl_info().clone();
        let canvas = presenter.begin_frame(BACKGROUND)?;
        pattern.draw(canvas, &gl);
        presenter.end_frame()?;
    }

    let (info, size) = (presenter.gl_info().clone(), presenter.size());
    // Drop the presenter first so the console is back in text mode before we print.
    drop(presenter);
    report(&info, size, &pattern);
    Ok(())
}

#[cfg(not(feature = "kms"))]
fn run_kms(_args: &Args) -> Result<()> {
    bail!("this binary was built without the `kms` feature")
}

fn report(gl: &avionics_gfx::GlInfo, size: (u32, u32), pattern: &TestPattern) {
    let (w, h) = size;
    println!("\n=== M1 spike result ===");
    println!("  resolution : {w}x{h}");
    println!("  vendor     : {}", gl.vendor);
    println!("  renderer   : {}", gl.renderer);
    println!("  version    : {}", gl.version);
    println!("  GLES2 path : {}", gl.is_gles2);
    println!("  frames     : {}", pattern.frame_count());
    println!("  last fps   : {:.1}", pattern.fps());
    println!("\nGo/no-go: the pattern must show range rings, rotating chevrons, crisp text at");
    println!("all four sizes, BOTH weather mosaics, and a smooth alpha ramp. A missing or");
    println!("corrupt NPOT mosaic is not fatal — it means M5 must pad to a power of two.");
}
