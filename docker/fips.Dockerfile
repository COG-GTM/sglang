# PROTOTYPE: FIPS-capable / IL5-oriented SGLang runtime image.
#
# This is a UBI 9 (RHEL) based variant of the `runtime` stage of
# docker/Dockerfile, intended as the starting point for an IL5 deployment
# (Iron Bank / Platform One baselines are UBI-based).
#
# Why UBI 9:
# - RHEL's OpenSSL and crypto modules are CMVP-validated and automatically
#   operate in FIPS mode when the host kernel boots with fips=1 (no image
#   level switch needed; verify with /proc/sys/crypto/fips_enabled).
# - CPython's ssl/hashlib link against the system OpenSSL, so the SRT
#   server's --ssl-keyfile/--ssl-certfile TLS path uses validated crypto.
#
# Known FIPS caveats NOT solved by the base image alone (document in SSP or
# terminate TLS at a FIPS-validated proxy instead):
# - grpcio wheels bundle their own BoringSSL (not the validated BoringCrypto
#   build). Keep gRPC endpoints plaintext inside the accreditation boundary
#   or rebuild grpcio against the system OpenSSL.
# - NCCL/RDMA inter-node traffic is unencrypted; multi-node deployments need
#   network-layer encryption (IPsec/MACsec or an encrypting CNI).
# - The sgl-model-gateway must be built from docker/gateway-fips.Dockerfile
#   (aws-lc-rs FIPS rustls backend) if it terminates TLS.
#
# Build (from repo root):
#   docker build -f docker/fips.Dockerfile \
#     --build-arg CUDA_VERSION=12.8.1 --build-arg SGL_VERSION=<version> \
#     -t sglang:fips .

ARG CUDA_VERSION=12.8.1
FROM nvidia/cuda:${CUDA_VERSION}-cudnn-devel-ubi9 AS runtime

ARG CUDA_VERSION
ARG SGL_VERSION
ARG PIP_DEFAULT_INDEX

ENV DEBIAN_FRONTEND=noninteractive \
    CUDA_HOME=/usr/local/cuda

ENV PATH="${PATH}:/usr/local/nvidia/bin:/usr/local/cuda/bin:/usr/local/cuda/nvvm/bin" \
    LD_LIBRARY_PATH="${LD_LIBRARY_PATH}:/usr/local/nvidia/lib:/usr/local/nvidia/lib64"

# System dependencies. Note: no openssh-server, no dev tooling (STIG hygiene).
# InfiniBand/RDMA userland comes from RHEL's rdma-core packaging.
RUN dnf install -y \
    python3.12 python3.12-devel python3.12-pip \
    ca-certificates \
    curl-minimal \
    git \
    openmpi \
    numactl-libs \
    rdma-core \
    libibverbs \
    librdmacm \
    infiniband-diags \
    glibc-langpack-en \
    ninja-build \
    && dnf clean all \
    && alternatives --install /usr/bin/python3 python3 /usr/bin/python3.12 2 \
    && alternatives --set python3 /usr/bin/python3.12 \
    && ln -sf /usr/bin/python3.12 /usr/bin/python \
    && python3 -m pip install --upgrade pip setuptools wheel

ENV LANG=en_US.UTF-8 \
    LANGUAGE=en_US:en \
    LC_ALL=en_US.UTF-8

# Mirror override for air-gapped / IL5 registries
RUN if [ -n "${PIP_DEFAULT_INDEX}" ]; then \
    python3 -m pip config set global.index-url ${PIP_DEFAULT_INDEX}; \
fi

# Install SGLang. For an air-gapped build, replace with a vendored wheelhouse.
RUN case "$CUDA_VERSION" in \
        12.6.1) CUINDEX=126 ;; \
        12.8.1) CUINDEX=128 ;; \
        12.9.1) CUINDEX=129 ;; \
        13.0.1) CUINDEX=130 ;; \
        *) echo "Unsupported CUDA version: $CUDA_VERSION" && exit 1 ;; \
    esac \
    && if [ -z "$SGL_VERSION" ]; then echo "ERROR: SGL_VERSION must be set" && exit 1; fi \
    && python3 -m pip install --extra-index-url https://download.pytorch.org/whl/cu${CUINDEX} \
        "sglang[all]==${SGL_VERSION}" \
    && python3 -m pip cache purge

# Non-root runtime user (STIG requirement)
RUN useradd --system --uid 1001 --create-home sglang \
    && mkdir -p /home/sglang/.cache && chown -R 1001:1001 /home/sglang
USER 1001
WORKDIR /home/sglang

# Verify at container start that the host is in FIPS mode, e.g.:
#   test "$(cat /proc/sys/crypto/fips_enabled)" = "1" || exit 1
CMD ["python3", "-m", "sglang.launch_server", "--help"]
