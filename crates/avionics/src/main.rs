//! The cockpit display.
//!
//! Wires a data source (live Stratux, or a recorded/synthesised replay) to [`AppState`], renders
//! it with [`avionics_ui`], and presents it either straight to the panel via DRM/KMS or headless
//! to image files for verification.
//!
//! ```text
//! # On the Pi, on the panel:
//! sudo avionics
//! sudo avionics --replay session.jsonl        # fly a recording on the real display
//!
//! # On the dev machine, no Pi needed — writes a filmstrip of PPM frames:
//! avionics --replay synth.jsonl --offscreen --out /tmp/frames --dump-every 30 --frames 300
//! ```
//!
//! # Why the ingest and render loops are separate
//!
//! Ingest is async and bursty (`/traffic` replays every current target the moment it connects);
//! rendering is a synchronous loop paced by the display's page flip. Running them on one thread
//! would make a burst of traffic stall a frame, or a blocked page flip stall ingest. Instead the
//! ingest task owns the socket and pushes into a shared [`AppState`] behind a mutex; the render
//! loop takes the lock once per frame, for as long as it takes to walk the target list.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use avionics_gfx::Presenter;
use avionics_ui::{Layout, Orientation, Theme, Ui, ViewState};
use stratux_client::state::AgePolicy;
use stratux_client::{live, record, AppState};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "\
avionics — Stratux cockpit display

Data source
  (default)            connect to a live Stratux
  --host H             Stratux address                      [default: 127.0.0.1]
  --port P             Stratux management port              [default: 80]
  --replay FILE        replay a recording instead of connecting
  --speed X            replay rate multiplier               [default: 1.0]
  --repeat             loop the recording

Output
  (default)            render to the panel via DRM/KMS
  --window             open an interactive window (dev machine; needs --features desktop)
  --device PATH        DRM node                             [default: /dev/dri/card0]
  --no-vt              don't put the console into graphics mode
  --offscreen          render headless and write image files
  --out DIR            output directory for --offscreen      [default: ./frames]
  --size WxH           offscreen render size                 [default: 800x480]
  --dump-every N       write every Nth frame                 [default: 0 = last only]

View
  --range NM           initial range ring: 2, 5, 10, 20, 40  [default: 10]
  --track-up           start in track-up instead of north-up
  --weather-page       start on the FIS-B text page
  --ahrs-page          start on the attitude page
  --no-underlay        don't draw the NEXRAD precipitation underlay

Other
  --frames N           render N frames then exit; 0 = until Ctrl-C
  --check              verify the install and exit; does not take over the display
  -h, --help
";

#[derive(Debug, Clone)]
enum Source {
    Live { host: String, port: u16 },
    Replay { path: PathBuf, speed: f64, repeat: bool },
}

#[derive(Debug)]
struct Args {
    source: Source,
    offscreen: bool,
    window: bool,
    out: PathBuf,
    size: (u32, u32),
    dump_every: u64,
    device: Option<String>,
    take_vt: bool,
    frames: u64,
    check: bool,
    view: ViewState,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            source: Source::Live {
                host: "127.0.0.1".into(),
                port: 80,
            },
            offscreen: false,
            window: false,
            out: PathBuf::from("frames"),
            size: (800, 480),
            dump_every: 0,
            device: None,
            take_vt: true,
            frames: 0,
            check: false,
            view: ViewState::default(),
        }
    }
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    let mut host = "127.0.0.1".to_string();
    let mut port = 80u16;
    let mut replay: Option<PathBuf> = None;
    let mut speed = 1.0f64;
    let mut repeat = false;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || -> Result<String> {
            argv.next()
                .with_context(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--host" => host = value()?,
            "--port" => port = value()?.parse().context("bad --port")?,
            "--replay" => replay = Some(PathBuf::from(value()?)),
            "--speed" => {
                speed = value()?.parse().context("bad --speed")?;
                if speed <= 0.0 {
                    bail!("--speed must be greater than zero");
                }
            }
            "--repeat" => repeat = true,
            "--offscreen" => args.offscreen = true,
            "--window" => args.window = true,
            "--out" => args.out = PathBuf::from(value()?),
            "--dump-every" => args.dump_every = value()?.parse().context("bad --dump-every")?,
            "--device" => args.device = Some(value()?),
            "--no-vt" => args.take_vt = false,
            "--check" => args.check = true,
            "--frames" => args.frames = value()?.parse().context("bad --frames")?,
            "--track-up" => args.view.orientation = Orientation::TrackUp,
            "--weather-page" => args.view.page = avionics_ui::Page::Weather,
            "--ahrs-page" => args.view.page = avionics_ui::Page::Ahrs,
            "--no-underlay" => args.view.show_weather_underlay = false,
            "--range" => {
                let v: f32 = value()?.parse().context("bad --range")?;
                if !ViewState::RANGES.contains(&v) {
                    bail!("--range must be one of {:?}", ViewState::RANGES);
                }
                args.view.range_nm = v;
            }
            "--size" => {
                let v = value()?;
                let (w, h) = v
                    .split_once(['x', 'X'])
                    .with_context(|| format!("bad --size {v:?}, want e.g. 800x480"))?;
                args.size = (w.trim().parse()?, h.trim().parse()?);
            }
            other => bail!("unrecognised argument {other:?}\n\n{USAGE}"),
        }
    }

    args.source = match replay {
        Some(path) => Source::Replay { path, speed, repeat },
        None => Source::Live { host, port },
    };
    Ok(Some(args))
}

