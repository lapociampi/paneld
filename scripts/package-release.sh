#!/bin/sh
# Pack a built static paneld into a release archive.
#
# usage: scripts/package-release.sh VERSION TARGET [OUT_DIR]
#   VERSION  the release version without the v (must match Cargo.toml)
#   TARGET   aarch64-unknown-linux-musl or x86_64-unknown-linux-musl,
#            built first with scripts/build-static.sh --check TARGET
#   OUT_DIR  where the archive goes (default: dist); relative paths,
#            CARGO_TARGET_DIR's too, are from the repo root
#
# Output: OUT_DIR/paneld-vVERSION-<arch>-linux-musl.tar.gz holding the
# binary, the systemd unit, INSTALL.md, LICENSE and the licenses of
# everything linked into the binary (licenses/). Needs git, jq, GNU tar.
set -eu
cd "$(dirname "$0")/.."

usage() { sed -n 's/^# \{0,1\}//; 4,9p' "$0" >&2; exit 2; }
[ $# -ge 2 ] && [ $# -le 3 ] || usage
version=$1
target=$2
out_dir=${3:-dist}

case $target in
    aarch64-unknown-linux-musl|x86_64-unknown-linux-musl) ;;
    *) echo "$0: unknown target $target" >&2; usage ;;
esac
crate=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ "$version" = "$crate" ] || { echo "$0: version $version is not Cargo.toml's $crate" >&2; exit 1; }

bin=${CARGO_TARGET_DIR:-target}/$target/release/paneld
# build-static.sh --check leaves the unstripped copy it checked.
[ -f "$bin" ] && [ -f "$bin.unstripped" ] || { echo "$0: build $bin with scripts/build-static.sh --check $target first" >&2; exit 1; }
# The binary carries its version as text (its help line): a build of
# another version must not be packed under this name.
grep -qF "paneld $version - " "$bin" || { echo "$0: $bin is not version $version" >&2; exit 1; }
if [ "${target%%-*}" = "$(uname -m)" ]; then
    [ "$("$bin" --version)" = "paneld $version" ] || { echo "$0: $bin --version is not paneld $version" >&2; exit 1; }
fi

name=paneld-v$version-${target%%-*}-linux-musl
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
lic=$dir/licenses
mkdir -p "$lic/rust"

install -m 0755 "$bin" "$dir/paneld"
cp packaging/paneld.service LICENSE "$dir/"
sed "s/VERSION/$version/g; s/TARGET/$target/" packaging/INSTALL.md > "$dir/INSTALL.md"

# C code: LVGL with the libraries it builds in (QR code generator,
# LodePNG, stb_truetype for TinyTTF), and libdrm, whose MIT terms
# are in its source headers (the files with other holders included).
cp vendor/lvgl/LICENCE.txt "$lic/lvgl.txt"
cp vendor/lvgl/src/libs/qrcode/LICENSE.txt "$lic/qrcodegen.txt"
cp vendor/lvgl/src/libs/lodepng/LICENSE.txt "$lic/lodepng.txt"
cp vendor/lvgl/src/libs/tiny_ttf/LICENSE.txt "$lic/stb_truetype.txt"
notice() {
    printf 'From %s:\n\n' "${1#vendor/libdrm/}"
    sed -n '/^ \* Copyright/,/^ \*\//p' "$1" | sed 's/^ \*\/$//; s/^ \* \{0,1\}//'
}
for f in vendor/libdrm/xf86drm.h vendor/libdrm/xf86drmMode.c vendor/libdrm/include/drm/drm_fourcc.h; do
    notice "$f"
done > "$lic/libdrm.txt"
grep -q "Precision Insight" "$lic/libdrm.txt" && grep -q "Tungsten Graphics" "$lic/libdrm.txt" \
    && grep -q "Intel Corporation" "$lic/libdrm.txt" || { echo "$0: a libdrm notice is missing" >&2; exit 1; }

# Fonts: the five TTF families, and Font Awesome inside LVGL's built-in
# Montserrat bitmap fonts.
cp crates/paneld-ui/fonts/OFL-*.txt "$lic/"
{ cat packaging/licenses/fontawesome.txt; sed -n '/^This Font Software is licensed/,$p' crates/paneld-ui/fonts/OFL-roboto.txt; } > "$lic/fontawesome.txt"

# Rust: the standard library, musl, and every crate linked into this
# target's binary, with the license files the crate ships.
cp packaging/licenses/musl.txt packaging/licenses/rust-std.txt "$lic/"
meta=$stage/metadata.json
cargo metadata --color never --format-version 1 --filter-platform "$target" > "$meta"
cargo tree --color never -p paneld --target "$target" --no-default-features --features device -e normal \
    --prefix none --format '{p}' | sed 's/ (\*)$//; / (\//d' | sort -u | while read -r crate ver; do
    ver=${ver#v}
    src=$(jq -r --arg n "$crate" --arg v "$ver" '.packages[] | select(.name == $n and .version == $v) | .manifest_path' "$meta")
    src=${src%/Cargo.toml}
    files=$(ls "$src" | grep -iE '^(licen[cs]e|copying)' || true)
    [ -n "$files" ] || { echo "$0: $crate $ver ships no license file" >&2; exit 1; }
    mkdir "$lic/rust/$crate-$ver"
    for f in $files; do
        cp "$src/$f" "$lic/rust/$crate-$ver/"
    done
done
rm "$meta"

mkdir -p "$out_dir"
# Same bytes for the same inputs: fixed order, owner, modes and times.
mtime=$(git log -1 --format=%ct)
tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner --mode='u+rwX,go=rX' \
    --mtime="@$mtime" -cf "$stage/$name.tar" "$name"
gzip -n -9 "$stage/$name.tar"
mv "$stage/$name.tar.gz" "$out_dir/$name.tar.gz"
ls -l "$out_dir/$name.tar.gz"
