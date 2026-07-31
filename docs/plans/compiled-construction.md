# Compiled Provider Construction

Status: completed.

Implemented across phases 1–9 in commits `61314ab`, `09f193b`, `da1f2bc`,
`c4c4ff6`, `d80d786`, `73da499`, and `a5a3759`. Follow-up test-surface work may
add broader matrix and composition coverage, but the construction architecture
and cleanup described here are complete.

## Goal

Give compiled HTTP provider construction one owner. A single internal construction
component should convert a coherent deployment description into a concrete HTTP
adapter while deriving these decisions together:

- Protocol and authorization compatibility.
- Concrete adapter selection.
- Protocol-specific interpretation of generic authentication intent.
- Request-time authorization mode.
- OpenAI Responses standard versus Codex request shape.
- Google base versus exact endpoint behavior.
- Use of compiler-owned shared HTTP clients.

Both `ProviderCompiler` and Mantle should use this component. Protocol adapters
should continue to own their wire formats and stream state, while runtime should
continue to own configuration and credential resolution.

The immediate architectural risk is that authorization and request shape remain
independently composable. The earlier observed Codex defect has been patched, but
the current representation still permits invalid states:

```rust
HttpAuth::RequestTime {
    credentials,
    codex_responses: bool,
}
```

Construction must make a Codex request shape a consequence of coherent Codex
Responses intent rather than a caller-selected boolean.

## Current State

`ProviderCompiler::compile_http` in
`crates/qq-provider/src/compiler.rs` matches `HttpProtocol` and constructs the
four direct HTTP adapters. It also translates `HttpAuth` into adapter-specific
static authentication, a `RequestAuthorizer`, and, for OpenAI Responses, a body
shape flag.

Mantle independently performs much of the same work in
`crates/qq-provider/src/mantle.rs`. It matches protocols, translates its API-key
or SigV4 authorization into adapter-specific values, constructs three concrete
adapters, and maintains its own provider dispatch enum. Google is supported by
the direct compiler but rejected by Mantle.

The direct adapters expose crate-private constructors that accept independently
selected construction parts. In particular, the Responses constructor receives
static authentication, a request authorizer, and request shape separately. This
allows the compiler to assemble combinations the type system cannot validate.

The concrete public adapter constructors such as `OpenAi::new` and
`OpenAi::with_endpoint` also construct clients. They are a standalone public
compatibility surface, not the compiled path targeted by this plan.

### Codex defect status

The architecture review identified a composition defect where request-time
Codex authorization could add Codex headers while the Responses adapter retained
the standard request shape. Commit `49fb7f6` repaired that direct failure by
adding `codex_responses` to `HttpAuth::RequestTime`, and a compiler-level test now
checks that request-time Codex omits `max_output_tokens`.

That repair does not establish a durable boundary. The public recipe can still
represent combinations such as:

- A Codex credential provider with `codex_responses: false`.
- A bearer credential provider with `codex_responses: true`.
- `codex_responses: true` for Chat Completions or Anthropic, where it is ignored.
- A request-time provider that returns a different credential kind than
  construction expected.

The new construction owner must remove those states from its internal model.

## Boundary

### Compiled construction component

The internal construction component owns:

- The compatibility matrix between an HTTP protocol and authorization intent.
- Mapping generic API-key intent to protocol-specific headers.
- Selection of the concrete HTTP adapter.
- Selection of a Responses request kind.
- Creation of the request-time authorizer appropriate to the intent.
- Google endpoint-kind propagation.
- Construction with a client supplied by the caller.
- A single compiled HTTP provider dispatch type, if dynamic dispatch is still
  needed internally.

It must not create its own `reqwest::Client`.

### Provider compiler

`ProviderCompiler` continues to own:

- Shared HTTPS and direct-loopback clients.
- Resolution and validation of `EndpointSpec`.
- Selection of the correct shared client for the resolved endpoint.
- Native Bedrock compiler construction.
- Top-level dispatch from `ProviderRecipe`.

After resolving the endpoint and client, it delegates HTTP compatibility,
authorization interpretation, adapter selection, and adapter construction.

### Runtime

`src/runtime.rs` continues to own:

- Configuration parsing.
- Credential and profile lookup.
- Provider cache-key construction.
- Translation of user configuration into a provider recipe.
- Deciding which configured credential source is intended.

Runtime must stop selecting a Responses body shape with a boolean. It should
express coherent intent such as request-time Codex or request-time bearer.

### Mantle

Mantle is not a separate provider-construction path. It is a lazy deployment
resolver whose output is the same coherent HTTP construction specification used
by direct configuration. Before that shared seam, `mantle.rs` owns only:

