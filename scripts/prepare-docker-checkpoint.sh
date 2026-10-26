#!/bin/bash
# Create a Shuru checkpoint with Docker Engine installed and one pinned image
# pulled, so `shuru run --from docker -- with-dockerd docker run ...` works
# with networking disabled.
#
# Everything is pinned: Docker apt packages by exact version, the apt signing
# key by SHA-256, the image by manifest digest. The VM that performs the
# install only reaches the hosts listed below.
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

WITH_DOCKERD_B64="$(base64 << 'HELPER' | tr -d '\n'
#!/bin/sh
# Start dockerd, run a command, then stop dockerd so the disk is consistent.
# usage: with-dockerd CMD [ARGS...]
set -eu
if [ "$#" -eq 0 ]; then
    echo "usage: with-dockerd CMD [ARGS...]" >&2
    exit 64
fi
LOG=/var/log/dockerd.log
if docker info > /dev/null 2>&1; then
    exec "$@"
fi
dockerd > "$LOG" 2>&1 &
DOCKERD_PID=$!
i=0
until docker info > /dev/null 2>&1; do
    i=$((i + 1))
    if ! kill -0 "$DOCKERD_PID" 2> /dev/null; then
        echo "with-dockerd: dockerd exited during startup" >&2
        tail -n 30 "$LOG" >&2
        exit 1
    fi
    if [ "$i" -ge 300 ]; then
        echo "with-dockerd: dockerd not ready after 60s" >&2
        tail -n 30 "$LOG" >&2
        kill "$DOCKERD_PID" 2> /dev/null || true
        exit 1
    fi
    sleep 0.2
done
set +e
"$@"
STATUS=$?
set -e
kill -TERM "$DOCKERD_PID" 2> /dev/null || true
j=0
while kill -0 "$DOCKERD_PID" 2> /dev/null && [ "$j" -lt 150 ]; do
    sleep 0.2
    j=$((j + 1))
done
sync
exit "$STATUS"
HELPER
)"

GUEST_SCRIPT="$(cat << EOF
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
echo "${WITH_DOCKERD_B64}" | base64 -d > /usr/local/bin/with-dockerd
chmod 755 /usr/local/bin/with-dockerd
echo "==> Pulling ${DOCKER_IMAGE}"
with-dockerd sh -c 'docker pull "${DOCKER_IMAGE}" && docker image ls --digests'
echo "==> Installed versions"
docker --version
containerd --version
runc --version | head -n 1
EOF
)"
GUEST_SCRIPT_B64="$(printf '%s\n' "$GUEST_SCRIPT" | base64 | tr -d '\n')"

ARGS=(checkpoint create "$CHECKPOINT_NAME"
    --allow-net
    --disk-size "$DISK_SIZE_MB"
    --memory "$MEMORY_MB"
    --allow-host deb.debian.org
    --allow-host security.debian.org
    --allow-host download.docker.com
    --allow-host "*.docker.io"
    --allow-host "*.docker.com"
    --allow-host "*.cloudflarestorage.com")
if [ -n "$FROM_CHECKPOINT" ]; then
    ARGS+=(--from "$FROM_CHECKPOINT")
fi
if [ -n "${VERBOSE:-}" ]; then
    ARGS+=(--verbose)
fi

echo "==> Creating checkpoint '${CHECKPOINT_NAME}' with Docker ${DOCKER_VERSION}"
exec "$SHURU_BIN" "${ARGS[@]}" -- sh -c "echo ${GUEST_SCRIPT_B64} | base64 -d | sh"
