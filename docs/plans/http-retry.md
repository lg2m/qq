# Pre-Stream HTTP Exchange Retry

Status: completed.

## Goal

Absorb transient provider overload and rate-limit failures inside
`crates/qq-provider/src/http.rs` before any success body is handed to an
adapter. A single shared pre-stream retry policy should cover the four direct
HTTP streaming adapters without changing request shapes, SSE state machines,
provider error classification, or mid-stream behavior.

The immediate pain is OpenAI returning overloaded / 5xx / 429 responses before
SSE begins. Those failures are transport-level and already funnel through
`HttpExchange::execute`.

## Current State

`HttpExchange::execute` authorizes once, sends once, and returns either:

- `ExchangeOutcome::Success` with a wire-limited body stream, or
- `ExchangeOutcome::Rejected` with status plus a bounded error body, or
- `ProviderError::Transport` on connect/send failure.

Adapters map rejections into provider-specific `ProviderError::Api` values.
There is no attempt loop, no `Retry-After` handling, and no backoff. Bedrock
uses the AWS SDK with retries explicitly disabled and stays out of this plan's
initial scope.

## Boundary

### Shared HTTP module

`http.rs` owns:

- The retry policy value object and its defaults.
- Which pre-stream outcomes are retryable.
- Delay selection: exponential backoff, full jitter, `Retry-After`, and a total
  retry budget.
- Cloning the built request for subsequent attempts when possible.
- Re-running request-time authorization on every attempt.
- Returning the last rejection or transport error after attempts are exhausted.

The module remains `pub(crate)`. Retry is an internal transport detail, not a
new public provider API.

### Protocol adapters

Adapters keep owning:

- Request construction and JSON bodies.
- Success metadata policy and SSE decoding.
- Final non-2xx envelope decoding and user-visible error text.
- Output-byte accounting and terminal events.

Adapters must not grow local retry loops, status allowlists, or sleep calls.
They continue to call `HttpExchange::execute` once per model stream.

### Out of scope for this plan

- Mid-stream retries after `ExchangeOutcome::Success` is returned.
- Parsing provider JSON error envelopes to decide retryability.
- Bedrock / Mantle SDK retry configuration.
- User-visible "retrying" events or TUI affordances.
- Configuration surface in `qq` config files.
- Public transport traits or pluggable middleware stacks.
- Changing connect/read/request timeouts, wire budgets, or endpoint policy.

## Policy

Defaults:

| Knob | Value |
| --- | --- |
| max attempts | 3 (1 try + 2 retries) |
| base delay | 250 ms |
| max delay | 4 s |
| total retry budget | 15 s |
| jitter | full jitter on the chosen delay |
| `Retry-After` | honored when present and parseable; capped by max delay |

Retryable pre-stream outcomes:

- Transport failures from `client.execute` before a response is obtained.
- HTTP statuses `408`, `429`, `500`, `502`, `503`, `504`.

Non-retryable:

- `401` / `403` / `400` / `404` / `409` / `422` and other non-listed statuses.
- Successful response headers (`ExchangeOutcome::Success`), even if the later
  body stream fails.
- Requests that cannot be cloned for a later attempt; those stay single-shot.
- A policy with `max_attempts == 1` (`RetryPolicy::disabled()`), required for
  live canaries and deterministic single-attempt tests.

Delay selection for attempt `n` after a retryable failure:

1. Compute exponential delay `min(max_delay, base_delay * 2^n)`.
2. If the rejection carried a parseable `Retry-After`, take
   `max(exponential, retry_after)` and cap at `max_delay`.
3. Apply full jitter: sleep `random(0..=chosen)`.
4. Stop retrying when the next delay would exceed the remaining total budget.

`Retry-After` parsing accepts delta-seconds. Invalid or missing values fall
back to exponential backoff only. HTTP-date forms may be added later if a
provider needs them; they are not required for the OpenAI overload path.

## Proposed Shape

Exact names may change during implementation:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    total_budget: Duration,
}

impl RetryPolicy {
    const fn default_policy() -> Self { /* 3 / 250ms / 4s / 15s */ }
    const fn disabled() -> Self { /* max_attempts = 1 */ }
}