fn install_signal_handlers() -> Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    extern "C" fn handler(_: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    let action = SigAction::new(SigHandler::Handler(handler), SaFlags::empty(), SigSet::empty());
    // SAFETY: the handler does nothing but an atomic store, which is async-signal-safe.
    unsafe {
        sigaction(Signal::SIGINT, &action).context("installing SIGINT handler")?;
        sigaction(Signal::SIGTERM, &action).context("installing SIGTERM handler")?;
    }
    Ok(())
}

/// Shared between the ingest task and the render loop.
type Shared = Arc<Mutex<AppState>>;

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

    if args.check {
        // Deliberately before the runtime is started and before anything touches the display.
        return run_check(&args);
    }

    // A missing recording is fatal, and deliberately unlike a missing Stratux.
    //
    // The live path must always come up — a panel showing NO STRATUX CONNECTION beats a service
    // that refused to start. Replay is the opposite: it is a measurement and test harness, so
    // carrying on with an empty state renders a blank scene at a convincing 60 fps and reports
    // timings for drawing nothing. That has already produced two meaningless measurements here
    // after /tmp was cleared by a reboot, and neither looked like a failure.
    if let Source::Replay { path, .. } = &args.source {
        if let Err(e) = std::fs::File::open(path) {
            bail!("cannot read the recording {}: {e}", path.display());
        }
    }

    // The render loop is synchronous and owns the main thread; ingest gets its own runtime thread.
    // Rendering must not be at the mercy of the async scheduler when a page flip is due.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("starting the Tokio runtime")?;

    let state: Shared = Arc::new(Mutex::new(AppState::new()));
    runtime.spawn(ingest(args.source.clone(), Arc::clone(&state)));

    if args.offscreen {
        run_offscreen(&args, state)
    } else if args.window {
        run_window(&args, state)
    } else {
        run_kms(&args, state, runtime.handle().clone())
    }
}

