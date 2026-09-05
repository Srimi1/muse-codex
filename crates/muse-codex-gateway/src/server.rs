use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::extract::Request;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_transport::{ModelInfo, Transport};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::sse;

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 64 * 1024;
pub(crate) const READY_SCHEMA_VERSION: u8 = 2;
const MODEL_CATALOG_STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const UPSTREAM_ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(5);
const PARENT_WATCH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    token: Arc<[u8]>,
    transport: Arc<Transport>,
}

#[derive(Debug, Serialize)]
struct ReadyFile<'a> {
    schema_version: u8,
    base_url: String,
    token: &'a str,
    default_model: &'a str,
    models: &'a [ModelInfo],
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartupErrorCode {
    AuthenticationRequired,
    AuthenticationFailed,
    CustomBaseUrlRequiresApiKey,
    InvalidBaseUrl,
    CredentialStoreUnavailable,
    CatalogRateLimited,
    CatalogRejected,
    CatalogInvalid,
    CatalogTimeout,
    NetworkUnavailable,
    GatewayStartFailed,
}

#[derive(Debug, Serialize)]
struct StartupErrorFile {
    schema_version: u8,
    error: StartupErrorBody,
}

#[derive(Debug, Serialize)]
struct StartupErrorBody {
    code: StartupErrorCode,
}

pub async fn run(
    bind: SocketAddr,
    ready_file: PathBuf,
    parent_pid: u32,
    transport: Transport,
) -> Result<()> {
    let token = generate_token()?;
    let catalog_models =
        tokio::time::timeout(MODEL_CATALOG_STARTUP_TIMEOUT, transport.list_models())
            .await
            .context("Codex model catalog timed out before gateway readiness")??;
    let default_model = catalog_models
        .iter()
        .find(|model| model.is_default)
        .map(|model| model.id.as_str())
        .context("Codex model catalog has no default model")?;
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind private gateway to {bind}"))?;
    let address = listener.local_addr().context("read gateway address")?;
    if !address.ip().is_loopback() {
        bail!("private gateway resolved to a non-loopback address");
    }

    write_ready_file(
        &ready_file,
        &ReadyFile {
            schema_version: READY_SCHEMA_VERSION,
            base_url: format!("http://{address}"),
            token: &token,
            default_model,
            models: &catalog_models,
        },
    )?;

    let state = AppState {
        token: Arc::from(token.as_bytes()),
        transport: Arc::new(transport),
    };
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/muse-code/models", get(models))
        .route("/responses", post(responses))
        .route("/muse-code/search", post(search))
        .route("/muse-code/browser_open", post(browser_open))
        .route("/muse-code/feedback", post(feedback))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authorization,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state);

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(parent_pid))
        .await
        .context("serve private gateway");
    let _ = fs::remove_file(&ready_file);
    result
}

pub fn generate_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("generate loopback bearer token")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

async fn health() -> Response {
    Json(json!({"status":"ok","schema_version":READY_SCHEMA_VERSION})).into_response()
}

async fn models() -> StatusCode {
    // The launcher atomically seeds Muse's isolated normalized catalog from
    // the authenticated models embedded in the private readiness file. Muse
    // 1.0.3 cannot represent multiple provider rows when optional limits are
    // unknown, so a 304 keeps that lossless cache rather than coercing unknown
    // values to fabricated numbers.
    StatusCode::NOT_MODIFIED
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    let mut request = match extract_json(payload) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if let Err(message) = prepare_responses_request(&mut request) {
        return gateway_error(StatusCode::BAD_REQUEST, message, "invalid_request");
    }
    let request_model = request
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let upstream_headers = upstream_request_headers(&headers);
    match state
        .transport
        .stream_responses(request, upstream_headers)
        .await
    {
        Ok(upstream) => proxy_sse_response(
            upstream.status,
            &upstream.headers,
            upstream.body,
            request_model.as_deref(),
        ),
        Err(_) => terminal_sse_response(
            &HeaderMap::new(),
            sse::gateway_failure_frame(
                request_model.as_deref(),
                "invalid_request",
                "The Codex transport failed before streaming began; the request was not replayed and the session is resumable.",
                None,
            ),
        ),
    }
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    let request = match extract_json(payload) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match state
        .transport
        .search_muse_request(request, upstream_request_headers(&headers))
        .await
    {
        Ok(upstream) => proxy_auxiliary_response(
            upstream.status,
            &upstream.headers,
            upstream.body,
            "Codex search was rejected by the upstream endpoint.",
            "codex_search_error",
        ),
        Err(codex_transport::Error::InvalidRequest(_)) => gateway_error(
            StatusCode::BAD_REQUEST,
            "The search request was invalid.",
            "invalid_request",
        ),
        Err(_) => gateway_error(
            StatusCode::BAD_GATEWAY,
            "Codex search failed. The session is resumable.",
            "codex_search_error",
        ),
    }
}

