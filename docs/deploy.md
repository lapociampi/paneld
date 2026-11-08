---
title: "Deploy"
description: "Cross-compile paneld for an arm64 Linux board and run it as a systemd service."
date: 2026-10-30
updated: 2026-11-08
tags: [paneld, deploy, pi, systemd]
series: paneld
order: 7
---

v1. The first target is a Raspberry Pi 3B+ (arm64) running Raspberry Pi OS Lite ([Requirements](requirements.md)), but the same build runs on any Linux board that has `/dev/dri/card*` (DRM/KMS) or `/dev/fb0` (fbdev) and a touchscreen on `/dev/input/event*`.

## Build toolchain

paneld cross-compiles from a Linux dev box using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild). The daemon statically links `musl` libc, so the binary runs on any Linux kernel without a runtime libc version match.

`zig cc` compiles the C parts (LVGL, libdrm); the link uses `rust-lld` (ships with rustup) and Rust's own musl, whose allocator is musl's mallocng ([ADR 0020](decisions/0020-musl-static-link.md)). `.cargo/musl-static.toml` sets this up and is passed with `--config`: without it, zig links its own libc, whose malloc (`heap.SmpAllocator`) never gives memory back and lets paneld's RSS grow with every command run.

```
rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
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
# From the workspace root. Produces a stripped, about 2.7 MB static binary.
cargo zigbuild --config .cargo/musl-static.toml \
    --target aarch64-unknown-linux-musl \
    -p paneld --no-default-features --features device --release
```

`scripts/build-static.sh [--check] [target] [-- cargo args]` runs the same command (target defaults to `aarch64-unknown-linux-musl`; `x86_64-unknown-linux-musl` gives the desktop static build, about 3.2 MB, static-pie; cargo args after `--` replace `--features device`). Sizes are for commit `9f129b2`; later features add to them. Never drop `--config .cargo/musl-static.toml`.

`--features device` is the DRM/KMS backend; without it the binary has fbdev only. It needs libdrm, in one of two ways:

- **Vendored** (the default; static, nothing to install on the Pi): `vendor/libdrm` is a submodule at `libdrm-2.4.134` ([ADR 0017](decisions/0017-vendored-libdrm.md)); `git submodule update --init --recursive` fetches it, and `lvgl-sys` compiles libdrm's core into the binary (about 80 KB more). `PANELD_LIBDRM_SRC` points at another checkout instead.
- **From the system**: a cross-sysroot that carries `libdrm`'s headers and library (e.g. a Pi root tarball under `--sysroot`); the default zig toolchain has neither.

The resulting binary lives at `target/aarch64-unknown-linux-musl/release/paneld`.

### Allocator check

`scripts/check-musl-malloc.sh <binary>...` fails unless `malloc`, `free`, `calloc` and `realloc` resolve to musl's mallocng (each defined once, `malloc` is musl's weak alias, `free` / `realloc` go to `__libc_free` / `__libc_realloc`); a strong override, such as Zig's libc malloc (`heap.SmpAllocator`), fails it. It needs `nm` and `llvm-objdump`. It reads symbols, so it needs an unstripped build (`CARGO_PROFILE_RELEASE_STRIP=none`). `scripts/build-static.sh --check [target]` does both: builds with symbols, copies that binary to `paneld.unstripped` next to the release one and checks it, then builds the stripped binary. Run it before every deploy.

```
scripts/build-static.sh --check aarch64-unknown-linux-musl
scripts/build-static.sh --check x86_64-unknown-linux-musl
```

## Releases

Each release on GitHub carries the static binaries, so a board needs no build toolchain: `paneld-vX.Y.Z-aarch64-linux-musl.tar.gz` (Raspberry Pi 3, 4 and 5 on a 64-bit OS, other arm64 boards) and `paneld-vX.Y.Z-x86_64-linux-musl.tar.gz` (PCs, mini PCs), plus `SHA256SUMS`. An archive holds the binary, `paneld.service`, `INSTALL.md` (the steps below, short), `LICENSE` and the licenses of everything built into the binary in `licenses/`.

```
# Next to the downloaded archive and SHA256SUMS:
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf paneld-vX.Y.Z-aarch64-linux-musl.tar.gz
cd paneld-vX.Y.Z-aarch64-linux-musl
```