/// Interactive window on the dev machine.
///
/// Structurally identical to the KMS loop: pump, draw, present. The only differences are where the
/// pixels go and that input arrives from a mouse and keyboard instead of a touchscreen — and even
/// that funnels into the same `avionics_ui::interact` calls the panel uses, so what is exercised
/// here is the real interaction code rather than a parallel implementation of it.
#[cfg(feature = "desktop")]
fn run_window(args: &Args, state: Shared) -> Result<()> {
    use avionics_gfx::desktop::{DesktopConfig, DesktopInput, DesktopPresenter};
    use avionics_gfx::Pump;

    let config = DesktopConfig {
        title: "avionics".into(),
        width: args.size.0,
        height: args.size.1,
        ..Default::default()
    };

    let mut presenter = DesktopPresenter::new(&config)?;
    let theme = Theme::dark();
    let mut ui = Ui::new(presenter.begin_frame(theme.background)?, theme.clone())?;
    let mut view = args.view.clone();
    let mut timing = RenderTiming::default();

    println!("{DESKTOP_KEYS}");

    while !SHUTDOWN.load(Ordering::SeqCst) {
        if args.frames != 0 && timing.frames >= args.frames {
            break;
        }
        if presenter.pump()? == Pump::Exit {
            break;
        }

        let inputs = presenter.drain_input();
        if !inputs.is_empty() {
            let (w, h) = presenter.size();
            let layout = Layout::for_size(w as f32, h as f32, &ui.theme);
            let guard = state.lock().expect("app state mutex poisoned");
            for input in inputs {
                match input {
                    // Left click is a tap: same entry point the touchscreen uses.
                    DesktopInput::Click { x, y } => {
                        avionics_ui::interact::handle_tap(&ui, &layout, &mut view, &guard, x, y)
                    }
                    DesktopInput::SecondaryClick => {
                        avionics_ui::interact::two_finger_tap(&mut view)
                    }
                    DesktopInput::Key(key) => apply_key(&mut view, key),
                    DesktopInput::Resized { .. } => {}
                }
            }
        }

        let canvas = presenter.begin_frame(theme.background)?;
        let started = Instant::now();
        let _stats = draw_frame(&mut ui, canvas, &state, &view);
        timing.record(started.elapsed());
        presenter.end_frame()?;
    }

    let mosaic = ui.mosaic_stats();
    timing.report(&mosaic);
    Ok(())
}

#[cfg(not(feature = "desktop"))]
fn run_window(_args: &Args, _state: Shared) -> Result<()> {
    bail!(
        "this binary was built without the `desktop` feature.\n\
         Rebuild with: cargo run --features desktop -p avionics -- --window ..."
    )
}

#[cfg(feature = "desktop")]
const DESKTOP_KEYS: &str = "\
Interactive harness
  left click     tap        (status bar switches page; body cycles range / scrolls weather)
  right click    two-finger tap (north-up <-> track-up)
  r / R          range up / down
  o              toggle orientation
  p              switch page
  w              toggle the NEXRAD underlay
  esc            quit
";

/// Keyboard shortcuts for the desktop harness.
///
/// These exist because clicking through to a specific state is tedious when iterating on drawing
/// code. They drive the same `ViewState` the touch path does, so nothing here can diverge from
/// what the panel will do.
#[cfg(feature = "desktop")]
fn apply_key(view: &mut ViewState, key: char) {
    match key {
        'r' => view.cycle_range(),
        'R' => view.cycle_range_down(),
        'o' => view.toggle_orientation(),
        'p' => view.page = view.page.next(),
        'w' => view.show_weather_underlay = !view.show_weather_underlay,
        _ => {}
    }
}

/// Drive the AHRS cage request: issue a confirmed one, and collect its result.
///
/// Called once per frame. Everything here is non-blocking — the render loop is paced by the page
/// flip and must never wait on a socket, however briefly.
#[cfg(feature = "kms")]
fn pump_cage(
    view: &mut ViewState,
    sender: &std::sync::mpsc::Sender<bool>,
    results: &std::sync::mpsc::Receiver<bool>,
    target: &Option<(String, u16)>,
    runtime: &tokio::runtime::Handle,
) {
    use avionics_ui::CageState;

    let now = Instant::now();
    // Retire a lapsed arm or a finished result first, so a stale CONFIRM cannot sit on the key.
    view.tick_cage(now);

    if let Ok(ok) = results.try_recv() {
        view.set_cage(CageState::Done { ok }, now);
    }

    if view.cage == CageState::Requested {
        match target {
            Some((host, port)) => {
                view.set_cage(CageState::InFlight, now);
                let (host, port) = (host.clone(), *port);
                let tx = sender.clone();
                runtime.spawn(async move {
                    let outcome = stratux_client::control::cage_ahrs(&host, port).await;
                    match &outcome {
                        Ok(()) => tracing::info!("AHRS caged: attitude reference zeroed"),
                        Err(e) => tracing::error!(error = %e, "caging the AHRS failed"),
                    }
                    // A closed channel just means the display is shutting down.
                    let _ = tx.send(outcome.is_ok());
                });
            }
            None => {
                tracing::warn!("no live Stratux to cage (replaying a recording)");
                view.set_cage(CageState::Done { ok: false }, now);
            }
        }
    }
}

