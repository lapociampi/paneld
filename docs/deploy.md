---
title: "Deploy"
description: "Cross-compile paneld for an arm64 Linux board and run it as a systemd service."
date: 2026-10-30
tags: [paneld, deploy, pi, systemd]
series: paneld
order: 7
---

v1. The first target is a Raspberry Pi 4 (arm64) running Raspberry Pi OS Lite ([Requirements](requirements.md)), but the same build runs on any Linux board that has `/dev/dri/card*` (DRM/KMS) or `/dev/fb0` (fbdev) and a touchscreen on `/dev/input/event*`.

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
# From the workspace root. Produces a stripped, 1.5-2 MB static binary.
cargo zigbuild --target aarch64-unknown-linux-musl \
    -p paneld --no-default-features --release
```

If the Pi has `libdrm` installed system-wide (Raspberry Pi OS Lite does), add `--features device` to turn on the DRM/KMS backend. The build needs a cross-sysroot that carries `libdrm`'s headers (e.g. a Pi root tarball under `--sysroot`); the default zig toolchain does not include them.

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

Device paths vary: `card0` on some boards, `card1` on the Pi. The touch controller shows up under `/dev/input/`; `evtest` lists the available devices.

## Diagnostics

- `journalctl -u paneld -f` -- daemon logs.
- `paneld check -c /etc/paneld/paneld.conf` -- offline config validation ([Config format](config-format.md)).
- `paneld ctl ping` / `reload` / `exit` -- talk to a running daemon over its Unix socket at `$PANELD_SOCK` (set to `/run/paneld/paneld.sock` by the service file).