impl HttpExchange {
    fn new(...) -> Self; // uses RetryPolicy::default_policy()
    fn with_retry_policy(self, policy: RetryPolicy) -> Self;

    async fn execute(...) -> Result<ExchangeOutcome, ProviderError> {
        // authorize + send in a pre-stream loop
        // clone request when another attempt remains
        // never retry after Success is constructed
    }
}
```

Invariants:

- Authorization runs on every attempt so short-lived tokens stay fresh.
- Static and ephemeral redactions are normalized per attempt from the exchange
  redaction set plus that attempt's authorizer output.
- The first non-retryable rejection is returned immediately.
- The last retryable rejection or transport error is returned when attempts or
  budget are exhausted.
- No adapter source changes are required for default enablement beyond any
  constructor plumbing needed to pass a custom policy in tests.

## Sequencing

1. **Policy types and pure helpers. Complete.** Added `RetryPolicy`,
   retryable-status classification, `Retry-After` delta-seconds parsing,
   exponential delay selection, budget remaining checks, full jitter, and
   deterministic unit tests. `HttpExchange` stores the policy and exposes
   `with_retry_policy`; `execute` is still single-shot until stage 2.
2. **Pre-stream attempt loop. Complete.** `HttpExchange::execute` clones the
   request when another attempt remains, re-authorizes every send, sleeps with
   full jitter under the total budget, and returns the last rejection or
   transport error. Default policy is on; `with_retry_policy` covers tests and
   future canaries. Localhost multi-response tests cover success-after-503,
   no-retry on 401, transport retry, disabled policy, exhausted attempts,
   `Retry-After` capping, and per-attempt re-authorization.
3. **Adapter and canary wiring. Complete.** All four HTTP adapters construct
   `HttpExchange` with the default retry policy and need no request-path
   changes. Each exposes `without_retries()` so future live canaries and
   single-shot probes can force `RetryPolicy::disabled()` without touching the
   exchange internals. Existing localhost adapter fixtures already use
   non-retryable statuses or one-shot 200 streams, so no extra OpenAI
   exhaustion fixture was required.
4. **Cleanup and docs. Complete.** Provider design docs record the pre-stream
   retry policy and the canary `without_retries()` requirement. The older
   http-exchange non-goal now points at this plan. No temporary test-only seams
   remained to delete.

## Test And Acceptance Plan

Stage 1:

- Default policy constants match the table above.
- `disabled()` allows exactly one attempt.
- Retryable and non-retryable statuses are exhaustive for the listed set.
- `Retry-After: 2` yields two seconds; invalid values are ignored.
- Exponential delays cap at `max_delay`.
- Budget helper reports when the next sleep is unaffordable.

Stage 2+:

- `cargo test -p qq-provider` covers multi-attempt exchange behavior with fake
  localhost servers.
- Existing adapter wire-contract tests pass without fixture changes.
- A 503 then 200 exchange succeeds once and hits the server twice.
- A 401 exchange hits the server once.
- Disabled policy never sleeps and never resends.
- Exhausted retries return the last rejection status and bounded body.
- Request-time authorization still redacts dynamic credentials on every
  attempt.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` and
  `cargo test --workspace` pass at the end.

## Risks And Decisions

- **Duplicate spend:** only pre-stream failures retry. Once headers succeed,
  the body belongs to the adapter and is not retried.
- **Non-cloneable bodies:** JSON requests built by adapters are cloneable today.
  If cloning fails, degrade to a single attempt rather than buffering bodies in
  the exchange.
- **Latency:** worst case adds roughly the retry budget, not unbounded sleeps.
  Keep the budget well under the existing request timeout.
- **Error wording:** retry must not rewrite adapter error text. Return the last
  rejection bytes unchanged.
- **Bedrock:** left disabled and separate so SDK policy does not block the HTTP
  fix for OpenAI overload.
- **Visibility:** no progress events in this plan; silent transport retry is
  acceptable for v1.

## Non-Goals

- Public retry configuration in application config.
- Mid-stream resume, idempotency keys, or replay of partial SSE.
- Homogenizing provider API error enums.
- Enabling AWS SDK retries.
- Telemetry exporters or metrics pipelines.