/// Own the source and fold everything it produces into the shared state.
async fn ingest(source: Source, state: Shared) {
    let policy = AgePolicy::default();
    let mut rx = match source {
        Source::Live { host, port } => {
            tracing::info!(%host, port, "connecting to Stratux");
            live::spawn(live::LiveConfig {
                host,
                port,
                ..Default::default()
            })
        }
        Source::Replay { path, speed, repeat } => {
            let frames = match record::read_all(&path) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::error!(error = %e, "could not read the recording");
                    return;
                }
            };
            tracing::info!(frames = frames.len(), path = %path.display(), "replaying");
            record::spawn(
                frames,
                record::ReplayConfig {
                    speed,
                    repeat,
                    ..Default::default()
                },
            )
        }
    };

    while let Some(event) = rx.recv().await {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let mut guard = match state.lock() {
            Ok(guard) => guard,
            // The render loop panicked while holding the lock. Nothing useful to do but stop.
            Err(_) => return,
        };
        guard.apply(&event, now);
        guard.prune(now, &policy);
    }
}

/// Verify everything the service needs, without disturbing whatever is on screen.
///
/// Exists because every failure mode this catches is otherwise discovered as a black panel in the
/// aircraft: a font removed by an unrelated `apt autoremove`, a DSI ribbon that came loose, a touch
/// controller that enumerated under a different name, Stratux not listening.
fn run_check(args: &Args) -> Result<()> {
    let mut failures = Vec::new();
    println!("=== avionics install check ===");

    // --- font ---
    match avionics_ui::font::find() {
        Ok(path) => println!("  font        : OK   {}", path.display()),
        Err(e) => {
            println!("  font        : FAIL {e}");
            failures.push("font");
        }
    }

    // --- display ---
    #[cfg(feature = "kms")]
    {
        let device = args
            .device
            .clone()
            .unwrap_or_else(|| "/dev/dri/card0".to_string());
        match avionics_gfx::kms::probe(std::path::Path::new(&device)) {
            Ok(probe) => {
                for (name, connected, mode) in &probe.connectors {
                    println!(
                        "  connector   : {name} {} {}",
                        if *connected { "connected" } else { "disconnected" },
                        mode.as_deref().unwrap_or("(no modes)")
                    );
                }
                match &probe.usable_output {
                    Some(output) => println!("  display     : OK   {output}"),
                    None => {
                        println!(
                            "  display     : FAIL no drivable output. Check the DSI ribbon and \
                             that dtoverlay=vc4-kms-dsi-7inch is set."
                        );
                        failures.push("display");
                    }
                }
            }
            Err(e) => {
                println!("  display     : FAIL {e:#}");
                failures.push("display");
            }
        }
    }

    // --- touch ---
    // A missing touchscreen is a warning, not a failure: the display still shows traffic, the pilot
    // just cannot change range.
    #[cfg(feature = "kms")]
    match avionics_input::TouchReader::open_auto((800.0, 480.0)) {
        Ok(reader) => println!("  touch       : OK   {}", reader.path().display()),
        Err(e) => println!("  touch       : WARN {e}"),
    }

    // --- Stratux ---
    if let Source::Live { host, port } = &args.source {
        let address = format!("{host}:{port}");
        match std::net::TcpStream::connect_timeout(
            &address
                .parse()
                .or_else(|_| {
                    use std::net::ToSocketAddrs;
                    address
                        .to_socket_addrs()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                        .and_then(|mut it| {
                            it.next().ok_or_else(|| anyhow::anyhow!("no address"))
                        })
                })
                .map_err(|e| anyhow::anyhow!("resolving {address}: {e}"))?,
            Duration::from_secs(3),
        ) {
            Ok(_) => println!("  stratux     : OK   {address} accepting connections"),
            Err(e) => {
                // Not fatal. The display is designed to come up and show NO STRATUX CONNECTION,
                // and on the aircraft it will normally lose the startup race against Stratux.
                println!("  stratux     : WARN {address} unreachable ({e})");
            }
        }
    }

    println!();
    if failures.is_empty() {
        println!("All required checks passed.");
        Ok(())
    } else {
        bail!("{} required check(s) failed: {}", failures.len(), failures.join(", "))
    }
}

/// Measures how long the render loop actually takes, which is the M4 exit criterion that cannot be
/// checked by eye.
#[derive(Default)]
struct RenderTiming {
    frames: u64,
    total: Duration,
    worst: Duration,
    fps: f32,
    window_start: Option<Instant>,
    window_frames: u64,
}