- Lazy AWS configuration and initialization.
- Region resolution.
- Canonical Mantle endpoint generation.
- AWS credential loading.
- Retryable initialization after transient failure.
- Declaring Mantle deployment capabilities for shared compatibility validation.

After Mantle has resolved an endpoint and authorization intent, construction
must neither know nor care that those inputs came from Mantle. Mantle stops
matching protocols to concrete adapters, translating authorization into
adapter-specific authentication enums, and maintaining a separate provider
dispatch implementation. A thin lazy wrapper may remain solely to preserve the
current asynchronous initialization lifecycle; once warm, it delegates to the
same compiled HTTP provider type as direct configuration.

### Protocol adapters

The direct HTTP adapters continue to own:

- Request serialization and provider-specific body types.
- Protocol headers and static-header restrictions.
- Success metadata policy.
- SSE and stream state machines.
- Response decoding and provider-specific errors.
- Output accounting and terminal events.

Their low-level constructors may remain `pub(crate)`, but compiled callers reach
them only through the construction component.

### Native Bedrock

The native Bedrock adapter remains separate. This plan does not combine AWS SDK
construction with HTTP adapter construction and does not change AWS SDK retry
configuration.

### Standalone public constructors

Public constructors such as `OpenAi::new`, `OpenAi::with_endpoint`, and their
other protocol equivalents remain supported. This plan establishes one owner
for compiled construction; it does not require every external user to construct
providers through `ProviderCompiler`.

## Compatibility Matrix

The construction component should make supported combinations explicit. The
initial matrix is:

| Protocol | Supported authorization intent | Derived behavior |
| --- | --- | --- |
| OpenAI Responses | none, API key/bearer, named header, static Codex, request-time bearer, request-time Codex, Mantle SigV4 | Standard or Codex body shape is derived from intent |
| OpenAI Chat Completions | none, API key/bearer, named header, request-time bearer, Mantle SigV4 | Bearer-compatible static auth; no Codex body mode |
| Anthropic Messages | none, API key, bearer, named header, request-time bearer, Mantle SigV4 | API key maps to `x-api-key`; Mantle API key uses `x-api-key` |
| Google GenerateContent | none, API key, bearer, named header | Endpoint mode preserved; unsupported request-time or Mantle combinations rejected |

This table documents current intended behavior, not an obligation to broaden
support. Existing behavior must be confirmed by tests before codifying a
combination. Unsupported combinations return `ProviderError::Configuration`
before network I/O.

Static Codex authorization is valid only for OpenAI Responses. Request-time
Codex intent is valid only for OpenAI Responses. Request-time bearer intent may
be admitted only for protocols whose current request authorizer and static auth
mapping support it.

## Proposed Shape

Exact names may change during implementation. The important property is that the
internal representation cannot independently select authorization semantics and
request shape.

One possible shape is:

```rust
enum CompiledHttpAuth {
    NoAuth,
    ApiKey(String),
    Bearer(String),
    Header {
        name: String,
        value: String,
    },
    Codex {
        access_token: String,
        account_id: String,
        is_fedramp: bool,
    },
    RequestTimeBearer(SharedRequestCredentialProvider),
    RequestTimeCodex(SharedRequestCredentialProvider),
    MantleSigV4(RequestAuthorizer),
}

struct CompiledHttpSpec {
    protocol: HttpProtocol,
    endpoint: reqwest::Url,
    endpoint_kind: EndpointKind,
    auth: CompiledHttpAuth,
    headers: Vec<(String, String)>,
}

enum CompiledHttpProvider {
    Responses(OpenAi),
    ChatCompletions(OpenAiChatCompletions),
    Anthropic(AnthropicMessages),
    Google(GoogleGenerateContent),
}

fn construct_http_provider(
    client: reqwest::Client,
    spec: CompiledHttpSpec,
) -> Result<CompiledHttpProvider, ProviderError>;
```

The implementation may instead use private protocol-specific spec variants or
return `Arc<dyn Provider>`. It should optimize for a small construction seam and
compile-time exclusion of invalid combinations, not preserve this sketch
literally.

Request-time credential authorization must retain the expected credential
family. A request-time Codex construction should reject a bearer credential,
and a request-time bearer construction should reject a Codex credential,
instead of applying whichever credential kind happens to be returned.

## Implementation Plan

### 1. Freeze the compiled seam with interface tests

Before changing construction, add a shared loopback fixture under the provider
crate's test-only surface and exercise:

```text
ProviderCompiler::compile -> Provider::stream
```

Cover at least:

- Responses with static bearer authentication.
- Responses with static Codex authentication.
- Responses with request-time Codex authentication.
- Chat Completions with request-time bearer authentication.
- Anthropic with API-key authentication.
- Google base and exact endpoint behavior.
- Mantle API keys using bearer for OpenAI protocols.
- Mantle API keys using `x-api-key` for Anthropic.
- Every unsupported protocol/authorization combination.
- Request-time credential-kind mismatch without a network request.

