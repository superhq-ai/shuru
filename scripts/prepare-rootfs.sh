#!/bin/bash
set -euo pipefail

DEBIAN_RELEASE="trixie"
DATA_DIR="${SHURU_DATA_DIR:-${HOME}/.local/share/shuru}"
ROOTFS_IMG="${DATA_DIR}/rootfs.ext4"
KERNEL_PATH="${DATA_DIR}/Image"
INITRAMFS_PATH="${DATA_DIR}/initramfs.cpio.gz"
GUEST_BINARY="target/aarch64-unknown-linux-musl/release/shuru-guest"
ROOTFS_SIZE_MB=1024

echo "==> Shuru rootfs preparation script"
echo "    Debian ${DEBIAN_RELEASE} (kernel + rootfs)"
echo ""

if [[ "$(uname)" == "Darwin" ]]; then
    if ! command -v docker &>/dev/null; then
        echo "ERROR: Docker is required on macOS to create ext4 images."
        echo "       Install Docker Desktop or use: brew install --cask docker"
        exit 1
    fi
fi

if [ ! -f "$GUEST_BINARY" ]; then
    echo "ERROR: Guest binary not found at ${GUEST_BINARY}"
    echo "       Run: cargo build -p shuru-guest --target aarch64-unknown-linux-musl --release"
    exit 1
fi

GUEST_BINARY="$(cd "$(dirname "$GUEST_BINARY")" && pwd)/$(basename "$GUEST_BINARY")"

mkdir -p "$DATA_DIR"

if [ ! -f "$KERNEL_PATH" ]; then
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    "${SCRIPT_DIR}/build-kernel.sh"
else
    echo "==> Kernel already present."
fi

if [ ! -f "$INITRAMFS_PATH" ]; then
    echo "==> Building minimal initramfs..."

    docker run --rm \
        --platform linux/arm64/v8 \
        -v "${DATA_DIR}:/output" \
        -v "${GUEST_BINARY}:/tmp/shuru-init:ro" \
        debian:${DEBIAN_RELEASE}-slim /bin/sh -c '
            set -e
            apt-get update -qq > /dev/null 2>&1
            apt-get install -y -qq busybox-static e2fsprogs pax-utils cpio > /dev/null 2>&1

            mkdir -p /initramfs/bin /initramfs/sbin /initramfs/usr/sbin
            mkdir -p /initramfs/proc /initramfs/dev /initramfs/newroot

            cp /bin/busybox /initramfs/bin/busybox
            mkdir -p /initramfs/etc
            for cmd in sh mount umount switch_root cp chmod echo ifconfig route cat; do
                ln -sf busybox "/initramfs/bin/${cmd}"
            done

            lddtree -l /sbin/e2fsck /usr/sbin/resize2fs | sort -u \
                | cpio --quiet -pmdL /initramfs

            cp /tmp/shuru-init /initramfs/bin/shuru-init
            chmod 755 /initramfs/bin/shuru-init

            cat > /initramfs/init << '\''INITEOF'\''
#!/bin/sh
mount -t proc none /proc
mount -t devtmpfs none /dev
/sbin/e2fsck -p /dev/vda > /dev/null 2>&1 || true
/usr/sbin/resize2fs /dev/vda > /dev/null 2>&1 || true
mount -t ext4 /dev/vda /newroot
cp /bin/shuru-init /newroot/usr/bin/shuru-init
chmod 755 /newroot/usr/bin/shuru-init
if ifconfig eth0 up 2>/dev/null; then
    ifconfig eth0 10.0.0.2 netmask 255.255.255.0 up
    route add default gw 10.0.0.1
    echo "nameserver 10.0.0.1" > /newroot/etc/resolv.conf
fi
umount /proc
exec switch_root /newroot /usr/bin/shuru-init
INITEOF
            chmod 755 /initramfs/init

            cd /initramfs
            find . | cpio -o -H newc 2>/dev/null | gzip > /output/initramfs.cpio.gz
            echo "==> Initramfs created: $(du -h /output/initramfs.cpio.gz | cut -f1)"
        '
    echo "    Initramfs saved to ${INITRAMFS_PATH}"
else
    echo "==> Initramfs already present."
fi

if [ -f "$ROOTFS_IMG" ]; then
    echo "==> Rootfs already present."
else
echo "==> Creating ext4 rootfs image (${ROOTFS_SIZE_MB}MB) with Debian ${DEBIAN_RELEASE}..."

truncate -s ${ROOTFS_SIZE_MB}M "$ROOTFS_IMG"

if [[ "$(uname)" == "Darwin" ]]; then
    echo ""
    echo "==> macOS detected. Using Docker for ext4 formatting and Debian bootstrap."
    echo ""

    docker run --rm --privileged \
        --platform linux/arm64/v8 \
        -e DEBIAN_RELEASE="${DEBIAN_RELEASE}" \
        -v "${ROOTFS_IMG}:/rootfs.ext4" \
        -v "${GUEST_BINARY}:/tmp/shuru-guest:ro" \
        debian:${DEBIAN_RELEASE}-slim /bin/sh -c '
            set -e
            apt-get update -qq
            apt-get install -y -qq debootstrap e2fsprogs > /dev/null 2>&1

            mkfs.ext4 -F -E lazy_itable_init=0 /rootfs.ext4
            mkdir -p /mnt/rootfs
            mount -o loop /rootfs.ext4 /mnt/rootfs

            echo "==> Running debootstrap (this may take a few minutes)..."
            debootstrap --arch=arm64 --variant=minbase ${DEBIAN_RELEASE} /mnt/rootfs http://deb.debian.org/debian

            mkdir -p /mnt/rootfs/etc/dpkg/dpkg.cfg.d
            cat > /mnt/rootfs/etc/dpkg/dpkg.cfg.d/01-nodoc << DPKGEOF
