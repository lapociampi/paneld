---
title: "ADR 0012: DRM/KMS display and evdev touch"
description: "Which LVGL display and input backends paneld uses on the device, and why."
date: 2026-10-29
tags: [paneld, adr, lvgl, platform]
series: paneld
order: 19
---

- Status: Accepted
- Date: 2026-10-29

## Context

On the device there is no X or Wayland: the program draws straight to the screen and reads touch from the kernel ([Requirements](../requirements.md), section 13). The first target is a Raspberry Pi 4 on Raspberry Pi OS Lite with the KMS driver (`vc4-kms-v3d`) and a DSI touchscreen whose touch controller shows up as an input device. LVGL 9 ships drivers for DRM, fbdev, evdev and libinput. The binary should stay static (musl).

## Options

Display:

- **DRM/KMS:** the current graphics path on the Pi and most Linux boards; page flips without tearing; needs libdrm (small, builds statically).
- **fbdev:** no dependency, but on KMS systems `/dev/fb0` is an emulation layer; no vsync, so tearing is possible; some newer boards drop it.

Touch:

- **evdev:** reads `/dev/input/eventN` directly; no dependency; LVGL's driver handles one pointer, axis swap and calibration.
- **libinput:** gestures and device hotplug, but pulls in libinput, libudev and mtdev, and makes a static build harder.

## Decision

- Display: **DRM/KMS** by default, **fbdev** as a fallback for boards without usable DRM. **SDL** on a PC. The `display { backend }` option chooses ([Schema](../schema.md), section 13); all three are compiled into the Linux build.
- Touch: **evdev**, device set by `display { touch }` or found by its touch capabilities. Rotation and axis swap follow `display { rotation }`.
- libdrm is built from source and linked statically like LVGL.

## Consequences

- One static binary works on the Pi and on other Linux boards; the fbdev fallback covers older or unusual ones.
- Single touch only: taps, holds and drags. Pinch and multi-finger gestures are out, which the UI does not use.
- The program needs access to `/dev/dri/card*` and `/dev/input/event*`: the service user joins the `video` and `input` groups.
