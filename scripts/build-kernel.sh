#!/bin/bash
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.12.114}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
DATA_DIR="${SHURU_DATA_DIR:-${HOME}/.local/share/shuru}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
DEFCONFIG="${REPO_DIR}/kernel/shuru_defconfig"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz"
BUILD_DIR="${DATA_DIR}/kernel-build"

echo "==> Building custom kernel ${KERNEL_VERSION} for Shuru"

if [ ! -f "$DEFCONFIG" ]; then
    echo "ERROR: Defconfig not found at ${DEFCONFIG}"
    exit 1
fi

mkdir -p "$DATA_DIR"

if [ ! -d "${BUILD_DIR}/linux-${KERNEL_VERSION}" ]; then
    echo "    Downloading kernel source..."
    mkdir -p "$BUILD_DIR"
    curl -sL "$KERNEL_URL" -o "${BUILD_DIR}/linux-${KERNEL_VERSION}.tar.xz"
    echo "    Extracting..."
    tar xf "${BUILD_DIR}/linux-${KERNEL_VERSION}.tar.xz" -C "$BUILD_DIR"
    rm -f "${BUILD_DIR}/linux-${KERNEL_VERSION}.tar.xz"
fi

if [ "$(uname -m)" = "aarch64" ] && [ "$(uname -s)" = "Linux" ]; then
    echo "    Native aarch64 Linux detected, building without Docker"

    cd "${BUILD_DIR}/linux-${KERNEL_VERSION}"

    cp "$DEFCONFIG" arch/arm64/configs/shuru_defconfig
    make ARCH=arm64 shuru_defconfig

    echo "    Compiling kernel (this takes a few minutes)..."
    make ARCH=arm64 -j"$(nproc)" Image 2>&1 | tail -5

    cp arch/arm64/boot/Image "${DATA_DIR}/Image"
    echo "    Kernel built: $(du -h "${DATA_DIR}/Image" | cut -f1)"
else
    echo "    Building in Docker (arm64 container)"

    docker run --rm \
        --platform linux/arm64/v8 \
        -v "${DEFCONFIG}:/tmp/shuru_defconfig:ro" \
        -v "${BUILD_DIR}/linux-${KERNEL_VERSION}:/src:rw" \
        -v "${DATA_DIR}:/output" \
        debian:trixie-slim /bin/sh -c '
            set -e

            apt-get update -qq > /dev/null 2>&1
            apt-get install -y -qq build-essential bc flex bison libelf-dev \
                libssl-dev > /dev/null 2>&1

            cd /src

            cp /tmp/shuru_defconfig arch/arm64/configs/shuru_defconfig
            make ARCH=arm64 shuru_defconfig > /dev/null 2>&1

            echo "    Compiling kernel (this takes a few minutes)..."
            make ARCH=arm64 -j$(nproc) Image 2>&1 | tail -5

            cp arch/arm64/boot/Image /output/Image
            echo "    Kernel built: $(du -h /output/Image | cut -f1)"
        '
fi

echo "==> Kernel ready at ${DATA_DIR}/Image"
