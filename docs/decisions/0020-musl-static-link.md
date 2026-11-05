---
title: "ADR 0020: Static musl builds link Rust's musl with rust-lld"
description: "Which libc and allocator the static paneld binaries link, and why not Zig's."
date: 2026-11-05
tags: [paneld, adr, build]
series: paneld
order: 27
---

- Status: Accepted. Amends the build side of [ADR 0002](0002-rust-lvgl.md) and [ADR 0017](0017-vendored-libdrm.md).
- Date: 2026-11-05

## Context

- `cargo zigbuild` links with `zig cc`. zig 0.17 links its own libc, whose `malloc` sits on `heap.SmpAllocator`: per-CPU freelists, small blocks never go back.
- paneld frees in other threads than it allocates (one runner thread per command run). On the Pi, RSS grew about 1.2 KB per command run, about 13 MB a day, with jumps of tens of MB.
- Rust's `*-linux-musl` targets ship a self-contained musl (`libc.a`, crt objects) built with mallocng, and rustup ships `rust-lld`.

## Decision

- `.cargo/musl-static.toml` sets, for `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`: `linker = "rust-lld"`, `-C link-self-contained=yes`. The link takes Rust's musl, so `malloc` is mallocng.
- `cargo zigbuild` still drives the build: `zig cc -target <arch>-linux-musl` compiles LVGL and libdrm through `cc`, and it sets up bindgen. The file is passed with `--config` because cargo-zigbuild sets `CARGO_TARGET_<T>_LINKER`, which beats `.cargo/config.toml` but not `--config`.
- `scripts/build-static.sh` wraps the command; `scripts/check-musl-malloc.sh` reads an unstripped binary and fails unless `malloc`, `free`, `calloc` and `realloc` are musl's: each defined once, `malloc` the weak alias at `default_malloc`'s address, `free` / `realloc` branching to `__libc_free` / `__libc_realloc`, `calloc` calling `malloc`, mallocng (`__malloc_alloc_meta`) present. A strong override (Zig's libc, or any other) fails it.

Rejected: pinning an older zig that still builds musl's own malloc (none installed; ties the build to an old toolchain for a libc detail), a Rust global allocator (LVGL and libdrm call `malloc` too), glibc static (NSS and `dlopen` caveats).

## Consequences

- RSS stays flat: 3 `sh` sources every 200 ms for 3 minutes (about 2700 runs), x86_64 headless: 1.9 MB flat, against 3.1 to 13.1 MB with zig's libc.
- Size, stripped, `--features device`, measured on a clean build of commit `9f129b2`: aarch64 2,641,520 to 2,660,832 bytes (about 2.7 MB, +19 KB); x86_64 3,049,408 to 3,156,600 bytes (about 3.2 MB, +105 KB; Rust's x86_64 musl target links static-pie).
- `cargo zigbuild` without `--config .cargo/musl-static.toml` builds a binary with the leaking allocator; the check catches it.
