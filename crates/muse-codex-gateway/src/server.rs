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
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_transport::{ModelInfo, Transport};
use serde::Serialize;
use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::sse;

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

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
}

pub async fn run(
    bind: SocketAddr,
    ready_file: PathBuf,
    parent_pid: u32,
    transport: Transport,
) -> Result<()> {
    let token = generate_token()?;
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
            schema_version: 1,
            base_url: format!("http://{address}"),
            token: &token,
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
    Json(json!({"status":"ok","schema_version":1})).into_response()
}

async fn models(State(state): State<AppState>) -> Response {
    match state.transport.list_models().await {
        Ok(models) => Json(catalog(models)).into_response(),
        Err(_) => gateway_error(
            StatusCode::BAD_GATEWAY,
            "Unable to load the Codex model catalog. Run `muse-codex login` and retry.",
            "codex_catalog_error",
        ),
    }
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<Value>,
) -> Response {
    let Some(object) = request.as_object_mut() else {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "The Responses request must be a JSON object.",
            "invalid_request",
        );
    };
    object.insert("store".to_string(), Value::Bool(false));
    if object.get("stream") == Some(&Value::Bool(false)) {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "muse-codex requires streaming Responses requests.",
            "invalid_request",
        );
    }
    object.insert("stream".to_string(), Value::Bool(true));

    let upstream_headers = upstream_request_headers(&headers);
    match state
        .transport
        .stream_responses(request, upstream_headers)
        .await
    {
        Ok(upstream) => proxy_sse_response(upstream.status, &upstream.headers, upstream.body),
        Err(_) => gateway_error(
            StatusCode::BAD_GATEWAY,
            "The Codex response transport failed before streaming began. The session is resumable.",
            "codex_transport_error",
        ),
    }
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    match state
        .transport
        .search_muse_request(request, upstream_request_headers(&headers))
        .await
    {
        Ok(upstream) => proxy_response(upstream.status, &upstream.headers, upstream.body),
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
    Json(request): Json<Value>,
) -> Response {
    match state
        .transport
        .browser_open_muse_request(request, upstream_request_headers(&headers))
        .await
    {
        Ok(upstream) => proxy_response(upstream.status, &upstream.headers, upstream.body),
        Err(_) => gateway_error(
            StatusCode::BAD_GATEWAY,
            "Codex browser-open failed. The session is resumable.",
            "codex_search_error",
        ),
    }
}

async fn feedback() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn require_authorization(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !bearer_matches(&state.token, request.headers()) {
        return gateway_error(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid private gateway bearer token.",
            "unauthorized",
        );
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
    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        let allowed = name == CONTENT_TYPE
            || name.as_str() == "retry-after"
            || name.as_str() == "x-request-id"
            || name.as_str().starts_with("x-ratelimit-")
            || name.as_str().starts_with("x-codex-")
            || name.as_str().starts_with("openai-");
        if allowed {
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

fn proxy_sse_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: codex_transport::RawBody,
) -> Response {
    if !status.is_success() {
        return proxy_response(status, headers, body);
    }
    proxy_response(status, headers, sse::validated_stream(body))
}

fn catalog(models: Vec<ModelInfo>) -> Value {
    let data = models.into_iter().map(model_entry).collect::<Vec<_>>();
    json!({"object":"list","data":data})
}

fn model_entry(model: ModelInfo) -> Value {
    let mut metadata = Map::new();
    metadata.insert("is_hidden".to_string(), Value::Bool(false));
    if let Some(description) = model.description {
        metadata.insert("description".to_string(), Value::String(description));
    }

    let mut limit = Map::new();
    if let Some(context) = model.context_window {
        limit.insert("context".to_string(), Value::from(context));
    }
    if let Some(output) = model.max_output_tokens {
        limit.insert("output".to_string(), Value::from(output));
    }
    if !limit.is_empty() {
        metadata.insert("limit".to_string(), Value::Object(limit));
    }
    if !model.supported_reasoning_efforts.is_empty() {
        metadata.insert(
            "reasoning".to_string(),
            json!({
                "efforts": model.supported_reasoning_efforts,
                "default": model.default_reasoning_effort,
            }),
        );
    }

    let mut entry = Map::new();
    entry.insert("id".to_string(), Value::String(model.id));
    entry.insert("object".to_string(), Value::String("model".to_string()));
    if let Some(label) = model.display_name {
        entry.insert("display_name".to_string(), Value::String(label));
    }
    entry.insert(
        "metadata".to_string(),
        json!({"muse-code": Value::Object(metadata)}),
    );
    Value::Object(entry)
}

fn gateway_error(status: StatusCode, message: &str, kind: &str) -> Response {
    (
        status,
        Json(json!({"error":{"message":message,"type":kind}})),
    )
        .into_response()
}

fn write_ready_file(path: &Path, ready: &ReadyFile<'_>) -> Result<()> {
    let parent = path
        .parent()
        .context("ready file has no parent directory")?;
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
    let bytes = serde_json::to_vec(ready).context("encode ready file")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("create private ready file")?;
    file.write_all(&bytes).context("write ready file")?;
    file.sync_all().context("sync ready file")?;
    drop(file);
    fs::rename(&temporary, path).context("publish ready file")?;
    Ok(())
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
    // SAFETY: signal 0 performs an existence/permission check and does not signal the process.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
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
        write_ready_file(
            &path,
            &ReadyFile {
                schema_version: 1,
                base_url: "http://127.0.0.1:1234".to_string(),
                token: "secret",
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let parsed: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
    }

    #[test]
    fn custom_catalog_never_fabricates_limits() {
        let value = model_entry(ModelInfo {
            id: "gpt-test".to_string(),
            display_name: None,
            description: None,
            context_window: None,
            max_output_tokens: None,
            supported_reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            is_default: false,
        });
        assert!(value["metadata"]["muse-code"].get("limit").is_none());
    }
}
