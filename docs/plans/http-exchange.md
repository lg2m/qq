# Deepen HTTP Exchange Execution

Status: in progress. Stage 1 behavior-freezing tests are complete.

## Goal

Make `crates/qq-provider/src/http.rs` the single owner of transport invariants
shared by the four direct HTTP streaming adapters, while leaving wire formats
and stream state in those adapters.

This is primarily a refactor. Existing request shapes, authentication headers,
error classification and text, redaction, SSE acceptance, terminal rules, and
resource limits are compatibility constraints. The one intentional policy
tightening is to treat `user-agent` as universally client-controlled; Anthropic
and Google already do so, and OpenAI adapters must gain equivalent coverage.

## Current State

`http.rs` currently owns client construction, endpoint validation, transport
error sanitization, SSE content-type recognition, and a 16 KiB error-body
reader. The adapters still repeat the rest of an exchange:

1. Build a POST request and apply static headers and JSON.
2. Optionally authorize the built request at request time.
3. Execute it and sanitize transport failures.
4. Branch on HTTP success versus a bounded error body.
5. Validate streaming response metadata.
6. Read chunks while maintaining a checked wire-byte count.
7. Maintain a second checked count for emitted text and tool arguments.

Header construction also repeats parsing, sensitive marking, duplicate checks,
redaction normalization, and nearly identical controlled-header lists. The
local copies already differ: for example, some auth checks reject whitespace-
only secrets while Responses deliberately preserves a nonempty value, and only
some controlled-header lists include `user-agent`. Consolidation must not
silently resolve these protocol-level differences.

## Boundary

### Shared HTTP module

`http.rs` should own:

- Universal request-controlled header names.
- Safe insertion of configured headers: parse names and values, reject
  case-insensitive duplicates and reserved names, and mark values sensitive.
- Redaction normalization (longest first, deterministic, deduplicated).
- Request authorization followed by `reqwest::Client::execute`.
- URL-free, redacted transport errors from request building, authorization,
  sending, and body reads.
- The success/non-success status split.
- Reading at most 16 KiB from a non-success body.
- Streaming a success body with checked cumulative wire-byte enforcement.
- Existing client and endpoint policy.

The module remains `pub(crate)`. It is an implementation boundary, not a new
public provider API.

### Protocol adapters

Each adapter should continue to own:

- Endpoint shaping, JSON request structs, and model request conversion.
- Authentication mode validation and protocol-owned headers such as
  `x-api-key`, `anthropic-version`, `x-goog-api-key`, and Codex headers.
- Provider-specific non-2xx envelope decoding, fallback text, sanitization, and
  resulting `ProviderError::Api`.
- Success metadata policy. Most adapters require `text/event-stream`; Responses
  retains the Codex missing-content-type exception.
- SSE decoder configuration and all streamed protocol state.
- Which decoded fields count toward output bytes and provider-specific error
  wording.
- Terminal event requirements and `ProviderEvent` conversion.

The shared exchange must not receive a provider enum, an SSE decoder, or
callbacks for provider behavior. Such an interface would relocate branching
without deepening the module.

### Limits module

Move reusable checked accounting into `limits.rs`:

- `ByteCounter` (or equivalently narrow helper) stores current and maximum
  bytes and rejects checked-add overflow or a value above the maximum.
- It accepts adapter-owned overflow and limit messages, preserving current
  diagnostics.
- The exchange wraps it for wire bytes; adapters use it for output bytes.
- Bedrock remains outside the HTTP exchange but migrates its output accounting
  to this primitive, removing the fifth copy.

`StreamLimits::new` and its budget calculation remain unchanged.

## Proposed Shape

Exact Rust names may change during implementation, but the seam should have the
following semantics:

```rust
struct HttpExchange {
    client: reqwest::Client,
    authorizer: RequestAuthorizer,
    redactions: Arc<[String]>,
}

enum ExchangeOutcome {
    Success(HttpResponse),
    Rejected(HttpRejection),
}

struct HttpRejection {
    status: reqwest::StatusCode,
    body: Vec<u8>, // already bounded to ERROR_BODY_BYTES_LIMIT
}

impl HttpExchange {
    async fn execute(
        &self,
        request: reqwest::Request,
        wire_limit: usize,
        messages: ExchangeMessages,
    ) -> Result<ExchangeOutcome, ProviderError>;
}

impl HttpResponse {
    fn status(&self) -> reqwest::StatusCode;
    fn headers(&self) -> &reqwest::header::HeaderMap;
    fn into_body(self) -> impl Stream<Item = Result<bytes::Bytes, ProviderError>>;
}
```

`execute` clones the static redactions for this request, applies the
`RequestAuthorizer`, appends and normalizes any ephemeral credential values,
then sends. A non-success response is consumed immediately into a bounded
`HttpRejection`. A success response exposes metadata and a body stream that
cannot bypass wire accounting.

Provider names in existing overflow and limit errors are observable in tests.
`ExchangeMessages` can carry static wire-overflow and wire-limit messages while
migration is in progress. Once all adapters use the seam, tests can determine
whether neutral wording is safe; changing wording is not required by this
refactor.

The response wrapper must not implement `Deref<Target = reqwest::Response>` or
expose an unrestricted `bytes_stream()`, because that would make the wire limit
optional. Metadata access should be narrow, followed by consuming the wrapper
into its bounded body.

