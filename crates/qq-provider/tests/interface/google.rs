use qq_provider::HttpProtocol;

use crate::support::{
    assert_401_classifies_as_authentication, assert_output_limit_ends_the_stream, oversized_text,
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
