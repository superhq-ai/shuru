#!/bin/bash
# Build the Shuru guest kernel on a Hugging Face Job instead of local Docker.
#
# Same defconfig and kernel version as build-kernel.sh, cross-compiled on an
# x86_64 CPU job. The job uploads the Image and the resolved .config to a
# private dataset repo, and this script downloads them into the data dir.
# Nothing is written to the data dir until the download at the end, so the
# installed kernel is untouched until then. Point SHURU_DATA_DIR somewhere
# else to keep the result fully isolated.
#
# Requires: hf CLI logged in (hf auth whoami), a billable HF account.
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.12.114}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
DATA_DIR="${SHURU_DATA_DIR:-${HOME}/.local/share/shuru}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
DEFCONFIG="${REPO_DIR}/kernel/shuru_defconfig"
HF_FLAVOR="${HF_FLAVOR:-cpu-upgrade}"
HF_USER="$(hf auth whoami --format json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["user"])')"
HF_REPO="${HF_REPO:-${HF_USER}/shuru-kernel-build}"
JOB_NAME="shuru-kernel-${KERNEL_VERSION//./-}"

if [ ! -f "$DEFCONFIG" ]; then
    echo "ERROR: Defconfig not found at ${DEFCONFIG}"
    exit 1
fi

export HF_TOKEN="${HF_TOKEN:-$(hf auth token)}"
DEFCONFIG_B64="$(base64 < "$DEFCONFIG" | tr -d '\n')"

echo "==> Building kernel ${KERNEL_VERSION} on Hugging Face Jobs (${HF_FLAVOR})"
echo "    artifacts: https://huggingface.co/datasets/${HF_REPO} (private)"

hf repos create "$HF_REPO" --type dataset --private --exist-ok > /dev/null

read -r -d '' JOB_SCRIPT << 'JOBEOF' || true
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null
apt-get install -y -qq build-essential bc flex bison libelf-dev libssl-dev \
    gcc-aarch64-linux-gnu curl xz-utils ca-certificates python3 python3-venv > /dev/null
curl -LsSf https://hf.co/cli/install.sh | bash > /dev/null
export PATH="$HOME/.local/bin:$PATH"

cd /tmp
curl -sSLO "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz"
curl -sSL "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/sha256sums.asc" \
    | grep " linux-${KERNEL_VERSION}.tar.xz$" | sha256sum -c -
tar xf "linux-${KERNEL_VERSION}.tar.xz"
cd "linux-${KERNEL_VERSION}"

echo "$DEFCONFIG_B64" | base64 -d > arch/arm64/configs/shuru_defconfig
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- shuru_defconfig
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j"$(nproc)" Image 2>&1 | tail -3

cp arch/arm64/boot/Image "/tmp/Image-${KERNEL_VERSION}"
cp .config "/tmp/config-${KERNEL_VERSION}"
(cd /tmp && sha256sum "Image-${KERNEL_VERSION}" | tee "Image-${KERNEL_VERSION}.sha256")
hf upload "$HF_REPO" "/tmp/Image-${KERNEL_VERSION}" "Image-${KERNEL_VERSION}" --type dataset > /dev/null
hf upload "$HF_REPO" "/tmp/Image-${KERNEL_VERSION}.sha256" "Image-${KERNEL_VERSION}.sha256" --type dataset > /dev/null
hf upload "$HF_REPO" "/tmp/config-${KERNEL_VERSION}" "config-${KERNEL_VERSION}" --type dataset > /dev/null
echo "KERNEL_BUILD_DONE"
JOBEOF

JOB_ID="$(hf jobs run debian:trixie-slim bash -c "$JOB_SCRIPT" \
    --ssh --detach --flavor "$HF_FLAVOR" --timeout 2h \
    --name "$JOB_NAME" \
    --secrets HF_TOKEN \
    --env "KERNEL_VERSION=${KERNEL_VERSION}" \
    --env "KERNEL_MAJOR=${KERNEL_MAJOR}" \
    --env "HF_REPO=${HF_REPO}" \
    --env "DEFCONFIG_B64=${DEFCONFIG_B64}" \
    --format quiet)"
echo "==> Job ${JOB_ID} launched"

echo "==> SSH access check (dry run)"
hf jobs ssh "$JOB_ID" --dry-run || true

echo "==> Waiting for the build (10 to 20 minutes on ${HF_FLAVOR})..."
hf jobs wait "$JOB_ID" --timeout 2h
hf jobs logs "$JOB_ID" | tail -5 | grep -q KERNEL_BUILD_DONE

mkdir -p "$DATA_DIR"
DL_DIR="$(mktemp -d)"
hf download "$HF_REPO" "Image-${KERNEL_VERSION}" "Image-${KERNEL_VERSION}.sha256" "config-${KERNEL_VERSION}" \
    --type dataset --local-dir "$DL_DIR" > /dev/null
(cd "$DL_DIR" && sha256sum -c "Image-${KERNEL_VERSION}.sha256")
cp "${DL_DIR}/Image-${KERNEL_VERSION}" "${DATA_DIR}/Image"
cp "${DL_DIR}/config-${KERNEL_VERSION}" "${DATA_DIR}/Image.config"
rm -rf "$DL_DIR"

echo "==> Kernel ready at ${DATA_DIR}/Image ($(du -h "${DATA_DIR}/Image" | cut -f1))"
echo "    Resolved config saved to ${DATA_DIR}/Image.config"
