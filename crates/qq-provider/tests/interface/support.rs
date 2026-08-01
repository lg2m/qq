use std::sync::Arc;

use futures_util::StreamExt;
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest, Provider,
    ProviderCompiler, ProviderError, ProviderErrorKind, ProviderRecipe,
    test_support::LoopbackServer,
};

/// Compiles an HTTP recipe against a loopback base URL, exactly as runtime
/// configuration does.
pub fn compile_http(base_url: String, protocol: HttpProtocol, auth: HttpAuth) -> Arc<dyn Provider> {
    ProviderCompiler::new()
        .expect("compiler must construct")
        .compile(ProviderRecipe::http(HttpProviderRecipe::new(
            EndpointSpec::base(base_url, true),
            protocol,
            auth,
        )))
        .expect("loopback recipe must compile")
}

/// Streams one request and asserts the provider surfaces a 401 rejection as
/// an authentication-classified API error.
pub async fn assert_401_classifies_as_authentication(
    protocol: HttpProtocol,
    base_path: &str,
    error_body: &str,
) {
    let server = LoopbackServer::respond(401, "application/json", error_body);
    let provider = compile_http(
        format!("{}{base_path}", server.base_url),
        protocol,
        HttpAuth::ApiKey("interface-test-secret".into()),
    );

    let events = provider
        .stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            64,
        ))
        .collect::<Vec<_>>()
        .await;

    let [Err(error)] = &events[..] else {
        panic!("{protocol:?} must yield exactly one error event: {events:?}");
    };
    assert!(
        matches!(error, ProviderError::Api { status: 401, .. }),
        "{protocol:?} must preserve the 401 status: {error:?}"
    );
    assert_eq!(
        error.kind(),
        ProviderErrorKind::Authentication,
        "{protocol:?} must classify 401 as an authentication failure"
    );
    server.capture();
}

/// Streams a response whose output text exceeds the smallest output byte
/// limit and asserts the stream ends in a protocol error instead of
/// buffering without bound.
pub async fn assert_output_limit_ends_the_stream(protocol: HttpProtocol, sse_body: String) {
    let server = LoopbackServer::sse(sse_body);
    let provider = compile_http(
        server.base_url.clone(),
        protocol,
        HttpAuth::ApiKey("interface-test-secret".into()),
    );

    // max_output_tokens = 1 clamps the output budget to its 64 KiB floor.
    let events = provider
        .stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            1,
        ))
        .collect::<Vec<_>>()
        .await;

    let Some(Err(error)) = events.last() else {
        panic!("{protocol:?} must end the stream with an error: {events:?}");
    };
    assert!(
        matches!(error, ProviderError::Protocol(message)
            if message.contains("exceeded the configured size limit")),
        "{protocol:?} must report the exceeded limit: {error:?}"
    );
    server.capture();
}

/// A single output-text payload one byte past the 64 KiB output floor.
pub fn oversized_text() -> String {
    "a".repeat(64 * 1_024 + 1)
}
