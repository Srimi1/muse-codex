use crate::CODEX_WIRE_COMPATIBILITY_VERSION;
use crate::Error;
use crate::Result;
use crate::auth::AuthConfig;
use crate::auth::validate_api_key;
use async_trait::async_trait;
use bytes::Bytes;
use bytes::BytesMut;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::UnauthorizedRecovery;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::models::ResponseItem;
use futures::Stream;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use http::header::ACCEPT;
use http::header::AUTHORIZATION;
use http::header::CONTENT_TYPE;
use http::header::USER_AGENT;
use secrecy::ExposeSecret;
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use url::Url;

const MAX_MODELS_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_MODELS: usize = 4096;
const MAX_PRESTREAM_ATTEMPTS: usize = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2);
const CREDENTIAL_REFRESH_LOCK_NAME: &str = ".credential-refresh.lock";
const MUSE_CODEX_USER_AGENT: &str = concat!("muse-codex/", env!("CARGO_PKG_VERSION"));
const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
const CODEX_ROUTING_HINT_HEADER: &str = "x-codex-routing-hint";
const BUNDLED_MODEL_CATALOG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../vendor/openai-codex/codex-rs/models-manager/models.json"
));
static BROWSER_OPEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static RESPONSES_LITE_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub type RawBody = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

#[derive(Clone, Copy)]
struct JsonRequestOptions<'a> {
    accept: &'static str,
    use_responses_lite: bool,
    routing_hint: Option<&'a HeaderValue>,
}

/// An upstream response whose bytes have not been interpreted by this crate.
///
/// The gateway remains responsible for removing hop-by-hop response headers
/// before writing this response to its downstream client.
pub struct RawResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: RawBody,
}

impl fmt::Debug for RawResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .field("headers", &"<redacted>")
            .field("body", &"<stream>")
            .finish()
    }
}

impl RawResponse {
    fn from_reqwest(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::Http));
        Self {
            status,
            headers,
            body: Box::pin(body),
        }
    }
}

/// Stable, serde-friendly model metadata for the gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supported_reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub is_visible: bool,
    pub is_default: bool,
    /// Provider-wire capability used internally and omitted from Muse's stable
    /// readiness/catalog format.
    #[serde(skip)]
    pub use_responses_lite: bool,
    /// Upstream execution selector used to fail closed on unknown wire modes.
    #[serde(skip)]
    pub tool_mode: Option<String>,
    /// Authenticated input capabilities. Muse's stable catalog has no field
    /// for these, so they remain private to request validation.
    #[serde(skip)]
    pub input_modalities: Vec<String>,
    /// Whether the bundled Codex metadata permits API-key use. The public
    /// readiness schema intentionally omits this provider-only selector.
    #[serde(skip)]
    pub supported_in_api: bool,
    /// Whether the authenticated model catalog permits OpenAI Fast mode.
    /// This provider-only capability never enters Muse's stable model schema.
    #[serde(skip)]
    pub supports_fast_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelCapabilities {
    is_visible: bool,
    use_responses_lite: bool,
    tool_mode: Option<String>,
    input_modalities: Vec<String>,
    supported_reasoning_efforts: Vec<String>,
    supports_fast_mode: bool,
}

/// Muse's compact search request. It is mapped to Codex's pinned
/// `POST /alpha/search` request shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub request_id: String,
    pub iteration_index: u64,
}

impl SearchRequest {
    fn to_upstream(&self) -> Result<Value> {
        if self.query.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "search query cannot be empty".to_string(),
            ));
        }
        if self.request_id.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "search request_id cannot be empty".to_string(),
            ));
        }
        Ok(serde_json::json!({
            "id": format!("{}:{}", self.request_id, self.iteration_index),
            "input": self.query,
        }))
    }
}

/// Provider-only transport backed by the pinned OpenAI Codex login stack.
#[derive(Clone)]
pub struct Transport {
    config: AuthConfig,
    api_key_override: Option<SecretString>,
    api_key_base_url: Option<Url>,
    auth_manager: Option<Arc<AuthManager>>,
    client: reqwest::Client,
    model_capabilities: Arc<RwLock<BTreeMap<String, ModelCapabilities>>>,
    fast_mode: bool,
}

impl fmt::Debug for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Transport")
            .field("auth_home", &"<redacted>")
            .field("has_api_key_override", &self.api_key_override.is_some())
            .field("has_custom_base_url", &self.api_key_base_url.is_some())
            .field("fast_mode", &self.fast_mode)
            .finish_non_exhaustive()
    }
}

impl Transport {
    /// Creates a transport. Passing `api_key_override` avoids keyring access and
    /// is intended for a one-process override supplied by the launcher.
    pub async fn new(
        config: AuthConfig,
        api_key_override: Option<SecretString>,
        api_key_base_url: Option<Url>,
        fast_mode: bool,
    ) -> Result<Self> {
        if let Some(api_key) = api_key_override.as_ref() {
            validate_api_key(api_key.expose_secret())?;
        }
        if let Some(base_url) = api_key_base_url.as_ref() {
            validate_base_url(base_url)?;
        }

        let auth_manager = if api_key_override.is_some() {
            None
        } else {
            Some(config.manager().await)
        };
        let transport = Self {
            config,
            api_key_override,
            api_key_base_url,
            auth_manager,
            client: build_http_client()?,
            model_capabilities: Arc::new(RwLock::new(BTreeMap::new())),
            fast_mode,
        };

        // Fail during construction rather than after the gateway begins
        // accepting requests.
        transport.resolve_auth().await?;
        Ok(transport)
    }

    /// Fetches and normalizes the pinned Codex `/models` response.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let use_bundled_api_metadata =
            self.api_key_base_url.is_none() && self.resolve_auth().await?.is_api_key_auth();
        let response = self
            .execute_with_401_recovery(|| async { self.send_models_once().await })
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(upstream_error(response).await);
        }
        let bytes = collect_response(response, MAX_MODELS_RESPONSE_BYTES).await?;
        let models = if use_bundled_api_metadata {
            normalize_api_models_with_bundled_metadata(&bytes)?
        } else {
            normalize_models(&bytes)?
        };
        let capabilities = models
            .iter()
            .map(|model| {
                (
                    model.id.clone(),
                    ModelCapabilities {
                        is_visible: model.is_visible,
                        use_responses_lite: model.use_responses_lite,
                        tool_mode: model.tool_mode.clone(),
                        input_modalities: model.input_modalities.clone(),
                        supported_reasoning_efforts: model.supported_reasoning_efforts.clone(),
                        supports_fast_mode: model.supports_fast_mode,
                    },
                )
            })
            .collect();
        *self
            .model_capabilities
            .write()
            .map_err(|_| Error::Provider("model capability state is unavailable".to_string()))? =
            capabilities;
        Ok(models)
    }

    /// Forwards a Responses API JSON body and preserves upstream response bytes,
    /// including SSE framing and chunk contents.
    pub async fn stream_responses(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<RawResponse> {
        let api_key_auth = self.resolve_auth().await?.is_api_key_auth();
        let capabilities = self.validate_model_request(&body, api_key_auth)?;
        validate_fast_mode_availability(
            self.fast_mode,
            self.api_key_base_url.is_some(),
            capabilities.as_ref(),
        )?;
        let use_responses_lite = capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.use_responses_lite);
        let lite_request_namespace =
            use_responses_lite.then(|| responses_lite_request_namespace(&extra_headers));
        let mut body = body;
        apply_fast_service_tier(&mut body, self.fast_mode)?;
        let body = normalize_responses_request(
            body,
            api_key_auth,
            use_responses_lite,
            lite_request_namespace.as_deref(),
        )?;
        let routing_hint = if api_key_auth {
            None
        } else {
            Some(subscription_routing_hint(&body)?)
        };
        let response = self
            .execute_with_401_recovery(|| async {
                self.send_json_once(
                    Method::POST,
                    "responses",
                    &body,
                    &extra_headers,
                    JsonRequestOptions {
                        accept: "text/event-stream",
                        use_responses_lite,
                        routing_hint: routing_hint.as_ref(),
                    },
                )
                .await
            })
            .await?;
        Ok(RawResponse::from_reqwest(response))
    }

    fn validate_model_request(
        &self,
        body: &Value,
        api_key_auth: bool,
    ) -> Result<Option<ModelCapabilities>> {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidRequest("model must be a string".to_string()))?;
        let capabilities = self
            .model_capabilities
            .read()
            .map_err(|_| Error::Provider("model capability state is unavailable".to_string()))?
            .get(model)
            .cloned();
        let Some(capabilities) = capabilities else {
            if api_key_auth {
                // Public and compatible custom Responses endpoints can accept a model
                // that their compact /models response did not describe. In that case
                // retain the standard Responses request shape. Subscription requests
                // must remain bound to the authenticated Codex catalog.
                return Ok(None);
            }
            return Err(Error::InvalidRequest(
                "model is not available in the authenticated Codex catalog".to_string(),
            ));
        };
        if !capabilities.is_visible {
            return Err(Error::InvalidRequest(
                "model is not available for interactive use".to_string(),
            ));
        }
        if capabilities.tool_mode.as_deref() == Some("code_mode_only")
            && !capabilities.use_responses_lite
        {
            return Err(Error::InvalidRequest(
                "model requires the Responses Lite tool protocol".to_string(),
            ));
        }
        if request_contains_image(body)
            && !capabilities
                .input_modalities
                .iter()
                .any(|modality| modality == "image")
        {
            return Err(Error::InvalidRequest(
                "model does not accept image input".to_string(),
            ));
        }
        if let Some(effort) = body
            .get("reasoning")
            .and_then(Value::as_object)
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
            && !capabilities.supported_reasoning_efforts.is_empty()
            && !capabilities
                .supported_reasoning_efforts
                .iter()
                .any(|supported| supported == effort)
        {
            return Err(Error::InvalidRequest(
                "reasoning effort is not supported by the selected model".to_string(),
            ));
        }
        Ok(Some(capabilities))
    }

    /// Executes Codex's raw search endpoint. The encrypted search output is not
    /// parsed or decrypted here; Muse remains the protocol owner.
    pub async fn search(
        &self,
        request: SearchRequest,
        extra_headers: HeaderMap,
    ) -> Result<RawResponse> {
        let body = request.to_upstream()?;
        let response = self
            .execute_with_401_recovery(|| async {
                self.send_json_once(
                    Method::POST,
                    "alpha/search",
                    &body,
                    &extra_headers,
                    JsonRequestOptions {
                        accept: "application/json",
                        use_responses_lite: false,
                        routing_hint: None,
                    },
                )
                .await
            })
            .await?;
        Ok(RawResponse::from_reqwest(response))
    }

    /// Accepts the stock Muse `/muse-code/search` JSON contract.
    pub async fn search_muse_request(
        &self,
        request: Value,
        extra_headers: HeaderMap,
    ) -> Result<RawResponse> {
        let request: SearchRequest = serde_json::from_value(request).map_err(|error| {
            Error::InvalidRequest(format!("invalid Muse search request: {error}"))
        })?;
        self.search(request, extra_headers).await
    }

    /// Maps Muse's browser-open request to the pinned Codex search `open`
    /// command. Codex treats URLs as open `ref_id` values.
    pub async fn browser_open_muse_request(
        &self,
        request: Value,
        extra_headers: HeaderMap,
    ) -> Result<RawResponse> {
        let object = request.as_object().ok_or_else(|| {
            Error::InvalidRequest("Muse browser_open request must be an object".to_string())
        })?;
        let url = object
            .get("url")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "Muse browser_open request must contain a non-empty url".to_string(),
                )
            })?;
        let parsed_url = Url::parse(url)
            .map_err(|error| Error::InvalidRequest(format!("invalid browser_open URL: {error}")))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(Error::InvalidRequest(
                "browser_open URL must use http or https".to_string(),
            ));
        }

        let request_id = object
            .get("request_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                extra_headers
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| {
                format!(
                    "muse-browser-open-{}-{}",
                    std::process::id(),
                    BROWSER_OPEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                )
            });
        let body = serde_json::json!({
            "id": request_id,
            "commands": {
                "open": [{"ref_id": parsed_url.as_str()}]
            }
        });
        let response = self
            .execute_with_401_recovery(|| async {
                self.send_json_once(
                    Method::POST,
                    "alpha/search",
                    &body,
                    &extra_headers,
                    JsonRequestOptions {
                        accept: "application/json",
                        use_responses_lite: false,
                        routing_hint: None,
                    },
                )
                .await
            })
            .await?;
        Ok(RawResponse::from_reqwest(response))
    }

    async fn send_models_once(&self) -> Result<reqwest::Response> {
        let (mut url, headers) = self
            .prepare_request("models", &HeaderMap::new(), "application/json")
            .await?;
        url.query_pairs_mut()
            .append_pair("client_version", CODEX_WIRE_COMPATIBILITY_VERSION);
        self.client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(Error::Http)
    }

    async fn send_json_once(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        extra_headers: &HeaderMap,
        options: JsonRequestOptions<'_>,
    ) -> Result<reqwest::Response> {
        let (url, mut headers) = self
            .prepare_request(path, extra_headers, options.accept)
            .await?;
        set_internal_responses_lite_header(&mut headers, options.use_responses_lite);
        set_internal_routing_hint_header(&mut headers, options.routing_hint);
        self.client
            .request(method, url)
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(Error::Http)
    }

    async fn prepare_request(
        &self,
        path: &str,
        extra_headers: &HeaderMap,
        accept: &'static str,
    ) -> Result<(Url, HeaderMap)> {
        let auth = self.resolve_auth().await?;
        let custom_base = if auth.is_api_key_auth() {
            self.api_key_base_url.as_ref().map(ToString::to_string)
        } else {
            None
        };
        let provider = ModelProviderInfo::create_openai_provider(custom_base)
            .to_api_provider(Some(auth.auth_mode()))
            .map_err(|error| Error::Provider(error.to_string()))?;
        let url = Url::parse(&provider.url_for_path(path))
            .map_err(|error| Error::Provider(error.to_string()))?;

        let mut headers = provider.headers.clone();
        merge_safe_request_headers(&mut headers, extra_headers);
        headers.insert(
            HeaderName::from_static("version"),
            HeaderValue::from_static(CODEX_WIRE_COMPATIBILITY_VERSION),
        );
        attach_auth_headers(&mut headers, &auth)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        let upstream_agent = codex_login::default_client::get_codex_user_agent();
        let user_agent = HeaderValue::from_str(&format!(
            "{MUSE_CODEX_USER_AGENT} wire/{CODEX_WIRE_COMPATIBILITY_VERSION} {upstream_agent}"
        ))
        .map_err(|_| Error::InvalidHeader("generated user-agent".to_string()))?;
        headers.insert(USER_AGENT, user_agent);
        Ok((url, headers))
    }

    async fn resolve_auth(&self) -> Result<CodexAuth> {
        let auth = if let Some(api_key) = self.api_key_override.as_ref() {
            CodexAuth::from_api_key(api_key.expose_secret())
        } else {
            self.auth_manager
                .as_ref()
                .expect("auth manager exists without an override")
                .auth()
                .await
                .ok_or(Error::NotAuthenticated)?
        };

        if !auth.is_api_key_auth() && !auth.is_chatgpt_auth() {
            return Err(Error::UnsupportedAuthMode);
        }
        if self.api_key_base_url.is_some() && !auth.is_api_key_auth() {
            return Err(Error::CustomBaseUrlRequiresApiKey);
        }
        Ok(auth)
    }

    fn recovery(&self) -> Option<Box<dyn Recovery>> {
        self.auth_manager.as_ref().map(|manager| {
            Box::new(CodexRecovery(manager.unauthorized_recovery())) as Box<dyn Recovery>
        })
    }

    async fn execute_with_401_recovery<F, Fut>(&self, send: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<reqwest::Response>>,
    {
        let lock_home = self
            .auth_manager
            .as_ref()
            .map(|_| self.config.home().to_path_buf());
        execute_with_401_recovery(send, self.recovery(), lock_home).await
    }
}

