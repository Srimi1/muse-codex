mod server;
mod sse;

use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use codex_transport::{
    AuthConfig, AuthStatus, LoginMode, LoginPrompt, login, login_with_prompt_handler, logout,
    set_api_key_from_reader, status,
};
use secrecy::SecretString;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "muse-codex-gateway",
    version,
    about = "Private Codex transport for muse-codex"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve(ServeArgs),
    Auth(AuthArgs),
    SelfTest,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    #[arg(long)]
    ready_file: PathBuf,
    #[arg(long)]
    parent_pid: u32,
    /// Read an invocation-only OpenAI API key from stdin.
    #[arg(long)]
    api_key_stdin: bool,
    /// Override the OpenAI API endpoint. Accepted only with API-key authentication.
    #[arg(long)]
    upstream_base_url: Option<Url>,
}

#[derive(Debug, Args)]
struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Login {
        #[arg(long)]
        device_auth: bool,
    },
    Logout,
    SetApiKey,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => serve_with_error_reporting(args).await,
        Command::Auth(args) => auth(args).await,
        Command::SelfTest => self_test(),
    }
}

async fn serve_with_error_reporting(args: ServeArgs) -> Result<()> {
    let ready_file = args.ready_file.clone();
    match serve(args).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let code = classify_startup_error(&error);
            let _ = server::write_startup_error_file(&ready_file, code);
            Err(error)
        }
    }
}

fn classify_startup_error(error: &anyhow::Error) -> server::StartupErrorCode {
    use codex_transport::Error as TransportError;
    use server::StartupErrorCode;

    if error
        .chain()
        .any(|cause| cause.is::<tokio::time::error::Elapsed>())
    {
        return StartupErrorCode::CatalogTimeout;
    }

    for cause in error.chain() {
        let Some(error) = cause.downcast_ref::<TransportError>() else {
            continue;
        };
        return match error {
            TransportError::NotAuthenticated => StartupErrorCode::AuthenticationRequired,
            TransportError::Authentication(_)
            | TransportError::UnsupportedAuthMode
            | TransportError::EmptyApiKey
            | TransportError::ApiKeyTooLong(_)
            | TransportError::ApiKeyNotUtf8
            | TransportError::ApiKeyContainsWhitespace
            | TransportError::ApiKeyContainsControlCharacters => {
                StartupErrorCode::AuthenticationFailed
            }
            TransportError::CustomBaseUrlRequiresApiKey => {
                StartupErrorCode::CustomBaseUrlRequiresApiKey
            }
            TransportError::InvalidBaseUrl => StartupErrorCode::InvalidBaseUrl,
            TransportError::DataDirectoryUnavailable
            | TransportError::InvalidAuthHome(_)
            | TransportError::CredentialRefreshLock(_)
            | TransportError::Io(_) => StartupErrorCode::CredentialStoreUnavailable,
            TransportError::Upstream { status, .. }
                if *status == http::StatusCode::UNAUTHORIZED
                    || *status == http::StatusCode::FORBIDDEN =>
            {
                StartupErrorCode::AuthenticationFailed
            }
            TransportError::Upstream { status, .. }
                if *status == http::StatusCode::TOO_MANY_REQUESTS =>
            {
                StartupErrorCode::CatalogRateLimited
            }
            TransportError::Upstream { status, .. } if status.is_client_error() => {
                StartupErrorCode::CatalogRejected
            }
            TransportError::Upstream { .. } | TransportError::Http(_) => {
                StartupErrorCode::NetworkUnavailable
            }
            TransportError::InvalidUpstreamResponse(_) => StartupErrorCode::CatalogInvalid,
            TransportError::InvalidRequest(_)
            | TransportError::InvalidHeader(_)
            | TransportError::Provider(_)
            | TransportError::HttpClient(_) => StartupErrorCode::GatewayStartFailed,
        };
    }
    server::StartupErrorCode::GatewayStartFailed
}

async fn serve(args: ServeArgs) -> Result<()> {
    if !args.bind.ip().is_loopback() {
        bail!("refusing to expose the private gateway on a non-loopback address");
    }
    if args.parent_pid == 0 {
        bail!("--parent-pid must be a live process id");
    }

    // This OS thread deliberately starts before stdin, Keychain, or network
    // access. If the launcher disappears while any of those operations is
    // blocked, it terminates the gateway process rather than waiting for the
    // async runtime (or a blocking credential task) to unwind.
    server::start_parent_exit_watchdog(args.parent_pid, args.ready_file.clone())?;

    let invocation_key = if args.api_key_stdin {
        Some(read_secret_line(&mut io::stdin().lock())?)
    } else {
        None
    };
    let auth = AuthConfig::for_muse_codex()?;
    let transport = codex_transport::Transport::new(auth, invocation_key, args.upstream_base_url)
        .await
        .context("initialize Codex transport")?;
    server::run(args.bind, args.ready_file, args.parent_pid, transport).await
}

