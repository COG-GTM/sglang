# FIPS-capable build of the sgl-model-gateway.
#
# Differences from gateway.Dockerfile:
# - Base image: Red Hat UBI 9. RHEL/UBI userland uses CMVP-validated crypto
#   modules and automatically operates in FIPS mode when the host kernel
#   boots with fips=1 (required for IL5 hosts).
# - The gateway is built with `--no-default-features --features grpc-client,fips`,
#   which swaps the rustls crypto backend from ring (not FIPS-validated) to
#   aws-lc-rs in FIPS mode (CMVP-validated AWS-LC cryptographic module).
# - `vendored-openssl` is NOT used: any OpenSSL usage dynamically links the
#   host's validated OpenSSL instead of a statically vendored copy.
#
# Build (from repo root):
#   docker build -f docker/gateway-fips.Dockerfile -t sgl-model-gateway:fips .
#
# Runtime validation on a FIPS host:
#   cat /proc/sys/crypto/fips_enabled   # must be 1
#   The gateway only negotiates FIPS-approved TLS cipher suites.

######################## BASE IMAGE ##########################
FROM registry.access.redhat.com/ubi9/ubi:latest AS base

ARG PYTHON_VERSION=3.12

ENV PATH="/root/.local/bin:${PATH}"
ENV UV_HTTP_TIMEOUT=500
ENV VIRTUAL_ENV="/opt/venv"
ENV UV_PYTHON_INSTALL_DIR=/opt/uv/python
ENV UV_LINK_MODE="copy"
ENV PATH="$VIRTUAL_ENV/bin:$PATH"

RUN dnf install -y python${PYTHON_VERSION} python${PYTHON_VERSION}-devel \
    && dnf clean all

# install uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# Use the system (FIPS-capable) CPython rather than a uv-managed standalone
# build, so that Python's ssl/hashlib link against the validated OpenSSL.
RUN uv venv --python /usr/bin/python${PYTHON_VERSION} --seed ${VIRTUAL_ENV}

FROM scratch AS local_src
COPY . /src

######################### BUILD IMAGE #########################
FROM base AS build-image

ENV PATH="/root/.cargo/bin:${PATH}"

# aws-lc-fips-sys (pulled in by rustls's `fips` feature) requires
# cmake, golang, and perl at build time.
RUN dnf install -y git gcc gcc-c++ make cmake perl golang \
    openssl-devel pkgconf-pkg-config unzip \
    && dnf clean all

# protoc is not packaged in the UBI9 repos; install the official release.
ARG PROTOC_VERSION=29.3
RUN curl -fsSL -o /tmp/protoc.zip \
        https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip \
    && unzip -o /tmp/protoc.zip -d /usr/local bin/protoc 'include/*' \
    && rm -f /tmp/protoc.zip

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    && rustc --version && cargo --version && protoc --version

COPY --from=local_src /src /opt/sglang

WORKDIR /opt/sglang/sgl-model-gateway

# Build the standalone gateway binary and the Python wheel with the FIPS
# crypto backend and WITHOUT vendored OpenSSL.
RUN uv pip install maturin patchelf \
    && cargo build --release --bin sgl-model-gateway \
        --no-default-features --features grpc-client,fips \
    && cd bindings/python \
    && rm -rf dist/ \
    && ulimit -n 65536 \
    && maturin build --release --out dist \
        --no-default-features \
        --features "pyo3/extension-module,sgl-model-gateway/grpc-client,sgl-model-gateway/fips" \
    && rm -rf /root/.cache

######################### ROUTER IMAGE #########################
FROM base AS router-image

RUN dnf install -y openssl && dnf clean all

COPY --from=build-image /opt/sglang/sgl-model-gateway/target/release/sgl-model-gateway /usr/local/bin/sgl-model-gateway
COPY --from=build-image /opt/sglang/sgl-model-gateway/bindings/python/dist/*.whl dist/

RUN uv pip install --force-reinstall dist/*.whl && rm -rf /root/.cache dist/

# Run as non-root (STIG requirement)
RUN useradd --system --uid 1001 --create-home gateway
USER 1001

ENTRYPOINT ["python3", "-m", "sglang_router.launch_router"]