fn request_contains_image(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| {
                        content.iter().any(|part| {
                            matches!(
                                part.get("type").and_then(Value::as_str),
                                Some("input_image" | "image_url")
                            )
                        })
                    })
                    || item
                        .get("output")
                        .and_then(Value::as_array)
                        .is_some_and(|output| {
                            output.iter().any(|part| {
                                matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("input_image" | "image_url")
                                )
                            })
                        })
            })
        })
}

pub(crate) fn build_http_client() -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .default_headers(codex_login::default_client::default_headers())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(150))
        .redirect(reqwest::redirect::Policy::none());
    let builder = codex_http_client::with_chatgpt_cloudflare_cookie_store(builder);
    codex_http_client::build_reqwest_client_with_custom_ca(builder)
        .map_err(|error| Error::HttpClient(error.to_string()))
}

fn validate_fast_mode_availability(
    enabled: bool,
    custom_base_url: bool,
    capabilities: Option<&ModelCapabilities>,
) -> Result<()> {
    if !enabled || custom_base_url || capabilities.is_none() {
        return Ok(());
    }
    if capabilities.is_some_and(|capabilities| capabilities.supports_fast_mode) {
        return Ok(());
    }
    Err(Error::FastModeUnavailable)
}

/// The launcher owns the service-tier selection. Always remove any value that
/// arrived from stock Muse, then add the pinned Codex request spelling only
/// when Fast was explicitly selected for this process.
fn apply_fast_service_tier(body: &mut Value, enabled: bool) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::InvalidRequest("request body must be an object".to_string()))?;
    object.remove("service_tier");
    if enabled {
        object.insert(
            "service_tier".to_string(),
            Value::String(ServiceTier::Fast.request_value().to_string()),
        );
    }
    Ok(())
}

/// ChatGPT's Codex backend uses this non-secret hint alongside the canonical
/// Responses body. API-key requests intentionally omit it, matching upstream.
fn subscription_routing_hint(body: &Value) -> Result<HeaderValue> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRequest("model must be a string".to_string()))?;
    let hint = match body.get("service_tier").and_then(Value::as_str) {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    };
    HeaderValue::from_str(&hint)
        .map_err(|_| Error::InvalidHeader("generated Codex routing hint".to_string()))
}

/// Converts Muse's provider-neutral Responses body into the stateless request
/// shape emitted by the pinned Codex client. Public API-key requests retain
/// standard API parameters that the ChatGPT subscription backend does not
/// accept.
fn normalize_responses_request(
    mut body: Value,
    api_key_auth: bool,
    use_responses_lite: bool,
    lite_request_namespace: Option<&str>,
) -> Result<Value> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::InvalidRequest("request body must be an object".to_string()))?;

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRequest("model must be a string".to_string()))?;
    validate_model_id(model).map_err(Error::InvalidRequest)?;

    match object.get("instructions") {
        None => {
            object.insert("instructions".to_string(), Value::String(String::new()));
        }
        Some(Value::String(_)) => {}
        Some(_) => {
            return Err(Error::InvalidRequest(
                "instructions must be a string".to_string(),
            ));
        }
    }

    reject_stateful_reference(object, "previous_response_id")?;
    reject_stateful_reference(object, "conversation")?;

    match object.get("tools") {
        None => {
            object.insert("tools".to_string(), Value::Array(Vec::new()));
        }
        Some(Value::Array(tools))
            if tools.iter().all(|tool| {
                tool.as_object()
                    .is_some_and(|tool| tool.get("type").is_some_and(Value::is_string))
            }) => {}
        Some(Value::Array(_)) => {
            return Err(Error::InvalidRequest(
                "each tool must be an object with a string type".to_string(),
            ));
        }
        Some(_) => {
            return Err(Error::InvalidRequest("tools must be an array".to_string()));
        }
    }
    let namespace_calls = collect_namespace_calls(
        object
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools was normalized to an array"),
    )?;

    let input = object
        .remove("input")
        .ok_or_else(|| Error::InvalidRequest("input is required".to_string()))?;
    object.insert(
        "input".to_string(),
        normalize_response_input(input, &namespace_calls)?,
    );

    match object.get("tool_choice") {
        None => {
            object.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        }
        Some(Value::String(_)) => {}
        Some(Value::Object(_)) if api_key_auth => {}
        Some(_) => {
            return Err(Error::InvalidRequest(
                "tool_choice has an unsupported shape".to_string(),
            ));
        }
    }

    match object.get("parallel_tool_calls") {
        None => {
            object.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        }
        Some(Value::Bool(_)) => {}
        Some(_) => {
            return Err(Error::InvalidRequest(
                "parallel_tool_calls must be a boolean".to_string(),
            ));
        }
    }

    normalize_reasoning(object, api_key_auth)?;
    normalize_include(object)?;

    if api_key_auth {
        validate_optional_positive_integer(object, "max_output_tokens")?;
    } else {
        // The pinned ChatGPT Codex request does not send this standard public
        // API parameter. Muse always supplied it for the Meta provider.
        object.remove("max_output_tokens");
        // Do not forward legacy provider telemetry through subscription auth.
        object.remove("client_metadata");
        object.remove("metadata");
    }

    if use_responses_lite {
        normalize_responses_lite_request(
            object,
            lite_request_namespace.ok_or_else(|| {
                Error::InvalidRequest(
                    "Responses Lite request namespace was not initialized".to_string(),
                )
            })?,
        )?;
    }

    // Muse owns persistence and always replays the complete normalized history.
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    Ok(body)
}

