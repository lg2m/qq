use qq_provider::HttpProtocol;

use crate::support::{
    assert_401_classifies_as_authentication, assert_output_limit_ends_the_stream,
    assert_output_truncation_is_typed, oversized_text,
};

#[tokio::test]
async fn classifies_a_401_rejection_as_an_authentication_failure() {
    assert_401_classifies_as_authentication(
        HttpProtocol::OpenAiResponses,
        "/v1",
        r#"{"error":{"message":"invalid key"}}"#,
    )
    .await;
}

#[tokio::test]
async fn enforces_the_output_byte_limit_end_to_end() {
    assert_output_limit_ends_the_stream(
        HttpProtocol::OpenAiResponses,
        format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
            oversized_text()
        ),
    )
    .await;
}

#[tokio::test]
async fn types_an_output_token_truncation_end_to_end() {
    assert_output_truncation_is_typed(
        HttpProtocol::OpenAiResponses,
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":64}}}\n\n",
        )
        .to_owned(),
        Some(64),
    )
    .await;
}