For headers, prefer a small stateful builder over one highly parameterized
function:

```rust
let mut headers = SafeHeaders::new(protocol_owned_names);
headers.insert_configured(static_headers)?;
headers.insert_auth(name, value, secret)?;
let (headers, redactions) = headers.finish();
```

The universal denylist includes hop-by-hop/framing and request-shaping headers
currently repeated by adapters: `accept`, `connection`, `content-length`,
`content-type`, `expect`, `host`, `http2-settings`, `keep-alive`,
`proxy-authenticate`, `proxy-authorization`, `proxy-connection`, `te`,
`trailer`, `transfer-encoding`, `upgrade`, and `user-agent`. Adapters pass
additional protocol-owned names. The builder must support intentional insertion
of provider-owned headers after configured-header validation; it must not make
all headers generic configuration.

## Sequencing

1. **Freeze behavior with focused tests. Complete.** Added focused tests for
   the universal controlled-header set (including the intentional OpenAI
   `user-agent` tightening), case-insensitive duplicates, sensitive configured
   values, deterministic deduplicated redactions, exact/over/overflow byte
   accounting with preserved diagnostics, bounded and read-failing error
   bodies, successful response streaming, and sanitized body-read failures.
   Existing adapter contract tests retain non-SSE success policy and
   provider-specific HTTP error decoding.
2. **Extract byte accounting.** Add the checked counter to `limits.rs`; migrate
   output counters in the four HTTP adapters and Bedrock. Preserve exact error
   strings and test overflow plus one-byte-over-limit behavior centrally.
3. **Introduce the exchange types.** Implement request-time authorization,
   execution, status split, bounded rejection, and limited success stream in
   `http.rs`. Unit-test this seam against a localhost server, including dynamic
   credential redaction.
4. **Migrate Chat Completions.** It is the simplest full path with a request
   authorizer. Delete its local send/status code, wire counter, and direct error
   body read after contract tests pass.
5. **Migrate Anthropic and Google.** Google currently uses `send()` and no
   request-time authorizer; use the default authorizer so both still traverse
   the same exchange. Preserve their error envelopes and protocol-owned header
   sets.
6. **Migrate Responses last.** Preserve built-request authorization, Codex
   redactions, standard-versus-Codex body shape, and the Codex exception for an
   absent content type.
7. **Consolidate safe header construction.** Migrate one adapter at a time and
   delete each local universal controlled-header predicate only when its tests
   pass. Keep protocol-specific secret emptiness semantics explicit at adapter
   call sites.
8. **Delete obsolete helpers and duplication.** `read_error_body` should become
   internal to exchange rejection handling; adapters consume `HttpRejection`.
   Verify no direct adapter calls `Client::execute`/`send`,
   `Response::bytes_stream`, or local wire-counter helpers remain.

The exchange and byte-counter steps may land separately. Header consolidation
should follow exchange migration rather than enlarging the first change with a
second independent compatibility surface.

## Test And Acceptance Plan

Run `cargo test -p qq-provider` after each migration and the workspace checks at
the end. Acceptance requires:

- Existing deterministic wire-contract tests pass without fixture changes.
- All four direct HTTP adapters execute through the shared exchange.
- Request-time bearer/Codex credentials are redacted from build, send, body
  read, HTTP-error, and decoder errors.
- Non-success bodies never exceed 16 KiB in memory and preserve status plus the
  bytes needed by adapter-specific envelope parsing.
- Success bodies cannot be consumed without checked wire-limit enforcement.
- Exact-limit bodies succeed; one byte over and arithmetic overflow fail.
- Output accounting covers text, refusals, and tool arguments as before.
- Non-SSE success handling remains adapter-owned, including the Responses Codex
  exception.
- Static headers cannot override universal or protocol-owned headers; duplicate
  names remain case-insensitive; configured values remain sensitive. OpenAI
  adapters reject configured `user-agent` just like Anthropic and Google.
- Cancellation drops the in-flight request/body stream without a background
  draining task.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` and
  `cargo test --workspace` pass.

A source-level search should show transport invariants in one place, not merely
wrapped copies: no local `add_wire_bytes`, no adapter `read_error_body`, and no
four-way universal controlled-header list.

## Risks And Decisions

- **Error compatibility:** central helpers can accidentally homogenize provider
  wording. Carry static messages or keep message construction adapter-owned.
- **Secret lifetime:** request-time credentials are only known after
  authorization. Normalize them into the per-request redaction set before any
  send or response operation can fail.
- **Hidden limit bypass:** exposing raw `reqwest::Response` defeats the design.
  Use narrow metadata methods and a consuming bounded stream.
- **Over-abstraction:** status parsing and SSE interpretation vary legitimately.
  Keep them out of the exchange even if a callback would remove a few lines.
- **Header semantic drift:** whitespace rules currently differ. Consolidate
  mechanics, not policy; adapter auth validation remains explicit.
- **Bedrock:** its AWS SDK event stream is not an HTTP exchange consumer. Only
  common checked output accounting applies.

## Non-Goals

- A public transport trait or pluggable production transport.
- Combining protocol adapters or their SSE state machines.
- Retrying, backoff, telemetry, or live-canary infrastructure.
- Changing timeout values, endpoint policy, stream-budget formulas, provider
  error classification, or request wire shapes.
- Moving Mantle or Bedrock SDK execution into this exchange in the initial
  implementation.
