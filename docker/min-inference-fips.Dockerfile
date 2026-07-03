# Minimal single-node inference image on a Chainguard FIPS python base.
#
# This is the FIPS variant of docker/min-inference.Dockerfile. The builder stage
# is identical; only the runtime base changes: instead of wolfi-base it targets
# Chainguard's `python-fips` image, which ships a CMVP-validated redistribution
# of the OpenSSL FIPS provider module and is STIG-hardened and non-root.
#
# Runtime base selection (RUNTIME_BASE / RUNTIME_APK):
#   - Production (licensed):  cgr.dev/<ORG>/python-fips:3.12
#       python 3.12 is already in the base, so RUNTIME_APK is empty. Under the
#       per-image license there is NO open APK repository, so the host-compile
#       toolchain needed for runtime kernel JIT (gcc-14-default, glibc-dev,
#       binutils) must be baked into your licensed image via Chainguard
#       "Custom Assembly" rather than `apk add` here. The non-root 10001 user
#       likewise comes from the base / Custom Assembly.
#   - Public prototype (no license needed): build on the same Chainguard
#       substrate python-fips is assembled from (wolfi + python-3.12), which is
#       filesystem-equivalent to python-fips:3.12 minus the validated OpenSSL
#       provider and STIG hardening. Override both args (see Build below).
#
# Why python-fips and not pytorch-fips: this image brings its own pinned wheel
# stack (torch 2.11 cu13 + sgl-kernel + flashinfer, all cp312). A thin python
# base keeps that stack under our control; pytorch-fips ships its own torch/CUDA
# on Chainguard's cadence and would collide with our pinned ABI.
#
# FIPS runtime notes:
#   - python-fips enforces the validated provider; non-approved algorithms fail.
#     ML/HF code routinely uses md5/sha1 for cache keys -- expect to pass
#     usedforsecurity=False at a few hashlib call sites the first time you run
#     under enforced FIPS.
#   - FIPS 140-3 also requires the host kernel in FIPS mode; that is a
#     cluster/node property, not an image property.
#
# GPU arch / CUDA / forward-compat / non-root / TLS / air-gap behavior is
# identical to docker/min-inference.Dockerfile -- see that file's header.
#
# Build (public prototype, builds today):
#   docker build -f docker/min-inference-fips.Dockerfile \
#     --build-arg RUNTIME_BASE=cgr.dev/chainguard/wolfi-base:latest \
#     --build-arg RUNTIME_APK="python-3.12 py3.12-pip python-3.12-dev gcc-14-default glibc-dev binutils numactl libgomp libstdc++ zlib openssl curl" \
#     -t sglang:min-inference-fips .
# Build (licensed FIPS base):
#   docker build -f docker/min-inference-fips.Dockerfile \
#     --build-arg RUNTIME_BASE=cgr.dev/<ORG>/python-fips:3.12 \
#     -t sglang:min-inference-fips .
# Run: same as min-inference.Dockerfile.

ARG CUDA_VERSION=13.0.1
ARG SGLANG_VERSION=0.5.12.post1
ARG FLASHINFER_VERSION=0.6.11.post1
ARG TORCH_CUDA_ARCH_LIST="9.0;10.0"
# Set to 0 for a compiler-free runtime image (no runtime kernel JIT).
ARG INCLUDE_NVCC=1
# Runtime base: licensed FIPS python by default; override for the public prototype.
ARG RUNTIME_BASE=cgr.dev/chainguard/python-fips:3.12
# APK packages to install in the runtime stage. Empty for a licensed python-fips
# base (python is present; toolchain comes via Custom Assembly). Set to the full
# substrate set for the public wolfi-base prototype (see header).
ARG RUNTIME_APK=""

########################################################
# Stage 1: builder (identical to min-inference.Dockerfile)
########################################################
FROM nvidia/cuda:${CUDA_VERSION}-cudnn-devel-ubuntu24.04 AS builder

ARG SGLANG_VERSION
ARG FLASHINFER_VERSION
ARG TORCH_CUDA_ARCH_LIST
ENV DEBIAN_FRONTEND=noninteractive
ENV TORCH_CUDA_ARCH_LIST=${TORCH_CUDA_ARCH_LIST}

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3.12-full python3.12-dev curl ca-certificates git patch \
    && update-alternatives --install /usr/bin/python3 python3 /usr/bin/python3.12 2 \
    && curl -sS https://bootstrap.pypa.io/get-pip.py | python3.12 - --break-system-packages \
    && python3 -m pip config set global.break-system-packages true \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Only needed for the `kernels lock/download` prefetch below.
COPY python/pyproject.toml ./python/pyproject.toml

# Pinned sglang release wheel (prebuilt, includes the grpc rust extension) plus
# prebuilt sglang-kernel / flash-attn / flashinfer wheels and torch cu13 from
# PyPI. Nothing is compiled against the local CUDA toolkit.
RUN --mount=type=cache,target=/root/.cache/pip \
    python3 -m pip install "sglang==${SGLANG_VERSION}" \
    # kernels>=0.15 requires LayerRepository(revision=...), which
    # transformers 5.8.1 does not pass yet; pin to the last compatible release.
    && python3 -m pip install "kernels==0.14.1" \
    && python3 -m pip install flashinfer-jit-cache==${FLASHINFER_VERSION} \
        --index-url https://flashinfer.ai/whl/cu130