async fn browser_open(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    let request = match extract_json(payload) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    match state
        .transport
        .browser_open_muse_request(request, upstream_request_headers(&headers))
        .await
    {
        Ok(upstream) => proxy_auxiliary_response(
            upstream.status,
            &upstream.headers,
            upstream.body,
            "Codex browser-open was rejected by the upstream endpoint.",
            "codex_browser_open_error",
        ),
        Err(codex_transport::Error::InvalidRequest(_)) => gateway_error(
            StatusCode::BAD_REQUEST,
            "The browser-open request was invalid.",
            "invalid_request",
        ),
        Err(_) => gateway_error(
            StatusCode::BAD_GATEWAY,
            "Codex browser-open failed. The session is resumable.",
            "codex_browser_open_error",
        ),
    }
}

async fn feedback() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn extract_json(
    payload: std::result::Result<Json<Value>, JsonRejection>,
) -> std::result::Result<Value, Box<Response>> {
    payload.map(|Json(value)| value).map_err(|rejection| {
        let status = rejection.status();
        Box::new(gateway_error(
            status,
            if status == StatusCode::PAYLOAD_TOO_LARGE {
                "The gateway request body exceeded the size limit."
            } else if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
                "The gateway request must use application/json."
            } else {
                "The gateway request body was not valid JSON."
            },
            "invalid_request",
        ))
    })
}

fn prepare_responses_request(request: &mut Value) -> std::result::Result<(), &'static str> {
    let object = request
        .as_object_mut()
        .ok_or("The Responses request must be a JSON object.")?;
    if object.get("stream") == Some(&Value::Bool(false)) {
        return Err("muse-codex requires streaming Responses requests.");
    }
    if object.get("background") == Some(&Value::Bool(true)) {
        return Err("muse-codex does not support background Responses requests.");
    }

    // The auth-aware transport validates provider-side history handles. Leave
    // them intact here so a non-null dependency is rejected rather than
    // silently dropping context; null handles are removed by the transport.
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    Ok(())
}

async fn require_authorization(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !bearer_matches(&state.token, request.headers()) {
        let mut response = gateway_error(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid private gateway bearer token.",
            "unauthorized",
        );
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            "Bearer realm=\"muse-codex-gateway\""
                .parse()
                .expect("static WWW-Authenticate header is valid"),
        );
        return response;
    }
    next.run(request).await
}

fn bearer_matches(expected: &[u8], headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let supplied = candidate.as_bytes();
    expected.len() == supplied.len() && bool::from(expected.ct_eq(supplied))
}

fn upstream_request_headers(source: &HeaderMap) -> HeaderMap {
    let mut target = HeaderMap::new();
    for name in ["accept-language", "x-request-id"] {
        if let (Ok(name), Some(value)) = (HeaderName::from_bytes(name.as_bytes()), source.get(name))
        {
            target.insert(name, value.clone());
        }
    }
    target
}

