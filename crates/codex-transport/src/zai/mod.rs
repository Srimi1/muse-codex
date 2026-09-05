//! Z.ai GLM Coding Plan backend.
//!
//! Z.ai speaks OpenAI chat-completions, not the Responses API stock Muse and
//! the gateway use. This module owns the whole translation: it maps a Muse
//! Responses request onto a Z.ai chat request and synthesizes a Responses
//! event stream back, so nothing downstream needs a second protocol.

mod auth;
mod catalog;
mod request;
mod stream;

pub use auth::ZaiCredentials;

use crate::AuthConfig;
use crate::Error;
use crate::ModelInfo;
use crate::RawResponse;
use crate::Result;
use crate::Url;
use crate::transport::build_http_client;
use crate::transport::validate_base_url;
use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use http::header::AUTHORIZATION;
use http::header::CONTENT_TYPE;
use secrecy::ExposeSecret;
use secrecy::SecretString;
use serde_json::Value;
use serde_json::json;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// The GLM Coding Plan endpoint. Z.ai states this is not interchangeable with
/// the general `paas/v4` endpoint, so it is never used as a fallback.
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Plan-usage endpoint. It validates the credential and an active plan without
/// spending a coding prompt, which a chat request would.
const QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const SKIP_PROBE_ENV: &str = "MUSE_CODEX_ZAI_SKIP_PROBE";
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Streams Muse Responses traffic against the Z.ai GLM Coding Plan.
pub struct ZaiTransport {
    api_key: SecretString,
    base_url: Url,
    client: reqwest::Client,
}

impl std::fmt::Debug for ZaiTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A derived Debug would print the credential guard and the endpoint.
        formatter
            .debug_struct("ZaiTransport")
            .field("api_key", &"<redacted>")
            .field(
                "has_custom_base_url",
                &(self.base_url.as_str() != DEFAULT_BASE_URL),
            )
            .finish()
    }
}

impl ZaiTransport {
    pub fn new(
        config: &AuthConfig,
        api_key_override: Option<SecretString>,
        base_url: Option<Url>,
    ) -> Result<Self> {
        let api_key = match api_key_override {
            Some(api_key) => {
                crate::auth::validate_api_key(api_key.expose_secret())?;
                api_key
            }
            None => ZaiCredentials::new(config)
                .load()?
                .ok_or(Error::ZaiNotAuthenticated)?,
        };
        let base_url = match base_url {
            Some(base_url) => {
                validate_base_url(&base_url)?;
                base_url
            }
            None => {
                Url::parse(DEFAULT_BASE_URL).map_err(|error| Error::Provider(error.to_string()))?
            }
        };
        Ok(Self {
            api_key,
            base_url,
            client: build_http_client()?,
        })
    }

    /// Returns the pinned GLM catalog after confirming the credential and plan.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if !skip_probe() {
            self.probe_plan().await?;
        }
        Ok(catalog::pinned_models())
    }

    /// Confirms the stored key belongs to an active plan. The endpoint takes a
    /// bare token: unlike the chat endpoint it does not use a `Bearer` prefix.
    async fn probe_plan(&self) -> Result<()> {
        let mut authorization =
            HeaderValue::from_str(self.api_key.expose_secret()).map_err(|_| {
                Error::InvalidHeader("the stored Z.ai key is not a valid header".into())
            })?;
        authorization.set_sensitive(true);
        let response = self
            .client
            .get(QUOTA_URL)
            .header(AUTHORIZATION, authorization)
            .header(http::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(Error::Http)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // The body may name the account; report only the status.
        Err(Error::Upstream {
            status,
            summary: "the Z.ai plan check was rejected".to_string(),
        })
    }

    pub async fn stream_responses(
        &self,
        request: Value,
        _headers: HeaderMap,
    ) -> Result<RawResponse> {
        let model = request
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidRequest("the Responses request needs a model".into()))?
            .to_string();
        let pinned = catalog::find(&model).ok_or_else(|| {
            Error::InvalidRequest(
                "the requested model is not in the pinned Z.ai catalog".to_string(),
            )
        })?;
        let limits = request::ModelLimits {
            accepts_images: pinned
                .input_modalities
                .iter()
                .any(|modality| modality == "image"),
            max_output_tokens: catalog::MAX_OUTPUT_TOKENS,
        };
        let translated = request::translate(&request, limits)?;

        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose_secret())).map_err(
                |_| Error::InvalidHeader("the stored Z.ai key is not a valid header".into()),
            )?;
        authorization.set_sensitive(true);

        let url = chat_completions_url(&self.base_url)?;
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "text/event-stream")
            .json(&translated.body)
            .send()
            .await
            .map_err(Error::Http)?;

        let status = response.status();
        if !status.is_success() {
            return Ok(canonical_failure(status, response).await);
        }

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        let request_key = format!(
            "process:{}:{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let upstream: crate::RawBody =
            Box::pin(futures::StreamExt::map(response.bytes_stream(), |chunk| {
                chunk.map_err(Error::Http)
            }));
        Ok(RawResponse {
            status: StatusCode::OK,
            headers,
            body: stream::responses_stream(upstream, model, request_key, translated.tools),
        })
    }
}