# GLM-4 MoE shared-expert TP patches (from exa/fedstart/inference_server/patches),
# applied against the installed release.
COPY patches ./patches
RUN patch -p2 -d /usr/local/lib/python3.12/dist-packages < patches/glm4_moe_shared_expert_tp.patch \
    && patch -p2 -d /usr/local/lib/python3.12/dist-packages < patches/glm4_moe_shared_output_tp_scale.patch

# Pre-download sgl-kernel cubins (kernels-community) so the runtime image
# never hits the network for them.
RUN --mount=type=cache,target=/root/.cache/pip \
    cd /build/python && kernels lock . && kernels download . \
    && mkdir -p /root/.cache/sglang /root/.cache/huggingface \
    && if [ -f kernels.lock ]; then cp kernels.lock /root/.cache/sglang/; fi

# Strip caches/tests to shrink the copy into the runtime stage.
RUN find /usr/local/lib/python3.12/dist-packages -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true

# Assemble a minimal CUDA compiler subset (nvcc front-end, ptxas, nvvm/cicc,
# headers, device runtime) for runtime kernel JIT. ~400MB vs ~5GB for the full
# toolkit; left empty when INCLUDE_NVCC=0.
ARG INCLUDE_NVCC
RUN mkdir -p /opt/cuda-min/targets/x86_64-linux/lib \
    && if [ "${INCLUDE_NVCC}" = "1" ]; then \
        cuda=/usr/local/cuda; \
        cp -a ${cuda}/bin /opt/cuda-min/bin; \
        cp -a ${cuda}/nvvm /opt/cuda-min/nvvm; \
        cp -a ${cuda}/targets/x86_64-linux/include /opt/cuda-min/targets/x86_64-linux/include; \
        cp -a ${cuda}/targets/x86_64-linux/lib/libcudadevrt.a \
              ${cuda}/targets/x86_64-linux/lib/libcudart.so* \
              ${cuda}/targets/x86_64-linux/lib/libcudart_static.a \
              /opt/cuda-min/targets/x86_64-linux/lib/; \
        ln -s targets/x86_64-linux/include /opt/cuda-min/include; \
        ln -s targets/x86_64-linux/lib /opt/cuda-min/lib64; \
        # Wolfi glibc declares rsqrt/rsqrtf with noexcept under _GNU_SOURCE
        # (predefined by g++); align CUDA's host declarations so runtime JIT
        # host compiles succeed against Wolfi/Chainguard headers.
        sed -i -E \
            -e 's/(__device_builtin__ double +rsqrt\(double x\)) *;/\1 noexcept (true);/' \
            -e 's/(__device_builtin__ float +rsqrtf\(float x\)) *;/\1 noexcept (true);/' \
            /opt/cuda-min/targets/x86_64-linux/include/crt/math_functions.h; \
    fi

########################################################
# Stage 2: runtime (Chainguard python-fips / public substrate)
########################################################
ARG RUNTIME_BASE
FROM ${RUNTIME_BASE} AS runtime

# Chainguard bases default to a non-root user; switch to root for setup.
USER root

# RUNTIME_APK is empty on a licensed python-fips base (python preinstalled; the
# gcc-14/glibc-dev/binutils toolchain for runtime JIT comes via Custom Assembly).
# On the public wolfi-base prototype it installs python 3.12 + that toolchain.
# gcc 14 is the newest host compiler CUDA 13's nvcc accepts (it rejects gcc > 15).
ARG RUNTIME_APK
RUN if [ -n "${RUNTIME_APK}" ]; then apk add --no-cache ${RUNTIME_APK}; fi \
    && (addgroup -g 10001 sglang 2>/dev/null || true) \
    && (adduser -D -u 10001 -G sglang -h /home/sglang sglang 2>/dev/null || true) \
    && mkdir -p /home/sglang && chown 10001:10001 /home/sglang

# Wheels and their bundled CUDA libs, copied from the Ubuntu builder.
# manylinux wheels are glibc-based and run unchanged on Chainguard/Wolfi.
COPY --from=builder /usr/local/lib/python3.12/dist-packages /usr/lib/python3.12/site-packages
COPY --from=builder /usr/local/bin /usr/local/bin
COPY --from=builder --chown=10001:10001 /root/.cache/huggingface /home/sglang/.cache/huggingface
COPY --from=builder --chown=10001:10001 /root/.cache/sglang /home/sglang/.cache/sglang

# CUDA forward-compatibility libs for older host drivers (opt-in, see header).
COPY --from=builder /usr/local/cuda/compat /usr/local/cuda/compat

# Minimal CUDA compiler subset for runtime JIT (empty when INCLUDE_NVCC=0).
COPY --from=builder /opt/cuda-min /usr/local/cuda

RUN mkdir -p /workspace && chown 10001:10001 /workspace

# GPU driver libraries are injected here by the NVIDIA container toolkit.
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility \
    PATH="${PATH}:/usr/local/cuda/bin:/usr/local/nvidia/bin" \
    LD_LIBRARY_PATH="/usr/local/nvidia/lib:/usr/local/nvidia/lib64" \
    CUDA_HOME=/usr/local/cuda \
    PYTHONUNBUFFERED=1 \
    HOME=/home/sglang \
    TRITON_CACHE_DIR=/home/sglang/.cache/triton

USER 10001:10001
WORKDIR /workspace

CMD ["python3", "-m", "sglang.launch_server", "--help"]
