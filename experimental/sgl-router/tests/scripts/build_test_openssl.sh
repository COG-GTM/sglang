#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
# SPDX-License-Identifier: Apache-2.0

# This provider is for CI behavior tests, not a qualified deployment module.
set -euo pipefail

prefix="${1:?usage: build_test_openssl.sh ABSOLUTE_INSTALL_PREFIX}"
[[ "$prefix" = /* ]] || { echo "Install prefix must be absolute" >&2; exit 1; }
version=3.6.4
sha256=9bffaa1ad1e07b354c21bd3324ec02fa15579f45a7d0494b3e74bc449b7333ef
mkdir -p "$prefix/source"
archive="$prefix/source/openssl-$version.tar.gz"
curl --fail --location --retry 3 \
    "https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz" \
    --output "$archive"
printf '%s  %s\n' "$sha256" "$archive" | sha256sum --check
tar -xzf "$archive" -C "$prefix/source"
(
    cd "$prefix/source/openssl-$version"
    ./Configure enable-fips shared no-tests \
        --prefix="$prefix" --openssldir="$prefix/ssl" --libdir=lib
    make -j"${OPENSSL_BUILD_JOBS:-2}" build_sw
    make install_sw install_fips
)