/// Applies the Responses Lite wire contract used by current Codex models.
/// Muse still owns the complete stateless history and agent loop; only the
/// placement of instructions and tool schemas changes at this boundary.
fn normalize_responses_lite_request(
    object: &mut Map<String, Value>,
    request_namespace: &str,
) -> Result<()> {
    let instructions = object
        .remove("instructions")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| Error::InvalidRequest("instructions must be a string".to_string()))?;
    let tools = object
        .remove("tools")
        .and_then(|value| value.as_array().cloned())
        .ok_or_else(|| Error::InvalidRequest("tools must be an array".to_string()))?;
    let tools = group_responses_lite_tools(tools)?;

    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::InvalidRequest("input must be an array".to_string()))?;
    strip_responses_lite_image_details(input);

    let mut prefix = vec![serde_json::json!({
        "id": stable_prefix_item_id("at", request_namespace, &Value::Array(tools.clone()))?,
        "type": "additional_tools",
        "role": "developer",
        "tools": tools,
    })];
    if !instructions.is_empty() {
        prefix.push(serde_json::json!({
            "id": stable_prefix_item_id("msg", request_namespace, &Value::String(instructions.clone()))?,
            "type": "message",
            "role": "developer",
            "content": [{"type":"input_text", "text":instructions}],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["model.base_instructions"]
            },
        }));
    }
    input.splice(0..0, prefix);

    object.insert("parallel_tool_calls".to_string(), Value::Bool(false));
    let reasoning = object
        .entry("reasoning".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::InvalidRequest("reasoning must be an object".to_string()))?;
    reasoning.insert(
        "context".to_string(),
        Value::String("all_turns".to_string()),
    );
    Ok(())
}

fn group_responses_lite_tools(tools: Vec<Value>) -> Result<Vec<Value>> {
    let mut functions = Vec::new();
    let mut function_namespace_description = String::new();
    let mut function_namespace_index = None;
    let mut grouped = Vec::new();

    for tool in tools {
        let tool_type = tool.get("type").and_then(Value::as_str).ok_or_else(|| {
            Error::InvalidRequest("each tool must have a string type".to_string())
        })?;
        if is_hosted_responses_tool(tool_type) {
            // Responses Lite accepts only client-executed tool schemas. Muse's
            // provider-independent browser and extension tools are namespaces
            // and continue through this adapter unchanged.
            continue;
        }
        let is_function_namespace = tool_type == "namespace"
            && tool.get("name").and_then(Value::as_str) == Some("functions");
        if matches!(tool_type, "function" | "custom") {
            function_namespace_index.get_or_insert(grouped.len());
            functions.push(tool);
        } else if is_function_namespace {
            function_namespace_index.get_or_insert(grouped.len());
            let namespace = tool.as_object().expect("validated tool object");
            if let Some(description) = namespace
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.trim().is_empty())
            {
                function_namespace_description = description.to_string();
            }
            let namespace_tools = namespace
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::InvalidRequest("functions namespace tools must be an array".to_string())
                })?;
            functions.extend(namespace_tools.iter().cloned());
        } else {
            grouped.push(tool);
        }
    }

    if let Some(index) = function_namespace_index
        && !functions.is_empty()
    {
        grouped.insert(
            index,
            serde_json::json!({
                "type": "namespace",
                "name": "functions",
                "description": function_namespace_description,
                "tools": functions,
            }),
        );
    }
    Ok(grouped)
}

fn is_hosted_responses_tool(tool_type: &str) -> bool {
    matches!(
        tool_type,
        "web_search"
            | "web_search_preview"
            | "file_search"
            | "computer_use"
            | "computer_use_preview"
            | "code_interpreter"
            | "image_generation"
    )
}

fn strip_responses_lite_image_details(input: &mut [Value]) {
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
                    strip_image_details_from_parts(content);
                }
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                if let Some(output) = item.get_mut("output").and_then(Value::as_array_mut) {
                    strip_image_details_from_parts(output);
                }
            }
            _ => {}
        }
    }
}

fn strip_image_details_from_parts(parts: &mut [Value]) {
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("input_image")
            && let Some(part) = part.as_object_mut()
        {
            part.remove("detail");
        }
    }
}

fn responses_lite_request_namespace(headers: &HeaderMap) -> String {
    if let Some(request_id) = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 512)
    {
        return format!("request:{request_id}");
    }
    format!(
        "process:{}:{}",
        std::process::id(),
        RESPONSES_LITE_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn stable_prefix_item_id(prefix: &str, request_namespace: &str, value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        Error::InvalidRequest(format!("could not encode Responses Lite prefix: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(request_namespace.as_bytes());
    hasher.update([0]);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("{prefix}_{suffix}"))
}

fn validate_model_id(model: &str) -> std::result::Result<(), String> {
    if model.is_empty() || model.len() > MAX_MODEL_ID_BYTES || model.starts_with('-') {
        return Err(format!(
            "model must contain between 1 and {MAX_MODEL_ID_BYTES} bytes"
        ));
    }
    if model.chars().any(char::is_whitespace) || model.chars().any(char::is_control) {
        return Err("model must not contain whitespace or control characters".to_string());
    }
    Ok(())
}

fn reject_stateful_reference(object: &mut Map<String, Value>, field: &str) -> Result<()> {
    match object.remove(field) {
        None | Some(Value::Null) => Ok(()),
        Some(_) => Err(Error::InvalidRequest(format!(
            "{field} cannot be used because Muse sends complete stateless history"
        ))),
    }
}

type NamespaceCallMap = BTreeMap<String, (String, String)>;

fn collect_namespace_calls(tools: &[Value]) -> Result<NamespaceCallMap> {
    let mut calls = NamespaceCallMap::new();
    let standalone_names = tools
        .iter()
        .filter_map(Value::as_object)
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    for tool in tools {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        if tool.get("type").and_then(Value::as_str) != Some("namespace") {
            continue;
        }
        let Some(namespace) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(namespace_tools) = tool.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for namespace_tool in namespace_tools {
            let Some(namespace_tool) = namespace_tool.as_object() else {
                continue;
            };
            if namespace_tool.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let Some(name) = namespace_tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let flattened = format!("{namespace}.{name}");
            if standalone_names.contains(flattened.as_str()) {
                return Err(Error::InvalidRequest(format!(
                    "namespace tool conflicts with standalone function {flattened}"
                )));
            }
            let split = (namespace.to_string(), name.to_string());
            if let Some(previous) = calls.insert(flattened.clone(), split.clone())
                && previous != split
            {
                return Err(Error::InvalidRequest(format!(
                    "namespace tools have ambiguous flattened call name {flattened}"
                )));
            }
        }
    }
    Ok(calls)
}

fn normalize_response_input(input: Value, namespace_calls: &NamespaceCallMap) -> Result<Value> {
    let items = match input {
        Value::String(text) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        })],
        Value::Array(items) if !items.is_empty() => items,
        Value::Array(_) => {
            return Err(Error::InvalidRequest(
                "input must contain at least one item".to_string(),
            ));
        }
        _ => {
            return Err(Error::InvalidRequest(
                "input must be a string or an array".to_string(),
            ));
        }
    };

    let normalized = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| normalize_response_item(item, index, namespace_calls))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(normalized))
}

fn normalize_response_item(
    mut item: Value,
    index: usize,
    namespace_calls: &NamespaceCallMap,
) -> Result<Value> {
    let object = item
        .as_object_mut()
        .ok_or_else(|| Error::InvalidRequest(format!("input item {index} must be an object")))?;
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRequest(format!("input item {index} has no string type")))?;
    if !matches!(
        item_type,
        "message"
            | "reasoning"
            | "local_shell_call"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "tool_search_call"
            | "tool_search_output"
            | "web_search_call"
            | "image_generation_call"
            | "compaction"
            | "compaction_summary"
            | "compaction_trigger"
            | "context_compaction"
    ) {
        return Err(Error::InvalidRequest(format!(
            "input item {index} has unsupported type"
        )));
    }
    if item_type == "message" {
        normalize_message_item(object, index)?;
    }

    let mut parsed: ResponseItem = serde_json::from_value(item).map_err(|error| {
        Error::InvalidRequest(format!("input item {index} is invalid: {error}"))
    })?;
    if matches!(parsed, ResponseItem::Other) {
        return Err(Error::InvalidRequest(format!(
            "input item {index} has unsupported type"
        )));
    }
    // The pinned client accepts legacy rollout IDs when reading local state but
    // never forwards them unless they contain a non-empty type prefix and
    // suffix. This prevents old arbitrary IDs from becoming server identities.
    if parsed.id().is_some_and(|id| !id.is_prefixed()) {
        parsed.set_id(None);
    }
    let mut normalized = serde_json::to_value(parsed).map_err(|error| {
        Error::InvalidRequest(format!(
            "input item {index} could not be normalized: {error}"
        ))
    })?;

    // Stock Muse flattens namespaced calls when it records history (for
    // example `namespace:muse, name:read_file` becomes `name:muse.read_file`).
    // The Responses API requires the original split when replaying that call.
    // Reverse only exact aliases from the request's advertised namespace tools
    // so ordinary dotted function names are left untouched.
    let namespace_split = (normalized.get("type").and_then(Value::as_str) == Some("function_call")
        && normalized.get("namespace").is_none())
    .then(|| normalized.get("name").and_then(Value::as_str))
    .flatten()
    .and_then(|flattened| namespace_calls.get(flattened))
    .cloned();
    if let Some((namespace, name)) = namespace_split
        && let Some(object) = normalized.as_object_mut()
    {
        object.insert("namespace".to_string(), Value::String(namespace.clone()));
        object.insert("name".to_string(), Value::String(name.clone()));
    }

    // Muse can attach the invoked tool name to result-history items. The
    // Responses input schema identifies results solely by `call_id`; `name`
    // belongs on the preceding call item and is rejected on the result item by
    // the ChatGPT backend. Keep the filter type-specific so call names remain
    // intact.
    if matches!(
        normalized.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    ) && let Some(object) = normalized.as_object_mut()
    {
        object.remove("name");
    }

    Ok(normalized)
}

fn normalize_message_item(object: &mut Map<String, Value>, index: usize) -> Result<()> {
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRequest(format!("message item {index} has no string role")))?;
    if !matches!(role, "assistant" | "developer" | "system" | "user") {
        return Err(Error::InvalidRequest(format!(
            "message item {index} has unsupported role"
        )));
    }
    let assistant = role == "assistant";
    let content = object
        .get_mut("content")
        .ok_or_else(|| Error::InvalidRequest(format!("message item {index} has no content")))?;
    if let Value::String(text) = content {
        *content = Value::Array(vec![serde_json::json!({
            "type": if assistant { "output_text" } else { "input_text" },
            "text": std::mem::take(text),
        })]);
    }
    let Value::Array(parts) = content else {
        return Err(Error::InvalidRequest(format!(
            "message item {index} content must be a string or array"
        )));
    };
    for (part_index, part) in parts.iter_mut().enumerate() {
        normalize_content_part(part, assistant, index, part_index)?;
    }
    Ok(())
}