`uname -m` on the board says which archive: `aarch64` or `x86_64`; a 32-bit OS (`armv7l`) has no release build. Then install as below, copying the unpacked `paneld` and `paneld.service` to the device instead of the built ones (or follow `INSTALL.md` on the device).

Making a release takes one run of `.github/workflows/release.yml`: Actions, `release`, Run workflow, or `gh workflow run release.yml -f bump=patch` (`patch`, `minor` or `major`; `draft` and `prerelease` are optional; with `draft`, the tag is made when you publish the draft on the releases page). It runs from `main` and:

1. Raises that part of `version` in `Cargo.toml` (0.1.0 becomes 0.1.1, 0.2.0 or 1.0.0), updates `Cargo.lock` to match and pushes the two as one commit, `release vX.Y.Z`, on `main`. It stops first if tag `vX.Y.Z` exists, and pushes nothing if `main` moved meanwhile.
2. Runs `.github/workflows/release-executor.yml` on that commit: checks that the version is `X.Y.Z` (or `X.Y.Z-pre`), matches `Cargo.toml` and has no tag yet; runs the tests, clippy and `paneld check` on the examples; builds both targets with `scripts/build-static.sh --check` (the allocator check above); packs them with `scripts/package-release.sh`; then creates tag `vX.Y.Z` on that commit and the release, with notes from the commit messages since the last tag. Nothing is published unless every step passes.

`release` refuses to bump a version that was never released: the first release, or the bump of a run whose executor failed. Release that version with `release executor` by hand (`gh workflow run release-executor.yml -f version=X.Y.Z`, for the version already in `Cargo.toml` on `main`; `-f sha=...` picks a commit). After a failed run, fix the cause and use "Re-run failed jobs" on it: that releases the bump commit it pushed. "Re-run all jobs", or a new `release` run, would bump once more. Run one release at a time.

The bump is pushed with the workflow's own token, so a protected `main` refuses it unless a ruleset lets GitHub Actions bypass it (or the push uses a deploy key).

`scripts/package-release.sh VERSION TARGET [OUT_DIR]` packs a local build the same way (after `scripts/build-static.sh --check TARGET`; needs `jq` and GNU tar), into `dist/` by default. A run that fails while uploading can leave a draft release behind; delete it on the releases page before running again.

## Install on the device

```
# Copy the binary and the systemd unit.
scp target/aarch64-unknown-linux-musl/release/paneld pi@panel:/tmp/paneld
scp packaging/paneld.service pi@panel:/tmp/paneld.service

# On the device:
sudo install -m 0755 /tmp/paneld /usr/local/bin/paneld
sudo install -m 0644 /tmp/paneld.service /etc/systemd/system/paneld.service
sudo useradd --system --no-create-home --shell /usr/sbin/nologin --groups video,input paneld
sudo install -d -m 0750 -o paneld -g paneld /etc/paneld /var/lib/paneld
sudo cp paneld.conf /etc/paneld/paneld.conf

sudo -u paneld paneld check -c /etc/paneld/paneld.conf
sudo systemctl daemon-reload
sudo systemctl enable --now paneld
```

## The text console

Raspberry Pi OS Lite shows a login prompt (`getty@tty1`) and the kernel console on the screen. While paneld runs it owns the display, so neither is visible; when paneld exits, the kernel puts the console back. On a panel that has no keyboard that console is only noise, so the device setup:

- disables the login prompt: `sudo systemctl disable --now getty@tty1` (log in over SSH; `systemctl start getty@tty1` brings it back for a local session);
- adds `vt.global_cursor_default=0 logo.nologo quiet` to the kernel command line (`/boot/firmware/cmdline.txt` on the Pi, one line): no blinking cursor, no boot logo, fewer boot messages.

While paneld restarts after a crash (`Restart=on-failure`, 5s) the screen is black, or shows the last kernel messages. paneld's unit does not touch the console itself: the setup belongs to the device, not to the program.

## Config block for the device

Nothing is needed: without a `display` block paneld picks DRM on the card that has a connected screen (fbdev if there is none) and the first touchscreen, and logs both at start ([Schema](schema.md), section 13):