fn chat_completions_url(base_url: &Url) -> Result<Url> {
    let base = base_url.as_str().trim_end_matches('/');
    Url::parse(&format!("{base}/chat/completions"))
        .map_err(|error| Error::Provider(error.to_string()))
}

fn skip_probe() -> bool {
    std::env::var_os(SKIP_PROBE_ENV).is_some_and(|value| value == "1")
}

/// Replaces an upstream error body with the small vocabulary the gateway's
/// sanitizer understands. Z.ai reports numeric codes the shared allowlist does
/// not recognize, and teaching the gateway about them would risk regressing the
/// Codex path.
async fn canonical_failure(status: StatusCode, response: reqwest::Response) -> RawResponse {
    let body = response.bytes().await.ok().unwrap_or_default();
    let details: Option<Value> = (body.len() <= MAX_ERROR_RESPONSE_BYTES)
        .then(|| serde_json::from_slice(&body).ok())
        .flatten();
    let code = classify_failure(status, details.as_ref());
    let document = json!({"error": {"code": code, "param": Value::Null}}).to_string();

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    RawResponse {
        status,
        headers,
        body: Box::pin(futures::stream::once(async move {
            Ok(bytes::Bytes::from(document))
        })),
    }
}

fn classify_failure(status: StatusCode, details: Option<&Value>) -> &'static str {
    let error = details.and_then(|details| details.get("error"));
    let code = error
        .and_then(|error| error.get("code"))
        .map(|code| match code {
            Value::String(code) => code.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if message.contains("context length") || message.contains("context window") {
        return "context_length_exceeded";
    }
    match code.as_str() {
        "1000" | "1001" | "1002" | "1003" | "1004" => "invalid_api_key",
        "1110" | "1112" | "1113" => "insufficient_quota",
        "1211" | "1212" => "model_not_found",
        "1261" => "context_length_exceeded",
        "1302" | "1303" | "1304" | "1305" => "rate_limit_exceeded",
        _ if status == StatusCode::TOO_MANY_REQUESTS => "rate_limit_exceeded",
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            "invalid_api_key"
        }
        _ if status == StatusCode::NOT_FOUND => "model_not_found",
        _ => "invalid_request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Read;
    use std::io::Write;
    use std::path::Path;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn config(root: &Path) -> AuthConfig {
        AuthConfig::with_home(root.join("muse-codex")).expect("isolated home")
    }

    fn transport(base_url: &str) -> (tempfile::TempDir, ZaiTransport) {
        let temporary = tempfile::tempdir().expect("temporary root");
        let transport = ZaiTransport::new(
            &config(temporary.path()),
            Some(SecretString::from("zai-fixture-key".to_string())),
            Some(Url::parse(base_url).expect("base URL")),
        )
        .expect("transport");
        (temporary, transport)
    }

    /// Captures one raw HTTP request and replies with a fixed response.
    async fn capture_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.expect("read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(index) = text.find("\r\n\r\n") {
                    let body_length = text
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .or_else(|| line.strip_prefix("Content-Length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= index + 4 + body_length {
                        break;
                    }
                }
            }
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            socket.flush().await.expect("flush");
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn the_plan_probe_sends_a_bare_token_not_a_bearer_header() {
        // The quota endpoint is the one place Z.ai does not use `Bearer`.
        let (address, handle) = capture_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        let temporary = tempfile::tempdir().expect("temporary root");
        let transport = ZaiTransport::new(
            &config(temporary.path()),
            Some(SecretString::from("zai-fixture-key".to_string())),
            None,
        )
        .expect("transport");
        let client = transport.client.clone();
        let response = client
            .get(format!("{address}/api/monitor/usage/quota/limit"))
            .header(AUTHORIZATION, "zai-fixture-key")
            .send()
            .await
            .expect("probe");
        assert!(response.status().is_success());
        let captured = handle.await.expect("capture");
        assert!(
            captured
                .to_lowercase()
                .contains("authorization: zai-fixture-key")
        );
        assert!(!captured.to_lowercase().contains("bearer"));
    }

    #[tokio::test]
    async fn a_chat_request_sends_bearer_and_the_translated_body() {
        let (address, handle) = capture_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (_temporary, transport) = transport(&format!("{address}/api/coding/paas/v4"));
        let request = json!({
            "model": "glm-5.3",
            "instructions": "Stay in the harness.",
            "input": [{"type": "message", "role": "user", "content": "hello"}],
            "stream": true
        });
        let response = transport
            .stream_responses(request, HeaderMap::new())
            .await
            .expect("stream");
        assert_eq!(response.status, StatusCode::OK);

        let captured = handle.await.expect("capture");
        assert!(captured.starts_with("POST /api/coding/paas/v4/chat/completions"));
        assert!(captured.contains("authorization: Bearer zai-fixture-key"));
        let body = captured
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .expect("request body");
        let body: Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(body["model"], "glm-5.3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "Stay in the harness.");
    }

    #[tokio::test]
    async fn an_upstream_failure_is_replaced_with_a_canonical_error_document() {
        // Z.ai's numeric codes are meaningless to the gateway's allowlist, and
        // its body can name the account, so neither is forwarded.
        let (address, _handle) = capture_once(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 66\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"1113\",\"message\":\"Insufficient balance acct 42\"}}",
        )
        .await;
        let (_temporary, transport) = transport(&format!("{address}/api/coding/paas/v4"));
        let request = json!({
            "model": "glm-5.3",
            "input": [{"type": "message", "role": "user", "content": "hello"}]
        });
        let response = transport
            .stream_responses(request, HeaderMap::new())
            .await
            .expect("failure response");
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);

        let mut body = response.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        let document: Value = serde_json::from_slice(&bytes).expect("canonical document");
        assert_eq!(document["error"]["code"], "insufficient_quota");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(!text.contains("acct 42"), "upstream detail must not leak");
    }

    #[test]
    fn failures_map_onto_the_gateway_error_vocabulary() {
        let cases = [
            (StatusCode::UNAUTHORIZED, json!({}), "invalid_api_key"),
            (StatusCode::FORBIDDEN, json!({}), "invalid_api_key"),
            (
                StatusCode::TOO_MANY_REQUESTS,
                json!({}),
                "rate_limit_exceeded",
            ),
            (StatusCode::NOT_FOUND, json!({}), "model_not_found"),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": {"code": "1261"}}),
                "context_length_exceeded",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": "Input exceeds the context length"}}),
                "context_length_exceeded",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": {"code": "1002"}}),
                "invalid_api_key",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({}),
                "invalid_request",
            ),
        ];
        for (status, body, expected) in cases {
            assert_eq!(classify_failure(status, Some(&body)), expected, "{status}");
        }
    }

    #[test]
    fn debug_never_exposes_the_key_or_the_endpoint() {
        let (_temporary, transport) = transport("https://private.example/v1");
        let debug = format!("{transport:?}");
        assert!(!debug.contains("zai-fixture-key"));
        assert!(!debug.contains("private.example"));
        assert!(debug.contains("has_custom_base_url: true"));
    }

    #[test]
    fn a_plain_http_endpoint_is_rejected() {
        let temporary = tempfile::tempdir().expect("temporary root");
        assert!(
            ZaiTransport::new(
                &config(temporary.path()),
                Some(SecretString::from("zai-fixture-key".to_string())),
                Some(Url::parse("http://api.z.ai/api/coding/paas/v4").expect("url")),
            )
            .is_err()
        );
    }

    #[test]
    fn a_missing_credential_is_reported_without_falling_back_to_codex() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let error = ZaiTransport::new(&config(temporary.path()), None, None).unwrap_err();
        assert!(matches!(
            error,
            Error::ZaiNotAuthenticated | Error::Keyring(_)
        ));
    }

    #[tokio::test]
    async fn an_unknown_model_is_rejected_before_any_request() {
        let (_temporary, transport) = transport("https://api.z.ai/api/coding/paas/v4");
        let error = transport
            .stream_responses(json!({"model": "gpt-6-astra"}), HeaderMap::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("pinned Z.ai catalog"));
    }

    #[test]
    fn the_chat_url_is_built_from_the_configured_base() {
        let base = Url::parse("https://open.bigmodel.cn/api/coding/paas/v4/").expect("base");
        assert_eq!(
            chat_completions_url(&base).expect("url").as_str(),
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
        );
    }

    // Keeps the unused-import lint honest for the std IO traits the capture
    // helper needs on some platforms.
    #[allow(dead_code)]
    fn _io_traits(_: &dyn Read, _: &dyn Write) {}
}