fn normalize_content_part(
    part: &mut Value,
    assistant: bool,
    item_index: usize,
    part_index: usize,
) -> Result<()> {
    let object = part.as_object_mut().ok_or_else(|| {
        Error::InvalidRequest(format!(
            "message item {item_index} content part {part_index} must be an object"
        ))
    })?;
    let part_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "message item {item_index} content part {part_index} has no string type"
            ))
        })?
        .to_string();
    let expected_text_type = if assistant {
        "output_text"
    } else {
        "input_text"
    };
    match part_type.as_str() {
        "text" | "input_text" | "output_text" => {
            if !object.get("text").is_some_and(Value::is_string) {
                return Err(Error::InvalidRequest(format!(
                    "message item {item_index} content part {part_index} has no string text"
                )));
            }
            object.insert(
                "type".to_string(),
                Value::String(expected_text_type.to_string()),
            );
        }
        "image_url" => {
            let image = object.remove("image_url").ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "message item {item_index} content part {part_index} has no image_url"
                ))
            })?;
            match image {
                Value::String(url) => {
                    object.insert("image_url".to_string(), Value::String(url));
                }
                Value::Object(mut image) => {
                    let url = image.remove("url").filter(Value::is_string).ok_or_else(|| {
                        Error::InvalidRequest(format!(
                            "message item {item_index} content part {part_index} has no image URL"
                        ))
                    })?;
                    object.insert("image_url".to_string(), url);
                    if let Some(detail) = image.remove("detail") {
                        object.insert("detail".to_string(), detail);
                    }
                }
                _ => {
                    return Err(Error::InvalidRequest(format!(
                        "message item {item_index} content part {part_index} has invalid image_url"
                    )));
                }
            }
            object.insert("type".to_string(), Value::String("input_image".to_string()));
        }
        "input_image" if !assistant => {}
        _ => {
            return Err(Error::InvalidRequest(format!(
                "message item {item_index} content part {part_index} has unsupported type"
            )));
        }
    }
    Ok(())
}

fn normalize_reasoning(object: &mut Map<String, Value>, api_key_auth: bool) -> Result<bool> {
    let Some(reasoning) = object.get_mut("reasoning") else {
        return Ok(false);
    };
    if reasoning.is_null() {
        return Ok(false);
    }
    let reasoning = reasoning
        .as_object_mut()
        .ok_or_else(|| Error::InvalidRequest("reasoning must be an object or null".to_string()))?;
    if let Some(effort) = reasoning.get("effort")
        && !effort.is_null()
        && !effort.as_str().is_some_and(is_reasoning_effort)
    {
        return Err(Error::InvalidRequest(
            "reasoning effort is not supported".to_string(),
        ));
    }
    if let Some(summary) = reasoning.get("summary")
        && !summary.is_null()
        && !summary.as_str().is_some_and(is_reasoning_summary)
    {
        return Err(Error::InvalidRequest(
            "reasoning summary is not supported".to_string(),
        ));
    }
    if !api_key_auth {
        reasoning.retain(|key, _| matches!(key.as_str(), "effort" | "summary"));
        if reasoning.get("summary").and_then(Value::as_str) == Some("none") {
            reasoning.remove("summary");
        }
    }
    Ok(true)
}

fn is_reasoning_effort(value: &str) -> bool {
    matches!(
        value,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
    )
}

fn is_reasoning_summary(value: &str) -> bool {
    matches!(value, "auto" | "concise" | "detailed" | "none")
}

fn normalize_include(object: &mut Map<String, Value>) -> Result<()> {
    let include = object
        .entry("include".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(include) = include else {
        return Err(Error::InvalidRequest(
            "include must be an array".to_string(),
        ));
    };
    if !include.iter().all(Value::is_string) {
        return Err(Error::InvalidRequest(
            "include entries must be strings".to_string(),
        ));
    }
    let mut unique = Vec::with_capacity(include.len() + 1);
    for value in std::mem::take(include) {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    let encrypted_reasoning = Value::String("reasoning.encrypted_content".to_string());
    if !unique.contains(&encrypted_reasoning) {
        unique.push(encrypted_reasoning);
    }
    *include = unique;
    Ok(())
}

fn validate_optional_positive_integer(object: &Map<String, Value>, field: &str) -> Result<()> {
    if let Some(value) = object.get(field)
        && !value.is_null()
        && value.as_u64().is_none_or(|value| value == 0)
    {
        return Err(Error::InvalidRequest(format!(
            "{field} must be a positive integer or null"
        )));
    }
    Ok(())
}

pub(crate) fn validate_base_url(url: &Url) -> Result<()> {
    let secure_scheme = url.scheme() == "https";
    #[cfg(test)]
    let secure_scheme = secure_scheme
        || (url.scheme() == "http"
            && url
                .host_str()
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|host| host.is_loopback()));

    if !secure_scheme
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::InvalidBaseUrl);
    }
    Ok(())
}

fn attach_auth_headers(headers: &mut HeaderMap, auth: &CodexAuth) -> Result<()> {
    let token = auth
        .get_token()
        .map_err(|_| Error::Authentication("credential has no bearer token".to_string()))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| Error::Authentication("credential is not a valid header value".to_string()))?;
    authorization.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization);

    if let Some(account_id) = auth.get_account_id() {
        let mut account = HeaderValue::from_str(&account_id).map_err(|_| {
            Error::Authentication("account id is not a valid header value".to_string())
        })?;
        account.set_sensitive(true);
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
    }
    if auth.is_fedramp_account() {
        headers.insert(
            HeaderName::from_static("x-openai-fedramp"),
            HeaderValue::from_static("true"),
        );
    }
    Ok(())
}

fn merge_safe_request_headers(destination: &mut HeaderMap, source: &HeaderMap) {
    for (name, value) in source {
        if !is_forbidden_forwarded_header(name) {
            destination.insert(name.clone(), value.clone());
        }
    }
}

fn set_internal_responses_lite_header(headers: &mut HeaderMap, enabled: bool) {
    if enabled {
        headers.insert(
            HeaderName::from_static(RESPONSES_LITE_HEADER),
            HeaderValue::from_static("true"),
        );
    } else {
        headers.remove(RESPONSES_LITE_HEADER);
    }
}

fn set_internal_routing_hint_header(headers: &mut HeaderMap, value: Option<&HeaderValue>) {
    if let Some(value) = value {
        headers.insert(
            HeaderName::from_static(CODEX_ROUTING_HINT_HEADER),
            value.clone(),
        );
    } else {
        headers.remove(CODEX_ROUTING_HINT_HEADER);
    }
}

fn is_forbidden_forwarded_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "chatgpt-account-id"
            | "x-openai-fedramp"
            | "x-api-key"
            | "api-key"
            | "openai-organization"
            | "openai-project"
            | "originator"
            | "user-agent"
            | "version"
            | RESPONSES_LITE_HEADER
            | CODEX_ROUTING_HINT_HEADER
    )
}

#[async_trait]
trait Recovery: Send {
    fn has_next(&self) -> bool;
    async fn next(&mut self) -> Result<()>;
}

struct CodexRecovery(UnauthorizedRecovery);

#[async_trait]
impl Recovery for CodexRecovery {
    fn has_next(&self) -> bool {
        self.0.has_next()
    }

    async fn next(&mut self) -> Result<()> {
        self.0.next().await.map(|_| ()).map_err(|_| {
            Error::Authentication(
                "credential refresh failed; run `muse-codex login` again".to_string(),
            )
        })
    }
}

trait ResponseStatus {
    fn response_status(&self) -> StatusCode;

    fn retry_after(&self) -> Option<Duration> {
        None
    }
}

impl ResponseStatus for reqwest::Response {
    fn response_status(&self) -> StatusCode {
        self.status()
    }

    fn retry_after(&self) -> Option<Duration> {
        self.headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .map(|delay| delay.min(MAX_RETRY_AFTER))
    }
}