fn proxy_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: impl futures_util::Stream<Item = Result<bytes::Bytes, codex_transport::Error>>
    + Send
    + 'static,
) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(CACHE_CONTROL, "no-store")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff");
    for (name, value) in headers {
        if is_forwarded_response_header(name) {
            response = response.header(name, value);
        }
    }
    response.body(Body::from_stream(body)).unwrap_or_else(|_| {
        gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to build the streamed response.",
            "gateway_error",
        )
    })
}

fn proxy_auxiliary_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: codex_transport::RawBody,
    failure_message: &'static str,
    failure_kind: &'static str,
) -> Response {
    if status.is_success() {
        return proxy_response(status, headers, body);
    }

    // Search and browser-open failures are JSON gateway errors. Do not expose
    // an upstream body, which may contain echoed input or account details.
    drop(body);
    let mut response = gateway_error(status, failure_message, failure_kind);
    for (name, value) in headers {
        if name != CONTENT_TYPE && is_forwarded_response_header(name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

fn proxy_sse_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: codex_transport::RawBody,
    request_model: Option<&str>,
) -> Response {
    if !status.is_success() {
        let model = request_model.map(ToOwned::to_owned);
        return terminal_sse_stream(
            headers,
            futures_util::stream::once(async move {
                let failure = inspect_upstream_failure(status, body).await;
                Ok(sse::gateway_failure_frame(
                    model.as_deref(),
                    failure.code,
                    &failure.message,
                    failure.parameter.as_deref(),
                ))
            }),
        );
    }
    // Some compatible endpoints mislabel a valid streaming body. The typed
    // SSE validator, not the MIME header, is authoritative. Force the private
    // downstream contract to event-stream and retain only a bounded safe media
    // type for a failure diagnostic if the bytes do not validate.
    let media_type = safe_upstream_media_type(headers);
    let mut downstream_headers = headers.clone();
    downstream_headers.insert(
        CONTENT_TYPE,
        "text/event-stream".parse().expect("static header"),
    );
    proxy_response(
        StatusCode::OK,
        &downstream_headers,
        sse::validated_stream_with_media_type(body, media_type),
    )
}

fn terminal_sse_response(headers: &HeaderMap, frame: bytes::Bytes) -> Response {
    terminal_sse_stream(
        headers,
        futures_util::stream::once(async move { Ok(frame) }),
    )
}

fn terminal_sse_stream(
    headers: &HeaderMap,
    body: impl futures_util::Stream<Item = Result<bytes::Bytes, codex_transport::Error>>
    + Send
    + 'static,
) -> Response {
    let mut headers = headers.clone();
    headers.insert(
        CONTENT_TYPE,
        "text/event-stream".parse().expect("static header"),
    );
    proxy_response(StatusCode::OK, &headers, body)
}

#[derive(Debug, PartialEq, Eq)]
struct SanitizedUpstreamFailure {
    code: &'static str,
    message: String,
    parameter: Option<String>,
}

async fn inspect_upstream_failure(
    status: StatusCode,
    body: codex_transport::RawBody,
) -> SanitizedUpstreamFailure {
    let details = tokio::time::timeout(UPSTREAM_ERROR_BODY_TIMEOUT, async move {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.ok()?;
            let new_len = bytes.len().checked_add(chunk.len())?;
            if new_len > MAX_UPSTREAM_ERROR_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&bytes).ok()
    })
    .await
    .ok()
    .flatten();

    sanitize_upstream_failure(status, details.as_ref())
}

fn sanitize_upstream_failure(status: StatusCode, body: Option<&Value>) -> SanitizedUpstreamFailure {
    let error = body
        .and_then(|body| body.get("error"))
        .unwrap_or(&Value::Null);
    let upstream_code = error.get("code").and_then(Value::as_str);
    let parameter = error
        .get("param")
        .and_then(Value::as_str)
        .filter(|parameter| safe_error_parameter(parameter))
        .map(ToOwned::to_owned);

    let code = match upstream_code {
        Some("context_length_exceeded") => "context_length_exceeded",
        Some("rate_limit_exceeded" | "usage_limit_reached") => "rate_limit_exceeded",
        Some("invalid_api_key") => "invalid_api_key",
        Some("model_not_found") => "model_not_found",
        Some("insufficient_quota") => "insufficient_quota",
        Some("invalid_request") => "invalid_request",
        _ if status == StatusCode::TOO_MANY_REQUESTS => "rate_limit_exceeded",
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            "invalid_api_key"
        }
        // Keep unknown and server failures non-retryable in stock Muse. Only
        // the small semantic allowlist above crosses the trust boundary.
        _ => "invalid_request",
    };

    let message = match code {
        "context_length_exceeded" => format!(
            "The request exceeded the model context window (HTTP {}); it was not replayed and the session is resumable.",
            status.as_u16()
        ),
        "rate_limit_exceeded" => format!(
            "The Codex endpoint reported a rate limit (HTTP {}); the request was not replayed and the session is resumable.",
            status.as_u16()
        ),
        "invalid_api_key" => format!(
            "The Codex endpoint rejected authentication (HTTP {}); the request was not replayed and login is required.",
            status.as_u16()
        ),
        "model_not_found" => format!(
            "The requested Codex model was unavailable (HTTP {}); the request was not replayed and the session is resumable.",
            status.as_u16()
        ),
        "insufficient_quota" => format!(
            "The Codex endpoint reported insufficient quota (HTTP {}); the request was not replayed and the session is resumable.",
            status.as_u16()
        ),
        _ if parameter.is_some() => format!(
            "The Codex endpoint rejected request parameter '{}' (HTTP {}); the request was not replayed and the session is resumable.",
            parameter.as_deref().unwrap_or_default(),
            status.as_u16()
        ),
        _ if status.is_client_error() => format!(
            "The Codex endpoint rejected the request (HTTP {}); the request was not replayed and the session is resumable.",
            status.as_u16()
        ),
        _ => format!(
            "The Codex endpoint failed (HTTP {}); the request was not replayed and the session is resumable.",
            status.as_u16()
        ),
    };

    SanitizedUpstreamFailure {
        code,
        message,
        parameter,
    }
}

