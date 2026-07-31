# Tagging the SDR serials so 1090 and 978 cannot swap

Stratux decides which dongle demodulates 1090 MHz (ADS-B) and which does 978 MHz (UAT +
FIS-B) by reading a role tag out of each dongle's **EEPROM serial string**. The convention is

    stx:<freq>:<ppm>          e.g.  stx:1090:0   stx:978:0

## Why this matters

Observed on the target 2026-07-31:

```
1-1.2.2  NESDR Nano 2  serial='stx:978:0'    <- correctly tagged for UAT
1-1.2.3  NESDR Nano 2  serial='stx:0:0'      <- not a valid role tag
```

It works *right now* — Stratux reports `Devices: 2` and 1090 is decoding — because with one
dongle claimed by its tag, the other is left over and falls into the remaining role. That is
luck, not configuration. An untagged dongle has no pinned role, so the assignment can differ
across reboots or if USB enumeration order changes (and it will: these sit behind a hub with a
GPS and a NIC that all enumerate in whatever order they wake up).

If the roles swap, **the 1090 antenna feeds the UAT decoder and vice versa**. Both radios stay
"connected", no error appears anywhere, and the traffic picture silently goes half-blind. This
is exactly the class of failure that is invisible in flight, so tag it before trusting it.

## Do NOT install Debian's `rtl-sdr` on the Pi

The obvious move — `apt install rtl-sdr` to get `rtl_eeprom` — is a trap here, and
`deploy/push-packages.sh` refuses it:

```
rtl-sdr : Depends: librtlsdr0 (= 0.6.0-4) but 2.0.2-2 is to be installed
```

The image carries **`librtlsdr0 2.0.2-2`**, which is not in any configured archive:

```
 *** 2.0.2-2 100
        100 /var/lib/dpkg/status          <- locally built, part of the Stratux image
     0.6.0-4 500
        500 http://deb.debian.org/debian bookworm/main arm64 Packages
```

Debian only ships `0.6.0-4`, and its `rtl-sdr` package pins that exact version. Installing it
would **downgrade the SDR library that `dump1090` is linked against and currently decoding
with** (`ldd /opt/stratux/bin/dump1090` → `librtlsdr.so.0`). Do not pass `--allow-upgrades` to
force it.

`/opt/stratux/bin/sdr-tool.sh` is Stratux's own interactive wrapper for this job and is the
right tool in principle — but it shells out to `rtl_eeprom`, which the image does not ship, so
it cannot run as-is.

## Recommended: tag on the dev machine

Ubuntu 26.04 packages `rtl-sdr 2.0.2-2build1` — the same upstream version as the Pi's library —
so the tool matches the library the dongles run under in service. Nothing is installed on the
flight box and nothing there changes.

```sh
sudo apt install rtl-sdr                 # dev machine only
```

Then, **one dongle at a time** — this is the important part:

```sh
# 1. Unplug BOTH dongles from the Pi. Plug ONE into the dev machine.
rtl_test -t                              # confirm exactly one device, note it is index 0

# 2. Back up its EEPROM before writing. A botched write can brick the dongle.
rtl_eeprom -d 0 -r ~/sdr-eeprom-backup-$(date +%F-%H%M).bin

# 3. Write the role tag.
sudo rtl_eeprom -d 0 -s stx:1090:0       # or stx:978:0 for the UAT dongle

# 4. Unplug and replug it (the serial is only re-read on enumeration), then confirm:
rtl_test -t                              # serial should now read stx:1090:0
```

Repeat for the second dongle if it also needs tagging. Restore from the backup with
`rtl_eeprom -d 0 -w <file>` if anything goes wrong.

**Only ever have one dongle attached while doing this.** `rtl_eeprom -d 0` addresses device
index 0, and with two identical NESDRs plugged in, index 0 is whichever enumerated first —
which is not knowable in advance and not stable. Stratux's own `sdr-tool.sh` hardcodes `-d 0`
for the same reason. Tagging the wrong dongle is silent and leaves you with two identically
tagged radios.

## Then verify on the Pi

Plug both back in and check that the tags stuck and the roles landed:

```sh
for d in /sys/bus/usb/devices/*/; do
  v=$(cat "$d/idVendor" 2>/dev/null); p=$(cat "$d/idProduct" 2>/dev/null)
  [ "$v" = "0bda" ] && [ "$p" = "2838" ] && echo "$(basename "$d") $(cat "$d/serial")"
done

curl -s http://127.0.0.1/getStatus | python3 -m json.tool | grep -E 'Devices|UAT_messages|ES_messages'
```

Both dongles should now read `stx:1090:0` and `stx:978:0`, and the assignment is pinned
regardless of enumeration order. Reboot once and re-check the serials to prove it survives.

## Fix the power first

At the time of writing the Pi reports `throttled=0x50005` — under-voltage **and actively
throttled**, at 43 °C, so it is supply, not heat. Do not judge UAT reception, message rates, or
anything else while that is true: an under-volted Pi throttles the CPU that `dump1090` and
`dump978` depend on, and dropped messages will look like an antenna or tagging problem. See
the power notes in the README's M0 section.
