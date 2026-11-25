#!/bin/sh
# Render the studio theme's icons and art from the SVG sources in src/
# (drawn for this example) to the PNGs paneld reads. Needs rsvg-convert
# (librsvg). Run from anywhere: sh make-assets.sh
set -eu
cd "$(dirname "$0")"
mkdir -p icons images
for f in src/icons/*.svg; do
    rsvg-convert -w 96 -h 96 "$f" -o "icons/$(basename "$f" .svg).png"
done
for f in src/art/*.svg; do
    rsvg-convert "$f" -o "images/$(basename "$f" .svg).png"
done
