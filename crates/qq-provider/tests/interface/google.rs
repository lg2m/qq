use qq_provider::HttpProtocol;

use crate::support::{
    assert_401_classifies_as_authentication, assert_output_limit_ends_the_stream,
    assert_output_truncation_is_typed, oversized_text,
};

#[tokio::test]
async fn classifies_a_401_rejection_as_an_authentication_failure() {
    assert_401_classifies_as_authentication(
        HttpProtocol::GoogleGenerateContent,
        "",
        r#"{"error":{"message":"invalid key","status":"UNAUTHENTICATED"}}"#,
    )
    .await;
}

#[tokio::test]
async fn enforces_the_output_byte_limit_end_to_end() {
    assert_output_limit_ends_the_stream(
        HttpProtocol::GoogleGenerateContent,
        format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\"}}]}},\"index\":0}}]}}\n\n",
            oversized_text()
        ),
    )
    .await;
}

#[tokio::test]
async fn types_an_output_token_truncation_end_to_end() {
    assert_output_truncation_is_typed(
        HttpProtocol::GoogleGenerateContent,
        concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"MAX_TOKENS\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":64,\"totalTokenCount\":69}}\n\n",
        )
        .to_owned(),
        Some(64),
    )
    .await;
}