Assertions should inspect the actual outgoing request after compiler
construction: endpoint, relevant headers, and request body. The request-time
Codex case must assert that `max_output_tokens` is absent, while standard
Responses must assert that it is present when configured.

Introduce one reusable loopback request reader rather than copying another
`read_request` helper. Existing copies currently live in compiler and adapter
test modules. Do not migrate protocol-local malformed-event tests merely to
increase interface-test counts.

### 2. Replace the Codex shape boolean with coherent intent

Remove `codex_responses: bool` from the construction model and replace ambiguous
request-time authorization with distinct bearer and Codex intent.

Runtime should construct one of those coherent variants directly. The compiler
should derive Responses request kind from the selected variant. Other protocols
must reject Codex intent rather than ignore a shape flag.

If source compatibility for the public `HttpAuth` enum must be preserved,
translate its old form at the recipe boundary into the new private
representation and reject inconsistent combinations. Do not retain the boolean
inside the construction component. Any temporary compatibility path should be
marked for removal and covered by rejection tests.

### 3. Make request-time credential kind explicit

Update `RequestAuthorizer` or its constructor inputs so it knows whether bearer
or Codex credentials are expected.

Authorization must:

- Apply only the expected credential kind.
- Return a bounded, redacted configuration or authorization error for a kind
  mismatch.
- Avoid sending the request on mismatch.
- Preserve request-time credential refresh and per-retry authorization.
- Preserve the redaction behavior centralized by `HttpExchange`.

This change must not move credential resolution into protocol adapters.

### 4. Introduce one HTTP adapter factory

Create the private construction component, initially in `compiler.rs` or in a
new private `construction.rs` module. It should contain the only compiled-path
match from `HttpProtocol` to a concrete direct HTTP adapter.

Move into it:

- Protocol/auth compatibility checks.
- Adapter-specific static-auth mapping.
- Authorizer selection.
- Responses request-kind derivation.
- Concrete adapter construction.
- The compiled HTTP provider dispatch enum if one is retained.

The factory accepts a resolved URL and caller-supplied client. It does not
resolve `EndpointSpec`, initialize AWS state, or build clients.

### 5. Route `ProviderCompiler::compile_http` through the factory

Reduce `compile_http` to:

1. Resolve and validate the endpoint.
2. Select the compiler's HTTPS or direct-loopback shared client.
3. Translate the recipe into the coherent internal construction spec.
4. Delegate to the adapter factory.

Delete the separate protocol auth helpers such as `responses_auth`,
`chat_completions_auth`, `anthropic_auth`, and `google_auth` once their behavior
is represented by the construction component.

Run compiler interface tests after each protocol migrates. Migrate one protocol
at a time if that keeps review and failures localized.

### 6. Route Mantle through the same factory

Keep Mantle's lazy initialization sequence, but after resolving region,
endpoint, and API-key versus SigV4 intent, call the shared adapter factory.

Delete `mantle::build_provider` and either:

- Replace `MantleProvider` with the shared compiled HTTP provider type; or
- Move the dispatch type into the construction component and use it from both
  paths.

Consolidate repeated Mantle supported-protocol checks so the same policy is not
implemented by `Mantle::new`, provider construction, and endpoint generation.

Preserve these behavioral properties:

- Initialization remains lazy.
- Failed initialization remains retryable where it is retryable today.
- Concurrent initialization remains bounded and shared.
- The warm `stream` path remains synchronous and does not initialize again.
- Mantle uses the client supplied by `ProviderCompiler`.
- Existing benchmark intent in `benches/provider_compiler.rs` remains valid.

### 7. Narrow bypass-capable adapter constructors

After compiler and Mantle use the factory:

- Remove the independent Responses shape boolean from
  `OpenAi::with_client_and_authorizer`.
- Derive request kind before entering the adapter or use a private coherent
  Responses construction value.
- Narrow low-level constructor visibility where doing so does not affect the
  standalone public API.
- Ensure no compiled caller can independently provide static auth, request-time
  authorizer, and request shape.

Do not remove public standalone constructors as incidental cleanup.

### 8. Finish interface-level construction coverage

Complete the construction-relevant portion of the architecture review's
`test-surface` recommendation:

- Reuse one loopback fixture across compiler and adapter integration tests.
- Give every direct HTTP protocol at least one full compile-and-stream test.
- Exercise Mantle construction through the same factory.
- Keep decoder state-machine, malformed event, usage accounting, and
  protocol-error tests local to adapters.
- Remove duplicated private request readers when their tests can use the shared
  fixture without losing clarity.

