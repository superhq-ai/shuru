#!/bin/sh
# Renders SVG app icons to the PNG sizes shuru-desktop draws, 16 and 32 px
# and their doubles, into the hicolor theme under a prefix.
#   render-icons.sh PREFIX NAME=FILE.svg ...
# such as render-icons.sh /usr/local/share utilities-terminal=/path/terminal.svg
set -eu
prefix=$1
shift
for pair in "$@"; do
  name=${pair%%=*}
  svg=${pair#*=}
  for size in 16 32 64; do
    dir="$prefix/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    rsvg-convert -w "$size" -h "$size" "$svg" -o "$dir/$name.png"
  done
done
