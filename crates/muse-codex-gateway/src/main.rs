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
        Command::Serve(args) => serve(args).await,
        Command::Auth(args) => auth(args).await,
        Command::SelfTest => self_test(),
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    if !args.bind.ip().is_loopback() {
        bail!("refusing to expose the private gateway on a non-loopback address");
    }
    if args.parent_pid == 0 {
        bail!("--parent-pid must be a live process id");
    }

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
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("API key stdin was empty");
    }
    if value.chars().any(char::is_whitespace) {
        bail!("API key stdin contains whitespace");
    }
    Ok(SecretString::from(value))
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

    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn secret_input_is_trimmed_but_not_logged() {
        let mut input = Cursor::new(b"sk-test-secret\r\n");
        let secret = read_secret_line(&mut input).unwrap();
        assert_eq!(secret.expose_secret(), "sk-test-secret");
    }

    #[test]
    fn empty_secret_is_rejected() {
        let mut input = Cursor::new(b"\n");
        assert!(read_secret_line(&mut input).is_err());
    }
}
