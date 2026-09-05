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
pub const CODEX_GIT_REVISION: &str = "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a";

/// Responses/catalog compatibility level implemented by this adapter.
///
/// Bump this only with the pinned source revision and after the corresponding
/// model request shapes pass fixture and live compatibility tests.
pub const CODEX_WIRE_COMPATIBILITY_VERSION: &str = "0.153.4";