This plan does not require every adapter test to enter through the compiler. The
interface is the test surface for composition; adapter internals remain the test
surface for protocol behavior.

### 9. Delete obsolete construction paths

Once all tests pass, remove:

- `codex_responses` and equivalent request-shape booleans.
- Mantle's protocol-to-adapter construction match.
- Duplicated protocol-specific auth conversion helpers.
- Any duplicate compiled provider dispatch enum.
- Crate-private constructors used only by the old compiled paths.
- Obsolete test helpers replaced by the shared loopback fixture.

Search the repository to verify there is one compiled protocol-to-adapter match
and no caller can independently combine Codex request authorization with a
standard Responses request kind.

## Test Strategy

### Compatibility tests

Use table-driven unit tests for the construction compatibility matrix. Every
supported pair should construct, and every unsupported pair should return a
configuration error before any request can be sent.

Tests must avoid including secrets in `Debug`, `Display`, or error output.

### Compiler interface tests

Use loopback HTTP fixtures to compile and stream through the public provider
seam. Assert:

- Exact path and query behavior.
- Protocol-specific authentication headers.
- Static custom headers.
- Standard versus Codex Responses body fields.
- Request-time credentials being loaded for each request attempt.
- The expected provider events from a minimal valid response.

At least one test should combine construction with `HttpExchange` retry to prove
request-time authorization still runs per attempt after the seam changes.

### Mantle tests

Preserve and adapt tests for:

- API-key construction for all supported protocols.
- SigV4 construction without leaking signing into adapters.
- Unsupported Google protocol rejection.
- Lazy initialization.
- Retry after initialization failure.
- Concurrent initialization.
- Warm dispatch.

Use injected clients and deterministic credential providers. No test should
require live AWS or provider access.

### Adapter tests

Retain protocol-local tests for:

- Serialization details beyond the construction contract.
- SSE decoding and fragmented events.
- Error-envelope parsing.
- Stream and output limits.
- Usage and terminal event rules.

These tests should not reimplement compiler composition assertions.

## Validation

Run focused checks throughout:

```sh
cargo test -p qq-provider compiler
cargo test -p qq-provider mantle
cargo test -p qq-provider request_auth
cargo test -p qq-provider
```

At completion run workspace validation:

```sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

If the repository's standard validation differs, use the canonical project
commands while retaining equivalent provider, workspace, formatting, and lint
coverage.

## Acceptance Criteria

- `ProviderCompiler` and Mantle use the same internal HTTP adapter factory.
- There is one compiled-path match from HTTP protocol to concrete adapter.
- Invalid protocol/authorization combinations fail before network I/O.
- Codex request shape cannot be selected independently from Codex
  authorization intent.
- No `codex_responses` or equivalent request-shape boolean remains in the
  compiled recipe or adapter constructor path.
- Request-time bearer and Codex credential-kind mismatches fail without sending
  a request.
- Request-time authorization continues to run on every `HttpExchange` retry.
- Every eagerly compiled HTTP adapter uses a compiler-owned shared client.
- Mantle uses its supplied compiler client after lazy initialization.
- Mantle retains lazy, retryable initialization and its warm-stream behavior.
- Google base and exact endpoint behavior is unchanged.
- Existing wire shapes, protocol stream behavior, limits, retries, and error
  classification remain unchanged except for intentional early rejection of
  invalid construction states.
- Public standalone adapter constructors remain source-compatible unless an API
  change is separately approved.
- Construction composition is tested through
  `ProviderCompiler::compile -> Provider::stream` for every direct HTTP
  protocol.
- A shared loopback fixture replaces duplicated transport helpers where useful,
  while protocol-local state-machine tests remain local.
- Provider and workspace tests pass, formatting is clean, and Clippy reports no
  warnings.

## Out of Scope

- Changing provider wire formats or SSE state machines.
- Changing HTTP retry policy, limits, timeout policy, or endpoint safety rules.
- Mid-stream retries.
- Moving runtime configuration parsing or credential storage into the provider
  crate.
- Redesigning provider cache keys.
- Combining native Bedrock SDK construction with HTTP adapter construction.
- Consolidating all AWS implementation; that belongs to the later
  `aws-locality` work.
- Rearranging adapter files or introducing an adapter directory solely for
  layout; that belongs to the later `file-layout` work.
- Forcing external users of standalone concrete adapters through
  `ProviderCompiler`.
- Moving every protocol-local test through the compiler.

## Follow-Up Order

After this plan is complete:

1. Finish any remaining `test-surface` fixture consolidation and interface
   coverage that is not necessary for construction.
2. Reassess `aws-locality` against the new construction boundary.
3. Perform `file-layout` changes only after those deeper module boundaries have
   stabilized.