```
paneld: display: drm on /dev/dri/card0 (picked)
paneld: touch: /dev/input/event0 (found: 10-0038 generic ft5x06 (79))
```

Set what the detection gets wrong, or turn the picture:

```
display {
    backend  = drm
    device   = /dev/dri/card1
    touch    = /dev/input/event0
    rotation = 180
}
```

Device paths vary: `card0` on the Pi 3B+ (one GPU device), `card1` on the Pi 4 and 5 (`card0` is the 3D-only V3D there). `ls /sys/class/drm/` shows which card has the connector (e.g. `card0-DSI-1`). The touch controller shows up under `/dev/input/`; `cat /proc/bus/input/devices` or `evtest` lists the available devices.

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

## Remotes over ssh

A `remote_kind` with `via = ssh` ([Schema](schema.md), section 17) runs `ssh` as the service user, which has no home folder. It needs its own key and a `known_hosts` it can write:

```
# On the panel: a key only paneld can read.
sudo -u paneld ssh-keygen -t ed25519 -N '' -C paneld -f /var/lib/paneld/id_ed25519

# Trust each host once (or set `host_key = accept_new` in the kind).
ssh-keyscan -t ed25519 10.0.0.5 | sudo -u paneld tee -a /var/lib/paneld/known_hosts >/dev/null

# On each remote: allow the key, ideally for a user that can only do
# what the panel needs (sudo rules for `systemctl reboot`, ...).
cat /var/lib/paneld/id_ed25519.pub   # append to ~/.ssh/authorized_keys there
```

```
remote_kind linux {
    param host: string
    via  = ssh
    host = {{ host }}
    user = panel
    key  = /var/lib/paneld/id_ed25519
    ...
}
```

`known_hosts` defaults to the folder of `app:state` (`/var/lib/paneld`). Runs to the same host share one connection, kept open for a minute after the last one (`reuse`); its socket lives in `/run/paneld`. `via = http` needs `curl` (installed on Raspberry Pi OS); `via = socket` needs `sudo apt install socat`.

## Diagnostics

