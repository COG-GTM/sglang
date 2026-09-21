#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail
scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export OPENSSL_CONF="$scripts/../fixtures/openssl-fips.cnf"
export LD_LIBRARY_PATH="${OPENSSL_DIR:?}/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$@"
