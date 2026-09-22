// SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
// SPDX-License-Identifier: Apache-2.0

//! sgl-router: slim KV-aware OpenAI-compatible router for SGLang workers.
//!
//! See `~/.claude/projects/-Users-kangyan-zhou-sglang-workspace-sglang/specs/2026-05-14-sgl-router-slim-design.md`
//! for the design roadmap.

#[cfg(all(feature = "fips", feature = "fips-aws-lc"))]
compile_error!("select only one FIPS backend: fips or fips-aws-lc");

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod buckets_reorg;
pub mod config;
pub mod discovery;
pub mod health;
pub mod policies;
pub mod policies_reorg;
pub mod proxy;
pub mod server;
pub mod state;
pub mod tls;
pub mod tokenizer;
pub mod workers;
