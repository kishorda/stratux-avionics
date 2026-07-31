# stratux-avionics

A cockpit display that renders ADS-B traffic (1090 MHz), UAT traffic + FIS-B weather
(978 MHz), and own-ship GPS position straight to a DSI panel — no desktop, no compositor, no
browser. A headless [Stratux](https://github.com/stratux/stratux) backend does the RF work; a
single Rust process renders to DRM/KMS through GBM + EGL + [FemtoVG](https://github.com/femtovg/femtovg).

Target hardware: Raspberry Pi 3 Model B v1.2, Stratux GPYes 2.0 (u-blox 8), 2x NESDR Nano 2,
Hysong 7" DSI touchscreen, Debian Bookworm via the official Stratux image.

The full design and milestone breakdown lives in the plan file; this README covers what exists
and how to run it.

## Status

| Milestone | What | State |
| --- | --- | --- |
| M0 | Hardware & OS bring-up survey | `deploy/m0-survey.sh` ready to run |
| M1 | Rendering spike — go/no-go on the stack | **code complete, needs running on the Pi** |
| M2 | Presenter abstraction + interactive dev harness | **done** — offscreen + interactive window |
| M3 | Stratux client + replay harness | **done except live-Pi verification** |
| M4 | Traffic plan view | **done except on-panel verification** (touch untested without hardware) |
| M5 | Weather: text page + NEXRAD underlay | **done except validation against an independent mosaic** |
| M6 | Kiosk hardening | **scripts written, none run on hardware yet** |

94 tests passing, clippy clean.

## Layout

```
crates/avionics-gfx     Presenter trait + KMS (the real target), windowed and offscreen backends
crates/gfx-spike        M1 go/no-go test pattern
crates/stratux-client   Stratux ingest: wire structs, decoder, state fold, record/replay, synth
crates/avionics-ui      Plan view, NEXRAD underlay, weather page, projection, threat tiers
crates/avionics-input   evdev multitouch to gestures
crates/avionics         The display binary
tools/replay            record / synth / stats / play CLI
deploy/                 install, hardening and on-hardware test harnesses
```

## Deploying to the Pi

Order matters. Each step is idempotent and says how to undo itself.

```sh
# From the dev machine:
ssh pi@stratux.local 'bash -s' < deploy/m0-survey.sh | tee m0-survey.txt
./deploy/sync-sysroot.sh --offline            # one-time, ~270 MB, no Pi needed
./deploy/deploy.sh       pi@stratux.local     # cross-build and push

# On the Pi (deploy.sh puts the scripts in /tmp/avionics-deploy alongside the binary):
sudo /tmp/avionics --check                                    # verify before wiring anything up
cd /tmp/avionics-deploy
sudo AVIONICS_BINARY=/tmp/avionics ./install.sh --dry-run      # see exactly what would change
sudo AVIONICS_BINARY=/tmp/avionics ./install.sh
sudo systemctl start avionics

# Then, in this order:
sudo ./boot-trim.sh                 # disable services with no role here
sudo ./soak.sh --compare            # does the display cost the radios messages?
sudo ./powercut-check.sh baseline   # before starting the power-cut runs
# ... configure Stratux fully, then:
sudo ./overlay.sh enable            # read-only root, last because it freezes settings
```

`avionics --check` verifies the font, the DRM connectors and modes, the touch device and
whether Stratux is reachable — and does it **without taking DRM master or the console**, so it
is safe to run over SSH while something else is on screen. It is also wired as
`ExecStartPre=`, so a missing font or a loose DSI ribbon fails with a clear journal message
instead of a black panel.

### Hardening decisions

- **The service starts even when Stratux does not.** `avionics.service` is `After=stratux` but
  deliberately not `Requires=`. Both start at power-on and the display usually wins the race;
  the right answer is a panel showing `NO STRATUX CONNECTION` and retrying, not a service that
  refused to start.
- **The font is copied to `/opt/avionics/assets/`, not symlinked.** A display that cannot draw
  text because an unrelated `apt autoremove` took `fonts-dejavu-core` is a reliability bug.
- **The journal is volatile and capped at 32 MB.** Logs survive until reboot — long enough to
  diagnose a flight — and the SD card lasts much longer for it. This matters more once the root
  filesystem is read-only, since the journal then lives in the RAM overlay.
- **CPU pinning is written but commented out.** Pinning is a fix for a contention problem that
  has not been measured. `soak.sh --compare` runs the same window twice, with and without the
  display, and compares Stratux's own ES/UAT message counters — because the failure that
  matters is invisible from inside the display, where traffic just looks a bit thin. Turn
  pinning on only if that shows a real drop; hard-pinning can make things worse on a 4-core part.
- **Read-only root is a separate, explicit step**, and it is the one place this deviates from
  the plan. See below.

### Read-only root, and what it costs

The plan called for bind-mounting a small writable partition over `/etc/stratux.conf` and
`/var/log/stratux` so Stratux settings survive with the overlay enabled. `deploy/overlay.sh`
does **not** do that, and says so at the top of the file:

- It needs the card repartitioned. Automating destructive partition edits on the user's only
  boot medium is a bad trade against the problem it solves.
- The repartition-free alternative (a loopback image on the FAT boot partition) puts the one
  file we care about behind a filesystem with no journal — the very thing the overlay exists to
  protect against. It would look like persistence while being less safe.

So the supported posture is **configure Stratux once with the overlay off, then enable it**.
Changing settings later means `overlay.sh disable`, change, re-enable — a deliberate,
infrequent, on-the-ground operation. The manual procedure for a real persistent partition is
documented at the end of `overlay.sh` for when it is genuinely needed.

## Running the display

```sh
# No Pi needed: replay a synthetic session and write a filmstrip of PPM frames.
cargo run -p replay -- synth /tmp/synth.jsonl --duration 120 --targets 8 --conflict
cargo run --release -p avionics -- --replay /tmp/synth.jsonl --offscreen \
    --out /tmp/frames --frames 700 --dump-every 120 --range 5 --speed 4

# Interactive window on the dev machine — same UI, same interaction code, mouse instead of touch.
cargo run --release --features desktop -p avionics -- --window --replay /tmp/synth.jsonl --speed 4

# On the Pi, on the panel:
sudo avionics                              # live
sudo avionics --replay session.jsonl       # fly a recording on the real display
```

### The desktop harness

`--window` opens a real window via winit/glutin and runs the identical render loop. Left click is
a tap and right click is a two-finger tap, funnelled into the **same** `avionics_ui::interact`
calls the touchscreen uses — so what it exercises is the real interaction code, not a parallel
implementation. Keyboard shortcuts (`r`/`R` range, `o` orientation, `p` page, `w` underlay, `esc`
quit) exist because clicking into a specific state is tedious when iterating on drawing code.

It needs `--features desktop`, which is deliberately **not** in the default set: winit and glutin
have no business being cross-compiled into the binary that ships on the aircraft. `cargo tree -p
avionics` confirms the shipping build pulls in neither.

**It does not verify GLES 2.0 compatibility.** The context is requested as
`ContextApi::Gles(Some(2.0))` to match `vc4`, but EGL treats that as a *minimum* — Mesa returns
"OpenGL ES 3.2" here, and the harness logs a warning saying so. Code that works in the window can
still fail on the panel. The only real ES2 check is M1's spike on the Pi.

`--conflict` adds a deterministic head-on co-altitude target that closes from 4 nm, plus a
Mode-S target with no position. Random synthetic traffic almost never lands inside the alert
box, so without it the alert path and the no-position counter are never exercised.

Controls on the panel:

| Gesture | Plan view | Weather page |
| --- | --- | --- |
| Tap the status bar | switch page | switch page |
| Tap the body | cycle range ring | page the list (lower half forward, upper half back) |
| Two-finger tap | north-up ↔ track-up | — |

Deliberately minimal — richer gestures invite accidental changes from a hand steadying itself
against the panel in turbulence. The status bar is the one piece of chrome present and identical
on every page, which is what makes it the reliable navigation target.

### The NEXRAD underlay

Blocks are composited into a single 1024×1024 RGBA texture in **latitude/longitude space**, then
drawn as one rotated quad beneath the range rings. Two reasons for that shape:

- A full picture is ~100 blocks × 128 bins. As paths that is >10,000 draw calls per frame, which
  `vc4` will not do at 30 Hz. As one texture it is one draw call.
- Laying the texture out in lat/lon rather than screen space means heading changes in track-up
  don't invalidate it. Screen-aligned, every turn would force a rebuild several times a second.

The longitude span is divided by cos(latitude) so the texture covers a *square patch of ground*,
matching what the projection does; without that the mosaic would be stretched ~30% east-west at
mid latitudes and weather would appear displaced along track.

**Compositing costs ~14 ms** on a desktop iGPU (measured: 21 ms worst frame with the underlay vs
7 ms with `--no-underlay`). On a Pi 3 expect several times that — a visible multi-frame hitch.
So rebuilds are driven off things that actually changed, never a timer:

- the block set (`AppState::nexrad_revision`),
- own-ship drifting >10 nm from the patch centre,
- a change in `fade_fingerprint`, which buckets block age into three steps so fading causes at
  most two rebuilds per block lifetime rather than one every 30 s.

If the hitch turns out to be objectionable on the panel, the next lever is `MosaicConfig::
texture_size` (512 cuts the work 4× at the cost of resolution), then incremental repainting of
only the changed blocks.

### What the plan view will and will not do

- **It is not TCAS.** Threat tiers are range-and-relative-altitude boxes with no closure rate
  and no time-to-closest-approach. They exist to draw the eye, not to issue a resolution
  advisory.
- **Without own-ship altitude, nothing escalates to Alert.** Range-only alerts fire constantly
  in the circuit, and a display that cries wolf gets ignored precisely when it matters. The
  status bar shows `NO ALT REF` when this applies.
- **Targets beyond the selected range are culled, not drawn at the edge.** The count appears in
  the status bar as `+N out` so a quiet sky is distinguishable from a small range ring. Edge
  markers for off-scale traffic are a possible refinement, but without closure-rate logic there
  is no honest way to say which off-scale target matters.
- **Extrapolation is capped at 3 s** and coasting targets are dimmed. A confident-looking symbol
  several miles from where the aircraft actually is would be worse than an obviously stale one.
- **There is no bus voltage**, despite the original mockup showing one. Stratux's status
  structure has no voltage field and this build has no other power sensor, so a number there
  would be invented. It needs hardware first.
- **Weather text is shown raw, not decoded.** Pilots read raw METARs, the encoding fits a small
  panel, and a decoder would be one more thing that can be subtly wrong about weather. Long
  bodies are truncated rather than wrapped: a wrapped TAF runs four lines and pushes everything
  else off a 480 px panel, and the leading groups are what matter at a glance.
- **Each product carries its own age.** FIS-B delivery is opportunistic — one station's
  observation can be twenty minutes stale while its neighbour is current, and nothing on the wire
  warns you.
- **Precipitation older than 5 / 10 minutes is faded**, and dropped entirely at 15. Fading rather
  than hiding, because weather from five minutes ago is still useful provided it is visibly not
  current.

## Working with Stratux data

`stratux-client` funnels everything through one channel of `SourceEvent`. Two things can fill it
— a live WebSocket connection and a replay of a recorded or synthesised session — and consumers
cannot tell them apart. That is what lets the plan view be developed on the bench and flown
without a code change.

```sh
# No Pi needed: generate a deterministic synthetic session and inspect it.
cargo run -p replay -- synth /tmp/synth.jsonl --duration 120 --targets 8
cargo run -p replay -- stats /tmp/synth.jsonl
cargo run -p replay -- play  /tmp/synth.jsonl --speed 30

# Capture a real session from the Pi (run this from the dev machine or on the Pi).
cargo run -p replay -- record session.jsonl --host 192.168.10.1 --duration 300
```

`stats` is the one worth running on any new recording: it reports frame counts *and* what the
display actually managed to decode out of them. A recording full of undecodable frames looks
perfectly healthy by frame count alone.

Recordings are JSON Lines with the payload held as a **string**, so they are byte-faithful and a
parser bug seen in flight reproduces on the bench. To read one:

```sh
jq -r 'select(.stream=="traffic") | .payload' session.jsonl | jq .
```

`synth` is deterministic for a given seed, and it works by serialising the same wire structs the
decoder reads — so a synthetic session exercises the real parsing path rather than a shortcut
around it, and a failing test reproduces exactly.

## Running the M1 spike

M1 answers one question on real hardware: **does femtovg's OpenGL ES 2.0 path work on the
Pi 3's `vc4` driver, rendered straight to DRM/KMS?** Everything else is built on that
assumption, so it gets verified first.

On the dev machine (headless, writes an image — no Pi and no DRM master needed):

```sh
cargo run -p gfx-spike -- --offscreen --out /tmp/spike.ppm
```

On the Pi, from a console with no X/Wayland running:

```sh
./deploy/sync-sysroot.sh --offline            # one-time, ~270 MB, no Pi needed
./deploy/deploy.sh       pi@stratux.local
ssh -t pi@stratux.local 'sudo /tmp/gfx-spike'
```

## Building the cross sysroot when the Pi has no internet

The Pi sits on its own WiFi AP with no route out, so `apt-get install` on the target cannot
work. The `-dev` packages are a *build-time* artifact anyway — the display itself needs only
the runtime Mesa the image already ships — so they are fetched here instead:

```sh
./deploy/sync-sysroot.sh --offline        # recommended: never contacts the Pi
./deploy/sync-sysroot.sh pi@stratux.local # mirrors the real machine (see caveat below)
```

`--offline` downloads Debian Bookworm packages for the target architecture from
`deb.debian.org`, verified against `/usr/share/keyrings/debian-archive-keyring.gpg` (install
`debian-archive-keyring` if missing), and unpacks them into `./sysroot`. Nothing is installed
on the Pi and nothing on the Pi is modified.

The remote form still works and no longer needs Pi-side networking: it reads the Pi's
`/var/lib/dpkg/status`, downloads only the delta here (typically 3 packages / ~285 KB rather
than the full 80-package closure), scp's those over, `dpkg -i`'s them, then rsyncs the result
back. It refuses to downgrade anything on the target unless you pass `--allow-downgrade` —
Stratux images sometimes carry Raspberry Pi's Mesa rather than Debian's.

Prefer `--offline` unless you have a reason to mirror the real filesystem: it does not mutate
a configured flight machine, and it is reproducible from an archive snapshot.

### The cross-glibc trap

**`deploy/check-glibc.sh` exists because this bug is invisible until the Pi refuses to exec
the binary.** `deploy.sh` runs it automatically; run it by hand after any manual build:

```sh
./deploy/check-glibc.sh target/aarch64-unknown-linux-gnu/release/avionics
```

Ubuntu 26.04's cross toolchain carries glibc 2.43, and 2.43 **re-versioned the float maths
functions** — its libm exports `acosf@@GLIBC_2.43` as the default where Bookworm exports
`acosf@@GLIBC_2.17`. femtovg's trig picks those up, so a binary that links cleanly and
reports the right architecture dies on the Pi with ``version `GLIBC_2.43' not found``.

Two separate things have to be right to avoid it, and both are easy to get wrong:

1. **The sysroot needs a complete `libc6-dev`**, not just the runtime `libc6`. Without
   `libc.so`/`libm.so`/`crt1.o` in the sysroot the toolchain quietly falls back to its own.
2. **Absolute symlinks must be rewritten to stay inside the sysroot.** Debian ships
   `usr/lib/<triple>/libm.so -> /lib/<triple>/libm.so.6`; that leading slash resolves against
   the *host's* root, so `ld` links the dev machine's glibc even though `--sysroot` is set.
   `sync-sysroot.sh` relativises them; rsync's `--copy-unsafe-links` covers the mirror path.

Note also that the sysroot search path has to come *before* the toolchain's built-in one.
Neither `-C link-arg=-L…` nor `-L native=…` can do that — rustc emits both after its own `-l`
flags, and `ld` resolves each `-l` against only the `-L` paths seen so far. That is why
`.cargo/config.toml` points `linker` at `deploy/cross-cc-<triple>.sh` instead of at the
cross-gcc directly.

### Reading the result

The test pattern is diagnostic, not decorative. Each element proves one capability a later
milestone needs, so a partial failure localises the problem:

| Element | Proves | Needed by |
| --- | --- | --- |
| Range rings | Stencil path fill + stroke on arcs | M4 plan view |
| Rotating chevrons | Nested transforms | M4 traffic symbols |
| Text at 9/11/14/18 px | Glyph atlas upload and shaping | M4 tags, status bar |
| POT **and** NPOT mosaics | Texture upload + image paint | M5 NEXRAD underlay |
| Alpha ramp | Blending | M5 stale-weather fade |
| FPS counter | Sustained throughput at native res | everything |

A missing or corrupt **NPOT** mosaic is not a failure — GLES 2.0 only guarantees
non-power-of-two textures with `CLAMP_TO_EDGE` and no mipmaps. It means M5 must pad the NEXRAD
mosaic to a power of two. Both mosaics render correctly on Mesa/Intel; `vc4` is the real test.

A blank screen, missing text, or unfilled rings **is** a failure. In that case, before
rewriting anything, check `dmesg` for `vc4`/CMA errors and cross-check the driver itself with
`kmscube` and `eglinfo` from `mesa-utils`. If femtovg's ES2 path is genuinely broken on `vc4`,
Slint's `linuxkms` backend already solves this exact DRM/GBM/EGL/femtovg-on-GLES2 problem and
is the fallback to evaluate — before building UI on a broken foundation.

## Notes for whoever builds M2+

Things discovered the hard way, worth not rediscovering:

- **`drm` is pinned to 0.14, not the newer 0.15.** `gbm` 0.18's `drm-support` feature is built
  against 0.14. Mixing them puts two `drm` crates in the graph, and `BufferObject` then fails
  to implement the `drm::buffer::Buffer` that `add_framebuffer` wants. Bump both together.
- **Cross-linking needs only `libgbm`.** The `drm` crate talks to the kernel through
  `rustix`/`linux-raw-sys` (no libdrm), and `khronos-egl` uses its `dynamic` feature so libEGL
  is `dlopen`'d at runtime. Confirmed on the finished binary — `DT_NEEDED` is exactly
  `libgbm.so.1`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, all four of which the Stratux
  image already has. **Nothing needs installing on the Pi to run this.** The full sysroot
  mirror is convenience, not necessity.
- **The sysroot still needs `libc6-dev` even so**, and its absolute symlinks must be
  relativised, or you link against the dev machine's glibc and the binary will not start on
  the Pi. See "The cross-glibc trap" above; `deploy/check-glibc.sh` catches it.
- **femtovg's `Canvas::set_size()` secretly emits a `SetRenderTarget(Screen)` command** without
  updating the canvas's own `current_render_target` cache. Since `set_render_target()` is a
  no-op when that cache already matches, a later `set_render_target(Image(..))` gets silently
  dropped and you draw into the default framebuffer instead. In a surfaceless context that FBO
  is incomplete, so every draw fails with `GL_INVALID_FRAMEBUFFER_OPERATION` and the output is
  blank. See `bind_target` in `crates/avionics-gfx/src/offscreen.rs`. Don't call `set_size` per
  frame.
- **femtovg needs a stencil buffer.** Its path fill is stencil-based, so any EGL config must
  request `STENCIL_SIZE >= 8` or paths silently draw nothing.
- **Requesting a specific GLES version via EGL is a floor, not a match.** Asking for
  `Gles(Some(2.0))` on Mesa yields an ES 3.2 context, so a desktop harness cannot be trusted to
  enforce the Pi's ES2 constraints no matter how it asks. Log the negotiated version and treat the
  hardware as the authority.
- **femtovg's README claims "OpenGl (ES) 3.0+".** That line is stale; the renderer detects ES2
  at runtime (`version.starts_with("OpenGL ES 2.")`) and has ES2-specific code paths. This is
  why the Pi 3 is viable at all.
- **`set -euo pipefail` aborts on `v=$(pipeline-that-finds-nothing)`.** This bit both hardening
  scripts: a Stratux that simply wasn't running would kill a 30-minute soak, and a `grep` with no
  match would abort the power-cut check halfway, reporting neither pass nor fail. Guard every
  extraction with `|| true` and default the value.
- **`panic = "abort"` in the release profile means `Drop` does not run.** A panic will leave the
  console in graphics mode; `sudo chvt 1` recovers it. This is the right trade for the shipping
  kiosk (fail fast, let systemd restart clean) but it surprises you during development.

### On the Stratux side

- **No HTTP polling is needed.** `/status` pushes `globalStatus` every 1 s and `/situation`
  pushes `mySituation` every **100 ms**, both on a plain ticker. The plan originally called for
  polling `GET /getStatus`; the `/status` socket makes that unnecessary and removes any need for
  an HTTP client.
- **`/weather` does not replay the current buffer on connect**, despite the HTTP API docs saying
  it does. `handleWeatherWS` only calls `weatherUpdate.AddSocket(conn)`. Consequences: weather
  must never be cleared on reconnect (see `AppState::apply`), and a fresh start shows no weather
  at all until the next FIS-B cycle — minutes for text, ~5 for NEXRAD.
- **`/traffic` *does* replay current traffic on connect**, so a reconnect re-populates targets by
  itself and stale ones age out naturally.
- **Positions are Go `float32`.** `TrafficInfo.Lat/Lng` and `SituationData.GPSLatitude/Longitude`
  carry ~1e-6 degrees (~0.2 m) of rounding once widened to `f64`. Fine for a plan view; never
  compare a position for equality or use one as a map key.
- **`TRAFFIC_SOURCE_*` values**, confirmed from upstream: 1090ES = 1, UAT = 2, OGN = 4, AIS = 8.
  These drive the "which radio heard this" indication, so guessing them wrong is user-visible.
- **The two NEXRAD products do not share an intensity scale.** Upstream fills an empty *regional*
  block with 0 and an empty *CONUS* block with 1, which is the tell: on regional, 0 means "looked,
  below 5 dBZ", whereas on CONUS that state is 1 and 0 means "no data at all". So CONUS is offset
  by one. Treating them alike paints phantom precipitation everywhere or punches holes through
  real coverage — and both failures look completely plausible on screen. `crates/avionics-ui/
  tests/weather.rs` pins this, along with transposition and mirroring of the mosaic.
- **`UAT_messages_last_minute` comes *before* `ES_messages_last_minute`** in Stratux's status
  JSON. Extracting both with one order-dependent grep silently swaps them, and a soak report
  that blames the wrong radio is worse than no report. Pull status fields by name.
- **Stratux uses `golang.org/x/net/websocket`**, the old Go package. It does no ping/pong
  keepalive, so a wedged socket is detected by the per-stream staleness clock rather than by the
  transport. Timeouts are per-stream because the natural rates differ by orders of magnitude
  (3 s for 10 Hz own-ship, 600 s for weather).
