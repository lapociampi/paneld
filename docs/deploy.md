---
title: "Deploy"
description: "Cross-compile paneld for an arm64 Linux board and run it as a systemd service."
date: 2026-10-30
updated: 2026-11-03
tags: [paneld, deploy, pi, systemd]
series: paneld
order: 7
---

v1. The first target is a Raspberry Pi 3B+ (arm64) running Raspberry Pi OS Lite ([Requirements](requirements.md)), but the same build runs on any Linux board that has `/dev/dri/card*` (DRM/KMS) or `/dev/fb0` (fbdev) and a touchscreen on `/dev/input/event*`.

## Build toolchain

paneld cross-compiles from a Linux dev box using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild). The daemon statically links `musl` libc, so the binary runs on any Linux kernel without a runtime libc version match.

```
rustup target add aarch64-unknown-linux-musl
pacman -S zig              # or: brew install zig; apt install zig
cargo install cargo-zigbuild
```

## Feature flags

The `lvgl-sys` and `paneld-ui` crates expose Cargo features for the LVGL display backends ([ADR 0012](decisions/0012-display-input.md)):

| Feature  | Backend                      | Pulls in     |
| ---      | ---                          | ---          |
| `sdl`    | SDL2 window on a PC          | `libSDL2`    |
| `drm`    | DRM/KMS on a Linux device    | `libdrm`     |
| `device` | DRM/KMS (preset)             | `libdrm`     |
| default  | `sdl`                        | `libSDL2`    |

Every build also includes the fbdev + evdev drivers; they have no external dependency and `fbdev` is the always-available fallback.

## Cross-compile for the Pi

```
# From the workspace root. Produces a stripped, about 2.1 MB static binary.
cargo zigbuild --target aarch64-unknown-linux-musl \
    -p paneld --no-default-features --release
```

Add `--features device` for the DRM/KMS backend. It needs libdrm, in one of two ways:

- **Vendored** (the default; static, nothing to install on the Pi): `vendor/libdrm` is a submodule at `libdrm-2.4.134` ([ADR 0017](decisions/0017-vendored-libdrm.md)); `git submodule update --init --recursive` fetches it, and `lvgl-sys` compiles libdrm's core into the binary (about 80 KB more). `PANELD_LIBDRM_SRC` points at another checkout instead.
- **From the system**: a cross-sysroot that carries `libdrm`'s headers and library (e.g. a Pi root tarball under `--sysroot`); the default zig toolchain has neither.

The resulting binary lives at `target/aarch64-unknown-linux-musl/release/paneld`.

## Install on the device

```
# Copy the binary and the systemd unit.
scp target/aarch64-unknown-linux-musl/release/paneld pi@panel:/tmp/paneld
scp packaging/paneld.service pi@panel:/tmp/paneld.service

# On the device:
sudo install -m 0755 /tmp/paneld /usr/local/bin/paneld
sudo install -m 0644 /tmp/paneld.service /etc/systemd/system/paneld.service
sudo useradd --system --no-create-home --groups video,input paneld
sudo install -d -o paneld -g paneld /etc/paneld /var/lib/paneld
sudo cp paneld.conf /etc/paneld/paneld.conf

sudo systemctl daemon-reload
sudo systemctl enable --now paneld
```

## The text console

Raspberry Pi OS Lite shows a login prompt (`getty@tty1`) and the kernel console on the screen. While paneld runs it owns the display, so neither is visible; when paneld exits, the kernel puts the console back. On a panel that has no keyboard that console is only noise, so the device setup:

- disables the login prompt: `sudo systemctl disable --now getty@tty1` (log in over SSH; `systemctl start getty@tty1` brings it back for a local session);
- adds `vt.global_cursor_default=0 logo.nologo quiet` to the kernel command line (`/boot/firmware/cmdline.txt` on the Pi, one line): no blinking cursor, no boot logo, fewer boot messages.

While paneld restarts after a crash (`Restart=on-failure`, 5s) the screen is black, or shows the last kernel messages. paneld's unit does not touch the console itself: the setup belongs to the device, not to the program.

## Config block for the device

```
display {
    backend = drm
    device  = /dev/dri/card1
    touch   = /dev/input/event0
    width   = 800
    height  = 480
}
```

Device paths vary: `card0` on the Pi 3B+ (one GPU device), `card1` on the Pi 4 and 5 (`card0` is the 3D-only V3D there). `ls /sys/class/drm/` shows which card has the connector (e.g. `card0-DSI-1`). The touch controller shows up under `/dev/input/`; `evtest` lists the available devices.

## PIN for guarded actions

Actions marked `pin = true` ask for a PIN first ([Schema](schema.md), section 14). The config never holds the PIN, only an argon2 hash in a secret:

```
# On the device. The PIN is read from stdin, so it stays out of
# shell history and `ps`.
read -rs PIN && printf '%s\n' "$PIN" | paneld hash-pin \
    | sudo tee /etc/paneld/pin.hash >/dev/null
unset PIN
sudo chown paneld:paneld /etc/paneld/pin.hash
sudo chmod 0600 /etc/paneld/pin.hash
```

```
secret pin_hash { file = /etc/paneld/pin.hash }

security {
    pin        = pin_hash
    unlock_for = 1m     # one PIN unlocks every guarded action this long
    attempts   = 5      # wrong PINs in a row before a lockout
    lockout    = 5m
}
```

Without a usable `security.pin` a guarded action refuses to run. A config reload keeps any active lockout, and the unlock window unless the PIN itself changed.

## Diagnostics

- `journalctl -u paneld -f` -- daemon logs.
- `paneld check -c /etc/paneld/paneld.conf` -- offline config validation ([Config format](config-format.md)).
- `paneld shot -c paneld.conf -o page.png [--page P] [--size 800x480] [--tap X,Y | --hold X,Y | --wait MS]...` -- render the UI without a display into a PNG, optionally after taps, long presses and pauses. Sources and touched actions run for real; the alert state file is not opened.
- `paneld ctl status` / `check` / `reload` / `eval '<cel>'` / `set vars.x=1` / `goto <page>` / `alert fire|resolve|ack|snooze <id>` / `reminder done <name>` -- talk to a running daemon over its Unix socket at `$PANELD_SOCK` (set to `/run/paneld/paneld.sock` by the service file; mode 0660; without it `$XDG_RUNTIME_DIR/paneld.sock`, else `/tmp/paneld-<uid>/paneld.sock` in a directory only that user can open). A second daemon refuses to start on a socket that a running one answers on. `--json` prints the raw replies ([ADR 0014](decisions/0014-ctl-protocol-state-file.md)); scripts can also speak the JSON lines directly, e.g. with `socat`.