- `journalctl -u paneld -f` -- daemon logs.
- `paneld check -c /etc/paneld/paneld.conf` -- offline config validation ([Config format](config-format.md)).
- `paneld shot -c paneld.conf -o page.png [--page P] [--size 800x480] [--rotation 90] [--tap X,Y | --hold X,Y | --down X,Y | --up | --wait MS | --set VAR=VALUE | --source NAME=VALUE | --goto PAGE]...` -- render the UI without a display into a PNG, optionally after taps, long presses, a finger put down (or moved: a drag) and lifted again with the steps between done under it, pauses, var sets, page changes (`--goto`, as `paneld ctl goto`) and source results (`--source cpu=42`: a new result with that `value`, as a poll gives; `--source vms=@demo/vms.json`: the JSON in that file, relative to the current directory; a file that does not read or is not JSON is an error). With `--rotation`, `--size` is the panel's and the PNG and the tap points are the panel's too, as on the device. Sources and touched actions run for real (`--no-sources`: no source command runs; `--no-actions`: an action tapped runs nothing and ends at once, as done); screen commands (`dim`, `power`) do not; the alert state file is not opened. `--verify` compares the picture after each step, first as the screen has it (redrawn only where the page asked) with it redrawn whole, then with the same page built afresh, pixel for pixel once motion has ended (spinners and scrolling text left out), and exits 3 if they differ (`PANELD_VERIFY_DIR=<dir>`: the pictures are written there); `--fuzz N [--seed S]` then makes N random changes (source values and failed runs, lists growing, shrinking, reordered and with keys changed, vars, alerts fired, resolved, acknowledged and snoozed, call states, the clock; `--fuzz-taps`: random taps too, and changes landing while a finger is down; `--fuzz-pages`: going to other pages and back, switching the theme and reloading now and then, so changes land on pages kept off screen) ([ADR 0024](decisions/0024-in-place-updates.md)).
- `PANELD_PERF=1` in the daemon's environment (e.g. a test run by hand with the service stopped: `sudo -u paneld env PANELD_PERF=1 PANELD_SOCK=/tmp/x.sock paneld -c ...`) prints timings to stderr; read once at start, nothing is timed without it. On DRM, one line per drawn frame, `perf t= gap= pre= render= post= wait= total= px=`, times in microseconds unless noted:

  | Field | Meaning |
  | --- | --- |
  | `t` | Frame start, `CLOCK_MONOTONIC` |
  | `gap` | Milliseconds since the last frame started; about 33 while something animates (30 fps) |
  | `pre` | Layout and LVGL's buffer sync before drawing; large on the first frame of a new page |
  | `render` | Drawing, plus the flush: the wait for the last page flip and the copy of the changed areas into the panel buffer |
  | `post` | After drawing, until the frame ends |
  | `wait` | LVGL's own flush waits; about 0 on DRM, since the flip wait is inside `render` |
  | `total` | Whole frame; well under 33000 for a smooth animation |
  | `px` | Pixels redrawn (384000 is all of 800x480) |

  And one line per page rebuild, `perf build page=<page> same=<true: a value change, false: navigation> us=<time to build it> ctx= clear= render= [parse= eval= clone= props= font=] tr= at=`: building the context, clearing the old page, rendering the new one (inside it, as `us/count`: parsing `{{ }}` bodies, evaluating them, copying a component call's own properties onto its root (component params and `repeat` items open a scope over the context, nothing is copied: [ADR 0024](decisions/0024-in-place-updates.md)), applying node and style properties (includes their evaluation), setting fonts), starting the transition; `at` is the end, on the frame lines' clock. One `perf event at=` line per UI event (a tap) taken from the queue.

  A value change updates the page in place instead ([ADR 0024](decisions/0024-in-place-updates.md)): one line per update, `perf update page= us= ctx= walk= [...] visited= reapplied= redrawn= restructured= held= made= changes= at=`: the context, the walk over the page's nodes (the same fine slots), the nodes looked at, set up again and restyled, the subtrees built again and `repeat`s whose items changed (built in place: the rest of the page kept), whether something under a finger waits for the release (done then), objects made (0: every object kept), and what changed (`sources.cpu.value`, `vars.mode`, `alerts.count`, `now:minute`, `ago`, `call:<action>`). When an update cannot be done in place, `perf rebuild page= why= changes=` says why (`no such page`, `no page drawn`, `another page`, or a table that does not match the page: a bug, which verify reports) and a `perf build` line follows. Going back to a page kept in the cache (`app:page_cache`) updates it instead of building it: `perf cached page= us= ctx= clear= update= [...] Done { visited, reapplied, redrawn, restructured, held } tr= at=` (`Nothing`: nothing it reads changed); `perf rebuild page= why= cached=true` when it could not be (it is built then). `PANELD_UPDATES=0` builds the page on every change, as before, to compare the two. `PANELD_VERIFY_UPDATES=1` (on in every test) builds each updated page afresh on a scratch screen and compares the two object trees; a difference is logged with the node's place and the process exits 3.
- Memory: on DRM, LVGL draws into a shadow buffer of one frame (1.5 MB at 800x480) that is copied into the panel's buffers ([ADR 0012](decisions/0012-display-input.md)); count it in the RSS.
- `paneld ctl status` / `check` / `reload` / `eval '<cel>'` / `set vars.x=1` / `goto <page>` / `alert fire|resolve|ack|snooze <id>` / `reminder done <name>` -- talk to a running daemon over its Unix socket at `$PANELD_SOCK` (set to `/run/paneld/paneld.sock` by the service file; mode 0660; without it `$XDG_RUNTIME_DIR/paneld.sock`, else `/tmp/paneld-<uid>/paneld.sock` in a directory only that user can open). `status` also has `renders` (full page builds since start), `updates` (updates in place, [ADR 0024](decisions/0024-in-place-updates.md)), `nodes` (LVGL objects on the screen and its top layer), `node_records` and `node_objects` (the page's node table: records, and the objects they made), `objects_made` (objects pages made since start), `cached_pages`, `cache_bytes` and `cache_hits` (pages kept off screen, their estimated bytes, and pages shown from the cache since start; `nodes` counts their objects too) and `rss_kb` (resident memory). A second daemon refuses to start on a socket that a running one answers on. `--json` prints the raw replies ([ADR 0014](decisions/0014-ctl-protocol-state-file.md)); scripts can also speak the JSON lines directly, e.g. with `socat`.
