# Enabling KMS on the Stratux Pi — the M1 gate

Reconciled against the real machine on 2026-07-31 (Pi 3B v1.2, Stratux 2.0-pre4,
kernel 6.6.74+rpt-rpi-v8). Read this before touching `config.txt`; the generic
`config.txt.fragment` is the template, this is what it means *here*.

## Where things stand

`/dev/dri` does not exist. The image has no `vc4-kms-v3d` overlay at all, so the KMS
driver never loads — `lsmod` shows only the generic `drm` core. The panel currently runs
on the **legacy firmware framebuffer**:

    bcm2708_fb.fbwidth=800 bcm2708_fb.fbheight=480

so the panel's native mode is **800x480**, and touch arrives via the firmware path as
`raspberrypi-ts`. Mesa is now installed (24.2.8-1~bpo12+rpt5) and the cross-built binary
already resolves and executes there, so the overlay is the last thing between us and M1.

## The edit

Append to `/boot/firmware/config.txt`. Nothing needs removing — this image has no
`rpi-ft5406` and no `vc4-fkms-*`, so neither of the fragment's two hard rules applies.

```
# --- avionics display (M1) ---
dtoverlay=vc4-kms-v3d,cma-128
dtoverlay=vc4-kms-dsi-7inch
```

Two lines, nothing else. `disable_splash`, `boot_delay`, the watchdog and the
`cmdline.txt` additions all belong to M6 — keep this change minimal so that if the panel
goes dark there is exactly one thing to blame.

### Do not touch these

```
sdram_freq=450
core_freq=450
arm_freq=900
```

These are deliberate underclocks from Stratux issue #573, there to reduce EMI. On this
box that is not a performance knob, it is protecting SDR and GPS reception. Leave them.

Be aware that `core_freq=450` caps GPU throughput, so M1's frame rate here will be lower
than a stock Pi 3 would give. Judge the spike against the panel's refresh, not against
numbers from an unthrottled board.

## Before rebooting: keep your way in

**Reboot will drop the `10.0.0.240` address** — it was set with `ip addr add` and never
persisted. Make it survive first, or you lose SSH over ethernet:

```sh
sudo tee /etc/systemd/system/eth0-static.service >/dev/null <<'EOF'
[Unit]
Description=Static address on eth0 for development access
After=network-pre.target
Wants=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/sbin/ip link set eth0 up
ExecStart=/sbin/ip addr replace 10.0.0.240/24 dev eth0
ExecStop=/sbin/ip addr del 10.0.0.240/24 dev eth0

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable eth0-static.service
sudo systemctl start eth0-static.service   # safe to run now: `addr replace` is idempotent
```

`addr replace` rather than `addr add` on purpose — `add` fails with EEXIST if the address
is already present, which would make a `Type=oneshot` unit fail and leave you with no
ethernet. `replace` succeeds either way, so the unit can be started and tested immediately
instead of first being proven at the one moment you cannot afford it to be wrong.

Deliberately a bare `ip` unit rather than enabling `systemd-networkd` or installing a DHCP
client: it touches `eth0` and nothing else, so it cannot interfere with the WiFi AP that
Stratux manages. Remove it when the development phase is over.

## Back up first

`/boot/firmware` is a separate FAT partition, so a backup there survives anything that
happens to the root filesystem — including a kernel that will not boot.

```sh
sudo cp /boot/firmware/config.txt /boot/firmware/config.txt.pre-kms
```

## Recovery, in order of preference

1. **SSH over the Stratux AP.** Adding a display overlay does not stop the boot; it can
   only leave the screen dark. `wlan0` and the AP are independent of everything here, so
   join the `stratux` network and `ssh pi@192.168.10.1`. Restore and reboot:
   `sudo cp /boot/firmware/config.txt.pre-kms /boot/firmware/config.txt && sudo reboot`

2. **HDMI + the Riitek keyboard.** The Pi 3's HDMI keeps working under `vc4-kms-v3d`, so
   if the DSI panel stays dark an HDMI monitor still gets you a console. This is the
   likeliest failure mode, see below.

3. **Pull the SD card.** `mmcblk0p1` is FAT and mounts on any machine. Restore
   `config.txt.pre-kms` over `config.txt`. This always works, needs no network, and cannot
   be locked out of — it is the real safety net.

4. **Serial console.** `enable_uart=1` is already set. Appending
   `console=serial0,115200` to `cmdline.txt` would give a console for diagnosing a boot
   that never reaches userspace. Not worth adding pre-emptively.

## What is actually likely to go wrong

**The panel is a Hysong, not a genuine Raspberry Pi 7".** It works today through the
firmware path, which is forgiving. `vc4-kms-dsi-7inch` drives the official panel's
timings directly, and a clone that is not faithful may come up dark. This is recoverable
(routes 1–3) and is a real possibility, not a formality.

**Touch will change identity.** `raspberrypi-ts` is replaced by the `edt-ft5406`
controller bundled in the DSI overlay. That is expected and wanted — but it means
`avionics-input`, which matches on the device *name*, needs the new string. Capture it:

```sh
grep -A5 -iE 'ft5406|edt' /proc/bus/input/devices
```

**CMA at 128 MB takes it from 1 GB of system RAM**, alongside two SDRs. If memory
pressure appears, `cma-64` is very likely still enough for an 800x480 panel.

Low risk, checked: the `sc16is752-i2c` UART expander produces no `/dev/ttySC*`, so that
overlay is inert on this machine and will not fight the display.

## Verify after reboot

```sh
ls -l /dev/dri/                       # want card0 and renderD128
lsmod | grep -E 'vc4|v3d'             # want vc4 loaded
cat /sys/class/drm/*/modes            # want 800x480 first
grep Cma /proc/meminfo                # want CmaTotal ~131072 kB
systemctl is-active stratux.service   # must still be active
grep -A5 -iE 'ft5406|edt' /proc/bus/input/devices
dmesg | grep -iE 'vc4|dsi|drm' | head -20
```

Then, and only then, M1:

```sh
sudo /tmp/gfx-spike
```

## Rollback

```sh
sudo cp /boot/firmware/config.txt.pre-kms /boot/firmware/config.txt
sudo reboot
```
