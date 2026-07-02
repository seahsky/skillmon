use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum CountTokensError {
    #[error("count_tokens request failed: {0}")]
    Request(String),
    #[error("count_tokens returned HTTP {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },
    #[error("count_tokens response body was not valid JSON: {0}")]
    MalformedResponse(String),
}

/// The exact tier of ADR 0006: a live call to
/// `POST /v1/messages/count_tokens`. A failed call is treated identically to
/// "no key configured" by the caller (`footprint::compute`) -- both fall
/// back to the calibrated estimate.
pub trait CountTokensClient {
    fn count_tokens(&self, text: &str, model_id: &str, api_key: &str) -> Result<u32, CountTokensError>;
}

#[derive(Serialize)]
struct CountTokensRequest<'a> {
    model: &'a str,
    messages: Vec<MessageParam<'a>>,
}

#[derive(Serialize)]
struct MessageParam<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct CountTokensResponse {
    input_tokens: u32,
}

pub struct AnthropicCountTokensClient {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl AnthropicCountTokensClient {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// `base_url` is overridable so tests can point at a `mockito` server
    /// instead of the real Anthropic API.
    pub fn with_base_url(base_url: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client should always build with static config");
        Self { base_url: base_url.to_string(), client }
    }
}

impl Default for AnthropicCountTokensClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CountTokensClient for AnthropicCountTokensClient {
    fn count_tokens(&self, text: &str, model_id: &str, api_key: &str) -> Result<u32, CountTokensError> {
        let url = format!("{}/v1/messages/count_tokens", self.base_url);
        let body =
            CountTokensRequest { model: model_id, messages: vec![MessageParam { role: "user", content: text }] };

        let response = self
            .client
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .map_err(|e| CountTokensError::Request(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().unwrap_or_default();
            return Err(CountTokensError::UnexpectedStatus { status: status.as_u16(), body: body_text });
        }

        let parsed: CountTokensResponse = response.json().map_err(|e| CountTokensError::MalformedResponse(e.to_string()))?;

        Ok(parsed.input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_response_parses_input_tokens() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/v1/messages/count_tokens")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"input_tokens": 2095}"#)
            .create();

        let client = AnthropicCountTokensClient::with_base_url(&server.url());
        let result = client.count_tokens("Hello, world", "claude-sonnet-5", "sk-ant-test").unwrap();

        assert_eq!(result, 2095);
        mock.assert();
    }

    #[test]
    fn non_success_status_returns_unexpected_status_error() {
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/v1/messages/count_tokens")
            .with_status(401)
            .with_body(r#"{"error": {"type": "authentication_error", "message": "invalid x-api-key"}}"#)
            .create();

        let client = AnthropicCountTokensClient::with_base_url(&server.url());
        let err = client.count_tokens("Hello, world", "claude-sonnet-5", "bad-key").unwrap_err();

        assert!(matches!(err, CountTokensError::UnexpectedStatus { status: 401, .. }), "got {err:?}");
    }

    #[test]
    fn malformed_json_body_returns_malformed_response_error() {
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/v1/messages/count_tokens")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json")
            .create();

        let client = AnthropicCountTokensClient::with_base_url(&server.url());
        let err = client.count_tokens("Hello, world", "claude-sonnet-5", "sk-ant-test").unwrap_err();

        assert!(matches!(err, CountTokensError::MalformedResponse(_)), "got {err:?}");
    }

    #[test]
    fn unreachable_server_returns_request_error() {
        // Port 0 never accepts a connection -- guaranteed unreachable, no live server needed.
        let client = AnthropicCountTokensClient::with_base_url("http://127.0.0.1:0");
        let err = client.count_tokens("Hello, world", "claude-sonnet-5", "sk-ant-test").unwrap_err();

        assert!(matches!(err, CountTokensError::Request(_)), "got {err:?}");
    }
}
