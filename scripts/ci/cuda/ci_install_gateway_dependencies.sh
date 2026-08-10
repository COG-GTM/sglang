#!/bin/bash
# Install dependencies for the sgl-model-gateway CI jobs.
#
# Gateway-specific apt deps are installed here; protoc and the Rust toolchain
# are delegated to the shared installer. The shared installer exports
# RUSTUP_TOOLCHAIN pinned by rust/rust-toolchain.toml, which outranks the
# gateway's own sgl-model-gateway/rust-toolchain.toml, so this script
# re-exports RUSTUP_TOOLCHAIN with the gateway's pinned channel.
set -euxo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

GATEWAY_APT_PACKAGES=(libssl-dev pkg-config redis-server)
APT_OPTS=(
    -y
    -o "Acquire::Retries=5"
    -o "Acquire::http::Timeout=30"
    -o "Acquire::https::Timeout=30"
)
SUDO=""
command -v sudo >/dev/null 2>&1 && SUDO="sudo"

# GH-hosted runners' Azure Ubuntu mirrors flake periodically. Retry the
# whole install with backoff so we don't fail the whole CI on a 1-min
# DNS hiccup at apt-mirrors.txt → azure.archive.ubuntu.com.
for attempt in 1 2 3 4 5; do
    if $SUDO apt-get update "${APT_OPTS[@]}" \
       && $SUDO apt-get install "${APT_OPTS[@]}" "${GATEWAY_APT_PACKAGES[@]}"; then
        break
    fi
    if [ "$attempt" = 5 ]; then
        echo "apt-get install failed after 5 attempts; giving up." >&2
        exit 1
    fi
    sleep $((attempt * 15))
done

bash "${SCRIPT_DIR}/../utils/install_rust_protoc.sh"

# Make cargo/rustc/protoc visible in this shell.
. "$HOME/.cargo/env"

# install_rustup.sh writes RUSTUP_TOOLCHAIN (from rust/rust-toolchain.toml) to
# GITHUB_ENV, which overrides sgl-model-gateway/rust-toolchain.toml in later
# steps. Point RUSTUP_TOOLCHAIN at the gateway's own pinned channel instead.
GATEWAY_TOOLCHAIN_FILE="${SCRIPT_DIR}/../../../sgl-model-gateway/rust-toolchain.toml"
GATEWAY_CHANNEL="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' "${GATEWAY_TOOLCHAIN_FILE}" 2>/dev/null || true)"
if [ -n "${GATEWAY_CHANNEL}" ]; then
    rustup toolchain install --profile minimal --component clippy "${GATEWAY_CHANNEL}"
    export RUSTUP_TOOLCHAIN="${GATEWAY_CHANNEL}"
    if [ -n "${GITHUB_ENV:-}" ]; then
        mkdir -p "$(dirname "${GITHUB_ENV}")" 2>/dev/null || true
        echo "RUSTUP_TOOLCHAIN=${GATEWAY_CHANNEL}" >> "${GITHUB_ENV}" || true
    fi
fi

rustc --version
cargo --version
protoc --version