impl RenderTiming {
    fn record(&mut self, draw: Duration) {
        self.frames += 1;
        self.total += draw;
        self.worst = self.worst.max(draw);

        let start = *self.window_start.get_or_insert_with(Instant::now);
        self.window_frames += 1;
        let elapsed = start.elapsed().as_secs_f32();
        if elapsed >= 0.5 {
            self.fps = self.window_frames as f32 / elapsed;
            self.window_frames = 0;
            self.window_start = Some(Instant::now());
        }
    }

    fn mean(&self) -> Duration {
        self.total
            .checked_div(self.frames.max(1) as u32)
            .unwrap_or_default()
    }

    fn report(&self, mosaic: &avionics_ui::nexrad::MosaicStats) {
        println!("\n=== render timing ===");
        println!("  frames        : {}", self.frames);
        println!("  mean draw     : {:.2} ms", self.mean().as_secs_f64() * 1000.0);
        println!("  worst draw    : {:.2} ms", self.worst.as_secs_f64() * 1000.0);
        println!("  last fps      : {:.1}", self.fps);
        println!("  wx composites : {}", mosaic.composites);
        println!(
            "  wx blocks     : {} drawn, {} outside, {} bins",
            mosaic.blocks_composited, mosaic.blocks_skipped_outside, mosaic.bins_painted
        );
        println!(
            "\nDraw time is CPU spent building the frame, excluding the page-flip wait. On the Pi 3\n\
             this must stay well inside one core's budget: dump1090 and dump978 already occupy a\n\
             large share of two of the four, and a dropped ADS-B message is worse than a dropped frame."
        );
    }
}

/// Take the lock, draw, release. Keeping the critical section to exactly one frame's read means a
/// burst of ingest never waits on rendering for longer than a single draw.
fn draw_frame(
    ui: &mut Ui,
    canvas: &mut avionics_gfx::Canvas,
    state: &Shared,
    view: &ViewState,
) -> avionics_ui::FrameStats {
    let now = Instant::now();
    let guard = state.lock().expect("app state mutex poisoned");
    ui.draw(canvas, &guard, view, now)
}

