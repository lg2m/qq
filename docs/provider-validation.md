# Provider Validation

Status: validation standard and implementation roadmap

## Purpose

Provider support is valid only when QQ can prove all of the following:

1. Configuration resolves to the intended deployment, protocol, endpoint, and
   authentication mode.
2. The emitted request matches the provider contract without leaking secrets.
3. Streaming responses produce the expected provider-neutral events under
   arbitrary network chunk boundaries.
4. Authentication, provider errors, cancellation, and resource limits fail in
   predictable ways.
5. A real provider accepts a minimal request using the credentials and model
   expected in production.

No single test layer proves all five properties. Default tests must remain
offline and deterministic, while opt-in live canaries detect upstream API,
credential, permission, and model availability changes.

## Validation Matrix

Every supported deployment and authentication path must appear in one checked-in
matrix. A row is incomplete until it has deterministic contract coverage and an
assigned live-validation cadence.

| Deployment | Protocol | Authentication paths | Offline gate | Live gate |
| --- | --- | --- | --- | --- |
| OpenAI | Responses | bearer API key | every PR | nightly |
| OpenAI Codex | Responses | OAuth access token, account headers | every PR | manual and release |
| Anthropic | Messages | `x-api-key` | every PR | nightly |
| Google Gemini | GenerateContent | `x-goog-api-key` | every PR | nightly |
| Amazon Bedrock | ConverseStream | Bedrock API key, default AWS chain, named profile | every PR | nightly and release |
| Bedrock Mantle | Responses | API key, SigV4 | every PR | nightly and release |
| Bedrock Mantle | Chat Completions | API key, SigV4 | every PR | nightly and release |
| Bedrock Mantle | Anthropic Messages | API key, SigV4 | every PR | nightly and release |
| LiteLLM/custom | Configured HTTP protocol | configured bearer, key, header, or no auth | every PR | deployment-owned |

The future model registry may let users select only a model ID, but validation
must continue to record the resolved deployment. Tests must fail if a registry
change silently routes a model through a different provider or authentication
path.

## Test Layers

### 1. Configuration And Compilation

These tests run without sockets or credentials. For every matrix row, assert:

- Layered configuration produces the intended typed provider recipe.
- Model selection resolves to the expected deployment and provider model ID.
- Base endpoints append only the protocol-owned path.
- Exact endpoints are not rewritten.
- Region, profile, endpoint, header, and authentication values are validated.
- Unsupported protocol/authentication combinations fail before network access.
- Provider cache identity includes every value that changes request behavior.
- Debug and error formatting never expose credentials.

Run these tests whenever configuration, the model registry, provider recipes,
credential resolution, or provider compilation changes.

### 2. Deterministic Wire Contracts

Each protocol codec uses a localhost server or an SDK replay transport. The test
captures the request and returns controlled response frames. It must verify:

- HTTP method, URL, model path, content type, and streaming negotiation.
- Protocol-specific authentication headers and absence of credentials in URLs.
- Message roles, text, system instructions, and output-token limits.
- Success events, usage, terminal completion, and legal empty deltas.
- Frames split at every meaningful byte boundary, including UTF-8 boundaries.
- Multiple events delivered in one network chunk.
- Provider-declared errors in both HTTP bodies and stream events.
- Premature EOF, malformed frames, unknown events, and non-streaming responses.
- Response, event, and accumulated-output limits.
- Cancellation while connecting, reading, decoding, and waiting for credentials.
- Error classification and redaction of request and response secrets.

Fixtures should be minimal protocol examples rather than recordings of complete
production responses. Sanitized provider fixtures may supplement generated edge
cases, but they must contain no account IDs, request IDs, credentials, prompts,
or model output copied from private traffic.

Amazon Bedrock should use the AWS SDK replay/test transport where possible so
ConverseStream request construction and event-stream decoding remain
deterministic. Test-only transport injection must not become a production
endpoint override.

### 3. Runtime Composition

Composition tests exercise the same path as `qq ask` with a local fake provider.
They prove that configuration, credential lookup, compilation, `qq-core`, and
event rendering agree. At minimum, cover:

- One successful stream for every protocol.
- A provider authentication failure.
- A provider rate-limit or availability failure.
- Cancellation and bounded output.
- Stored, environment, and OAuth credential selection without real secrets.
- Model-registry resolution to the expected provider recipe.

These tests must use isolated config, trust, credential, and data directories.
They must never depend on a developer's `.qq/config.ron`, environment, keyring,
or plaintext credential store.

### 4. Credentialed Live Canaries

Live checks are explicit, bounded, and excluded from normal `cargo test`. The
target interface is:

```sh
cargo xtask providers check offline
cargo xtask providers check live --provider google
cargo xtask providers check live --all
```

These commands are an implementation target; `xtask` does not provide them yet.
The live runner should construct recipes directly from a checked-in, nonsecret
matrix instead of loading project configuration.

Each live case must:

1. Use a pinned canary model known to support the tested protocol.
2. Send only `Reply only with QQ_PROVIDER_SMOKE_OK`.
3. Request no more than 32 output tokens and perform no tool calls.
4. Require at least one text event and exactly one successful terminal event.
5. Record whether the marker appeared, but not fail solely for harmless prose
   around it.
6. Disable automatic inference retries to prevent duplicate spend.
7. Enforce connection, first-token, total-time, event-size, and output limits.
8. Emit only redacted metadata.

