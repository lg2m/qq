use qq_provider::HttpProtocol;

use crate::support::{
    assert_401_classifies_as_authentication, assert_output_limit_ends_the_stream, oversized_text,
};

#[tokio::test]
async fn classifies_a_401_rejection_as_an_authentication_failure() {
    assert_401_classifies_as_authentication(
        HttpProtocol::AnthropicMessages,
        "/v1",
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid key"}}"#,
    )
    .await;
}

#[tokio::test]
async fn enforces_the_output_byte_limit_end_to_end() {
    assert_output_limit_ends_the_stream(
        HttpProtocol::AnthropicMessages,
        format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_delta\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
            ),
            oversized_text()
        ),
    )
    .await;
}
