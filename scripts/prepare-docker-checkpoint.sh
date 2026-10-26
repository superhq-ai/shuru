#!/bin/bash
# Create a Shuru checkpoint with Docker Engine installed as a guest service
# (RFC 0004) and one pinned image pulled, so
# `shuru run --from docker -- docker run ...` works with networking disabled.
#
# Everything is pinned: Docker apt packages by exact version, the apt signing
# key by SHA-256, the image by manifest digest. The VM that performs the
# install only reaches the hosts listed below.
#
# The build has two steps. The first installs Docker and declares the
# dockerd service, saved as an intermediate checkpoint. The second boots from
# it, which starts dockerd through the service model, pulls the image, and
# saves the final checkpoint. The intermediate checkpoint is then deleted.
#
# Needs a kernel and guest init with container support (RFC 0003). Until a
# release ships them, point SHURU_KERNEL and SHURU_INITRD at a local build.
set -euo pipefail

SHURU_BIN="${SHURU_BIN:-shuru}"
CHECKPOINT_NAME="${CHECKPOINT_NAME:-docker}"
FROM_CHECKPOINT="${FROM_CHECKPOINT:-}"
DISK_SIZE_MB="${DISK_SIZE_MB:-4096}"
MEMORY_MB="${MEMORY_MB:-2048}"

DOCKER_VERSION="${DOCKER_VERSION:-5:29.9.2-1~debian.13~trixie}"
CONTAINERD_VERSION="${CONTAINERD_VERSION:-2.4.2-3~debian.13~trixie}"
# Docker Release (CE deb), fingerprint 9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88
DOCKER_GPG_SHA256="${DOCKER_GPG_SHA256:-1500c1f56fa9e26b9b8f42452a553675796ade0807cdce11975eb98170b3a570}"
# nginxinc/nginx-unprivileged 1.31.7-alpine-slim, multi-arch index digest,
# runs as uid 101 and listens on 8080.
DOCKER_IMAGE="${DOCKER_IMAGE:-nginxinc/nginx-unprivileged:1.31.7-alpine-slim@sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7}"

BASE_NAME="${CHECKPOINT_NAME}-base"

INSTALL_SCRIPT="$(cat << EOF
set -eu
export DEBIAN_FRONTEND=noninteractive
echo "==> Upgrading Debian base (main, security and updates archives)"
cat > /etc/apt/sources.list.d/debian.sources << SRCEOF
Types: deb
URIs: http://deb.debian.org/debian
Suites: trixie trixie-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb
URIs: http://security.debian.org/debian-security
Suites: trixie-security
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
SRCEOF
rm -f /etc/apt/sources.list
apt-get update -qq
apt-get upgrade -y -qq --no-install-recommends
echo "==> Adding Docker apt repository (key pinned by SHA-256)"
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
echo "${DOCKER_GPG_SHA256}  /etc/apt/keyrings/docker.asc" | sha256sum -c -
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=arm64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian trixie stable" > /etc/apt/sources.list.d/docker.list
apt-get update -qq
echo "==> Installing docker-ce ${DOCKER_VERSION}, containerd.io ${CONTAINERD_VERSION}"
apt-get install -y -qq --no-install-recommends \
    "docker-ce=${DOCKER_VERSION}" "docker-ce-cli=${DOCKER_VERSION}" "containerd.io=${CONTAINERD_VERSION}"
apt-mark hold docker-ce docker-ce-cli containerd.io > /dev/null
apt-get clean
rm -rf /var/lib/apt/lists/*
mkdir -p /etc/docker
printf '{\n  "no-new-privileges": true\n}\n' > /etc/docker/daemon.json
echo "==> Declaring the dockerd service"
mkdir -p /etc/shuru/services
cat > /etc/shuru/services/docker.json << SVCEOF
{
  "exec": ["dockerd"],
  "ready": ["docker", "info"],
  "ready_timeout_secs": 60,
  "stop_timeout_secs": 30
}
SVCEOF
cat /etc/shuru/services/docker.json
echo "==> Installed versions"
docker --version
containerd --version
runc --version | head -n 1
EOF
)"

PULL_SCRIPT="$(cat << EOF
set -eu
echo "==> Pulling ${DOCKER_IMAGE}"
docker pull "${DOCKER_IMAGE}"
docker image ls --digests
EOF
)"

ALLOW_ARGS=(
    --allow-net
    --disk-size "$DISK_SIZE_MB"
    --memory "$MEMORY_MB"
    --allow-host deb.debian.org
    --allow-host security.debian.org
    --allow-host download.docker.com
    --allow-host "*.docker.io"
    --allow-host "*.docker.com"
    --allow-host "*.cloudflarestorage.com")
if [ -n "${VERBOSE:-}" ]; then
    ALLOW_ARGS+=(--verbose)
fi

run_checkpoint() {
    local name="$1" from="$2" script="$3"
    local args=(checkpoint create "$name" "${ALLOW_ARGS[@]}")
    if [ -n "$from" ]; then
        args+=(--from "$from")
    fi
    local b64
    b64="$(printf '%s\n' "$script" | base64 | tr -d '\n')"
    "$SHURU_BIN" "${args[@]}" -- sh -c "echo ${b64} | base64 -d | sh"
}

echo "==> Step 1/2: installing Docker ${DOCKER_VERSION} into '${BASE_NAME}'"
run_checkpoint "$BASE_NAME" "$FROM_CHECKPOINT" "$INSTALL_SCRIPT"

echo "==> Step 2/2: pulling the image into '${CHECKPOINT_NAME}' with dockerd running as a service"
if run_checkpoint "$CHECKPOINT_NAME" "$BASE_NAME" "$PULL_SCRIPT"; then
    "$SHURU_BIN" checkpoint delete "$BASE_NAME"
    echo "==> Checkpoint '${CHECKPOINT_NAME}' ready"
else
    echo "==> Pull failed; intermediate checkpoint '${BASE_NAME}' left in place for debugging" >&2
    exit 1
fi