fn safe_error_parameter(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'[' | b']')
        })
}

fn safe_upstream_media_type(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)?;
    (!value.is_empty()
        && value.len() <= 127
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'.' | b'/')))
    .then(|| value.to_ascii_lowercase())
}

fn is_forwarded_response_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name == CONTENT_TYPE.as_str()
        || matches!(
            name,
            "retry-after"
                | "retry-after-ms"
                | "x-request-id"
                | "x-oai-request-id"
                | "openai-model"
                | "x-openai-model"
                | "x-reasoning-included"
                | "x-models-etag"
        )
        || name.starts_with("x-ratelimit-")
        || is_codex_usage_header(name)
}

fn is_codex_usage_header(name: &str) -> bool {
    name == "x-codex-active-limit"
        || name == "x-codex-primary-over-secondary-limit-percent"
        || name == "x-codex-promo-message"
        || name.starts_with("x-codex-credits-")
        || [
            "-limit-name",
            "-primary-used-percent",
            "-primary-window-minutes",
            "-primary-reset-at",
            "-secondary-used-percent",
            "-secondary-window-minutes",
            "-secondary-reset-at",
        ]
        .iter()
        .any(|suffix| name.starts_with("x-codex-") && name.ends_with(suffix))
}

fn gateway_error(status: StatusCode, message: &str, kind: &str) -> Response {
    let mut response = (
        status,
        Json(json!({"error":{"message":message,"type":kind}})),
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().expect("static header"));
    response.headers_mut().insert(
        X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header"),
    );
    response
}

fn write_ready_file(path: &Path, ready: &ReadyFile<'_>) -> Result<()> {
    write_private_json(path, ready)
}

pub fn startup_error_path(ready_file: &Path) -> PathBuf {
    let mut path = OsString::from(ready_file.as_os_str());
    path.push(".error");
    PathBuf::from(path)
}

pub fn write_startup_error_file(ready_file: &Path, code: StartupErrorCode) -> Result<PathBuf> {
    let path = startup_error_path(ready_file);
    write_private_json(
        &path,
        &StartupErrorFile {
            schema_version: 1,
            error: StartupErrorBody { code },
        },
    )?;
    Ok(path)
}