path-exclude /usr/share/doc/*
path-exclude /usr/share/man/*
path-exclude /usr/share/info/*
path-exclude /usr/share/locale/*
path-include /usr/share/locale/en*
DPKGEOF

            chroot /mnt/rootfs apt-get update -qq
            chroot /mnt/rootfs apt-get install -y -qq --no-install-recommends \
                ca-certificates curl git iproute2 \
                openssh-client jq less procps xz-utils libgomp1 libatomic1 > /dev/null 2>&1

            rm -rf /mnt/rootfs/usr/share/doc/* /mnt/rootfs/usr/share/man/* /mnt/rootfs/usr/share/info/*
            find /mnt/rootfs/usr/share/locale -mindepth 1 -maxdepth 1 ! -name "en*" -exec rm -rf {} + 2>/dev/null || true

            chroot /mnt/rootfs apt-get clean
            rm -rf /mnt/rootfs/var/lib/apt/lists/*

            cp /tmp/shuru-guest /mnt/rootfs/usr/bin/shuru-init
            chmod 755 /mnt/rootfs/usr/bin/shuru-init

            mkdir -p /mnt/rootfs/proc /mnt/rootfs/sys /mnt/rootfs/dev /mnt/rootfs/tmp /mnt/rootfs/run
            echo "shuru" > /mnt/rootfs/etc/hostname
            echo "nameserver 8.8.8.8" > /mnt/rootfs/etc/resolv.conf

            umount /mnt/rootfs
            echo "==> Debian rootfs populated successfully"
        '
else
    MISSING_PKGS=""
    command -v mkfs.ext4 &>/dev/null || MISSING_PKGS="e2fsprogs"
    command -v debootstrap &>/dev/null || MISSING_PKGS="${MISSING_PKGS} debootstrap"
    if [ -n "$MISSING_PKGS" ]; then
        sudo apt-get update && sudo apt-get install -y $MISSING_PKGS
    fi

    mkfs.ext4 -F -E lazy_itable_init=0 "$ROOTFS_IMG"
    MOUNT_DIR=$(mktemp -d)
    sudo mount -o loop "$ROOTFS_IMG" "$MOUNT_DIR"

    echo "==> Running debootstrap (this may take a few minutes)..."
    sudo debootstrap --arch=arm64 --variant=minbase "${DEBIAN_RELEASE}" "$MOUNT_DIR" http://deb.debian.org/debian

    sudo mkdir -p "${MOUNT_DIR}/etc/dpkg/dpkg.cfg.d"
    cat <<'DPKGEOF' | sudo tee "${MOUNT_DIR}/etc/dpkg/dpkg.cfg.d/01-nodoc" > /dev/null
path-exclude /usr/share/doc/*
path-exclude /usr/share/man/*
path-exclude /usr/share/info/*
path-exclude /usr/share/locale/*
path-include /usr/share/locale/en*
DPKGEOF

    sudo chroot "$MOUNT_DIR" apt-get update -qq
    sudo chroot "$MOUNT_DIR" apt-get install -y -qq --no-install-recommends \
        ca-certificates curl git iproute2 \
        openssh-client jq less procps xz-utils libgomp1 libatomic1 > /dev/null 2>&1

    sudo rm -rf "${MOUNT_DIR}/usr/share/doc/"* "${MOUNT_DIR}/usr/share/man/"* "${MOUNT_DIR}/usr/share/info/"*
    sudo find "${MOUNT_DIR}/usr/share/locale" -mindepth 1 -maxdepth 1 ! -name "en*" -exec rm -rf {} + 2>/dev/null || true

    sudo chroot "$MOUNT_DIR" apt-get clean
    sudo rm -rf "${MOUNT_DIR}/var/lib/apt/lists/"*

    sudo cp "$GUEST_BINARY" "${MOUNT_DIR}/usr/bin/shuru-init"
    sudo chmod 755 "${MOUNT_DIR}/usr/bin/shuru-init"

    sudo mkdir -p "${MOUNT_DIR}/proc" "${MOUNT_DIR}/sys" "${MOUNT_DIR}/dev" "${MOUNT_DIR}/tmp" "${MOUNT_DIR}/run"
    echo "shuru" | sudo tee "${MOUNT_DIR}/etc/hostname" > /dev/null
    echo "nameserver 8.8.8.8" | sudo tee "${MOUNT_DIR}/etc/resolv.conf" > /dev/null

    sudo umount "$MOUNT_DIR"
    rmdir "$MOUNT_DIR" 2>/dev/null || true
fi
fi # rootfs existence check

echo ""
echo "==> Done!"
echo "    Kernel:     ${KERNEL_PATH}"
echo "    Initramfs:  ${INITRAMFS_PATH}"
echo "    Rootfs:     ${ROOTFS_IMG}"
echo ""
echo "    To run:  cargo build -p shuru-cli && codesign --entitlements shuru.entitlements --force -s - target/debug/shuru"
echo "             ./target/debug/shuru run -- echo hello"
