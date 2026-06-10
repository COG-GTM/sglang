# Minimal single-node inference image for H100/H200 (sm_90) and B200 (sm_100).
#
# Two stages:
#   builder  - CUDA devel base. Installs sglang + prebuilt kernel wheels and the
#              FlashInfer JIT cache so no nvcc is needed at runtime.
#   runtime  - Wolfi (Chainguard) base. No CUDA toolkit, no nvcc, no dev tools.
#              CUDA libraries come bundled inside the pip wheels (torch cu13);
#              the GPU driver is injected by the NVIDIA container toolkit.
#
# Scope (on purpose):
#   - GPU archs: sm_90 (H100/H200) + sm_100 (B200) by default; override
#     TORCH_CUDA_ARCH_LIST (e.g. "10.0") for a single-arch build.
#     No GB200/GB300 (sm_103) or arm64 branches.
#   - One CUDA version: 13.0 (matches pyproject's cu13 pins).
#   - Single-node serving: no RDMA/InfiniBand, GDRCopy, DeepEP, Mooncake, nixl.
#   - No DeepGEMM JIT at runtime (no nvcc). FP8 MoE models that require
#     DeepGEMM kernel compilation need either a prebaked ~/.cache/deep_gemm
#     (mount or bake after a warmup run) or the full lmsysorg/sglang image.
#
# Hardened-deployment notes (FedRAMP/FedStart-style clusters):
#   - Runs as a non-root numeric user (10001). Caches live under /home/sglang.
#   - Native TLS: pass --ssl-certfile/--ssl-keyfile (and optionally
#     --ssl-ca-certs, --enable-ssl-refresh) to launch_server; mount cluster
#     cert material (ConfigMaps/Secrets) anywhere readable, e.g. /etc/certs.
#     For outbound trust, point SSL_CERT_FILE / REQUESTS_CA_BUNDLE at the
#     mounted CA bundle.
#   - Air-gapped weights: mount models to a volume and set HF_HUB_OFFLINE=1
#     with --model-path pointing at the mount.
#
# Build:
#   docker build -f docker/min-inference.Dockerfile -t sglang:min-inference .
# Run:
#   docker run --gpus all --shm-size 32g -p 30000:30000 sglang:min-inference \
#     python -m sglang.launch_server --model <model> --host 0.0.0.0

ARG CUDA_VERSION=13.0.1
ARG FLASHINFER_VERSION=0.6.11.post1
ARG TORCH_CUDA_ARCH_LIST="9.0;10.0"

########################################################
# Stage 1: builder
########################################################
FROM nvidia/cuda:${CUDA_VERSION}-cudnn-devel-ubuntu24.04 AS builder

ARG FLASHINFER_VERSION
ARG TORCH_CUDA_ARCH_LIST
ENV DEBIAN_FRONTEND=noninteractive
ENV TORCH_CUDA_ARCH_LIST=${TORCH_CUDA_ARCH_LIST}

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3.12-full python3.12-dev curl ca-certificates git protobuf-compiler \
    && update-alternatives --install /usr/bin/python3 python3 /usr/bin/python3.12 2 \
    && curl -sS https://bootstrap.pypa.io/get-pip.py | python3.12 - --break-system-packages \
    && python3 -m pip config set global.break-system-packages true \
    && rm -rf /var/lib/apt/lists/*

# Rust is required by setuptools-rust for the sglang-grpc extension.
ENV PATH="/root/.cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal

WORKDIR /build

COPY python ./python
COPY proto ./proto
COPY rust/sglang-grpc ./rust/sglang-grpc

# srt-only install: prebuilt sglang-kernel / flash-attn / flashinfer wheels,
# torch cu13 from PyPI. Nothing is compiled against the local CUDA toolkit.
RUN --mount=type=cache,target=/root/.cache/pip \
    python3 -m pip install ./python \
    # kernels>=0.15 requires LayerRepository(revision=...), which
    # transformers 5.8.1 does not pass yet; pin to the last compatible release.
    && python3 -m pip install "kernels==0.14.1" \
    && python3 -m pip install flashinfer-jit-cache==${FLASHINFER_VERSION} \
        --index-url https://flashinfer.ai/whl/cu130

# Pre-download sgl-kernel cubins (kernels-community) so the runtime image
# never hits the network for them.
RUN --mount=type=cache,target=/root/.cache/pip \
    cd /build/python && kernels lock . && kernels download . \
    && mkdir -p /root/.cache/sglang \
    && if [ -f kernels.lock ]; then cp kernels.lock /root/.cache/sglang/; fi

# Strip caches/tests to shrink the copy into the runtime stage.
RUN find /usr/local/lib/python3.12/dist-packages -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true

########################################################
# Stage 2: runtime (Wolfi / Chainguard base)
########################################################
FROM cgr.dev/chainguard/wolfi-base:latest AS runtime

# gcc + python headers are needed by Triton's runtime launcher compilation
# (host C stubs only - no CUDA toolkit / nvcc in this image).
RUN apk add --no-cache \
        python-3.12 py3.12-pip python-3.12-dev \
        gcc glibc-dev binutils \
        numactl libgomp libstdc++ zlib openssl curl \
    && addgroup -g 10001 sglang \
    && adduser -D -u 10001 -G sglang -h /home/sglang sglang

# Wheels and their bundled CUDA libs, copied from the Ubuntu builder.
# manylinux wheels are glibc-based and run unchanged on Wolfi.
COPY --from=builder /usr/local/lib/python3.12/dist-packages /usr/lib/python3.12/site-packages
COPY --from=builder /usr/local/bin /usr/local/bin
COPY --from=builder --chown=10001:10001 /root/.cache/huggingface /home/sglang/.cache/huggingface
COPY --from=builder --chown=10001:10001 /root/.cache/sglang /home/sglang/.cache/sglang

RUN mkdir -p /workspace && chown 10001:10001 /workspace

# GPU driver libraries are injected here by the NVIDIA container toolkit.
ENV PATH="${PATH}:/usr/local/nvidia/bin" \
    LD_LIBRARY_PATH="/usr/local/nvidia/lib:/usr/local/nvidia/lib64" \
    PYTHONUNBUFFERED=1 \
    HOME=/home/sglang \
    TRITON_CACHE_DIR=/home/sglang/.cache/triton

USER 10001:10001
WORKDIR /workspace

CMD ["python3", "-m", "sglang.launch_server", "--help"]
