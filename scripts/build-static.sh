#!/bin/sh
# Build the static musl paneld binary: C parts (LVGL, libdrm) with
# `zig cc` through cargo-zigbuild, the link with rust-lld against
# Rust's own musl (mallocng), see .cargo/musl-static.toml.
#
# usage: scripts/build-static.sh [--check] [target] [-- cargo args...]
#   target   aarch64-unknown-linux-musl (default, the Pi) or
#            x86_64-unknown-linux-musl (desktop static); any position
#   --check  first build with symbols and run scripts/check-musl-malloc.sh
#            on it (the unstripped copy is kept as paneld.unstripped)
#   -- ...   cargo args that replace the default `--features device`
#
# Output: ${CARGO_TARGET_DIR:-target}/<target>/release/paneld (stripped).
set -eu
cd "$(dirname "$0")/.."

usage() { sed -n 's/^# \{0,1\}//; 6,11p' "$0" >&2; exit 2; }

check=0
target=
while [ $# -gt 0 ]; do
    case $1 in
        --check) check=1 ;;
        -h|--help) usage ;;
        --) shift; break ;;
        aarch64-unknown-linux-musl|x86_64-unknown-linux-musl)
            [ -z "$target" ] || { echo "$0: target given twice" >&2; usage; }
            target=$1 ;;
        -*) echo "$0: unknown option $1" >&2; usage ;;
        *) echo "$0: unknown target $1" >&2; usage ;;
    esac
    shift
done
target=${target:-aarch64-unknown-linux-musl}
[ $# -gt 0 ] || set -- --features device

build() {
    cargo zigbuild --config .cargo/musl-static.toml --target "$target" \
        -p paneld --no-default-features --release "$@"
}

out=${CARGO_TARGET_DIR:-target}/$target/release/paneld
if [ $check = 1 ]; then
    CARGO_PROFILE_RELEASE_STRIP=none build "$@"
    cp "$out" "$out.unstripped"
    scripts/check-musl-malloc.sh "$out.unstripped"
fi
build "$@"
ls -l "$out"
