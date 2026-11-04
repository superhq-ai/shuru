#!/bin/sh
# Builds the xvfb package of Debian trixie with xorg-server-xtest-scroll.patch,
# in Docker. The packages land in OUT_DIR (default ./xvfb-deb); install them
# with apt-get install ./xvfb_*.deb ./xserver-common_*.deb.
#
#   build-xvfb-deb.sh [OUT_DIR]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
out=${1:-xvfb-deb}
mkdir -p "$out"
out=$(cd "$out" && pwd)

docker run --rm \
  -v "$here:/patches:ro" \
  -v "$out:/out" \
  -e DEBEMAIL="shuru <shuru@localhost>" \
  -e DEBIAN_FRONTEND=noninteractive \
  debian:trixie sh -euc '
    sed -i "s/^Types: deb$/Types: deb deb-src/" /etc/apt/sources.list.d/debian.sources
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends dpkg-dev devscripts fakeroot >/dev/null
    apt-get build-dep -y -qq xorg-server >/dev/null
    mkdir /build && cd /build
    apt-get source -qq xorg-server
    cd xorg-server-*/
    cp /patches/xorg-server-xtest-scroll.patch debian/patches/
    echo xorg-server-xtest-scroll.patch >> debian/patches/series
    dch --local +scroll "XTEST pointer with scroll axes, for smooth scrolling."
    DEB_BUILD_OPTIONS="nocheck parallel=$(nproc)" dpkg-buildpackage -b -uc -us -j"$(nproc)"
    cp ../xvfb_*.deb ../xserver-common_*.deb /out/
    ls -l /out
  '
