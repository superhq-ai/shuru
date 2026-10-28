guest_target := "aarch64-unknown-linux-musl"
binary := "target/debug/shuru"

# List available recipes
default:
    @just --list

# Build the guest init binary (cross-compiled to aarch64 musl)
build-guest:
    cargo build -p shuru-guest --target {{ guest_target }} --release

# Build the CLI binary (debug)
build-cli:
    cargo build -p shuru-cli

# Codesign the CLI binary with virtualization entitlement
codesign:
    codesign --entitlements shuru.entitlements --force -s - {{ binary }}

# Build everything: guest + CLI + codesign
build: build-guest build-cli codesign

# Prepare the rootfs, kernel, and initramfs (requires Docker)
prepare-rootfs:
    ./scripts/prepare-rootfs.sh

# Run a command inside the VM
run *args:
    {{ binary }} run -- {{ args }}

# Open an interactive shell in the VM
shell:
    {{ binary }} run -- sh

# Full setup from scratch: rootfs + build
setup: prepare-rootfs build

# Build shuru-node, shuru-vmm and shuru-netd as static PIE, on aarch64 Linux
build-node:
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER={{ justfile_directory() }}/scripts/static-pie-cc \
    RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=pie" \
    cargo build --release -p shuru-node --target aarch64-unknown-linux-gnu

# Check all crates compile (host targets only)
check:
    cargo check --workspace

# Run clippy on all crates
clippy:
    cargo clippy --workspace

# Install the binary to ~/.local/bin with codesign
install: build-guest
    cargo build -p shuru-cli --release
    codesign --entitlements shuru.entitlements --force -s - target/release/shuru
    mkdir -p ~/.local/bin
    cp target/release/shuru ~/.local/bin/shuru
    mkdir -p ~/.local/share/shuru
    cargo pkgid -p shuru-cli | sed 's/.*#//' > ~/.local/share/shuru/VERSION

# Tag and push a release (triggers GitHub Actions)
release version:
    git tag -a "v{{ version }}" -m "Release v{{ version }}"
    git push origin "v{{ version }}"
