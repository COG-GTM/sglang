# sgl-router

Slim, KV-aware, OpenAI-compatible router for SGLang workers.

Serves a single model and routes across its workers. Exposes
`/v1/tokenize`, `/v1/detokenize`, `/v1/models`, `/v1/chat/completions`
(buffered and SSE), plus `/healthz` / `/readyz` and `/metrics`. Worker
pools come from either a static URL list or Kubernetes EndpointSlice
discovery. Both edges speak cleartext HTTP/2 where the peer does — see
[HTTP/2](#http2).

## Building

```bash
cd experimental/sgl-router
cargo build --release
```

### FIPS backends

FIPS support is opt-in. Select one backend and build with the committed lockfile.
The backends are mutually exclusive; `--all-features` is not supported.

| Feature | TLS cryptography | Deployment requirement |
| --- | --- | --- |
| `fips` (alias for `fips-openssl`) | Rustls with `rustls-openssl` 0.4.1 and dynamically linked OpenSSL 3 | A compatible runtime library, configured FIPS provider, and vendor-supported operating environment |
| `fips-aws-lc` | Rustls with `aws-lc-rs` 1.18.1 and `aws-lc-fips-sys` 0.14.2, bundling AWS-LC FIPS 4.2.0 | Qualification of that exact module, build, and operating environment |

Both backends initialize before tokenizer loading or discovery, verify the
provider and TLS configuration, require TLS 1.2 Extended Master Secret, and
reject a previously installed Rustls provider. They cover worker forwarding,
introspection, engine monitoring, and Kubernetes API HTTPS. Neither backend
changes the default build, which continues to use ring for TLS.

**OpenSSL runtime integration.** Build in an environment with the target
runtime's OpenSSL development libraries and `pkg-config`:

```bash
OPENSSL_NO_VENDOR=1 OPENSSL_STATIC=0 \
  cargo build --locked --release --features fips
```

Package the resulting binary with the matching OpenSSL runtime libraries and
FIPS provider. Preserve the vendor's module configuration, integrity data, and
`OPENSSL_CONF` / `OPENSSL_MODULES` settings. The configuration must enable
`fips=yes`; an absent provider or disabled FIPS mode stops startup. Inspect
dynamic linkage with `ldd target/release/sgl-router` and run the vendor's
module verification procedure in the final image.

A FIPS Rust builder image is a build stage, not necessarily a runtime image.
For example, Chainguard documents building with `rust-fips` and running with a
compatible `glibc-openssl-fips` image. Image references, entitlement, digest
pins, and verification evidence belong in the deployment repository. See
[Chainguard's Rust FIPS guide](https://images.chainguard.dev/directory/image/rust-fips/overview)
and [runtime verification instructions](https://edu.chainguard.dev/chainguard/fips/verify-fips/#openssl).

**AWS-LC alternative.** The source build requires a C/C++ compiler, CMake, Go,
Perl, and Clang/libclang:

```bash
cargo build --locked --release --features fips-aws-lc
cargo tree --locked --features fips-aws-lc -i aws-lc-fips-sys
```

This normally embeds AWS-LC; a FIPS base image does not replace that module
with its OpenSSL provider. Startup logs report the AWS-LC version and module
generation. Review lockfile changes against the
[AWS-LC FIPS guidance](https://docs.rs/aws-lc-rs/1.18.1/aws_lc_rs/#fips).
The AWS-LC 3.1.0 validation ([CMVP #5314](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314))
does not qualify the bundled 4.2.0 module.

**Trust and assets.** `--tls-ca-bundle FILE` optionally names a PEM file whose
certificates replace the HTTPS trust roots, including Kubernetes roots. The
bundle is read once at initialization; unreadable, empty, or invalid bundles
stop startup. Without it, HTTP clients use Mozilla roots and Kubernetes uses
its normal kubeconfig/service-account trust configuration. FIPS builds reject
Kubernetes `insecure-skip-tls-verify`.

In either FIPS build, pass `--tokenizer-path` with a local file. Package the
model's required sibling files (`config.json`, `tokenizer_config.json`, chat
templates, and any native-format assets) with it. Repository IDs and implicit
Hugging Face download fallback are rejected before a Hub client is created.
The default build retains Hub downloads.

**Transport boundary.** Inbound HTTP/h2c, worker HTTP/h2c, ZMQ event traffic,
and the current KV indexer's plaintext gRPC transport remain cleartext at the
application layer. The router rejects HTTPS indexer endpoints because the linked
gRPC transport has no TLS support. It does not terminate inbound TLS.
Protect those paths with an appropriately configured proxy or
mesh and network policies. Kubernetes authentication and TLS server-name
overrides continue to work.

FIPS-mode checks and CI tests establish configuration and behavior, not CMVP
validation or FedRAMP/IL4/IL5 authorization. Production qualification includes
the exact cryptographic module, its security policy, approved services,
entropy source, build, and operating environment. Other dependencies still
contain cryptographic implementations, including tokenizer download code
that the FIPS entry points exclude and non-security cache hashing.

For local backend regression tests:

```bash
cargo test --locked --workspace -- --skip parity_matrix
cargo test --locked --workspace --features fips-aws-lc -- --skip parity_matrix

# Public test provider only; do not use this build as production evidence.
export OPENSSL_DIR="$HOME/sgl-router-test-openssl"
bash tests/scripts/build_test_openssl.sh "$OPENSSL_DIR"
export OPENSSL_NO_VENDOR=1 OPENSSL_STATIC=0
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="bash $PWD/tests/scripts/run_with_openssl_fips.sh"
cargo test --locked --workspace --features fips -- --skip parity_matrix
```

CI covers Linux x86_64. The test runner scopes the OpenSSL FIPS configuration to
test executables so it does not alter Cargo's own cryptographic dependencies.

## Running

The router is configured entirely through CLI flags (run
`sgl-router --help` for the full list). It serves exactly one model, so
`--model-id` is required, along with exactly one discovery backend.
`--tokenizer-path` is optional: give it a local `tokenizer.json` path or a
HuggingFace repo id, and when omitted the router downloads the tokenizer
for `--model-id` from HuggingFace (honoring `HF_TOKEN` / `HF_HOME`).

Static worker list:

```bash
sgl-router \
  --host 0.0.0.0 --port 30000 \
  --model-id qwen3 \
  --tokenizer-path /models/qwen3/tokenizer.json \
  --worker-urls http://10.0.0.1:30000 http://10.0.0.2:30000
```

Kubernetes EndpointSlice discovery:

```bash
sgl-router \
  --host 0.0.0.0 --port 30000 \
  --model-id qwen3 \
  --tokenizer-path /models/qwen3/tokenizer.json \
  --service-discovery \
  --service-discovery-namespace prod \
  --selector app=engines-qwen3
```

Omit `--service-discovery-namespace` to watch all namespaces (requires
cluster-wide RBAC). For prefill/decode disaggregation, replace `--selector`
with `--prefill-selector` and `--decode-selector`.

External KV indexer as the cache-aware signal source:

```bash
sgl-router \
  --model-id qwen3 \
  --tokenizer-path /models/qwen3/tokenizer.json \
  --worker-urls http://10.0.0.1:30000 http://10.0.0.2:30000 \
  --policy cache_aware \
  --cache-prefix-provider indexer \
  --kv-indexer-endpoint http://10.0.0.10:50051 \
  --kv-indexer-query-timeout-ms 100 \
  --kv-indexer-query-max-inflight 32
```

The Indexer replaces the Router-local radix tree as the native Cache-Aware
signal. Query timeouts and local concurrency are bounded by the two Indexer
options, which default to 100 ms and 32 respectively.

### Fleet-wide sampling contract

`--override-sampling-params` fixes the sampling configuration for every client
of this router, independently of what the engine's own defaults happen to be:

```bash
sgl-router \
  --model-id qwen3 \
  --tokenizer-path /models/qwen3/tokenizer.json \
  --worker-urls http://10.0.0.1:30000 \
  --override-sampling-params '{"temperature": 1, "top_p": 0.95, "n": 1}' \
  --sampling-param-conflict reject
```

It takes one JSON object keyed by the request-body field names
(`temperature`, `top_p`, `top_k`, `min_p`, `repetition_penalty`,
`frequency_penalty`, `presence_penalty`, `n`). A configured value is injected
whenever the request omits that field, so the engine's defaults cannot drift
from what the operator declared. `temperature`, `top_p`, `top_k`, `min_p` and
`repetition_penalty` are the five the engine resolves from the model's own
`generation_config`, which is what makes them drift when a deployed image
changes; the rest have fixed API defaults.

An explicit `null` counts as omitting the field, not as a client-supplied
value: the OpenAI API types these parameters as nullable with a documented
default, so `null` asks for the default — and on a governed fleet the
configured value is what the default is.

For a request that does send a value, `--sampling-param-conflict` decides:
`reject` (the default) 400s a differing value before admission, while `allow`
forwards the client's value untouched — the router never silently rewrites what
a client sent. A `reject` response carries
`x-router-error-code: sampling_contract_violation` and is counted in
`sgl_router_sampling_contract_rejections_total{param}`, so a rollout's blast
radius is visible per parameter rather than folded into `bad_request`.

`reject` also 400s a value it cannot read as a number, on a governed parameter
only. The engine coerces more than JSON numbers — a bool and a numeric string
both become numbers — by rules that are undocumented and need not match across
a fleet, so a value the router cannot read is one it cannot prove conforms, and
waving it through would make the pin bypassable. The common coercions are
matched exactly (`false` is 0, `"0.5"` and `"1_0"` are 0.5 and 10), so this
refuses only genuine garbage. `allow` is unaffected: it promises nothing, so
such a value keeps flowing to the engine, which owns the request schema.

A value may also be an inclusive band, `{"min": LO, "max": HI}`, for a
parameter that stays tunable inside a range. A band names no value to inject,
so it constrains only the requests that name the parameter; one that omits it
gets the model's own `generation_config` default, which the router cannot see.
Because a band can only ever reject, combining one with `allow` is a startup
error.

Values are range-checked at startup, so a misconfiguration fails the launch
instead of 400ing every request at the engine. Repeating a key in the flag is
also a startup error, rather than silently enforcing whichever copy came last.

| parameter | accepted | notes |
| --- | --- | --- |
| `temperature` | `[0, 2]` | |
| `top_p` | `(0, 1]` | |
| `top_k` | `>= 1`, or exactly `-1` | `-1` is the engine's "whole vocabulary" spelling and its default. Being non-contiguous it cannot bound a band. Note `top_k: 1` is greedy decoding, not "disabled". |
| `min_p` | `[0, 1]` | not an OpenAI parameter; the engine's domain |
| `repetition_penalty` | `(0, 2]` | not an OpenAI parameter; the engine's domain |
| `frequency_penalty` | `[-2, 2]` | |
| `presence_penalty` | `[-2, 2]` | |
| `n` | `[1, 128]` | |

The OpenAI domains are deliberately narrower than what the engine itself
accepts (`SamplingParams.verify` would take `temperature: 5`): these values are
injected into request bodies, and a fleet contract outside the range every
OpenAI client library validates against is far more likely a typo than an
intent.

Cost: a request that named every configured field forwards its original bytes
untouched. One that omits a field has the scalars spliced directly into the
body bytes — no parse, no re-serialize — which on a 16 MiB body is ~0.24 ms
against ~5.7 ms for a `serde_json` round-trip. Only `input_ids` and PD
bootstrap injection still parse and re-serialize, because those may have to
overwrite a key the client sent.

### Relationship to the engine's own `--preferred-sampling-params`

The engine has an inject-when-absent flag of its own,
`--preferred-sampling-params`, merged in
`python/sglang/srt/managers/tokenizer_manager.py` as
`{**preferred, **obj.sampling_params}`. On `/v1/chat/completions` it is
currently a no-op: `ChatCompletionRequest.to_sampling_params`
(`python/sglang/srt/entrypoints/openai/protocol.py`) resolves every sampling
key eagerly through `generation_config` and then its own defaults, so the
right-hand side of that merge is always fully populated and always wins.
(`/v1/responses`, in the same file, already omits `None` entries for exactly
this reason.) If that is fixed engine-side, `--preferred-sampling-params`
covers the inject-when-absent half for a single engine.

What stays the router's to own either way is the enforcement half: the
`reject` immutability contract with its 400 before admission — costing no
queue slot and no engine round-trip — the `sampling_contract_violation` code
and per-parameter counter, bands, and one contract applied at a shared ingress
across engines whose own flags the router operator may not control.

## Chat rendering

The router renders chat requests with dynamo-render (`dynamo-renderer`): the model's
HF Jinja template from `tokenizer_config.json` or a sibling
`chat_template.jinja`, or dynamo-render's built-in DeepSeek encoder (V4 family, V3.2)
for template-less models. Cache-aware routing hashes the rendered tokens so its
prefix queries match the blocks the engine caches. Models the engine encodes in
code but dynamo-render cannot tokenize here (Inkling) route via raw prompt
text, as does any model whose template fails to load or render.

Plain text chat requests (string `content`, no tools, no template kwargs or
reasoning controls or historical `reasoning_content`, no assistant continuation,
no consecutive users or non-leading system turns) additionally forward the
rendered tokens to the engine as `input_ids`, retaining the original messages,
so the engine skips re-tokenizing. Every other request shape is rendered for
routing only: the router renders with dynamo-render and does not replicate
SGLang's request normalization, so forwarding is enabled shape by shape as
parity is verified. Use matching model files on the router and workers; worker
template overrides and default kwargs are not observable from the request.

Set `--disable-input-ids-forwarding` for this router's model when worker-side
rendering has not been verified to match. This disables router-generated IDs
for every routing policy; cache-aware routing still renders and tokenizes
locally, and the original messages reach the workers for engine processing.
Caller-supplied `input_ids` remain caller-owned and pass through unchanged.

Forwarding logs its assumptions at startup. In particular, disable it for
`SGLANG_DEFAULT_THINKING=true`, a non-default `SGLANG_DSV4_REASONING_EFFORT`,
worker parser overrides such as `--tool-call-parser deepseekv32` that select a
native encoder over a shipped template, or conversation templates with stop
strings (the engine's `input_ids` path skips those template stops). These worker
settings are not inferred from the router's environment. Disabling forwarding preserves
engine behavior but does not establish parity for local routing hashes.

Also set `--disable-input-ids-forwarding` for array-only templates: Dynamo may wrap
string content into arrays differently from the worker. The pinned Dynamo renderer does not expose
its conversion flag, so the router cannot automatically block these templates.
Detailed content-format parity coverage follows in #39133.

The Dynamo crates are pinned exactly and `Cargo.lock` is committed; CI builds
with `--locked`, so rendered bytes cannot change without a reviewed diff.

## DeepSeek V4

Native V4 rendering follows SGLang's serving path (`serving_chat.py`), not
Dynamo's OpenAI defaults: all declared tools are rendered with SGLang's schema
defaults, reasoning effort comes from `reasoning` / `reasoning_effort`, and the
official/preview effort profile is detected from the checkpoint's
`encoding/encoding_dsv4.py` or overridden by `dsv4_reasoning_effort_profile` in
`config.json`, as in SGLang. Reference prompts live in `tests/fixtures/deepseek/`
and are regenerated by `tests/scripts/generate_deepseek_parity.py`.

V4.1 Flash uses Dynamo's separate V4.1 encoder with SGLang's numeric reasoning
budgets, tool payloads, and `<｜System｜>` markers. Developer messages and media
are left to the worker (the pinned encoder renders them differently), and a
non-default `SGLANG_DSV41_REASONING_EFFORT` needs the forwarding precautions above.

## Kimi-K3

Kimi-K3 renders through dynamo-render's native XTML formatter with SGLang's
request semantics (reasoning controls, tools, `response_format`, continuations)
and the checkpoint's chunked tiktoken encoding. `--tokenizer-path` accepts a
local `tiktoken.model` or an HF repo id, whose `tiktoken.model`, `config.json`
and `tokenizer_config.json` are downloaded when it has no `tokenizer.json`.
An explicit null `thinking_effort` with thinking enabled is not representable
in the pinned formatter and falls back to engine-side rendering.

## HTTP/2

There is nothing to configure. The router negotiates per connection inbound and
resolves the protocol per worker outbound; every combination below is reached
automatically, and HTTP/1.1 remains a supported peer on both edges.

**Inbound.** The listener accepts cleartext HTTP/2 (h2c, prior knowledge) and
HTTP/1.1 on the same `--port`, chosen per connection. A mesh sidecar or
load balancer that prefers h2c multiplexes over one connection instead of
opening one per request; an HTTP/1.1 client is unaffected.

**Outbound.** At registration the router reads each worker's `/server_info` and
forwards over cleartext h2c only when that worker reports `--enable-http2` (the
engine's Granian server, which serves h2c and HTTP/1.1 together) **and** the
worker URL is cleartext. Everything else uses the default client, which speaks
HTTP/1.1 in cleartext and negotiates ALPN `h2, http/1.1` over TLS:

| `/server_info` | worker URL | router forwards over |
|---|---|---|
| `enable_http2: true` | `http://` | cleartext h2c |
| `enable_http2: true` | `https://` | HTTP/2 over TLS, by ALPN |
| `enable_http2: false`, or absent | any | HTTP/1.1 (cleartext) or ALPN (TLS) |

The choice is per worker, not fleet-wide, so a mixed fleet works and one
worker's state never changes another's. The flag comes from the `/server_info`
launch record, which has reported it all along, so no engine change is needed.
A worker whose `/server_info` does not answer has no readable protocol and
forwards over HTTP/1.1 for as long as it stays registered — a throughput cost,
never a correctness one. Admin fan-out (`/flush_cache`) always uses the default
client, because it addresses every worker at once rather than a selected one.

Two things worth knowing when debugging. h2c is prior-knowledge only — there is
no negotiation and no fallback — which is why the router requires the engine's
own `enable_http2` report before using it. And a worker's protocol is fixed
for as long as that worker stays registered: it is read once, at registration,
and never re-read. Under the K8s backend a restarting engine flips its
EndpointSlice to `ready=false`, which is a `Removed` → `Added` cycle and so a
fresh reading; under `--worker-urls` the fan-out happens once at startup and
nothing re-registers. So a worker registered over h2c that later stops serving
it (a proxy interposed on its port, say) is not detected until it
is re-registered; its circuit breaker will open in the meantime.

## Upgrading from `cache_aware_zmq`

The `cache_aware_zmq` policy has been removed. Configurations using it should
select `--policy cache_aware` and choose a native cache-prefix source: the
Router-local radix tree (the default), or the external Indexer shown above.

The legacy `--cache-threshold`, `--balance-abs-threshold`, and
`--balance-rel-threshold` flags have also been removed. They do not have
one-to-one replacements; remove them and review the current `sgl-router
--help` output when tuning Cache-Aware routing.

## License

Apache-2.0.
