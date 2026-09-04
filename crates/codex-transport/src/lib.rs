//! Authentication and raw upstream transport for the Muse/Codex compatibility gateway.
//!
//! This crate deliberately owns only provider concerns. It does not parse agent
//! events, execute tools, or implement an agent loop.

mod auth;
mod error;
mod transport;

pub use auth::AuthConfig;
pub use auth::AuthStatus;
pub use auth::LoginMode;
pub use auth::LoginPrompt;
pub use auth::login;
pub use auth::login_with_prompt_handler;
pub use auth::logout;
pub use auth::set_api_key;
pub use auth::set_api_key_from_reader;
pub use auth::status;
pub use error::Error;
pub use error::Result;
pub use secrecy::SecretString;
pub use transport::ModelInfo;
pub use transport::RawBody;
pub use transport::RawResponse;
pub use transport::SearchRequest;
pub use transport::Transport;
pub use url::Url;

/// The immutable OpenAI Codex source revision used by this adapter.
pub const CODEX_GIT_REVISION: &str = "9474e5cfc4494b0ba319352aa86ce436c59e65c8";

/// Codex client version sent to upstream model discovery.
pub const CODEX_CLIENT_VERSION: &str = "0.133.0";
