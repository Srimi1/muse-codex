use http::StatusCode;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not determine a local data directory for Muse Codex")]
    DataDirectoryUnavailable,

    #[error("invalid Muse Codex home: {0}")]
    InvalidAuthHome(String),

    #[error("authentication failed: {0}")]
    Authentication(String),

    #[error(
        "no Codex credentials are stored; run `muse-codex login` or `muse-codex auth set --provider codex --api-key-stdin`"
    )]
    NotAuthenticated,

    #[error("the active credential type is not supported by this adapter")]
    UnsupportedAuthMode,

    #[error("a custom OpenAI base URL is allowed only with API-key authentication")]
    CustomBaseUrlRequiresApiKey,

    #[error("custom API base URL must use HTTPS and contain no credentials, query, or fragment")]
    InvalidBaseUrl,

    #[error("API key input is empty")]
    EmptyApiKey,

    #[error("API key input exceeds the {0}-byte limit")]
    ApiKeyTooLong(usize),

    #[error("API key input is not valid UTF-8")]
    ApiKeyNotUtf8,

    #[error("API key input contains whitespace")]
    ApiKeyContainsWhitespace,

    #[error("invalid request header: {0}")]
    InvalidHeader(String),

    #[error("failed to construct the pinned Codex provider: {0}")]
    Provider(String),

    #[error("upstream request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("could not build the pinned Codex HTTP client: {0}")]
    HttpClient(String),

    #[error("credential refresh lock failed: {0}")]
    CredentialRefreshLock(String),

    #[error("upstream returned HTTP {status}: {body}")]
    Upstream { status: StatusCode, body: String },

    #[error("upstream response was invalid: {0}")]
    InvalidUpstreamResponse(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