A canary validates both a pinned stable model and, when different, QQ's current
default model. The pinned model separates provider connectivity from model
catalog churn; the default-model check detects a broken product default.

### 5. Differential Diagnosis

Live failures are ambiguous because credentials expire, permissions change, and
providers have outages. Nightly and release automation should retain the last
green QQ binary and rerun the same canary with the same model and credential:

| Current binary | Last green binary | Interpretation |
| --- | --- | --- |
| pass | pass | healthy |
| fail | pass | probable QQ regression |
| fail | fail | credential, account, model, or provider failure |
| pass | fail | upstream recovery or baseline incompatibility |

The baseline is a diagnostic, not a release gate by itself. If a provider makes
an intentional breaking API change, both binaries may fail while current source
still needs an update.

## Live Credential Policy

- Live tests require an explicit opt-in such as `QQ_LIVE_PROVIDER_TESTS=1`.
- Use dedicated low-quota test projects and accounts, never personal production
  credentials.
- CI should use workload identity or OIDC and short-lived credentials where the
  provider supports them.
- Bedrock short-term API keys expire with their AWS session and last at most 12
  hours. Prefer an OIDC-assumed AWS role and SigV4 for unattended checks.
- Bedrock API-key checks remain necessary because bearer authentication is a
  separate supported path. Refresh those keys immediately before their gated
  run or use a deliberately bounded long-term test key.
- OpenAI Codex OAuth uses an interactive subscription identity. Keep its live
  check manual or on an explicitly approved secure runner; do not bypass OAuth
  or place a personal refresh token in general CI.
- Never print, serialize into artifacts, or include credentials in command-line
  arguments. Mark authorization headers sensitive.
- Do not log full model responses. The smoke marker, event counts, byte counts,
  and timings are sufficient.

Credential metadata proves only that a credential is stored. A live request is
the validity check for expiration, revocation, endpoint scope, and permissions.

## Result Records

Each live result should produce a small machine-readable record containing:

- UTC timestamp and QQ commit.
- Deployment, protocol, authentication mode, region, and model.
- Pass, fail, skip, or infrastructure-error outcome.
- HTTP/provider error category and sanitized provider request ID on failure.
- Connection time, time to first token, total time, event count, and output
  byte count.
- Baseline result when differential diagnosis ran.

Do not store prompts, generated text, headers, URLs containing query secrets, or
raw error bodies. Retain enough history to distinguish a one-off outage from a
regression trend.

## Required Cadence

| Trigger | Required validation |
| --- | --- |
| Every local provider change | affected package tests and affected contract matrix rows |
| Every PR | full offline workspace and provider matrix |
| Provider codec or auth change | affected live provider before merge |
| Provider SDK or HTTP dependency update | all offline contracts and affected live providers |
| Model registry update | resolver tests and live checks for changed defaults |
| Nightly | all unattended live canaries plus last-green differential on failure |
| Release candidate | every matrix row, including approved manual OAuth checks |

Live checks should report `skip` with a reason when credentials are unavailable;
they must never silently pass. Required release rows cannot remain skipped.

## Failure Triage

Classify before changing code:

1. Confirm the effective deployment, protocol, model, region, and auth mode.
2. Reproduce with the smallest live canary, not a full agent session.
3. Run the same credential and model through the last green binary.
4. Use status and provider request IDs to classify the failure using the table
   below.
5. Reproduce the failure with a sanitized local fixture before fixing code.
6. Add the fixture as a regression test, apply the fix, and rerun offline, live,
   and differential checks.

| Signal | Likely class |
| --- | --- |
| `401` or authentication `403` | expired, revoked, malformed, or wrong-scope credential |
| authorization `403` | missing provider or model permission |
| `404` | endpoint, region, protocol path, or model availability |
| `429` | quota or rate limiting |
| successful HTTP with decoder failure | protocol drift or framing regression |

Do not infer a QQ regression solely from timing. In the Bedrock API-key failure
investigated on 2026-07-22, both current source and the pre-provider-work binary
received the same authentication `403`; the stored provider credential, not the
provider changes, was the failing variable.

## Current Coverage And Gaps

Current strengths:

- OpenAI Responses, OpenAI Chat Completions, Anthropic Messages, and Google
  GenerateContent have localhost request and stream contract tests.
- Mantle tests cover protocol-specific API-key headers and SigV4 signing.
- Provider compilation and runtime construction have deterministic tests.
- Stream bounds, malformed input, terminal behavior, and secret redaction have
  focused coverage.

Current gaps:

- There is no checked-in executable provider matrix.
- `xtask` has no provider validation command.
- There is no CI workflow or scheduled live canary.
- Live results and last-green binaries are not retained for comparison.
- Bedrock SDK request/replay coverage is less complete than the HTTP codecs.
- Codex OAuth has deterministic login tests but no approved live release check.
- Model defaults and provider resolution are not yet owned by a model registry.

## Implementation Order

1. Add a checked-in matrix and `cargo xtask providers check offline` that maps
   every row to its deterministic tests.
2. Add isolated runtime composition tests for every protocol.
3. Add the opt-in live runner with one provider at a time, strict budgets, and
   sanitized result records.
4. Add nightly CI using dedicated credentials, with unavailable credential rows
   reported as skips.
5. Retain the last green binary and run differential diagnosis automatically on
   live failures.
6. Make the complete matrix a release checklist, including manual Codex OAuth.

A provider feature is complete only when its matrix rows exist, offline tests
pass, the required live check passes, and failures remain diagnosable without
exposing credentials or private model traffic.
