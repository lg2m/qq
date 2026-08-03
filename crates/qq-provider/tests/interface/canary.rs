use futures_util::StreamExt;
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    ProviderCompiler, ProviderError, ProviderRecipe, test_support::LoopbackServer,
};

#[tokio::test]
async fn facade_disables_retries_for_every_http_protocol() {
    for protocol in [
        HttpProtocol::OpenAiResponses,
        HttpProtocol::OpenAiChatCompletions,
        HttpProtocol::AnthropicMessages,
        HttpProtocol::GoogleGenerateContent,
    ] {
        let server = LoopbackServer::respond(503, "application/json", "{}");
        let provider = ProviderCompiler::new()
            .expect("compiler must construct")
            .compile_for_canary(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(server.base_url.clone(), true),
                protocol,
                HttpAuth::ApiKey("interface-test-secret".into()),
            )))
            .expect("canary recipe must compile");

        let events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(
            matches!(&events[..], [Err(ProviderError::Api { status: 503, .. })]),
            "{protocol:?} canary must surface the first 503 without retrying: {events:?}"
        );
        server.capture();
    }
}