async fn execute_with_401_recovery<T, F, Fut>(
    mut send: F,
    mut recovery: Option<Box<dyn Recovery>>,
    refresh_lock_home: Option<PathBuf>,
) -> Result<T>
where
    T: ResponseStatus,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut transient_attempts = 0_usize;
    let mut refresh_lock = None;
    loop {
        let response = match send().await {
            Ok(response) => response,
            Err(Error::Http(error))
                if transient_attempts + 1 < MAX_PRESTREAM_ATTEMPTS
                    && is_retryable_send_error(&error) =>
            {
                transient_attempts += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        if response.response_status().is_server_error()
            && transient_attempts + 1 < MAX_PRESTREAM_ATTEMPTS
        {
            transient_attempts += 1;
            let delay = response.retry_after();
            drop(response);
            if let Some(delay) = delay
                && !delay.is_zero()
            {
                tokio::time::sleep(delay).await;
            }
            continue;
        }
        if response.response_status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let Some(recovery) = recovery.as_mut() else {
            return Ok(response);
        };
        if !recovery.has_next() {
            return Ok(response);
        }
        drop(response);
        if refresh_lock.is_none()
            && let Some(home) = refresh_lock_home.clone()
        {
            refresh_lock = Some(acquire_credential_refresh_lock(home).await?);
        }
        recovery.next().await?;
    }
}

struct CredentialRefreshLock {
    file: File,
}

impl Drop for CredentialRefreshLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the file descriptor is live for the duration of this
            // call, and LOCK_UN has no pointer arguments.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

async fn acquire_credential_refresh_lock(home: PathBuf) -> Result<CredentialRefreshLock> {
    tokio::task::spawn_blocking(move || acquire_credential_refresh_lock_blocking(home))
        .await
        .map_err(|error| Error::CredentialRefreshLock(error.to_string()))?
}

fn acquire_credential_refresh_lock_blocking(home: PathBuf) -> Result<CredentialRefreshLock> {
    // AuthConfig stores a canonical home. Resolve it again immediately before
    // the open so a post-validation rename or symlink replacement fails closed.
    let canonical_home = std::fs::canonicalize(&home).map_err(|error| {
        Error::CredentialRefreshLock(format!("could not canonicalize private home: {error}"))
    })?;
    if canonical_home != home {
        return Err(Error::CredentialRefreshLock(
            "private home changed after validation".to_string(),
        ));
    }
    // The filename is internal and constant. Keep the containment check next to
    // construction so both reviewers and static analysis can verify the sink.
    let path = canonical_home.join(CREDENTIAL_REFRESH_LOCK_NAME);
    if !path.starts_with(&canonical_home) {
        return Err(Error::CredentialRefreshLock(
            "private lock path escaped its validated home".to_string(),
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(&path).map_err(|error| {
        Error::CredentialRefreshLock(format!("could not open private lock file: {error}"))
    })?;
    let metadata = file.metadata().map_err(|error| {
        Error::CredentialRefreshLock(format!("could not inspect private lock file: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(Error::CredentialRefreshLock(
            "private lock path is not a regular file".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;

        // SAFETY: `geteuid` takes no pointers and has no preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(Error::CredentialRefreshLock(
                "private lock file has unsafe ownership, mode, or link count".to_string(),
            ));
        }
        // SAFETY: the file descriptor is valid and LOCK_EX has no pointer
        // arguments. This runs on a blocking worker thread.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::CredentialRefreshLock(
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(CredentialRefreshLock { file })
}

fn is_retryable_send_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

async fn collect_response(response: reqwest::Response, limit: usize) -> Result<Bytes> {
    let mut stream = response.bytes_stream();
    let mut output = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::Http)?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(Error::InvalidUpstreamResponse(format!(
                "response exceeds {limit} bytes"
            )));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output.freeze())
}

async fn upstream_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let summary = match collect_response(response, MAX_ERROR_RESPONSE_BYTES).await {
        Ok(body) => summarize_upstream_error(&body),
        Err(_) => "response body omitted".to_string(),
    };
    Error::Upstream { status, summary }
}

fn summarize_upstream_error(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return "response body omitted".to_string();
    };
    let error = value.get("error").unwrap_or(&value);
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .and_then(safe_error_token);
    let error_code = error
        .get("code")
        .and_then(Value::as_str)
        .and_then(safe_error_token);
    match (error_type, error_code) {
        (Some(error_type), Some(error_code)) => {
            format!("type={error_type}, code={error_code}")
        }
        (Some(error_type), None) => format!("type={error_type}"),
        (None, Some(error_code)) => format!("code={error_code}"),
        (None, None) => "response body omitted".to_string(),
    }
}

fn safe_error_token(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then_some(value)
}

#[derive(Debug, Deserialize)]
struct UpstreamModelsResponse {
    #[serde(default)]
    models: Vec<UpstreamModel>,
    #[serde(default)]
    data: Vec<UpstreamModel>,
}

#[derive(Debug, Deserialize)]
struct UpstreamModel {
    #[serde(alias = "id")]
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
    #[serde(default)]
    supported_reasoning_levels: Vec<UpstreamReasoningPreset>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    minimal_client_version: Option<UpstreamClientVersion>,
    #[serde(default)]
    use_responses_lite: bool,
    #[serde(default)]
    tool_mode: Option<String>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default = "default_true")]
    supported_in_api: bool,
    #[serde(default)]
    service_tiers: Vec<UpstreamServiceTier>,
    #[serde(default)]
    additional_speed_tiers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UpstreamServiceTier {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UpstreamClientVersion {
    Text(String),
    Triple([u64; 3]),
}

#[derive(Debug, Deserialize)]
struct UpstreamReasoningPreset {
    effort: String,
}

fn normalize_models(bytes: &[u8]) -> Result<Vec<ModelInfo>> {
    let upstream: UpstreamModelsResponse = serde_json::from_slice(bytes).map_err(|error| {
        Error::InvalidUpstreamResponse(format!("could not decode models response: {error}"))
    })?;
    if !upstream.models.is_empty() && !upstream.data.is_empty() {
        return Err(Error::InvalidUpstreamResponse(
            "models response mixed subscription and public API catalog shapes".to_string(),
        ));
    }
    let subscription_catalog = !upstream.models.is_empty();
    let mut upstream_models = if subscription_catalog {
        upstream.models
    } else {
        upstream.data
    };
    if upstream_models.is_empty() {
        return Err(Error::InvalidUpstreamResponse(
            "models response contained no models".to_string(),
        ));
    }
    if upstream_models.len() > MAX_MODELS {
        return Err(Error::InvalidUpstreamResponse(format!(
            "models response exceeded the {MAX_MODELS}-model limit"
        )));
    }
    if subscription_catalog {
        upstream_models.sort_by_key(|model| model.priority);
    }

    let mut seen_ids = std::collections::HashSet::with_capacity(upstream_models.len());
    let mut models = upstream_models
        .into_iter()
        .map(|model| -> Result<ModelInfo> {
            validate_model_id(&model.slug).map_err(|_| {
                Error::InvalidUpstreamResponse("model catalog returned an invalid id".to_string())
            })?;
            if !seen_ids.insert(model.slug.clone()) {
                return Err(Error::InvalidUpstreamResponse(
                    "model catalog returned duplicate ids".to_string(),
                ));
            }
            let mut is_visible = match model.visibility.as_deref() {
                None | Some("list") => true,
                Some("hide" | "none") => false,
                Some(_) => {
                    return Err(Error::InvalidUpstreamResponse(
                        "model catalog returned unknown visibility".to_string(),
                    ));
                }
            };
            if let Some(minimal_version) = model.minimal_client_version.as_ref()
                && compare_client_version(minimal_version, CODEX_WIRE_COMPATIBILITY_VERSION)?
                    == std::cmp::Ordering::Greater
            {
                is_visible = false;
            }
            if model
                .tool_mode
                .as_deref()
                .is_some_and(|mode| !matches!(mode, "direct" | "code_mode" | "code_mode_only"))
            {
                is_visible = false;
            }
            let mut supported_reasoning_efforts = Vec::new();
            let had_reasoning_efforts = !model.supported_reasoning_levels.is_empty();
            for preset in model.supported_reasoning_levels {
                if !is_reasoning_effort(&preset.effort) {
                    continue;
                }
                if !supported_reasoning_efforts.contains(&preset.effort) {
                    supported_reasoning_efforts.push(preset.effort);
                }
            }
            if had_reasoning_efforts && supported_reasoning_efforts.is_empty() {
                is_visible = false;
            }
            let default_reasoning_effort = model.default_reasoning_level.filter(|default| {
                supported_reasoning_efforts
                    .iter()
                    .any(|supported| supported == default)
            });
            let input_modalities = normalize_input_modalities(model.input_modalities)?;
            let supports_fast_mode = model
                .service_tiers
                .iter()
                .any(|tier| tier.id == ServiceTier::Fast.request_value())
                || model
                    .additional_speed_tiers
                    .iter()
                    .any(|tier| tier == "fast");
            if !input_modalities.is_empty()
                && !input_modalities.iter().any(|modality| modality == "text")
            {
                is_visible = false;
            }
            Ok(ModelInfo {
                id: model.slug,
                display_name: normalize_catalog_text(model.display_name, 512, false)?,
                description: normalize_catalog_text(model.description, 16 * 1024, true)?,
                context_window: normalize_model_limit(model.context_window)?,
                max_output_tokens: normalize_model_limit(model.max_output_tokens)?,
                supported_reasoning_efforts,
                default_reasoning_effort,
                is_visible,
                is_default: false,
                use_responses_lite: model.use_responses_lite,
                tool_mode: model.tool_mode,
                input_modalities,
                supported_in_api: model.supported_in_api,
                supports_fast_mode,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let default_index = models
        .iter()
        .position(|model| model.is_visible)
        .ok_or_else(|| {
            Error::InvalidUpstreamResponse("model catalog has no visible models".to_string())
        })?;
    models[default_index].is_default = true;
    Ok(models)
}

/// The standard OpenAI `/v1/models` response proves model availability but is
/// intentionally sparse. For the first-party endpoint only, enrich models in
/// that response with the exact wire capabilities bundled at the pinned Codex
/// revision. The intersection rule avoids claiming access to a model the
/// account's API catalog did not return.
fn normalize_api_models_with_bundled_metadata(bytes: &[u8]) -> Result<Vec<ModelInfo>> {
    let mut available = normalize_models(bytes)?;
    let bundled = normalize_models(BUNDLED_MODEL_CATALOG)?;
    let mut merged = Vec::with_capacity(available.len());

    for metadata in bundled {
        let Some(index) = available.iter().position(|model| model.id == metadata.id) else {
            continue;
        };
        let account_model = available.remove(index);
        if !metadata.supported_in_api {
            continue;
        }
        let mut model = metadata;
        model.is_visible &= account_model.is_visible;
        model.is_default = false;
        merged.push(model);
    }
    let default_index = merged
        .iter()
        .position(|model| model.is_visible)
        .ok_or_else(|| {
            Error::InvalidUpstreamResponse("model catalog has no visible models".to_string())
        })?;
    merged[default_index].is_default = true;
    Ok(merged)
}

fn default_true() -> bool {
    true
}

fn normalize_catalog_text(
    value: Option<String>,
    max_bytes: usize,
    allow_newlines: bool,
) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    if value.len() > max_bytes
        || value.chars().any(|character| {
            character.is_control() && !(allow_newlines && matches!(character, '\n' | '\r' | '\t'))
        })
    {
        return Err(Error::InvalidUpstreamResponse(
            "model catalog returned invalid display text".to_string(),
        ));
    }
    Ok(Some(value))
}

fn normalize_model_limit(value: Option<i64>) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(value) if value > 0 => Ok(Some(value as u64)),
        Some(_) => Err(Error::InvalidUpstreamResponse(
            "model catalog returned a non-positive token limit".to_string(),
        )),
    }
}

fn normalize_input_modalities(modalities: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for modality in modalities {
        if modality.is_empty()
            || modality.len() > 32
            || modality.chars().any(char::is_whitespace)
            || modality.chars().any(char::is_control)
        {
            return Err(Error::InvalidUpstreamResponse(
                "model catalog returned an invalid input modality".to_string(),
            ));
        }
        if !normalized.contains(&modality) {
            normalized.push(modality);
        }
    }
    Ok(normalized)
}

fn compare_client_version(left: &UpstreamClientVersion, right: &str) -> Result<std::cmp::Ordering> {
    fn parse(version: &str) -> Option<[u64; 3]> {
        let mut parts = version.split('.');
        let parsed = [
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ];
        parts.next().is_none().then_some(parsed)
    }
    let left = match left {
        UpstreamClientVersion::Text(version) => parse(version).ok_or_else(|| {
            Error::InvalidUpstreamResponse(
                "model catalog returned an invalid minimum client version".to_string(),
            )
        })?,
        UpstreamClientVersion::Triple(version) => *version,
    };
    let right = parse(right).expect("wire compatibility version is a semantic version");
    Ok(left.cmp(&right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Debug)]
    struct FixtureResponse {
        status: StatusCode,
        body: &'static str,
    }

    impl ResponseStatus for FixtureResponse {
        fn response_status(&self) -> StatusCode {
            self.status
        }
    }

    struct FixtureRecovery {
        remaining: usize,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Recovery for FixtureRecovery {
        fn has_next(&self) -> bool {
            self.remaining > 0
        }

        async fn next(&mut self) -> Result<()> {
            self.remaining -= 1;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn normalizes_stock_muse_request_for_subscription_transport() {
        let request: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/muse-responses-request.json"
        ))
        .expect("request fixture");
        let normalized = normalize_responses_request(request, false, false, None)
            .expect("normalized subscription request");

        assert_eq!(normalized["store"], false);
        assert_eq!(normalized["stream"], true);
        assert_eq!(normalized["tool_choice"], "auto");
        assert_eq!(normalized["parallel_tool_calls"], false);
        assert!(normalized.get("max_output_tokens").is_none());
        assert!(normalized.get("previous_response_id").is_none());
        assert!(normalized.get("conversation").is_none());
        assert!(normalized.get("client_metadata").is_none());
        assert!(normalized.get("metadata").is_none());
        assert_eq!(
            normalized["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );

        let input = normalized["input"].as_array().expect("input array");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(
            input[1]["content"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert_eq!(input[3]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["id"], "rs_fixture");
        assert_eq!(input[2]["encrypted_content"], "encrypted-fixture");
        assert_eq!(input[3]["id"], "msg_fixture");
        assert_eq!(input[4]["id"], "fc_fixture");
        assert_eq!(input[4]["call_id"], "call_fixture");
        assert_eq!(input[5]["call_id"], "call_fixture");
        assert_eq!(input[5]["output"], "fixture output");
        assert_eq!(normalized["tools"][0]["type"], "namespace");
    }

    #[test]
    fn fast_mode_uses_priority_and_standard_mode_clears_untrusted_tiers() {
        let mut request = serde_json::json!({
            "model": "gpt-fixture",
            "input": "hello",
            "service_tier": "attacker-controlled"
        });
        apply_fast_service_tier(&mut request, false).expect("standard tier");
        assert!(request.get("service_tier").is_none());

        apply_fast_service_tier(&mut request, true).expect("Fast tier");
        assert_eq!(request["service_tier"], ServiceTier::Fast.request_value());
        assert_eq!(request["service_tier"], "priority");
    }

    #[test]
    fn subscription_routing_hint_tracks_model_and_canonical_tier() {
        let standard = subscription_routing_hint(&serde_json::json!({
            "model": "gpt-fixture"
        }))
        .expect("standard routing hint");
        assert_eq!(standard, "model=gpt-fixture");

        let fast = subscription_routing_hint(&serde_json::json!({
            "model": "gpt-fixture",
            "service_tier": "priority"
        }))
        .expect("Fast routing hint");
        assert_eq!(fast, "model=gpt-fixture;tier=priority");
    }

    #[test]
    fn fast_mode_requires_catalog_support_except_for_custom_api_endpoints() {
        let mut capabilities = ModelCapabilities {
            is_visible: true,
            use_responses_lite: false,
            tool_mode: None,
            input_modalities: vec!["text".to_string()],
            supported_reasoning_efforts: Vec::new(),
            supports_fast_mode: false,
        };
        assert!(validate_fast_mode_availability(false, false, Some(&capabilities)).is_ok());
        assert!(matches!(
            validate_fast_mode_availability(true, false, Some(&capabilities)),
            Err(Error::FastModeUnavailable)
        ));
        assert!(validate_fast_mode_availability(true, true, Some(&capabilities)).is_ok());
        capabilities.supports_fast_mode = true;
        assert!(validate_fast_mode_availability(true, false, Some(&capabilities)).is_ok());
    }

    #[test]
    fn tool_result_followup_keeps_call_names_but_strips_result_names() {
        let request: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/muse-tool-result-followup.json"
        ))
        .expect("tool-result followup fixture");
        let normalized = normalize_responses_request(request, false, false, None)
            .expect("normalized tool-result followup");
        let input = normalized["input"].as_array().expect("input array");

        assert_eq!(input[2]["type"], "reasoning");
        assert_eq!(input[2]["encrypted_content"], "encrypted-fixture");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["namespace"], "muse");
        assert_eq!(input[3]["name"], "read_file");
        assert_eq!(input[3]["call_id"], "call_fixture");
        assert_eq!(input[3]["arguments"], r#"{"path":"fixture.txt"}"#);
        assert_eq!(input[4]["type"], "function_call_output");
        assert!(input[4].get("name").is_none());
        assert_eq!(input[4]["call_id"], "call_fixture");
        assert_eq!(input[4]["output"], "fixture contents");

        assert_eq!(input[5]["type"], "custom_tool_call");
        assert_eq!(input[5]["name"], "exec");
        assert_eq!(input[6]["type"], "custom_tool_call_output");
        assert!(input[6].get("name").is_none());
        assert_eq!(input[6]["call_id"], "custom_fixture");
        assert_eq!(input[6]["output"], "fixture output");
    }

    #[test]
    fn namespace_reverse_mapping_preserves_unadvertised_dotted_function_names() {
        let normalized = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-fixture",
                "input": [{
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "standalone.dotted_name",
                    "arguments": "{}"
                }],
                "tools": [{
                    "type": "namespace",
                    "name": "muse",
                    "tools": [{"type":"function", "name":"read_file", "parameters":{}}]
                }]
            }),
            false,
            false,
            None,
        )
        .expect("normalized dotted standalone function");

        assert_eq!(normalized["input"][0]["name"], "standalone.dotted_name");
        assert!(normalized["input"][0].get("namespace").is_none());
    }

    #[test]
    fn namespace_reverse_mapping_rejects_ambiguous_standalone_name() {
        let error = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-fixture",
                "input": "hello",
                "tools": [
                    {"type":"function", "name":"muse.read_file", "parameters":{}},
                    {
                        "type":"namespace",
                        "name":"muse",
                        "tools":[{"type":"function", "name":"read_file", "parameters":{}}]
                    }
                ]
            }),
            false,
            false,
            None,
        )
        .expect_err("ambiguous flattened name must fail closed");

        assert!(matches!(error, Error::InvalidRequest(message) if message.contains("conflicts")));
    }

    #[test]
    fn api_key_request_preserves_public_parameters_and_explicit_parallel_choice() {
        let request: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/muse-responses-request.json"
        ))
        .expect("request fixture");
        let normalized = normalize_responses_request(request, true, false, None)
            .expect("normalized API request");

        assert_eq!(normalized["max_output_tokens"], 4096);
        assert_eq!(normalized["parallel_tool_calls"], false);
        assert_eq!(
            normalized["client_metadata"]["fixture"],
            "legacy-provider-value"
        );
        assert_eq!(normalized["metadata"]["fixture"], "public-api-value");
    }

    #[test]
    fn response_normalizer_rejects_server_side_history_and_malformed_items() {
        for field in ["previous_response_id", "conversation"] {
            let mut request = serde_json::json!({
                "model": "gpt-fixture",
                "input": "hello"
            });
            request[field] = Value::String("secret-state-id".to_string());
            let error = normalize_responses_request(request, false, false, None)
                .expect_err("stateful request must fail closed");
            assert!(matches!(error, Error::InvalidRequest(message) if message.contains(field)));
        }

        let error = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-fixture",
                "input": [{"type":"future_private_item","secret":"do-not-log"}]
            }),
            false,
            false,
            None,
        )
        .expect_err("unknown input item must fail closed");
        let rendered = error.to_string();
        assert!(rendered.contains("unsupported type"));
        assert!(!rendered.contains("do-not-log"));
    }

    #[test]
    fn subscription_reasoning_none_uses_the_pinned_omitted_summary_shape() {
        let request = serde_json::json!({
            "model": "gpt-fixture",
            "input": "hello",
            "reasoning": {"effort":"low", "summary":"none"}
        });
        let subscription = normalize_responses_request(request.clone(), false, false, None)
            .expect("subscription request");
        let api = normalize_responses_request(request, true, false, None).expect("API request");

        assert!(subscription["reasoning"].get("summary").is_none());
        assert_eq!(api["reasoning"]["summary"], "none");
        assert_eq!(
            subscription["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn response_normalizer_maps_image_alias_and_defaults_parallel_calls_on() {
        let normalized = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-fixture",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type":"text", "text":"hello"},
                        {"type":"image_url", "image_url": {
                            "url":"data:image/png;base64,AA==", "detail":"original"
                        }}
                    ]
                }]
            }),
            false,
            false,
            None,
        )
        .expect("normalized request");

        assert_eq!(normalized["instructions"], "");
        assert_eq!(normalized["parallel_tool_calls"], true);
        assert_eq!(normalized["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(normalized["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            normalized["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AA=="
        );
        assert_eq!(
            normalized["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn response_normalizer_strips_unprefixed_legacy_item_ids() {
        let normalized = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-fixture",
                "input": [
                    {
                        "type": "message",
                        "id": "legacy-id",
                        "role": "assistant",
                        "content": "legacy"
                    },
                    {
                        "type": "message",
                        "id": "msg_current",
                        "role": "assistant",
                        "content": "current"
                    }
                ]
            }),
            false,
            false,
            None,
        )
        .expect("normalized request");

        assert!(normalized["input"][0].get("id").is_none());
        assert_eq!(normalized["input"][1]["id"], "msg_current");
    }

    #[test]
    fn normalizes_models_fixture() {
        let models = normalize_models(include_bytes!("../tests/fixtures/models.json"))
            .expect("models fixture");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-fixture-new");
        assert!(models[0].is_default);
        assert_eq!(models[0].context_window, Some(200_000));
        assert_eq!(models[0].max_output_tokens, None);
        assert!(!models[0].supports_fast_mode);
        assert_eq!(models[0].supported_reasoning_efforts, ["low", "high"]);
        assert!(models[0].is_visible);
        assert!(!models[1].is_default);
    }

    #[test]
    fn normalizes_current_catalog_and_hides_unsupported_future_wire_modes() {
        let models = normalize_models(include_bytes!("../tests/fixtures/models-0.153.4.json"))
            .expect("current models fixture");
        assert_eq!(models[0].id, "gpt-6-astra");
        assert!(models[0].is_default);
        assert!(models[0].use_responses_lite);
        assert_eq!(models[0].tool_mode.as_deref(), Some("code_mode_only"));
        assert_eq!(models[0].input_modalities, ["text", "image"]);
        assert_eq!(models[0].context_window, Some(272_000));
        assert_eq!(models[0].max_output_tokens, None);
        assert!(models[0].supports_fast_mode);

        for id in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            assert!(
                models
                    .iter()
                    .find(|model| model.id == id)
                    .is_some_and(|model| model.is_visible && model.use_responses_lite)
            );
        }
        let luna = models
            .iter()
            .find(|model| model.id == "gpt-5.6-luna")
            .expect("Luna model");
        assert!(
            !luna
                .supported_reasoning_efforts
                .contains(&"ultra".to_string())
        );
        for id in ["future-wire-model", "future-tool-mode", "codex-auto-review"] {
            assert!(
                !models
                    .iter()
                    .find(|model| model.id == id)
                    .expect("fixture model")
                    .is_visible
            );
        }
    }

    #[test]
    fn pinned_bundled_catalog_parses_and_contains_every_latest_model() {
        let models = normalize_models(BUNDLED_MODEL_CATALOG).expect("pinned bundled catalog");
        assert_eq!(models[0].id, "gpt-6-astra");
        for id in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            let model = models
                .iter()
                .find(|model| model.id == id)
                .expect("latest model in bundled catalog");
            assert!(model.is_visible);
            assert!(model.supports_fast_mode);
            assert!(model.supported_in_api);
            assert!(model.use_responses_lite);
            assert_eq!(model.tool_mode.as_deref(), Some("code_mode_only"));
        }
    }

    #[test]
    fn public_api_catalog_is_enriched_only_for_returned_bundled_models() {
        let models = normalize_api_models_with_bundled_metadata(
            br#"{"data":[{"id":"unlisted-public-model"},{"id":"gpt-6-astra"}]}"#,
        )
        .expect("enriched public catalog");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-6-astra");
        assert!(models[0].is_default);
        assert!(models[0].use_responses_lite);
        assert_eq!(models[0].context_window, Some(272_000));
        assert!(
            !models
                .iter()
                .any(|model| model.id == "unlisted-public-model")
        );
        assert!(!models.iter().any(|model| model.id == "gpt-5.6-sol"));
    }

    #[test]
    fn responses_lite_moves_instructions_and_tools_into_stable_prefix_items() {
        let request: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/muse-responses-request.json"
        ))
        .expect("request fixture");
        let normalized =
            normalize_responses_request(request.clone(), false, true, Some("session-a"))
                .expect("Responses Lite request");
        let repeated = normalize_responses_request(request.clone(), false, true, Some("session-a"))
            .expect("repeated Responses Lite request");
        let other_session = normalize_responses_request(request, false, true, Some("session-b"))
            .expect("other-session Responses Lite request");

        assert!(normalized.get("instructions").is_none());
        assert!(normalized.get("tools").is_none());
        assert_eq!(normalized["parallel_tool_calls"], false);
        assert_eq!(normalized["reasoning"]["context"], "all_turns");
        assert_eq!(normalized["input"][0]["type"], "additional_tools");
        assert!(
            normalized["input"][0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("at_"))
        );
        assert_eq!(normalized["input"][1]["type"], "message");
        assert_eq!(normalized["input"][1]["role"], "developer");
        assert_eq!(
            normalized["input"][1]["content"][0]["text"],
            "Fixture instructions"
        );
        assert_eq!(normalized["input"][0]["id"], repeated["input"][0]["id"]);
        assert_eq!(normalized["input"][1]["id"], repeated["input"][1]["id"]);
        assert_ne!(
            normalized["input"][0]["id"],
            other_session["input"][0]["id"]
        );
        assert_ne!(
            normalized["input"][1]["id"],
            other_session["input"][1]["id"]
        );
        assert_eq!(normalized["input"][0]["tools"][0]["type"], "namespace");
        assert_eq!(normalized["input"][0]["tools"][0]["name"], "muse");
        assert!(normalized["input"][3]["content"][1].get("detail").is_none());
    }

    #[test]
    fn responses_lite_groups_default_tools_and_strips_tool_output_image_details() {
        let normalized = normalize_responses_request(
            serde_json::json!({
                "model": "gpt-6-astra",
                "instructions": "Use the available tools.",
                "input": [
                    {
                        "type": "function_call_output",
                        "call_id": "call-function",
                        "output": [{
                            "type": "input_image",
                            "image_url": "data:image/png;base64,AA==",
                            "detail": "high"
                        }]
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call-custom",
                        "output": [{
                            "type": "input_image",
                            "image_url": "data:image/png;base64,BB==",
                            "detail": "original"
                        }]
                    }
                ],
                "tools": [
                    {"type": "web_search"},
                    {
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type":"object","properties":{}}
                    },
                    {
                        "type": "custom",
                        "name": "exec",
                        "description": "Run JavaScript",
                        "format": {"type":"grammar","syntax":"lark","definition":"start: /.+/"}
                    },
                    {
                        "type": "namespace",
                        "name": "functions",
                        "description": "Existing default tools",
                        "tools": [{
                            "type": "function",
                            "name": "existing",
                            "parameters": {"type":"object","properties":{}}
                        }]
                    },
                    {
                        "type": "namespace",
                        "name": "muse",
                        "tools": [{
                            "type": "function",
                            "name": "read_file",
                            "parameters": {"type":"object","properties":{}}
                        }]
                    }
                ]
            }),
            false,
            true,
            Some("session-tools"),
        )
        .expect("Responses Lite request");

        let tools = normalized["input"][0]["tools"]
            .as_array()
            .expect("additional tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "namespace");
        assert_eq!(tools[0]["name"], "functions");
        assert_eq!(tools[0]["description"], "Existing default tools");
        assert_eq!(tools[0]["tools"][0]["name"], "lookup");
        assert_eq!(tools[0]["tools"][1]["name"], "exec");
        assert_eq!(tools[0]["tools"][2]["name"], "existing");
        assert_eq!(tools[1]["name"], "muse");
        assert_eq!(normalized["reasoning"]["context"], "all_turns");
        assert_eq!(
            normalized["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert!(normalized["input"][2]["output"][0].get("detail").is_none());
        assert!(normalized["input"][3]["output"][0].get("detail").is_none());
    }

    #[test]
    fn normalizes_public_api_models_shape() {
        let models = normalize_models(include_bytes!("../tests/fixtures/api-models.json"))
            .expect("API models fixture");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-api-fixture-1");
        assert!(models[0].is_default);
        assert_eq!(models[0].display_name, None);
    }

    #[test]
    fn selects_the_first_picker_visible_model_as_default() {
        let models = normalize_models(
            br#"{
                "models": [
                    {"slug":"hidden-first","priority":1,"visibility":"hide"},
                    {
                        "slug":"visible-second",
                        "priority":2,
                        "visibility":"list",
                        "supported_reasoning_levels":[
                            {"effort":"low"},{"effort":"high"},{"effort":"low"}
                        ]
                    },
                    {"slug":"not-listed","priority":3,"visibility":"none"}
                ]
            }"#,
        )
        .expect("mixed-visibility catalog");

        assert!(!models[0].is_visible);
        assert!(!models[0].is_default);
        assert!(models[1].is_visible);
        assert!(models[1].is_default);
        assert_eq!(models[1].supported_reasoning_efforts, ["low", "high"]);
        assert!(!models[2].is_visible);
    }

    #[test]
    fn accepts_legacy_array_minimum_versions_and_filters_future_versions() {
        let models = normalize_models(
            br#"{
                "models": [
                    {"slug":"current","minimal_client_version":[0,153,4]},
                    {"slug":"future","minimal_client_version":[99,0,0]}
                ]
            }"#,
        )
        .expect("array client versions");
        assert!(models[0].is_visible);
        assert!(!models[1].is_visible);
    }

    #[test]
    fn rejects_an_empty_model_catalog() {
        assert!(matches!(
            normalize_models(br#"{"models":[]}"#),
            Err(Error::InvalidUpstreamResponse(_))
        ));
    }

    #[test]
    fn rejects_unknown_or_entirely_hidden_visibility() {
        assert!(matches!(
            normalize_models(br#"{"models":[{"slug":"future","visibility":"future"}]}"#),
            Err(Error::InvalidUpstreamResponse(message)) if message.contains("unknown visibility")
        ));
        assert!(matches!(
            normalize_models(
                br#"{"models":[{"slug":"hidden","visibility":"hide"},{"slug":"none","visibility":"none"}]}"#
            ),
            Err(Error::InvalidUpstreamResponse(message)) if message.contains("no visible models")
        ));
    }

    #[test]
    fn rejects_ambiguous_duplicate_or_inconsistent_model_catalogs() {
        for fixture in [
            br#"{"models":[{"slug":"one"}],"data":[{"id":"two"}]}"#.as_slice(),
            br#"{"models":[{"slug":"same"},{"slug":"same"}]}"#.as_slice(),
            br#"{"models":[{"slug":"bad","context_window":0}]}"#.as_slice(),
            br#"{"models":[{"slug":"bad","minimal_client_version":"not-semver"}]}"#.as_slice(),
            br#"{"models":[{"slug":"bad","input_modalities":["bad modality"]}]}"#.as_slice(),
        ] {
            assert!(matches!(
                normalize_models(fixture),
                Err(Error::InvalidUpstreamResponse(_))
            ));
        }
    }

    #[test]
    fn future_reasoning_efforts_are_filtered_without_poisoning_the_catalog() {
        let models = normalize_models(
            br#"{"models":[{"slug":"usable","supported_reasoning_levels":[{"effort":"low"},{"effort":"future"}],"default_reasoning_level":"future"}]}"#,
        )
        .expect("catalog with a future effort");
        assert_eq!(models[0].supported_reasoning_efforts, ["low"]);
        assert_eq!(models[0].default_reasoning_effort, None);
        assert!(models[0].is_visible);
    }

    #[test]
    fn fast_catalog_gating_uses_priority_or_the_pinned_legacy_marker() {
        let models = normalize_models(
            br#"{"models":[
                {"slug":"priority-model","service_tiers":[{"id":"priority"}]},
                {"slug":"request-alias-only","service_tiers":[{"id":"fast"}]},
                {"slug":"legacy-model","additional_speed_tiers":["fast"]}
            ]}"#,
        )
        .expect("Fast capability catalog");
        assert!(models[0].supports_fast_mode);
        assert!(!models[1].supports_fast_mode);
        assert!(models[2].supports_fast_mode);
    }

    #[test]
    fn custom_base_url_requires_https() {
        assert!(validate_base_url(&Url::parse("http://example.com/v1").unwrap()).is_err());
        assert!(validate_base_url(&Url::parse("https://example.com/v1").unwrap()).is_ok());
        assert!(
            validate_base_url(&Url::parse("https://example.com/v1?token=bad").unwrap()).is_err()
        );
        assert!(validate_base_url(&Url::parse("http://127.0.0.1:1234/v1").unwrap()).is_ok());
    }

    #[test]
    fn search_request_maps_to_pinned_alpha_shape() {
        let value = SearchRequest {
            query: "fixture query".to_string(),
            request_id: "request-7".to_string(),
            iteration_index: 2,
        }
        .to_upstream()
        .expect("search body");
        assert_eq!(
            value,
            serde_json::json!({"id":"request-7:2","input":"fixture query"})
        );
    }

    #[test]
    fn caller_cannot_override_credentials_or_hop_by_hop_headers() {
        let mut destination = HeaderMap::new();
        let mut source = HeaderMap::new();
        source.insert(AUTHORIZATION, HeaderValue::from_static("Bearer stolen"));
        source.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("wrong"),
        );
        source.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("also-stolen"),
        );
        source.insert(
            HeaderName::from_static("x-muse-session"),
            HeaderValue::from_static("preserved"),
        );
        source.insert(
            HeaderName::from_static(RESPONSES_LITE_HEADER),
            HeaderValue::from_static("spoofed"),
        );
        source.insert(
            HeaderName::from_static(CODEX_ROUTING_HINT_HEADER),
            HeaderValue::from_static("model=stolen;tier=priority"),
        );
        merge_safe_request_headers(&mut destination, &source);
        assert!(!destination.contains_key(AUTHORIZATION));
        assert!(!destination.contains_key("chatgpt-account-id"));
        assert!(!destination.contains_key("x-api-key"));
        assert!(!destination.contains_key(RESPONSES_LITE_HEADER));
        assert!(!destination.contains_key(CODEX_ROUTING_HINT_HEADER));
        assert_eq!(destination["x-muse-session"], "preserved");

        set_internal_responses_lite_header(&mut destination, true);
        assert_eq!(destination[RESPONSES_LITE_HEADER], "true");
        set_internal_responses_lite_header(&mut destination, false);
        assert!(!destination.contains_key(RESPONSES_LITE_HEADER));

        let trusted_hint = HeaderValue::from_static("model=gpt-fixture;tier=priority");
        set_internal_routing_hint_header(&mut destination, Some(&trusted_hint));
        assert_eq!(destination[CODEX_ROUTING_HINT_HEADER], trusted_hint);
        set_internal_routing_hint_header(&mut destination, None);
        assert!(!destination.contains_key(CODEX_ROUTING_HINT_HEADER));
    }

    #[tokio::test]
    async fn fast_api_request_emits_priority_body_without_subscription_hint() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Fast request capture listener");
        let address = listener.local_addr().expect("capture listener address");
        let capture_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept captured request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            let header_end = loop {
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("read captured request headers");
                assert_ne!(read, 0, "request ended before its headers were complete");
                request.extend_from_slice(&buffer[..read]);
            };
            let headers = std::str::from_utf8(&request[..header_end])
                .expect("captured headers are UTF-8")
                .to_string();
            let content_length = headers
                .split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .expect("captured content length");
            while request.len() < header_end + content_length {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("read captured request body");
                assert_ne!(read, 0, "request ended before its body was complete");
                request.extend_from_slice(&buffer[..read]);
            }
            let body: Value =
                serde_json::from_slice(&request[header_end..header_end + content_length])
                    .expect("captured request body is JSON");

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write capture response");
            (headers, body)
        });

        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = AuthConfig::with_home(temporary.path().join("muse-codex"))
            .expect("private auth directory");
        let transport = Transport::new(
            config,
            Some(SecretString::from("sk-fixture".to_string())),
            Some(Url::parse(&format!("http://{address}/v1")).expect("capture base URL")),
            true,
        )
        .await
        .expect("local Fast transport");
        let mut caller_headers = HeaderMap::new();
        caller_headers.insert(
            HeaderName::from_static(CODEX_ROUTING_HINT_HEADER),
            HeaderValue::from_static("model=stolen;tier=priority"),
        );
        let response = transport
            .stream_responses(
                serde_json::json!({
                    "model": "gpt-fixture",
                    "input": "hello",
                    "service_tier": "stolen"
                }),
                caller_headers,
            )
            .await
            .expect("captured Fast response");
        assert_eq!(response.status, StatusCode::OK);

        let (headers, body) = capture_task.await.expect("Fast request capture task");
        assert_eq!(body["service_tier"], "priority");
        assert!(
            !headers
                .to_ascii_lowercase()
                .contains(CODEX_ROUTING_HINT_HEADER)
        );
        assert!(!headers.contains("stolen"));
    }

    #[tokio::test]
    async fn send_json_once_emits_one_trusted_responses_lite_header() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind request capture listener");
        let address = listener.local_addr().expect("capture listener address");
        let capture_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept captured request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("read captured request");
                assert_ne!(read, 0, "request ended before its headers were complete");
                request.extend_from_slice(&buffer[..read]);
            }

            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .expect("write capture response");
            String::from_utf8(request).expect("captured request is UTF-8")
        });

        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = AuthConfig::with_home(temporary.path().join("muse-codex"))
            .expect("private auth directory");
        let transport = Transport::new(
            config,
            Some(SecretString::from("sk-fixture".to_string())),
            Some(Url::parse(&format!("http://{address}/v1")).expect("capture base URL")),
            false,
        )
        .await
        .expect("local test transport");
        let mut caller_headers = HeaderMap::new();
        caller_headers.append(
            HeaderName::from_static(RESPONSES_LITE_HEADER),
            HeaderValue::from_static("spoofed"),
        );
        caller_headers.append(
            HeaderName::from_static(RESPONSES_LITE_HEADER),
            HeaderValue::from_static("also-spoofed"),
        );

        let response = transport
            .send_json_once(
                Method::POST,
                "responses",
                &serde_json::json!({"model":"gpt-fixture","input":[]}),
                &caller_headers,
                JsonRequestOptions {
                    accept: "text/event-stream",
                    use_responses_lite: true,
                    routing_hint: None,
                },
            )
            .await
            .expect("captured response");
        assert_eq!(response.status(), StatusCode::OK);

        let request = capture_task.await.expect("request capture task");
        let header_values = request
            .split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case(RESPONSES_LITE_HEADER))
            .map(|(_, value)| value.trim())
            .collect::<Vec<_>>();
        assert_eq!(header_values, ["true"]);
        assert!(!request.contains("spoofed"));
    }

    #[test]
    fn upstream_error_summary_never_echoes_messages_or_unknown_fields() {
        let body = br#"{
            "error": {
                "type": "invalid_request_error",
                "code": "bad_request",
                "message": "Bearer super-secret-token",
                "param": "private-user-value"
            }
        }"#;
        let summary = summarize_upstream_error(body);
        assert_eq!(summary, "type=invalid_request_error, code=bad_request");
        assert!(!summary.contains("super-secret-token"));
        assert!(!summary.contains("private-user-value"));
        assert_eq!(
            summarize_upstream_error(br#"{"error":{"code":"not safe! secret"}}"#),
            "response body omitted"
        );
        assert_eq!(
            summarize_upstream_error(b"not json and contains secret"),
            "response body omitted"
        );
    }

    #[test]
    fn raw_response_debug_redacts_all_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::SET_COOKIE,
            HeaderValue::from_static("session=super-secret"),
        );
        headers.insert(
            HeaderName::from_static("x-private-account"),
            HeaderValue::from_static("account-secret"),
        );
        let response = RawResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(futures::stream::empty()),
        };

        let debug = format!("{response:?}");
        assert!(debug.contains("header_count: 2"));
        assert!(!debug.contains("super-secret"));
        assert!(!debug.contains("account-secret"));
        assert!(!debug.contains("x-private-account"));
    }

    #[test]
    fn transport_debug_redacts_auth_paths_and_custom_endpoints() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = AuthConfig::with_home(temporary.path().join("muse-codex"))
            .expect("private auth directory");
        let private_path = config.home().display().to_string();
        let transport = Transport {
            config,
            api_key_override: Some(SecretString::from("sk-super-secret".to_string())),
            api_key_base_url: Some(
                Url::parse("https://private-api.example/v1").expect("private endpoint"),
            ),
            auth_manager: None,
            client: build_http_client().expect("HTTP client"),
            model_capabilities: Arc::new(RwLock::new(BTreeMap::new())),
            fast_mode: true,
        };

        let debug = format!("{transport:?}");
        assert!(debug.contains("has_api_key_override: true"));
        assert!(debug.contains("has_custom_base_url: true"));
        assert!(debug.contains("fast_mode: true"));
        assert!(!debug.contains(&private_path));
        assert!(!debug.contains("private-api.example"));
        assert!(!debug.contains("sk-super-secret"));
    }

    #[tokio::test]
    async fn unauthorized_fixture_triggers_one_recovery_then_retries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recovery_calls = Arc::new(AtomicUsize::new(0));
        let mut responses = VecDeque::from([
            FixtureResponse {
                status: StatusCode::UNAUTHORIZED,
                body: include_str!("../tests/fixtures/unauthorized.json"),
            },
            FixtureResponse {
                status: StatusCode::OK,
                body: include_str!("../tests/fixtures/search.json"),
            },
        ]);
        let response = execute_with_401_recovery(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = responses.pop_front().expect("fixture response");
                async move { Ok(response) }
            },
            Some(Box::new(FixtureRecovery {
                remaining: 1,
                calls: Arc::clone(&recovery_calls),
            })),
            None,
        )
        .await
        .expect("retry result");

        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains("encrypted_output"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(recovery_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unauthorized_reload_and_refresh_share_a_private_process_lock() {
        let directory = tempfile::tempdir().unwrap();
        let lock_home = std::fs::canonicalize(directory.path()).unwrap();
        let lock_path = lock_home.join(CREDENTIAL_REFRESH_LOCK_NAME);
        let calls = Arc::new(AtomicUsize::new(0));
        let recovery_calls = Arc::new(AtomicUsize::new(0));
        let mut responses = VecDeque::from([
            FixtureResponse {
                status: StatusCode::UNAUTHORIZED,
                body: "reload",
            },
            FixtureResponse {
                status: StatusCode::UNAUTHORIZED,
                body: "refresh",
            },
            FixtureResponse {
                status: StatusCode::OK,
                body: "success",
            },
        ]);
        let response = execute_with_401_recovery(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = responses.pop_front().expect("fixture response");
                async move { Ok(response) }
            },
            Some(Box::new(FixtureRecovery {
                remaining: 2,
                calls: Arc::clone(&recovery_calls),
            })),
            Some(lock_home),
        )
        .await
        .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(recovery_calls.load(Ordering::SeqCst), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn unauthorized_without_or_after_recovery_is_returned_without_looping() {
        for recovery_steps in [None, Some(1_usize)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recovery_calls = Arc::new(AtomicUsize::new(0));
            let recovery = recovery_steps.map(|remaining| {
                Box::new(FixtureRecovery {
                    remaining,
                    calls: Arc::clone(&recovery_calls),
                }) as Box<dyn Recovery>
            });
            let response = execute_with_401_recovery(
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        Ok(FixtureResponse {
                            status: StatusCode::UNAUTHORIZED,
                            body: "unauthorized",
                        })
                    }
                },
                recovery,
                None,
            )
            .await
            .expect("terminal unauthorized response");

            assert_eq!(response.status, StatusCode::UNAUTHORIZED);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                recovery_steps.unwrap_or(0) + 1
            );
            assert_eq!(
                recovery_calls.load(Ordering::SeqCst),
                recovery_steps.unwrap_or(0)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_refresh_lock_rejects_a_replaced_home() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("muse-codex");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        let canonical_home = std::fs::canonicalize(&home).unwrap();
        std::fs::rename(&home, root.path().join("original")).unwrap();
        symlink(&replacement, &home).unwrap();

        let error = acquire_credential_refresh_lock_blocking(canonical_home)
            .err()
            .expect("a replaced home must be rejected");
        assert!(matches!(error, Error::CredentialRefreshLock(_)));
        assert!(!replacement.join(CREDENTIAL_REFRESH_LOCK_NAME).exists());
    }

    #[tokio::test]
    async fn prestream_5xx_fixture_is_retried_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut responses = VecDeque::from([
            FixtureResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: include_str!("../tests/fixtures/unavailable.json"),
            },
            FixtureResponse {
                status: StatusCode::OK,
                body: include_str!("../tests/fixtures/search.json"),
            },
        ]);
        let response = execute_with_401_recovery(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = responses.pop_front().expect("fixture response");
                async move { Ok(response) }
            },
            None,
            None,
        )
        .await
        .expect("retry result");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limit_response_is_not_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let response = execute_with_401_recovery(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Ok(FixtureResponse {
                        status: StatusCode::TOO_MANY_REQUESTS,
                        body: include_str!("../tests/fixtures/rate-limited.json"),
                    })
                }
            },
            None,
            None,
        )
        .await
        .expect("terminal rate limit");

        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(response.body.contains("rate_limit"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn raw_sse_fixture_is_byte_for_byte_preserved() {
        let expected = include_bytes!("../tests/fixtures/responses.sse").to_vec();
        let pieces = expected
            .chunks(7)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<Result<Bytes>>>();
        let mut body: RawBody = Box::pin(futures::stream::iter(pieces));
        let mut actual = Vec::new();
        while let Some(chunk) = body.next().await {
            actual.extend_from_slice(&chunk.expect("fixture chunk"));
        }
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn bearer_http_client_does_not_follow_redirects() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let redirect_target = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect target");
        let redirect_target_address = redirect_target.local_addr().expect("target address");
        let redirector = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirector");
        let redirector_address = redirector.local_addr().expect("redirector address");

        let redirect_task = tokio::spawn(async move {
            let (mut socket, _) = redirector.accept().await.expect("accept redirect request");
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{redirect_target_address}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write redirect");
        });

        let client = build_http_client().expect("client");
        let response = client
            .get(format!("http://{redirector_address}/start"))
            .bearer_auth("fixture-secret")
            .send()
            .await
            .expect("redirect response");
        assert_eq!(response.status(), StatusCode::FOUND);
        redirect_task.await.expect("redirect task");

        let followed = tokio::time::timeout(Duration::from_millis(100), redirect_target.accept())
            .await
            .is_ok();
        assert!(
            !followed,
            "redirect target unexpectedly received credentials"
        );
    }
}