/// Starts a process-lifetime watchdog before any potentially blocking startup
/// work. The launcher is the credential and gateway lifetime authority, so an
/// orphaned gateway must not survive it or wait indefinitely for graceful SSE
/// shutdown.
pub fn start_parent_exit_watchdog(parent_pid: u32, ready_file: PathBuf) -> Result<()> {
    if !process_exists(parent_pid) {
        bail!("--parent-pid must name a live process");
    }
    std::thread::Builder::new()
        .name("muse-codex-parent-watch".to_string())
        .spawn(move || {
            loop {
                if !process_exists(parent_pid) {
                    let _ = fs::remove_file(&ready_file);
                    let _ = fs::remove_file(startup_error_path(&ready_file));
                    std::process::exit(0);
                }
                std::thread::sleep(PARENT_WATCH_INTERVAL);
            }
        })
        .context("start launcher-liveness watchdog")?;
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("private JSON file has no parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent).context("create ready-file directory")?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .context("secure ready-file directory")?;
    }
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("ready"),
        std::process::id()
    ));
    let bytes = serde_json::to_vec(value).context("encode private JSON file")?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .context("create private JSON file")?;
        file.write_all(&bytes).context("write private JSON file")?;
        file.sync_all().context("sync private JSON file")?;
        drop(file);
        fs::rename(&temporary, path).context("publish private JSON file")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

async fn shutdown_signal(parent_pid: u32) {
    tokio::select! {
        _ = async {
            let _ = tokio::signal::ctrl_c().await;
        } => {},
        _ = wait_for_parent_exit(parent_pid) => {},
    }
}