async fn auth(args: AuthArgs) -> Result<()> {
    let config = AuthConfig::for_muse_codex()?;
    match args.command {
        AuthCommand::Login { device_auth } => {
            let result = if device_auth {
                login_with_prompt_handler(&config, LoginMode::Device, |prompt| match prompt {
                    LoginPrompt::Device {
                        verification_url,
                        user_code,
                    } => {
                        println!("Open {verification_url} and enter code {user_code}");
                    }
                    LoginPrompt::Browser { .. } => unreachable!("device login prompt"),
                })
                .await?
            } else {
                // The pinned upstream implementation opens the system browser and
                // owns the localhost OAuth callback lifecycle.
                login(&config, LoginMode::Browser).await?
            };
            print_auth_status(&result);
        }
        AuthCommand::Logout => {
            if logout(&config).await? {
                println!("Logged out and removed muse-codex credentials.");
            } else {
                println!("No saved muse-codex credentials were found.");
            }
        }
        AuthCommand::SetApiKey => {
            set_api_key_from_reader(&config, &mut io::stdin().lock())?;
            println!("OpenAI API key saved in macOS Keychain.");
        }
        AuthCommand::Status => print_auth_status(&status(&config).await?),
    }
    Ok(())
}

fn print_auth_status(status: &AuthStatus) {
    match status {
        AuthStatus::SignedOut => println!("Not signed in."),
        AuthStatus::ApiKey => println!("Signed in with an OpenAI API key."),
        AuthStatus::ChatGpt { email, plan } => {
            print!("Signed in with ChatGPT");
            if let Some(email) = email {
                print!(" as {email}");
            }
            if let Some(plan) = plan {
                print!(" ({plan})");
            }
            println!(".");
        }
    }
}

fn read_secret_line(reader: &mut dyn io::BufRead) -> Result<SecretString> {
    const MAX_API_KEY_BYTES: usize = 16 * 1024;
    let mut bytes = Vec::with_capacity(256);
    reader
        .take((MAX_API_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read API key from stdin")?;
    if bytes.len() > MAX_API_KEY_BYTES {
        bail!("API key stdin exceeds {MAX_API_KEY_BYTES} bytes");
    }
    let value = String::from_utf8(bytes).context("API key stdin is not UTF-8")?;
    // A terminal contributes line endings, but silently trimming any other
    // character could turn an accidental paste into a different credential.
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        bail!("API key stdin was empty");
    }
    if value.chars().any(char::is_whitespace) {
        bail!("API key stdin contains whitespace");
    }
    if value.chars().any(char::is_control) {
        bail!("API key stdin contains control characters");
    }
    Ok(SecretString::from(value.to_owned()))
}

fn self_test() -> Result<()> {
    let encoded = server::generate_token()?;
    if base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &encoded)
        .map_or(true, |bytes| bytes.len() != 32)
    {
        bail!("secure token generator self-test failed");
    }
    println!("muse-codex-gateway self-test: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::Duration;

    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn secret_input_strips_only_terminal_line_endings() {
        let mut input = Cursor::new(b"sk-test-secret\r\n");
        let secret = read_secret_line(&mut input).unwrap();
        assert_eq!(secret.expose_secret(), "sk-test-secret");

        for bytes in [
            b" sk-test-secret\n".as_slice(),
            b"sk-test-secret \n".as_slice(),
            b"sk-test\tsecret\n".as_slice(),
            b"sk-test\0secret\n".as_slice(),
        ] {
            assert!(read_secret_line(&mut Cursor::new(bytes)).is_err());
        }
    }

    #[test]
    fn empty_secret_is_rejected() {
        let mut input = Cursor::new(b"\n");
        assert!(read_secret_line(&mut input).is_err());
    }

    #[test]
    fn startup_errors_are_classified_without_exposing_details() {
        use codex_transport::Error as TransportError;

        let error = anyhow::Error::new(TransportError::NotAuthenticated);
        assert_eq!(
            classify_startup_error(&error),
            server::StartupErrorCode::AuthenticationRequired
        );

        let error = anyhow::Error::new(TransportError::CustomBaseUrlRequiresApiKey);
        assert_eq!(
            classify_startup_error(&error),
            server::StartupErrorCode::CustomBaseUrlRequiresApiKey
        );

        let error = anyhow::Error::new(TransportError::Upstream {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            summary: "untrusted upstream detail".to_string(),
        });
        assert_eq!(
            classify_startup_error(&error),
            server::StartupErrorCode::CatalogRateLimited
        );
    }

    #[tokio::test]
    async fn startup_catalog_timeout_has_a_distinct_static_code() {
        let elapsed = tokio::time::timeout(
            Duration::from_millis(1),
            futures_util::future::pending::<()>(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            classify_startup_error(&anyhow::Error::new(elapsed)),
            server::StartupErrorCode::CatalogTimeout
        );
    }
}