#[cfg(feature = "kms")]
fn run_kms(args: &Args, state: Shared, runtime: tokio::runtime::Handle) -> Result<()> {
    use avionics_gfx::kms::{KmsConfig, KmsPresenter};

    let mut config = KmsConfig {
        take_vt: args.take_vt,
        ..Default::default()
    };
    if let Some(device) = &args.device {
        config.device = device.into();
    }

    let mut presenter = KmsPresenter::new(&config)?;
    let theme = Theme::dark();
    let mut ui = Ui::new(presenter.begin_frame(theme.background)?, theme.clone())?;
    let mut view = args.view.clone();
    let mut timing = RenderTiming::default();

    // Touch is optional: a missing or unrecognised panel controller must not stop the display from
    // showing traffic. The pilot loses range cycling, not situational awareness.
    let size = presenter.size();
    let mut touch = match avionics_input::TouchReader::open_auto((size.0 as f32, size.1 as f32)) {
        Ok(reader) => Some(reader),
        Err(e) => {
            tracing::warn!(error = %e, "no touch input; range and orientation are fixed");
            None
        }
    };

    // Where a completed cage request reports back. The render loop must never block on the
    // network, so the request runs on the Tokio runtime and answers through this channel, which
    // the loop drains without waiting.
    let (cage_tx, cage_rx) = std::sync::mpsc::channel::<bool>();
    let cage_target = match &args.source {
        Source::Live { host, port } => Some((host.clone(), *port)),
        // Replaying a recording: there is no Stratux to cage, and pretending otherwise would
        // show CAGED for something that never happened.
        Source::Replay { .. } => None,
    };

    tracing::info!("rendering to the panel; Ctrl-C to stop");
    while !SHUTDOWN.load(Ordering::SeqCst) {
        if args.frames != 0 && timing.frames >= args.frames {
            break;
        }

        pump_cage(&mut view, &cage_tx, &cage_rx, &cage_target, &runtime);

        if let Some(reader) = touch.as_mut() {
            match reader.poll() {
                Ok(gestures) if !gestures.is_empty() => {
                    let layout = Layout::for_size(size.0 as f32, size.1 as f32, &ui.theme);
                    let guard = state.lock().expect("app state mutex poisoned");
                    for gesture in gestures {
                        apply_gesture(&ui, &layout, &mut view, &guard, gesture);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    // Drop the device rather than erroring every frame for the rest of the flight.
                    tracing::error!(error = %e, "touch input failed; disabling it");
                    touch = None;
                }
            }
        }

        let canvas = presenter.begin_frame(theme.background)?;
        let started = Instant::now();
        let _stats = draw_frame(&mut ui, canvas, &state, &view);
        timing.record(started.elapsed());
        // Blocks until the page flip completes, which is what paces us to the panel refresh.
        presenter.end_frame()?;
    }

    let mosaic = ui.mosaic_stats();
    drop(presenter);
    timing.report(&mosaic);
    Ok(())
}

/// Map a gesture onto the view.
///
/// Deliberately minimal: one tap steps the range up, two fingers flip the orientation. Anything
/// richer (pan, pinch) invites accidental changes from a hand steadying itself against the panel in
/// turbulence, and a display that has silently wandered off its selected range is worse than one
/// with fewer controls.
#[cfg(feature = "kms")]
fn apply_gesture(
    ui: &Ui,
    layout: &Layout,
    view: &mut ViewState,
    state: &stratux_client::AppState,
    gesture: avionics_input::Gesture,
) {
    use avionics_input::Gesture;
    match gesture {
        Gesture::Tap { x, y } => {
            avionics_ui::interact::handle_tap(ui, layout, view, state, x, y);
            tracing::debug!(x, y, page = view.page.label(), range = view.range_nm, "tap");
        }
        Gesture::TwoFingerTap => {
            avionics_ui::interact::two_finger_tap(view);
            tracing::debug!(orientation = view.orientation.label(), "two-finger tap");
        }
    }
}

#[cfg(not(feature = "kms"))]
fn run_kms(_args: &Args, _state: Shared, _runtime: tokio::runtime::Handle) -> Result<()> {
    bail!("this binary was built without the `kms` feature")
}

#[cfg(feature = "offscreen")]
fn run_offscreen(args: &Args, state: Shared) -> Result<()> {
    use avionics_gfx::offscreen::{OffscreenConfig, OffscreenPresenter};

    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    let mut config = OffscreenConfig {
        width: args.size.0,
        height: args.size.1,
        ..Default::default()
    };
    if let Some(device) = &args.device {
        config.device = device.into();
    }

    let mut presenter = OffscreenPresenter::new(&config)?;
    let theme = Theme::dark();
    let mut ui = Ui::new(presenter.begin_frame(theme.background)?, theme.clone())?;
    let view = args.view.clone();
    let mut timing = RenderTiming::default();

    // Offscreen has no page flip to pace it, so without this the loop would spin as fast as the
    // GPU allows and race far ahead of the replay it is supposed to be showing.
    let frame_interval = Duration::from_millis(1000 / 30);
    let total = if args.frames == 0 { 300 } else { args.frames };
    let mut dumped = 0usize;

    for index in 0..total {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let frame_started = Instant::now();

        let canvas = presenter.begin_frame(theme.background)?;
        let draw_started = Instant::now();
        let stats = draw_frame(&mut ui, canvas, &state, &view);
        timing.record(draw_started.elapsed());
        presenter.end_frame()?;

        let is_last = index + 1 == total;
        let due = args.dump_every > 0 && index % args.dump_every == 0;
        if due || (args.dump_every == 0 && is_last) {
            let path = args.out.join(format!("frame-{index:05}.ppm"));
            presenter.write_ppm(&path)?;
            dumped += 1;
            tracing::info!(
                path = %path.display(),
                targets = stats.targets_drawn,
                alerts = stats.alerts,
                advisories = stats.advisories,
                outside = stats.targets_outside_range,
                "wrote frame"
            );
        }

        if let Some(remaining) = frame_interval.checked_sub(frame_started.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    timing.report(&ui.mosaic_stats());
    println!("  images written : {dumped} -> {}", args.out.display());
    Ok(())
}

#[cfg(not(feature = "offscreen"))]
fn run_offscreen(_args: &Args, _state: Shared) -> Result<()> {
    bail!("this binary was built without the `offscreen` feature")
}