async fn wait_for_parent_exit(parent_pid: u32) {
    loop {
        if !process_exists(parent_pid) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs an existence/permission check and does not signal the process.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_have_256_bits() {
        let token = generate_token().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(token).unwrap().len(), 32);
    }

    #[test]
    fn bearer_auth_is_exact_and_case_sensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer exact-secret".parse().unwrap());
        assert!(bearer_matches(b"exact-secret", &headers));
        assert!(!bearer_matches(b"Exact-secret", &headers));
        headers.insert(AUTHORIZATION, "Basic exact-secret".parse().unwrap());
        assert!(!bearer_matches(b"exact-secret", &headers));
    }

    #[test]
    fn ready_file_is_private_and_valid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.json");
        let models = vec![ModelInfo {
            id: "gpt-test".to_string(),
            display_name: Some("GPT Test".to_string()),
            description: None,
            context_window: Some(100_000),
            max_output_tokens: None,
            supported_reasoning_efforts: vec!["medium".to_string()],
            default_reasoning_effort: Some("medium".to_string()),
            is_visible: true,
            is_default: true,
            use_responses_lite: false,
            tool_mode: None,
            input_modalities: vec!["text".to_string()],
            supported_in_api: true,
        }];
        write_ready_file(
            &path,
            &ReadyFile {
                schema_version: READY_SCHEMA_VERSION,
                base_url: "http://127.0.0.1:1234".to_string(),
                token: "secret",
                default_model: "gpt-test",
                models: &models,
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let parsed: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], READY_SCHEMA_VERSION);
        assert_eq!(parsed["default_model"], "gpt-test");
        assert_eq!(parsed["models"][0]["id"], "gpt-test");
        assert!(parsed["models"][0]["max_output_tokens"].is_null());
    }

    #[test]
    fn startup_error_file_is_atomic_private_and_contains_only_a_static_code() {
        let directory = tempfile::tempdir().unwrap();
        let ready_file = directory.path().join("ready.json");
        let path =
            write_startup_error_file(&ready_file, StartupErrorCode::CustomBaseUrlRequiresApiKey)
                .unwrap();

        assert_eq!(path, directory.path().join("ready.json.error"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
            json!({
                "schema_version": 1,
                "error": {"code": "custom_base_url_requires_api_key"}
            })
        );
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn process_probe_rejects_unrepresentable_pids() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(u32::MAX));
    }

    #[tokio::test]
    async fn model_endpoint_preserves_the_launcher_seeded_cache() {
        assert_eq!(models().await, StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn responses_request_is_forced_streaming_without_hiding_history_dependencies() {
        let mut request = json!({
            "model": "gpt-test",
            "input": [{"role":"user","content":"hello"}],
            "store": true,
            "stream": true,
            "previous_response_id": "resp-stateful",
            "conversation": "conv-stateful",
        });
        prepare_responses_request(&mut request).unwrap();
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);
        assert_eq!(request["previous_response_id"], "resp-stateful");
        assert_eq!(request["conversation"], "conv-stateful");
        assert_eq!(request["input"][0]["content"], "hello");
    }

    #[test]
    fn responses_request_rejects_non_object_non_streaming_and_background_modes() {
        assert!(prepare_responses_request(&mut json!([])).is_err());
        assert!(prepare_responses_request(&mut json!({"stream": false})).is_err());
        assert!(prepare_responses_request(&mut json!({"background": true})).is_err());
    }

    #[test]
    fn upstream_media_type_is_bounded_and_drops_parameters() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        assert_eq!(
            safe_upstream_media_type(&headers).as_deref(),
            Some("text/event-stream")
        );
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        assert_eq!(
            safe_upstream_media_type(&headers).as_deref(),
            Some("application/json")
        );
        headers.insert(
            CONTENT_TYPE,
            "application/json; token=must-not-appear".parse().unwrap(),
        );
        assert_eq!(
            safe_upstream_media_type(&headers).as_deref(),
            Some("application/json")
        );
        headers.insert(CONTENT_TYPE, "invalid mime".parse().unwrap());
        assert_eq!(safe_upstream_media_type(&headers), None);
        headers.remove(CONTENT_TYPE);
        assert_eq!(safe_upstream_media_type(&headers), None);
    }

    #[test]
    fn response_header_allowlist_keeps_protocol_metadata_and_drops_secrets() {
        for name in [
            "content-type",
            "retry-after",
            "retry-after-ms",
            "x-request-id",
            "x-oai-request-id",
            "openai-model",
            "x-openai-model",
            "x-reasoning-included",
            "x-models-etag",
            "x-ratelimit-remaining-requests",
            "x-codex-primary-used-percent",
            "x-codex-bengalfox-secondary-reset-at",
        ] {
            assert!(
                is_forwarded_response_header(&HeaderName::from_bytes(name.as_bytes()).unwrap()),
                "expected {name} to be forwarded"
            );
        }
        for name in [
            "authorization",
            "set-cookie",
            "proxy-authenticate",
            "openai-organization",
            "x-codex-turn-state",
            "x-codex-inference-call-id",
            "x-codex-parent-thread-id",
            "x-codex-installation-id",
            "content-length",
            "connection",
        ] {
            assert!(
                !is_forwarded_response_header(&HeaderName::from_bytes(name.as_bytes()).unwrap()),
                "expected {name} to be dropped"
            );
        }
    }

    #[tokio::test]
    async fn successful_non_sse_response_is_rejected_without_reflecting_its_body() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        let raw: codex_transport::RawBody = Box::pin(futures_util::stream::iter(vec![Ok(
            bytes::Bytes::from_static(b"sensitive upstream body"),
        )]));
        let response = proxy_sse_response(StatusCode::OK, &headers, raw, Some("gpt-test"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(
            !body
                .windows(b"sensitive".len())
                .any(|window| window == b"sensitive")
        );
        assert!(
            body.windows(b"upstream media type application/json".len())
                .any(|window| window == b"upstream media type application/json")
        );
    }

    #[tokio::test]
    async fn valid_typed_sse_is_accepted_despite_json_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        let bytes = bytes::Bytes::from_static(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-ok\"}}\n\n",
        );
        let raw: codex_transport::RawBody =
            Box::pin(futures_util::stream::iter(vec![Ok(bytes.clone())]));
        let response = proxy_sse_response(StatusCode::OK, &headers, raw, Some("gpt-test"));
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let output = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(output, bytes);
    }

    #[tokio::test]
    async fn empty_204_is_clamped_to_200_and_emits_a_terminal_failure_body() {
        let raw: codex_transport::RawBody = Box::pin(futures_util::stream::empty());
        let response = proxy_sse_response(StatusCode::NO_CONTENT, &HeaderMap::new(), raw, None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert_eq!(text.matches("event: response.failed").count(), 1);
        assert!(text.contains("ended before a terminal response event"));
    }

    #[tokio::test]
    async fn upstream_http_failures_become_one_sanitized_terminal_sse_event() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("set-cookie", "secret=value".parse().unwrap());
        let raw: codex_transport::RawBody = Box::pin(futures_util::stream::iter(vec![Ok(
            bytes::Bytes::from_static(b"{\"error\":\"upstream secret\"}"),
        )]));
        let response = proxy_sse_response(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            raw,
            Some("gpt-test"),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        assert_eq!(response.headers()["retry-after"], "30");
        assert!(!response.headers().contains_key("set-cookie"));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert_eq!(text.matches("event: response.failed").count(), 1);
        assert!(text.contains("HTTP 429"));
        assert!(text.contains("\"model\":\"gpt-test\""));
        assert!(!text.contains("upstream secret"));
    }

    #[tokio::test]
    async fn auxiliary_http_failures_do_not_reflect_upstream_bodies_or_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        headers.insert("retry-after", "2".parse().unwrap());
        headers.insert("set-cookie", "private=value".parse().unwrap());
        let raw: codex_transport::RawBody = Box::pin(futures_util::stream::iter(vec![Ok(
            bytes::Bytes::from_static(b"sensitive search response"),
        )]));
        let response = proxy_auxiliary_response(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            raw,
            "Codex search was rejected by the upstream endpoint.",
            "codex_search_error",
        );
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()["retry-after"], "2");
        assert!(!response.headers().contains_key("set-cookie"));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("codex_search_error"));
        assert!(!text.contains("sensitive search response"));
        assert!(!text.contains("private=value"));
    }

    #[test]
    fn upstream_error_classifier_preserves_only_safe_actionable_fields() {
        let body = json!({
            "error": {
                "code": "context_length_exceeded",
                "param": "input[12].content",
                "message": "secret prompt and bearer token must never be reflected"
            }
        });
        let failure = sanitize_upstream_failure(StatusCode::BAD_REQUEST, Some(&body));
        assert_eq!(failure.code, "context_length_exceeded");
        assert_eq!(failure.parameter.as_deref(), Some("input[12].content"));
        assert!(failure.message.contains("context window"));
        assert!(!failure.message.contains("secret prompt"));

        let hostile = json!({
            "error": {
                "code": "future_retryable_error",
                "param": "input\nBearer secret",
                "message": "do not reflect me"
            }
        });
        let failure = sanitize_upstream_failure(StatusCode::INTERNAL_SERVER_ERROR, Some(&hostile));
        assert_eq!(failure.code, "invalid_request");
        assert_eq!(failure.parameter, None);
        assert!(!failure.message.contains("future_retryable_error"));
        assert!(!failure.message.contains("Bearer"));
    }

    #[tokio::test]
    async fn upstream_context_failure_body_is_bounded_parsed_and_sanitized() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        let raw: codex_transport::RawBody = Box::pin(futures_util::stream::iter(vec![Ok(
            bytes::Bytes::from_static(
                br#"{"error":{"code":"context_length_exceeded","param":"input[0]","message":"sensitive body"}}"#,
            ),
        )]));
        let response = proxy_sse_response(StatusCode::BAD_REQUEST, &headers, raw, Some("gpt-test"));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("\"code\":\"context_length_exceeded\""));
        assert!(text.contains("\"param\":\"input[0]\""));
        assert!(!text.contains("sensitive body"));
    }
}
