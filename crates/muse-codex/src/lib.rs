use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir};

mod msp;

pub const SUPPORTED_MUSE_VERSION: &str = "1.0.3-R2198.1";
pub const STOCK_MUSE_BASENAME: &str = "muse-bin-1.0.3-R2198.1";
pub const STOCK_MUSE_COMMAND: &str = "muse";
pub const GATEWAY_BASENAME: &str = "muse-codex-gateway";
pub const DEFAULT_GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(100);
pub const GATEWAY_READY_SCHEMA_VERSION: u32 = 2;
pub const SUPPORTED_CODEX_WIRE_VERSION: &str = "0.153.4";
const MAX_GATEWAY_READY_BYTES: u64 = 10 * 1024 * 1024;
const MAX_GATEWAY_MODELS: usize = 4096;
const STOCK_MODEL_CATALOG_FILE: &str = "6d657461__p746268.json";
const ULTRA_REASONING_GATE: &str = "MUSE_EXPERIMENTAL_ULTRA_REASONING_EFFORT";

const SECRET_ENVIRONMENT: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_HOME",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "META_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_PROJECT",
    "TBH_AUTH_BASE_URL",
    "TBH_MINT_BASE_URL",
    "MUSE_CUSTOM_HEADERS",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    GatewayAuth(Vec<OsString>),
    SelfTest,
    Muse(Vec<OsString>),
    Output { text: String, code: i32 },
}

/// Authentication always belongs to the wrapper, including help and invalid
/// arguments. It must never reach the stock provider's credential handlers.
pub fn classify_invocation(args: &[OsString]) -> Result<Invocation> {
    let (_, without_provider) = validate_and_remove_provider(args)?;
    if without_provider == [OsString::from("self-test")] {
        return Ok(Invocation::SelfTest);
    }
    if let Some(index) = stock_subcommand_index(&without_provider)
        && matches!(
            without_provider[index].to_str(),
            Some("auth" | "login" | "logout")
        )
    {
        if index != 0 {
            return Ok(Invocation::Output {
                text: "muse-codex: authentication commands accept --provider codex but no other startup options\n".into(),
                code: 2,
            });
        }
        return Ok(classify_auth_invocation(&without_provider));
    }
    Ok(Invocation::Muse(args.to_vec()))
}

fn classify_auth_invocation(args: &[OsString]) -> Invocation {
    use clap::{Arg, ArgAction, Command};

    let login = || {
        Command::new("login")
            .about("Sign in with a ChatGPT/Codex subscription")
            .arg(
                Arg::new("device-auth")
                    .long("device-auth")
                    .help("Use device authorization for a headless terminal")
                    .action(ArgAction::SetTrue),
            )
    };
    let logout = || Command::new("logout").about("Remove only saved Muse Codex credentials");
    let parser = Command::new("muse-codex")
        .disable_help_subcommand(true)
        .subcommand(login())
        .subcommand(logout())
        .subcommand(
            Command::new("auth")
                .about("Manage isolated OpenAI credentials")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(login())
                .subcommand(logout())
                .subcommand(Command::new("status").about("Show the saved authentication mode"))
                .subcommand(
                    Command::new("set")
                        .about("Store an OpenAI API key in macOS Keychain")
                        .after_help("The optional --provider codex flag is accepted. API keys are read only from stdin.")
                        .arg(
                            Arg::new("api-key-stdin")
                                .long("api-key-stdin")
                                .required(true)
                                .action(ArgAction::SetTrue),
                        ),
                ),
        );
    let parsed = match parser.try_get_matches_from(
        std::iter::once(OsString::from("muse-codex")).chain(args.iter().cloned()),
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Invocation::Output {
                text: error.to_string(),
                code: error.exit_code(),
            };
        }
    };
    let (name, matches) = parsed.subcommand().expect("an auth command was selected");
    let (name, matches) = if name == "auth" {
        matches.subcommand().expect("auth subcommand is required")
    } else {
        (name, matches)
    };
    let mut arguments = os_args(&["auth", if name == "set" { "set-api-key" } else { name }]);
    if name == "login" && matches.get_flag("device-auth") {
        arguments.push("--device-auth".into());
    }
    Invocation::GatewayAuth(arguments)
}

fn os_args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// Validates the public provider name. `meta` is an internal implementation
/// detail used only for the stock Muse child and is never accepted from users.
pub fn validate_provider_selection(args: &[OsString]) -> Result<()> {
    validate_and_remove_provider(args).map(|_| ())
}

pub fn strip_public_provider_args(args: &[OsString]) -> Result<Vec<OsString>> {
    validate_and_remove_provider(args).map(|(_, remaining)| remaining)
}

fn validate_and_remove_provider(args: &[OsString]) -> Result<(bool, Vec<OsString>)> {
    let mut provider_selected = false;
    let mut remaining = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            remaining.extend(args[index..].iter().cloned());
            break;
        }
        if arg == "--provider" {
            let Some(value) = args.get(index + 1) else {
                bail!("--provider requires the value 'codex'");
            };
            if is_option_like(value) {
                bail!("--provider requires the value 'codex'");
            }
            require_codex_provider(value)?;
            provider_selected = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg
            .to_str()
            .and_then(|argument| argument.strip_prefix("--provider="))
        {
            require_codex_provider(OsStr::new(value))?;
            provider_selected = true;
            index += 1;
            continue;
        }
        if option_takes_value(arg) && index + 1 < args.len() {
            remaining.extend(args[index..=index + 1].iter().cloned());
            index += 2;
            continue;
        }
        remaining.push(arg.clone());
        index += 1;
    }

    Ok((provider_selected, remaining))
}

fn require_codex_provider(value: &OsStr) -> Result<()> {
    if value == "codex" {
        return Ok(());
    }
    let value = value.to_string_lossy();
    bail!("unsupported provider '{value}'; muse-codex only accepts '--provider codex'")
}

/// Replaces provider-routing flags while preserving every other argument and
/// treating everything after `--` as opaque user input.
pub fn rewrite_muse_args(args: &[OsString], base_url: &str) -> Vec<OsString> {
    let mut rewritten = Vec::with_capacity(args.len() + 4);
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            rewritten.extend(args[index..].iter().cloned());
            break;
        }

        if (arg == "--provider" || arg == "--base-url")
            && index + 1 < args.len()
            && !is_option_like(&args[index + 1])
        {
            index += 2;
            continue;
        }
        if starts_with_os(arg, "--provider=") || starts_with_os(arg, "--base-url=") {
            index += 1;
            continue;
        }

        if option_takes_value(arg) && index + 1 < args.len() {
            rewritten.extend(args[index..=index + 1].iter().cloned());
            index += 2;
            continue;
        }

        rewritten.push(arg.clone());
        index += 1;
    }

    // Muse 1.0.3 dispatches a subcommand only when it is argv[1]. Injecting
    // startup flags before `exec` or `serve` silently enters the TUI instead.
    if let Some(index) = stock_subcommand_index(&rewritten) {
        let subcommand = rewritten.remove(index);
        let needs_routing_flags = subcommand == "exec" || subcommand == "resume";
        rewritten.insert(0, subcommand);
        if needs_routing_flags {
            rewritten.splice(
                1..1,
                os_args(&["--provider", "meta", "--base-url", base_url]),
            );
        }
        // `serve` does not accept provider flags. It uses the
        // isolated endpoint settings and the private bearer environment.
    } else {
        rewritten.splice(
            0..0,
            os_args(&["--provider", "meta", "--base-url", base_url]),
        );
    }
    rewritten
}

fn take_exec_api_key_stdin(args: &[OsString]) -> (bool, Vec<OsString>) {
    let is_exec = stock_subcommand_index(args).is_some_and(|index| args[index] == "exec");
    if !is_exec {
        return (false, args.to_vec());
    }
    let mut remaining = Vec::with_capacity(args.len());
    let mut selected = false;
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            remaining.extend(args[index..].iter().cloned());
            break;
        }
        if argument == "--api-key-stdin" {
            selected = true;
            index += 1;
            continue;
        }
        let count = if option_takes_value(argument) && index + 1 < args.len() {
            2
        } else {
            1
        };
        remaining.extend(args[index..index + count].iter().cloned());
        index += count;
    }
    (selected, remaining)
}

/// Information-only calls do not need authentication or a running gateway.
/// The stock parser owns help/version content; only provider labels are mapped.
pub fn is_informational_invocation(args: &[OsString]) -> bool {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            break;
        }
        if argument == "-h" || argument == "--help" || argument == "-V" || argument == "--version" {
            return true;
        }
        index += if option_takes_value(argument) { 2 } else { 1 };
    }
    false
}

/// These commands inspect or change local harness state without a model turn.
/// In particular, exporting the stable MSP schema must work while signed out.
pub fn is_local_invocation(args: &[OsString]) -> bool {
    stock_subcommand_index(args).is_some_and(|index| {
        matches!(
            args[index].to_str(),
            Some(
                "config"
                    | "export"
                    | "trace"
                    | "skills"
                    | "sandbox"
                    | "schema"
                    | "session-message"
                    | "init"
            )
        )
    })
}

fn stock_subcommand_index(args: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            return None;
        }
        if option_takes_value(argument) {
            index += 2;
            continue;
        }
        if argument == "-w" || argument == "--worktree" {
            index += 1;
            if args
                .get(index)
                .is_some_and(|value| value == "off" || value == "create" || value == "existing")
            {
                index += 1;
            }
            continue;
        }
        if !is_option_like(argument) {
            return matches!(
                argument.to_str(),
                Some(
                    "config"
                        | "export"
                        | "trace"
                        | "skills"
                        | "sandbox"
                        | "schema"
                        | "session-message"
                        | "init"
                        | "exec"
                        | "serve"
                        | "resume"
                        | "auth"
                        | "login"
                        | "logout"
                )
            )
            .then_some(index);
        }
        index += 1;
    }
    None
}

fn option_takes_value(argument: &OsStr) -> bool {
    matches!(
        argument.to_str(),
        Some(
            "--provider"
                | "--base-url"
                | "--agents"
                | "--preset"
                | "--model"
                | "--reasoning-effort"
                | "--image"
                | "--workspace"
                | "--worktree-base"
                | "--worktree-existing"
                | "--approval-mode"
                | "--permission-profile"
                | "--approval-judge"
                | "--echo-delay-ms"
                | "--sandbox-network"
                | "--prompt-file"
                | "--context-compaction-strategy"
                | "--context-compaction-soft-threshold"
                | "--context-compaction-hard-threshold"
                | "--max-model-steps"
                | "--max-tool-output-bytes"
                | "--session-id"
                | "--out"
                | "--format"
        )
    )
}

fn has_option(args: &[OsString], name: &str) -> bool {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            break;
        }
        if argument == name {
            return true;
        }
        index += if option_takes_value(argument) { 2 } else { 1 };
    }
    false
}

fn validate_msp_options(arguments: &[OsString]) -> Result<()> {
    if stock_subcommand_index(arguments).is_some_and(|index| arguments[index] == "serve")
        && has_option(arguments, "--no-session-log")
    {
        bail!(
            "Muse 1.0.3 cannot deliver MSP turn events with --no-session-log. Run `muse-codex serve` without that flag; sessions stay in the isolated Muse Codex profile."
        );
    }
    Ok(())
}

/// Returns the custom OpenAI-compatible upstream requested by the user. Muse
/// never receives this URL; only the loopback gateway does.
pub fn resolve_upstream_base_url(
    args: &[OsString],
    environment_value: Option<&OsStr>,
) -> Result<Option<OsString>> {
    let mut cli_value: Option<OsString> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--base-url" {
            let Some(value) = args.get(index + 1) else {
                bail!("--base-url requires a value");
            };
            if value.is_empty() || is_option_like(value) {
                bail!("--base-url requires a non-empty value");
            }
            merge_cli_base_url(&mut cli_value, value)?;
            index += 2;
            continue;
        }
        if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--base-url=")) {
            if value.is_empty() {
                bail!("--base-url requires a non-empty value");
            }
            merge_cli_base_url(&mut cli_value, OsStr::new(value))?;
        }
        index += if option_takes_value(arg) { 2 } else { 1 };
    }

    let environment_value = environment_value.filter(|value| !value.is_empty());
    match (cli_value, environment_value) {
        (Some(cli), Some(environment)) if cli != environment => {
            bail!("conflicting upstream base URLs: --base-url and OPENAI_BASE_URL differ")
        }
        (Some(cli), _) => Ok(Some(cli)),
        (None, Some(environment)) => Ok(Some(environment.to_os_string())),
        (None, None) => Ok(None),
    }
}

fn merge_cli_base_url(slot: &mut Option<OsString>, value: &OsStr) -> Result<()> {
    if let Some(existing) = slot {
        if existing != value {
            bail!("conflicting --base-url values were provided");
        }
    } else {
        *slot = Some(value.to_os_string());
    }
    Ok(())
}

fn starts_with_os(value: &OsStr, prefix: &str) -> bool {
    value
        .to_str()
        .is_some_and(|value| value.starts_with(prefix))
}

fn is_option_like(value: &OsStr) -> bool {
    value.to_str().is_some_and(|value| value.starts_with('-'))
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryInputs {
    pub muse_override: Option<PathBuf>,
    pub gateway_override: Option<PathBuf>,
    pub current_exe: Option<PathBuf>,
    pub install_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    pub path: Option<OsString>,
}

impl DiscoveryInputs {
    pub fn from_environment() -> Self {
        Self {
            muse_override: env::var_os("MUSE_CODEX_MUSE_BIN").map(PathBuf::from),
            gateway_override: env::var_os("MUSE_CODEX_GATEWAY_BIN").map(PathBuf::from),
            current_exe: env::current_exe().ok(),
            install_dir: env::var_os("MUSE_INSTALL_DIR").map(PathBuf::from),
            home_dir: env::var_os("HOME").map(PathBuf::from),
            path: env::var_os("PATH"),
        }
    }
}

pub fn discover_stock_muse(inputs: &DiscoveryInputs) -> Result<PathBuf> {
    if let Some(path) = &inputs.muse_override {
        return require_executable(path, "MUSE_CODEX_MUSE_BIN");
    }

    let mut candidates = Vec::new();
    if let Some(current_exe) = &inputs.current_exe
        && let Some(directory) = current_exe.parent()
    {
        candidates.push(directory.join(platform_binary_name(STOCK_MUSE_BASENAME)));
        candidates.push(directory.join(platform_binary_name(STOCK_MUSE_COMMAND)));
    }
    if let Some(directory) = &inputs.install_dir {
        candidates.push(directory.join(platform_binary_name(STOCK_MUSE_BASENAME)));
        candidates.push(directory.join(platform_binary_name(STOCK_MUSE_COMMAND)));
    }
    if let Some(home) = &inputs.home_dir {
        let user_bin = home.join(".local").join("bin");
        candidates.push(user_bin.join(platform_binary_name(STOCK_MUSE_BASENAME)));
        candidates.push(user_bin.join(platform_binary_name(STOCK_MUSE_COMMAND)));
    }
    candidates.extend(path_candidates(
        inputs.path.as_deref(),
        &platform_binary_name(STOCK_MUSE_BASENAME),
    ));
    candidates.extend(path_candidates(
        inputs.path.as_deref(),
        &platform_binary_name(STOCK_MUSE_COMMAND),
    ));

    first_executable(candidates).ok_or_else(|| {
        anyhow!(
            "Muse Code {SUPPORTED_MUSE_VERSION} is not installed; install the stock Muse CLI or set MUSE_CODEX_MUSE_BIN to its exact binary"
        )
    })
}

/// Confirms that an independently installed stock executable is the exact
/// compatibility baseline before it is allowed to own a session.
pub fn verify_stock_muse_version(path: &Path) -> Result<()> {
    if let Ok(current_exe) = env::current_exe()
        && let (Ok(candidate), Ok(launcher)) =
            (fs::canonicalize(path), fs::canonicalize(current_exe))
        && candidate == launcher
    {
        bail!("MUSE_CODEX_MUSE_BIN must point to stock Muse, not the muse-codex launcher");
    }
    let mut command = Command::new(path);
    command
        .arg("--version")
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .stdin(Stdio::null());
    scrub_secret_environment(&mut command);
    let output = command
        .output()
        .with_context(|| format!("failed to inspect stock Muse at {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "stock Muse version check failed for {} ({})",
            path.display(),
            display_exit_status(output.status)
        );
    }
    let actual = std::str::from_utf8(&output.stdout)
        .context("stock Muse --version output was not UTF-8")?
        .trim();
    let expected = format!("Muse Code 1.0.3 ({SUPPORTED_MUSE_VERSION})");
    if actual != expected {
        bail!(
            "stock Muse must be exactly {SUPPORTED_MUSE_VERSION}; {} reported {:?}",
            path.display(),
            actual
        );
    }
    Ok(())
}

pub fn discover_gateway(inputs: &DiscoveryInputs) -> Result<PathBuf> {
    if let Some(path) = &inputs.gateway_override {
        return require_executable(path, "MUSE_CODEX_GATEWAY_BIN");
    }

    let name = platform_binary_name(GATEWAY_BASENAME);
    let mut candidates = Vec::new();
    if let Some(current_exe) = &inputs.current_exe
        && let Some(directory) = current_exe.parent()
    {
        candidates.push(directory.join(&name));
    }
    candidates.extend(path_candidates(inputs.path.as_deref(), &name));

    first_executable(candidates).ok_or_else(|| {
        anyhow!(
            "{GATEWAY_BASENAME} was not found beside this executable or on PATH; set MUSE_CODEX_GATEWAY_BIN to override discovery"
        )
    })
}

fn platform_binary_name(base: &str) -> OsString {
    #[cfg(windows)]
    {
        format!("{base}.exe").into()
    }
    #[cfg(not(windows))]
    {
        base.into()
    }
}

fn path_candidates(path: Option<&OsStr>, binary_name: &OsStr) -> Vec<PathBuf> {
    path.map(env::split_paths)
        .into_iter()
        .flatten()
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(binary_name))
        .collect()
}

fn first_executable(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| is_executable(path))
}

fn require_executable(path: &Path, variable: &str) -> Result<PathBuf> {
    if is_executable(path) {
        return Ok(path.to_path_buf());
    }
    bail!(
        "{variable} does not point to an executable file: {}",
        path.display()
    )
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockDirectories {
    pub app_home: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
}

impl StockDirectories {
    pub fn discover() -> Result<Self> {
        let app_home = if let Some(path) = env::var_os("MUSE_CODEX_HOME") {
            PathBuf::from(path)
        } else {
            default_app_home()?
        };
        Ok(Self {
            config_home: app_home.join("stock-config"),
            data_home: app_home.join("stock-data"),
            app_home,
        })
    }

    fn create_private(&self) -> Result<()> {
        validate_app_home_location(&self.app_home)?;
        create_private_dir(&self.app_home)?;
        let canonical = fs::canonicalize(&self.app_home).with_context(|| {
            format!(
                "failed to canonicalize Muse Codex home {}",
                self.app_home.display()
            )
        })?;
        let canonical_parent = fs::canonicalize(
            self.app_home
                .parent()
                .context("Muse Codex home has no parent directory")?,
        )?;
        if canonical != canonical_parent.join("muse-codex") {
            bail!("Muse Codex home changed while it was being validated");
        }
        create_private_dir(&self.config_home)?;
        create_private_dir(&self.data_home)?;
        Ok(())
    }
}

fn default_app_home() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("muse-codex"))
    }
    #[cfg(windows)]
    {
        let local = env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("USERPROFILE"))
            .context("LOCALAPPDATA and USERPROFILE are not set")?;
        Ok(PathBuf::from(local).join("muse-codex"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(data_home).join("muse-codex"));
        }
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("muse-codex"))
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .with_context(|| format!("private directory {} has no parent", path.display()))?;
            if !parent.is_dir() {
                bail!("parent directory {} must already exist", parent.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            fs::DirBuilder::new().create(path)?;

            let metadata = fs::symlink_metadata(path)?;
            validate_private_directory(path, &metadata)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("private path {} is not a regular directory", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        if mode != 0o700 {
            bail!(
                "private directory {} must already have mode 0700 (found {mode:04o})",
                path.display()
            );
        }
        // SAFETY: `geteuid` takes no pointers and has no preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            bail!(
                "private directory {} is not owned by the current user",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_app_home_location(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("MUSE_CODEX_HOME must be an absolute path");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!("MUSE_CODEX_HOME must not contain `.` or `..` components");
    }
    if path
        .file_name()
        .is_none_or(|name| name != OsStr::new("muse-codex"))
    {
        bail!("MUSE_CODEX_HOME must end in `muse-codex`");
    }
    let parent = path
        .parent()
        .context("MUSE_CODEX_HOME has no parent directory")?;
    let canonical_parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "MUSE_CODEX_HOME parent {} must already exist",
            parent.display()
        )
    })?;

    if let Some(user_home) = env::var_os("HOME").map(PathBuf::from) {
        if path == user_home {
            bail!("MUSE_CODEX_HOME cannot be the user home directory");
        }
        let stock_codex_home = user_home.join(".codex");
        if path.starts_with(&stock_codex_home)
            || canonical_parent.starts_with(
                fs::canonicalize(&stock_codex_home).unwrap_or_else(|_| stock_codex_home.clone()),
            )
        {
            bail!("MUSE_CODEX_HOME cannot be inside the stock Codex home");
        }
    }
    Ok(())
}

/// Preserves stock Muse preferences while pinning provider transport and
/// disabling telemetry in the isolated muse-codex profile.
pub fn seed_stock_settings(config_home: &Path, base_url: &str) -> Result<PathBuf> {
    let muse_config = config_home.join("muse");
    create_private_dir(config_home)?;
    create_private_dir(&muse_config)?;
    let path = muse_config.join("settings.json");

    let mut root = if path.exists() {
        reject_symlink_or_non_file(&path)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read isolated settings at {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "isolated settings at {} are not valid JSON; refusing to overwrite them",
                path.display()
            )
        })?;
        value.as_object().cloned().ok_or_else(|| {
            anyhow!(
                "isolated settings at {} must contain a JSON object",
                path.display()
            )
        })?
    } else {
        Map::new()
    };

    root.insert("schema_version".into(), json!(1));
    root.insert(
        "endpoint_transport".into(),
        json!({"base_url": base_url, "auth": "bearer"}),
    );
    // The gateway owns the bounded pre-stream retry budget. Muse's default
    // policy retries HTTP 400 and disconnected streams up to ten times.
    let retry = root.entry("provider_retry").or_insert_with(|| json!({}));
    if !retry.is_object() {
        *retry = json!({});
    }
    let retry = retry.as_object_mut().expect("provider retry is an object");
    retry.remove("max_attempts");
    retry.insert("max_retries".into(), json!(0));
    match root.get_mut("telemetry") {
        Some(Value::Object(telemetry)) => {
            telemetry.insert("enabled".into(), Value::Bool(false));
        }
        _ => {
            root.insert("telemetry".into(), json!({"enabled": false}));
        }
    }

    let mut serialized = serde_json::to_vec_pretty(&Value::Object(root))?;
    serialized.push(b'\n');
    let mut temporary = NamedTempFile::new_in(&muse_config)
        .with_context(|| format!("failed to stage settings in {}", muse_config.display()))?;
    protect_file(temporary.as_file())?;
    temporary.write_all(&serialized)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| {
        anyhow!(
            "failed to install isolated settings at {}: {}",
            path.display(),
            error.error
        )
    })?;
    set_file_mode_0600(&path)?;
    Ok(path)
}

/// Writes Muse 1.0.3's normalized provider cache from the authenticated Codex
/// catalog. The stock raw-catalog decoder cannot preserve multiple rows when a
/// provider omits optional release dates or output limits, while this cache
/// schema represents those unknowns as JSON null without inventing values.
pub fn seed_stock_model_catalog(
    data_home: &Path,
    models: &[GatewayModel],
    default_model: &str,
) -> Result<PathBuf> {
    validate_gateway_models(models, default_model)?;
    let muse_data = data_home.join("muse");
    let catalog_directory = muse_data.join("model-catalog");
    ensure_owned_catalog_directory(&muse_data)?;
    ensure_owned_catalog_directory(&catalog_directory)?;
    let path = catalog_directory.join(STOCK_MODEL_CATALOG_FILE);
    if path.exists() {
        reject_symlink_or_non_file(&path)?;
    }

    let rows = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            json!({
                "model_id": model.id,
                "display_label": model.display_name.as_deref().unwrap_or(&model.id),
                "provider_id": "meta",
                "profile_id": "tbh",
                "visibility": if model.is_visible { "visible" } else { "hidden" },
                "release_date": null,
                "display_order": index,
                "is_current": model.is_default,
                "is_default": model.is_default,
                "roles": [],
                "context_limit": model.context_window,
                "output_limit": model.max_output_tokens,
                "description": model.description,
                "cost": null,
                "reasoning_effort_variants": reasoning_effort_variants(model),
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "schema_version": 1,
        "provider_id": "meta",
        "profile_id": "tbh",
        "source": "provider_catalog",
        "rows": rows,
    });
    let mut serialized = serde_json::to_vec_pretty(&value)?;
    serialized.push(b'\n');
    let mut temporary = NamedTempFile::new_in(&catalog_directory).with_context(|| {
        format!(
            "failed to stage model catalog in {}",
            catalog_directory.display()
        )
    })?;
    protect_file(temporary.as_file())?;
    temporary.write_all(&serialized)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| {
        anyhow!(
            "failed to install isolated model catalog at {}: {}",
            path.display(),
            error.error
        )
    })?;
    set_file_mode_0600(&path)?;
    Ok(path)
}

fn reasoning_effort_variants(model: &GatewayModel) -> Option<Vec<Value>> {
    (!model.supported_reasoning_efforts.is_empty()).then(|| {
        model
            .supported_reasoning_efforts
            .iter()
            .map(|effort| json!({"tier": effort, "description": null}))
            .collect()
    })
}

fn ensure_owned_catalog_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("model catalog path {} is not a directory", path.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                // SAFETY: geteuid has no preconditions and does not dereference pointers.
                let effective_uid = unsafe { libc::geteuid() };
                if metadata.uid() != effective_uid || metadata.permissions().mode() & 0o022 != 0 {
                    bail!(
                        "model catalog directory {} is not privately owned",
                        path.display()
                    );
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                match builder.mode(0o700).create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return ensure_owned_catalog_directory(path);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            match fs::DirBuilder::new().create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return ensure_owned_catalog_directory(path);
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn reject_symlink_or_non_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("refusing to use non-regular file {}", path.display());
    }
    Ok(())
}

fn protect_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_file_mode_0600(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GatewayReady {
    pub schema_version: u32,
    pub base_url: String,
    pub token: String,
    pub default_model: String,
    pub models: Vec<GatewayModel>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GatewayModel {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    pub is_visible: bool,
    pub is_default: bool,
}

impl GatewayReady {
    fn validate(self) -> Result<Self> {
        if self.schema_version != GATEWAY_READY_SCHEMA_VERSION {
            bail!(
                "unsupported gateway readiness schema {}; expected {}",
                self.schema_version,
                GATEWAY_READY_SCHEMA_VERSION
            );
        }
        validate_loopback_base_url(&self.base_url)?;
        validate_gateway_token(&self.token)?;
        validate_gateway_model(&self.default_model)?;
        validate_gateway_models(&self.models, &self.default_model)?;
        Ok(self)
    }
}

fn validate_loopback_base_url(value: &str) -> Result<()> {
    let port = value
        .strip_prefix("http://127.0.0.1:")
        .ok_or_else(|| anyhow!("gateway returned a non-loopback base_url"))?;
    if port.is_empty() || port.contains(['/', '?', '#', '@']) {
        bail!("gateway returned an invalid loopback base_url");
    }
    let port: u16 = port
        .parse()
        .map_err(|_| anyhow!("gateway returned an invalid loopback port"))?;
    if port == 0 {
        bail!("gateway returned loopback port zero");
    }
    Ok(())
}

fn validate_gateway_token(token: &str) -> Result<()> {
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .or_else(|_| URL_SAFE.decode(token))
        .map_err(|_| anyhow!("gateway returned a malformed bearer token"))?;
    if decoded.len() != 32 {
        bail!("gateway bearer token is not 256 bits");
    }
    Ok(())
}

fn validate_gateway_model(model: &str) -> Result<()> {
    if model.is_empty()
        || model.len() > 256
        || model.starts_with('-')
        || model
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("gateway returned an invalid default model id");
    }
    Ok(())
}

fn validate_gateway_models(models: &[GatewayModel], default_model: &str) -> Result<()> {
    if models.is_empty() || models.len() > MAX_GATEWAY_MODELS {
        bail!("gateway returned an invalid model count");
    }

    let mut ids = HashSet::with_capacity(models.len());
    let mut matching_default = 0_usize;
    let mut visible = 0_usize;
    for model in models {
        validate_gateway_model(&model.id)?;
        if !ids.insert(model.id.as_str()) {
            bail!("gateway returned duplicate model ids");
        }
        if model.display_name.as_ref().is_some_and(|label| {
            label.is_empty() || label.len() > 512 || label.chars().any(char::is_control)
        }) {
            bail!("gateway returned an invalid model display name");
        }
        if model
            .description
            .as_ref()
            .is_some_and(|description| description.len() > 16 * 1024 || description.contains('\0'))
        {
            bail!("gateway returned an invalid model description");
        }
        if model
            .context_window
            .is_some_and(|limit| limit == 0 || limit > i64::MAX as u64)
            || model
                .max_output_tokens
                .is_some_and(|limit| limit == 0 || limit > i64::MAX as u64)
        {
            bail!("gateway returned an invalid model limit");
        }
        let mut reasoning_efforts = HashSet::with_capacity(model.supported_reasoning_efforts.len());
        if model.supported_reasoning_efforts.len() > 32 {
            bail!("gateway returned too many reasoning effort variants");
        }
        for effort in &model.supported_reasoning_efforts {
            if effort.is_empty()
                || effort.len() > 64
                || effort
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
                || !reasoning_efforts.insert(effort.as_str())
            {
                bail!("gateway returned an invalid reasoning effort variant");
            }
        }
        if model
            .default_reasoning_effort
            .as_ref()
            .is_some_and(|effort| {
                !model
                    .supported_reasoning_efforts
                    .iter()
                    .any(|supported| supported == effort)
            })
        {
            bail!("gateway returned an unsupported default reasoning effort");
        }
        if model.is_visible {
            visible += 1;
        }
        if model.is_default {
            if !model.is_visible || model.id != default_model {
                bail!("gateway default model metadata is inconsistent");
            }
            matching_default += 1;
        }
    }
    if visible == 0 || matching_default != 1 {
        bail!("gateway returned no unique visible default model");
    }
    Ok(())
}

struct SecretInput(Vec<u8>);

impl SecretInput {
    fn from_environment(name: &str) -> Result<Option<Self>> {
        let Some(value) = env::var_os(name) else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            Self::parse(value.into_vec()).map(Some)
        }
        #[cfg(not(unix))]
        {
            let value = value
                .into_string()
                .map_err(|_| anyhow!("{name} is not valid Unicode"))?;
            Self::parse(value.into_bytes()).map(Some)
        }
    }

    fn parse(bytes: Vec<u8>) -> Result<Self> {
        const MAX_API_KEY_BYTES: usize = 16 * 1024;
        let mut secret = Self(bytes);
        if secret.0.len() > MAX_API_KEY_BYTES {
            bail!("API key input exceeds the 16384-byte limit");
        }
        let value = std::str::from_utf8(&secret.0).context("API key input must be UTF-8")?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            bail!("API key input must be non-empty and contain no whitespace");
        }
        let trimmed = value.as_bytes().to_vec();
        secret.0.fill(0);
        secret.0 = trimmed;
        Ok(secret)
    }

    fn from_reader(reader: impl Read) -> Result<Self> {
        let mut secret = Self(Vec::new());
        reader.take(16 * 1024 + 1).read_to_end(&mut secret.0)?;
        Self::parse(std::mem::take(&mut secret.0))
    }
}

impl Drop for SecretInput {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct GatewayProcess {
    child: Child,
    _temporary_directory: TempDir,
    ready: GatewayReady,
}

fn create_gateway_temporary_directory() -> Result<TempDir> {
    let mut builder = TempBuilder::new();
    builder.prefix("muse-codex-gateway-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }

    let temporary_directory = builder
        .tempdir()
        .context("failed to create gateway readiness directory")?;
    create_private_dir(temporary_directory.path())?;
    Ok(temporary_directory)
}

impl GatewayProcess {
    fn start(
        executable: &Path,
        upstream_base_url: Option<&OsStr>,
        api_key: Option<&SecretInput>,
    ) -> Result<Self> {
        let temporary_directory = create_gateway_temporary_directory()?;
        let ready_file = temporary_directory.path().join("ready.json");

        let mut command = Command::new(executable);
        command
            .arg("serve")
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--ready-file")
            .arg(&ready_file)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(upstream_base_url) = upstream_base_url {
            command.arg("--upstream-base-url").arg(upstream_base_url);
        }
        if api_key.is_some() {
            command.arg("--api-key-stdin").stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        scrub_secret_environment(&mut command);
        configure_hidden_gateway(&mut command);

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start gateway executable {}",
                executable.display()
            )
        })?;
        if let Some(api_key) = api_key {
            let write_result = (|| -> Result<()> {
                let mut stdin = child
                    .stdin
                    .take()
                    .context("gateway API-key stdin was not available")?;
                stdin.write_all(&api_key.0)?;
                stdin.write_all(b"\n")?;
                stdin.flush()?;
                Ok(())
            })();
            if let Err(error) = write_result {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.context("failed to deliver OPENAI_API_KEY to gateway stdin"));
            }
        }

        let ready = match wait_for_gateway_ready(&mut child, &ready_file) {
            Ok(ready) => ready,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            _temporary_directory: temporary_directory,
            ready,
        })
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }

        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_gateway_ready(child: &mut Child, ready_file: &Path) -> Result<GatewayReady> {
    let deadline = Instant::now() + gateway_ready_timeout();
    let mut last_parse_error = None;
    loop {
        if let Some(status) = child.try_wait()? {
            if let Some(message) = read_gateway_startup_failure(ready_file)? {
                bail!("{message}");
            }
            bail!(
                "gateway could not initialize authentication or load the Codex model catalog ({}); run `muse-codex auth status`, then `muse-codex login` if needed",
                display_exit_status(status)
            );
        }

        if let Some(message) = read_gateway_startup_failure(ready_file)? {
            bail!("{message}");
        }

        match read_gateway_ready(ready_file) {
            Ok(Some(ready)) => return ready.validate(),
            Ok(None) => {}
            Err(error) => last_parse_error = Some(error),
        }
        if Instant::now() >= deadline {
            if let Some(error) = last_parse_error {
                return Err(error.context("gateway readiness file never became valid"));
            }
            bail!(
                "Codex model catalog did not load before the gateway startup timeout; check your connection and retry"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_gateway_startup_failure(ready_file: &Path) -> Result<Option<&'static str>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StartupFailure {
        schema_version: u32,
        error: StartupError,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StartupError {
        code: String,
    }
    let mut path = ready_file.as_os_str().to_os_string();
    path.push(".error");
    let Some(file) = open_private_gateway_file(Path::new(&path), 4096)? else {
        return Ok(None);
    };
    let failure: StartupFailure = serde_json::from_reader(file.take(4097))
        .context("invalid private gateway startup error")?;
    if failure.schema_version != 1 {
        bail!("unsupported gateway startup error schema");
    }
    let message = match failure.error.code.as_str() {
        "authentication_required" => {
            "No Codex credentials are saved. Run `muse-codex login` or `muse-codex auth set --provider codex --api-key-stdin`."
        }
        "authentication_failed" => {
            "Codex authentication was rejected. Run `muse-codex login` to sign in again."
        }
        "custom_base_url_requires_api_key" => {
            "A custom base URL is allowed only with API-key authentication. Remove --base-url/OPENAI_BASE_URL to use your ChatGPT subscription."
        }
        "invalid_base_url" => {
            "The custom API base URL must use HTTPS and contain no credentials, query, or fragment."
        }
        "credential_store_unavailable" => {
            "The isolated credential store could not be opened. Check macOS Keychain access and run `muse-codex auth status`."
        }
        "catalog_rate_limited" => {
            "Codex model discovery is rate limited. Wait for the account limit to reset and retry."
        }
        "catalog_rejected" => {
            "The provider rejected model discovery. Check account access with `muse-codex auth status`."
        }
        "catalog_invalid" => {
            "The provider returned an incompatible model catalog. Update muse-codex before retrying."
        }
        "catalog_timeout" => {
            "Codex model discovery timed out. Check your connection and any pending macOS Keychain prompt, then retry."
        }
        "network_unavailable" => {
            "Could not reach the Codex provider. Check your connection and retry."
        }
        _ => {
            "The private gateway could not start. Check the installed launcher/gateway pair and retry."
        }
    };
    Ok(Some(message))
}

fn gateway_ready_timeout() -> Duration {
    env::var("MUSE_CODEX_GATEWAY_READY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|duration| !duration.is_zero())
        .unwrap_or(DEFAULT_GATEWAY_READY_TIMEOUT)
}

fn read_gateway_ready(path: &Path) -> Result<Option<GatewayReady>> {
    let Some(mut file) = open_private_gateway_file(path, MAX_GATEWAY_READY_BYTES)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_GATEWAY_READY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_GATEWAY_READY_BYTES {
        bail!("gateway readiness file exceeds the size limit");
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let ready = serde_json::from_slice(&bytes).context("invalid gateway readiness JSON")?;
    Ok(Some(ready))
}

fn open_private_gateway_file(path: &Path, limit: u64) -> Result<Option<File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("gateway readiness path is not a regular file");
    }
    if metadata.len() > limit {
        bail!("gateway readiness file exceeds the size limit");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o777 != 0o600 {
            bail!("gateway readiness file permissions are not 0600");
        }
        if metadata.nlink() != 1 {
            bail!("gateway readiness file has an unexpected link count");
        }
        // SAFETY: geteuid has no arguments or preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            bail!("gateway readiness file is not owned by the current user");
        }
    }
    Ok(Some(file))
}

fn scrub_secret_environment(command: &mut Command) {
    for variable in SECRET_ENVIRONMENT {
        command.env_remove(variable);
    }
    command.env("TBH_DISABLE_TELEMETRY", "1");
}

/// Opens Muse 1.0.3's built-in Ultra mode for the isolated Codex runtime.
/// Muse keeps ownership of proactive workflow delegation and maps the model
/// request to its catalog-defined underlying reasoning effort.
fn enable_ultra_reasoning(command: &mut Command) {
    command.env(ULTRA_REASONING_GATE, "1");
}

fn configure_hidden_gateway(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

pub fn run() -> Result<i32> {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    let discovery = DiscoveryInputs::from_environment();

    match classify_invocation(&arguments)? {
        Invocation::Output { text, code } => {
            if code == 0 {
                print!("{text}");
            } else {
                eprint!("{text}");
            }
            Ok(code)
        }
        Invocation::GatewayAuth(gateway_arguments) => {
            let gateway = discover_gateway(&discovery)?;
            run_gateway_auth(&gateway, &gateway_arguments)
        }
        Invocation::SelfTest => {
            let stock_muse = discover_stock_muse(&discovery)?;
            verify_stock_muse_version(&stock_muse)?;
            let gateway = discover_gateway(&discovery)?;
            verify_gateway_self_test(&gateway)?;
            println!("muse-codex self-test: ok");
            Ok(0)
        }
        Invocation::Muse(muse_arguments) => {
            let stock_muse = discover_stock_muse(&discovery)?;
            verify_stock_muse_version(&stock_muse)?;
            if is_informational_invocation(&muse_arguments) || is_local_invocation(&muse_arguments)
            {
                let stock_arguments = strip_public_provider_args(&muse_arguments)?;
                run_stock_information(&stock_muse, &stock_arguments)
            } else {
                let gateway = discover_gateway(&discovery)?;
                run_stock_muse(&gateway, &stock_muse, &muse_arguments)
            }
        }
    }
}

fn verify_gateway_self_test(executable: &Path) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GatewaySelfTest {
        status: String,
        ready_schema_version: u32,
        wire_compatibility_version: String,
    }

    let mut command = Command::new(executable);
    command
        .arg("self-test")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    scrub_secret_environment(&mut command);
    let output = command
        .output()
        .with_context(|| format!("failed to self-test gateway {}", executable.display()))?;
    if !output.status.success() {
        bail!(
            "gateway self-test failed ({})",
            display_exit_status(output.status)
        );
    }
    if output.stdout.len() > 4096 {
        bail!("gateway self-test output exceeded the size limit");
    }
    let result: GatewaySelfTest = serde_json::from_slice(&output.stdout)
        .context("gateway self-test returned invalid JSON")?;
    if result.status != "ok"
        || result.ready_schema_version != GATEWAY_READY_SCHEMA_VERSION
        || result.wire_compatibility_version != SUPPORTED_CODEX_WIRE_VERSION
    {
        bail!("gateway self-test reported an incompatible launcher/gateway protocol");
    }
    Ok(())
}

fn run_gateway_auth(executable: &Path, arguments: &[OsString]) -> Result<i32> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    scrub_secret_environment(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run {}", executable.display()))?;
    wait_with_signal_forwarding(&mut child)
}

fn run_stock_muse(
    gateway_executable: &Path,
    stock_muse: &Path,
    arguments: &[OsString],
) -> Result<i32> {
    validate_msp_options(arguments)?;
    let (api_key_stdin, arguments) = take_exec_api_key_stdin(arguments);
    let upstream_base_url =
        resolve_upstream_base_url(&arguments, env::var_os("OPENAI_BASE_URL").as_deref())?;
    let api_key = if api_key_stdin {
        Some(SecretInput::from_reader(std::io::stdin().lock())?)
    } else {
        SecretInput::from_environment("OPENAI_API_KEY")?
    };
    let gateway = GatewayProcess::start(
        gateway_executable,
        upstream_base_url.as_deref(),
        api_key.as_ref(),
    )?;
    drop(api_key);

    let directories = StockDirectories::discover()?;
    directories.create_private()?;
    seed_stock_model_catalog(
        &directories.data_home,
        &gateway.ready.models,
        &gateway.ready.default_model,
    )?;
    let settings_path = seed_stock_settings(&directories.config_home, &gateway.ready.base_url)?;
    let auth_path = directories.config_home.join("muse").join("auth.json");
    let rewritten = rewrite_muse_args(&arguments, &gateway.ready.base_url);
    let is_msp = rewritten
        .first()
        .is_some_and(|argument| argument == "serve");

    let mut command = Command::new(stock_muse);
    command
        .args(&rewritten)
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .env("XDG_CONFIG_HOME", &directories.config_home)
        .env("XDG_DATA_HOME", &directories.data_home)
        .env("MUSE_AUTH_PATH", auth_path)
        .env("META_API_KEY", &gateway.ready.token)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    scrub_secret_environment(&mut command);
    enable_ultra_reasoning(&mut command);
    if api_key_stdin {
        command.stdin(Stdio::null());
    }
    if is_msp {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
    }
    // META_API_KEY is intentionally reintroduced only after the inherited value
    // was removed; it authenticates exclusively to the private loopback gateway.
    command.env("META_API_KEY", &gateway.ready.token);

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to run stock Muse {} (isolated settings: {})",
            stock_muse.display(),
            settings_path.display()
        )
    })?;
    let relay = if is_msp {
        match msp::MspRelay::start(&mut child) {
            Ok(relay) => Some(relay),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    } else {
        None
    };
    let result = wait_with_signal_forwarding(&mut child);
    let protocol_result = relay.map(msp::MspRelay::finish).transpose();
    drop(gateway);
    protocol_result?;
    result
}

fn run_stock_information(stock_muse: &Path, arguments: &[OsString]) -> Result<i32> {
    let mut arguments = arguments.to_vec();
    if let Some(index) = stock_subcommand_index(&arguments) {
        let command = arguments.remove(index);
        arguments.insert(0, command);
    }
    let directories = StockDirectories::discover()?;
    directories.create_private()?;
    // The endpoint is deliberately unroutable and remains inside the isolated
    // profile. Information-only calls do not make provider requests.
    // A help/schema command must not overwrite a running session's endpoint.
    if !directories.config_home.join("muse/settings.json").exists() {
        seed_stock_settings(&directories.config_home, "http://127.0.0.1:9")?;
    }
    let auth_path = directories.config_home.join("muse").join("auth.json");

    let mut command = Command::new(stock_muse);
    command
        .args(&arguments)
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .env("XDG_CONFIG_HOME", &directories.config_home)
        .env("XDG_DATA_HOME", &directories.data_home)
        .env("MUSE_AUTH_PATH", auth_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    scrub_secret_environment(&mut command);
    if is_informational_invocation(&arguments) {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().context("failed to read stock Muse help")?;
        std::io::stdout().write_all(&rewrite_provider_help(&output.stdout))?;
        std::io::stderr().write_all(&rewrite_provider_help(&output.stderr))?;
        return Ok(exit_status_code(output.status));
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run stock Muse {}", stock_muse.display()))?;
    wait_with_signal_forwarding(&mut child)
}

fn rewrite_provider_help(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    text.replace("muse ", "muse-codex ")
        .replace(
            "Startup provider: echo or meta (default: meta)",
            "Startup provider: codex (default: codex)",
        )
        .replace(
            "Model id for non-echo providers",
            "Model id from the authenticated Codex catalog",
        )
        .replace("Meta reasoning effort:", "Model reasoning effort:")
        .replace(
            "Override the Meta provider base URL",
            "Override the OpenAI API base URL (API-key auth only)",
        )
        .replace(
            "Override the provider base URL",
            "Override the OpenAI API base URL (API-key auth only)",
        )
        .replace("Meta API parallel tool calls", "parallel tool calls")
        .replace(
            "Deterministic echo reply delay (echo provider only)",
            "Inactive compatibility option; the test provider is unavailable",
        )
        .replace(
            "Use memory-only sessions",
            "Unavailable for MSP turns with the pinned Muse 1.0.3 host",
        )
        .into_bytes()
}

fn wait_with_signal_forwarding(child: &mut Child) -> Result<i32> {
    #[cfg(unix)]
    let _signals = match SignalForwarder::new(child.id()) {
        Ok(signals) => signals,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.context("failed to install child signal forwarding"));
        }
    };
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    Ok(exit_status_code(status))
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn display_exit_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    "unknown process status".into()
}

#[cfg(unix)]
struct SignalForwarder {
    handle: signal_hook::iterator::Handle,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl SignalForwarder {
    fn new(child_pid: u32) -> Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        use signal_hook::iterator::Signals;

        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
        let handle = signals.handle();
        let thread = thread::spawn(move || {
            for signal in signals.forever() {
                unsafe {
                    libc::kill(child_pid as libc::pid_t, signal);
                }
            }
        });
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

#[cfg(unix)]
impl Drop for SignalForwarder {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn gateway_models() -> Vec<GatewayModel> {
        vec![
            GatewayModel {
                id: "gpt-visible".into(),
                display_name: Some("GPT Visible".into()),
                description: Some("Authenticated default".into()),
                context_window: Some(272_000),
                max_output_tokens: None,
                supported_reasoning_efforts: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                    "max".into(),
                    "ultra".into(),
                ],
                default_reasoning_effort: Some("high".into()),
                is_visible: true,
                is_default: true,
            },
            GatewayModel {
                id: "codex-hidden".into(),
                display_name: None,
                description: None,
                context_window: None,
                max_output_tokens: None,
                supported_reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                is_visible: false,
                is_default: false,
            },
        ]
    }

    fn create_executable(path: &Path) {
        File::create(path).expect("create executable fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("mark fixture executable");
        }
    }

    fn create_version_executable(path: &Path, version: &str) {
        fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"))
            .expect("write version fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("mark version fixture executable");
        }
    }

    #[test]
    fn intercepts_owned_auth_commands_only() {
        assert_eq!(
            classify_invocation(&args(&["self-test"])).unwrap(),
            Invocation::SelfTest
        );
        assert_eq!(
            classify_invocation(&args(&["login"])).unwrap(),
            Invocation::GatewayAuth(args(&["auth", "login"]))
        );
        assert_eq!(
            classify_invocation(&args(&["login", "--device-auth"])).unwrap(),
            Invocation::GatewayAuth(args(&["auth", "login", "--device-auth"]))
        );
        assert_eq!(
            classify_invocation(&args(&["logout"])).unwrap(),
            Invocation::GatewayAuth(args(&["auth", "logout"]))
        );
        assert_eq!(
            classify_invocation(&args(&[
                "auth",
                "set",
                "--provider",
                "codex",
                "--api-key-stdin",
            ]))
            .unwrap(),
            Invocation::GatewayAuth(args(&["auth", "set-api-key"]))
        );
        assert_eq!(
            classify_invocation(&args(&[
                "auth",
                "set",
                "--api-key-stdin",
                "--provider=codex",
            ]))
            .unwrap(),
            Invocation::GatewayAuth(args(&["auth", "set-api-key"]))
        );
        assert_eq!(
            classify_invocation(&args(&["--provider", "codex", "login", "--device-auth",]))
                .unwrap(),
            Invocation::GatewayAuth(args(&["auth", "login", "--device-auth"]))
        );
    }

    #[test]
    fn auth_help_status_and_invalid_forms_never_reach_stock_auth() {
        let help = args(&["login", "--help"]);
        assert!(matches!(
            classify_invocation(&help).unwrap(),
            Invocation::Output { code: 0, .. }
        ));
        assert_eq!(
            classify_invocation(&args(&["auth", "set", "--api-key-stdin"])).unwrap(),
            Invocation::GatewayAuth(args(&["auth", "set-api-key"]))
        );
        assert_eq!(
            classify_invocation(&args(&["auth", "status"])).unwrap(),
            Invocation::GatewayAuth(args(&["auth", "status"]))
        );
        for command in [
            args(&["auth", "set"]),
            args(&["login", "--bad"]),
            args(&["logout", "extra"]),
            args(&["--model", "gpt-test", "auth", "set"]),
        ] {
            assert!(matches!(
                classify_invocation(&command).unwrap(),
                Invocation::Output { code: 2, .. }
            ));
        }
    }

    #[test]
    fn provider_validation_accepts_codex_and_rejects_internal_or_unknown_names() {
        validate_provider_selection(&args(&["--provider", "codex", "exec"])).unwrap();
        validate_provider_selection(&args(&["exec", "--provider=codex"])).unwrap();
        validate_provider_selection(&args(&["exec", "--", "--provider", "meta"])).unwrap();

        for provider in ["meta", "echo", "openai", "unknown"] {
            let error = validate_provider_selection(&args(&["--provider", provider, "exec"]))
                .unwrap_err()
                .to_string();
            assert!(error.contains("only accepts '--provider codex'"));
        }
        assert!(validate_provider_selection(&args(&["--provider"])).is_err());
    }

    #[test]
    fn informational_calls_strip_public_provider_before_stock_muse() {
        assert_eq!(
            strip_public_provider_args(&args(&["--provider", "codex", "login", "--help",]))
                .unwrap(),
            args(&["login", "--help"])
        );
        assert_eq!(
            strip_public_provider_args(&args(&["exec", "--help", "--", "--provider", "codex",]))
                .unwrap(),
            args(&["exec", "--help", "--", "--provider", "codex"])
        );
    }

    #[test]
    fn rewrites_routing_flags_and_preserves_argument_order() {
        let original = args(&[
            "--provider=codex",
            "exec",
            "--base-url",
            "https://upstream.example/v1",
            "--json",
            "prompt",
        ]);
        assert_eq!(
            rewrite_muse_args(&original, "http://127.0.0.1:4321"),
            args(&[
                "exec",
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "--json",
                "prompt",
            ])
        );
    }

    #[test]
    fn rewrite_treats_everything_after_double_dash_as_opaque() {
        let original = args(&[
            "exec",
            "--",
            "--provider",
            "openai",
            "--base-url=https://literal.example",
        ]);
        assert_eq!(
            rewrite_muse_args(&original, "http://127.0.0.1:4321"),
            args(&[
                "exec",
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "--",
                "--provider",
                "openai",
                "--base-url=https://literal.example",
            ])
        );
    }

    #[test]
    fn rewrite_preserves_an_explicit_model() {
        let original = args(&["--model=gpt-explicit", "exec", "prompt"]);
        assert_eq!(
            rewrite_muse_args(&original, "http://127.0.0.1:4321"),
            args(&[
                "exec",
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "--model=gpt-explicit",
                "prompt",
            ])
        );
    }

    #[test]
    fn rewrite_keeps_a_literal_model_flag_after_double_dash_opaque() {
        let original = args(&["exec", "--", "--model", "literal"]);
        let rewritten = rewrite_muse_args(&original, "http://127.0.0.1:4321");
        assert_eq!(
            rewritten,
            args(&[
                "exec",
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "--",
                "--model",
                "literal",
            ])
        );
    }

    #[test]
    fn informational_detection_preserves_help_but_not_literal_prompt_flags() {
        assert!(is_informational_invocation(&args(&["login", "--help"])));
        assert!(is_informational_invocation(&args(&["--version"])));
        assert!(!is_informational_invocation(&args(&[
            "exec", "--", "--help"
        ])));
        assert!(!is_informational_invocation(&args(&[
            "exec",
            "--prompt-file",
            "--help"
        ])));
    }

    #[test]
    fn model_free_commands_and_serve_preserve_their_parser_scope() {
        assert!(is_local_invocation(&args(&[
            "schema",
            "generate-json-schema",
            "--out",
            "schema"
        ])));
        assert!(is_local_invocation(&args(&[
            "--provider",
            "codex",
            "skills",
            "list"
        ])));
        assert!(!is_local_invocation(&args(&["--model", "schema", "hello"])));
        assert!(!is_local_invocation(&args(&["exec", "schema"])));
        assert_eq!(
            rewrite_muse_args(
                &args(&["serve", "--no-session-log"]),
                "http://127.0.0.1:1234"
            ),
            args(&["serve", "--no-session-log"])
        );
        let resumed = rewrite_muse_args(&args(&["resume", "--last"]), "http://127.0.0.1:1234");
        assert_eq!(&resumed[..3], &args(&["resume", "--provider", "meta"]));
        assert!(validate_msp_options(&args(&["serve", "--no-session-log"])).is_err());
        assert!(validate_msp_options(&args(&["serve"])).is_ok());
        assert!(validate_msp_options(&args(&["exec", "--no-session-log", "hello"])).is_ok());
    }

    #[test]
    fn routing_never_consumes_another_options_literal_value() {
        let original = args(&[
            "exec",
            "--prompt-file",
            "--base-url=https://literal.invalid",
        ]);
        assert_eq!(resolve_upstream_base_url(&original, None).unwrap(), None);
        let rewritten = rewrite_muse_args(&original, "http://127.0.0.1:1234");
        assert_eq!(&rewritten[5..], &original[1..]);
        let literal_provider = args(&["exec", "--prompt-file", "--provider=meta"]);
        assert_eq!(
            strip_public_provider_args(&literal_provider).unwrap(),
            literal_provider
        );
    }

    #[test]
    fn exec_stdin_credentials_are_consumed_only_by_the_wrapper() {
        assert_eq!(
            take_exec_api_key_stdin(&args(&["exec", "--api-key-stdin", "hello"])),
            (true, args(&["exec", "hello"]))
        );
        for original in [
            args(&["exec", "--", "--api-key-stdin"]),
            args(&["exec", "--prompt-file", "--api-key-stdin"]),
        ] {
            assert_eq!(take_exec_api_key_stdin(&original), (false, original));
        }
        assert_eq!(
            SecretInput::from_reader(&b"sk-fixture\n"[..]).unwrap().0,
            b"sk-fixture"
        );
        assert!(SecretInput::parse(vec![b'x'; 16 * 1024 + 1]).is_err());
        assert!(SecretInput::parse(b"sk-one sk-two".to_vec()).is_err());
        assert!(SecretInput::parse(b" sk-fixture".to_vec()).is_err());
        assert!(SecretInput::parse(b"sk-fixture \n".to_vec()).is_err());
        assert!(SecretInput::parse(b"\nsk-fixture".to_vec()).is_err());
        assert!(SecretInput::parse(vec![0xff]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn readiness_open_rejects_links_and_nonprivate_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("ready.json");
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open_private_gateway_file(&path, 10).unwrap().is_some());
        assert!(open_private_gateway_file(&path, 1).is_err());
        let link = temporary.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(open_private_gateway_file(&link, 10).is_err());
        fs::hard_link(&path, temporary.path().join("hard-link")).unwrap();
        assert!(open_private_gateway_file(&path, 10).is_err());
        fs::remove_file(temporary.path().join("hard-link")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_private_gateway_file(&path, 10).is_err());
    }

    #[test]
    fn startup_diagnostics_are_static_and_never_echo_provider_payloads() {
        let temporary = tempfile::tempdir().unwrap();
        let ready = temporary.path().join("ready.json");
        let error_path = temporary.path().join("ready.json.error");
        fs::write(
            &error_path,
            br#"{"schema_version":1,"error":{"code":"custom_base_url_requires_api_key"}}"#,
        )
        .unwrap();
        set_file_mode_0600(&error_path).unwrap();
        assert!(
            read_gateway_startup_failure(&ready)
                .unwrap()
                .unwrap()
                .contains("API-key authentication")
        );
        fs::write(
            &error_path,
            br#"{"schema_version":1,"error":{"code":"untrusted-secret-value"}}"#,
        )
        .unwrap();
        assert!(
            !read_gateway_startup_failure(&ready)
                .unwrap()
                .unwrap()
                .contains("untrusted-secret-value")
        );
        fs::write(&error_path, br#"{"schema_version":1,"error":{"code":"authentication_failed","body":"untrusted-secret-value"}}"#).unwrap();
        assert!(read_gateway_startup_failure(&ready).is_err());
    }

    #[test]
    fn resolves_custom_upstream_and_rejects_conflicts() {
        let command = args(&["exec", "--base-url=https://example.test/v1"]);
        assert_eq!(
            resolve_upstream_base_url(&command, None).unwrap(),
            Some("https://example.test/v1".into())
        );
        assert_eq!(
            resolve_upstream_base_url(&command, Some(OsStr::new("https://example.test/v1")))
                .unwrap(),
            Some("https://example.test/v1".into())
        );
        assert!(
            resolve_upstream_base_url(&command, Some(OsStr::new("https://different.test/v1")))
                .is_err()
        );
    }

    #[test]
    fn stock_discovery_prefers_explicit_override() {
        let temporary = tempfile::tempdir().unwrap();
        let override_path = temporary.path().join("custom-muse");
        let sibling_path = temporary
            .path()
            .join(platform_binary_name(STOCK_MUSE_BASENAME));
        create_executable(&override_path);
        create_executable(&sibling_path);
        let inputs = DiscoveryInputs {
            muse_override: Some(override_path.clone()),
            current_exe: Some(temporary.path().join("muse-codex")),
            ..DiscoveryInputs::default()
        };
        assert_eq!(discover_stock_muse(&inputs).unwrap(), override_path);
    }

    #[test]
    fn stock_discovery_finds_exact_version_on_path() {
        let temporary = tempfile::tempdir().unwrap();
        let expected = temporary
            .path()
            .join(platform_binary_name(STOCK_MUSE_BASENAME));
        create_executable(&expected);
        let inputs = DiscoveryInputs {
            path: Some(env::join_paths([temporary.path()]).unwrap()),
            ..DiscoveryInputs::default()
        };
        assert_eq!(discover_stock_muse(&inputs).unwrap(), expected);
    }

    #[test]
    fn stock_discovery_finds_official_muse_command_on_path() {
        let temporary = tempfile::tempdir().unwrap();
        let expected = temporary
            .path()
            .join(platform_binary_name(STOCK_MUSE_COMMAND));
        create_executable(&expected);
        let inputs = DiscoveryInputs {
            path: Some(env::join_paths([temporary.path()]).unwrap()),
            ..DiscoveryInputs::default()
        };
        assert_eq!(discover_stock_muse(&inputs).unwrap(), expected);
    }

    #[test]
    fn stock_version_gate_requires_the_exact_baseline() {
        let temporary = tempfile::tempdir().unwrap();
        let exact = temporary.path().join("exact-muse");
        let wrong = temporary.path().join("wrong-muse");
        create_version_executable(&exact, "Muse Code 1.0.3 (1.0.3-R2198.1)");
        create_version_executable(&wrong, "Muse Code 1.0.4 (1.0.4-R2200.1)");
        verify_stock_muse_version(&exact).unwrap();
        assert!(verify_stock_muse_version(&wrong).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn gateway_self_test_requires_the_matching_pair_contract() {
        let temporary = tempfile::tempdir().unwrap();
        let exact = temporary.path().join("exact-gateway");
        let wrong = temporary.path().join("wrong-gateway");
        fs::write(
            &exact,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{{\"status\":\"ok\",\"ready_schema_version\":{},\"wire_compatibility_version\":\"{}\"}}'\n",
                GATEWAY_READY_SCHEMA_VERSION, SUPPORTED_CODEX_WIRE_VERSION
            ),
        )
        .unwrap();
        fs::write(
            &wrong,
            "#!/bin/sh\nprintf '%s\\n' '{\"status\":\"ok\",\"ready_schema_version\":999,\"wire_compatibility_version\":\"future\"}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exact, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&wrong, fs::Permissions::from_mode(0o700)).unwrap();

        verify_gateway_self_test(&exact).unwrap();
        assert!(verify_gateway_self_test(&wrong).is_err());
    }

    #[test]
    fn gateway_discovery_prefers_sibling_then_path() {
        let sibling_directory = tempfile::tempdir().unwrap();
        let path_directory = tempfile::tempdir().unwrap();
        let name = platform_binary_name(GATEWAY_BASENAME);
        let sibling = sibling_directory.path().join(&name);
        let on_path = path_directory.path().join(&name);
        create_executable(&sibling);
        create_executable(&on_path);
        let inputs = DiscoveryInputs {
            current_exe: Some(sibling_directory.path().join("muse-codex")),
            path: Some(env::join_paths([path_directory.path()]).unwrap()),
            ..DiscoveryInputs::default()
        };
        assert_eq!(discover_gateway(&inputs).unwrap(), sibling);
    }

    #[test]
    fn discovery_rejects_non_executable_override() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-executable");
        File::create(&file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let inputs = DiscoveryInputs {
            muse_override: Some(file),
            ..DiscoveryInputs::default()
        };
        assert!(discover_stock_muse(&inputs).is_err());
    }

    #[test]
    fn settings_seed_preserves_preferences_and_pins_transport() {
        let temporary = tempfile::tempdir().unwrap();
        let config_home = temporary.path().join("config");
        let muse_config = config_home.join("muse");
        fs::create_dir_all(&muse_config).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_home, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&muse_config, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            muse_config.join("settings.json"),
            br#"{"schema_version":1,"theme":"dark","telemetry":{"detail":"keep","enabled":true}}"#,
        )
        .unwrap();

        let path = seed_stock_settings(&config_home, "http://127.0.0.1:4321").unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["telemetry"]["detail"], "keep");
        assert_eq!(value["telemetry"]["enabled"], false);
        assert_eq!(value["provider_retry"]["max_retries"], 0);
        assert_eq!(
            value["endpoint_transport"],
            json!({"base_url": "http://127.0.0.1:4321", "auth": "bearer"})
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn model_catalog_seed_preserves_unknowns_and_visibility() {
        let temporary = tempfile::tempdir().unwrap();
        let data_home = temporary.path().join("stock-data");
        fs::create_dir(&data_home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&data_home, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let path = seed_stock_model_catalog(&data_home, &gateway_models(), "gpt-visible").unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["provider_id"], "meta");
        assert_eq!(value["profile_id"], "tbh");
        assert_eq!(value["rows"][0]["model_id"], "gpt-visible");
        assert_eq!(value["rows"][0]["display_label"], "GPT Visible");
        assert_eq!(value["rows"][0]["visibility"], "visible");
        assert_eq!(value["rows"][0]["context_limit"], 272_000);
        assert!(value["rows"][0]["output_limit"].is_null());
        assert!(value["rows"][0]["release_date"].is_null());
        assert_eq!(
            value["rows"][0]["reasoning_effort_variants"],
            json!([
                {"tier": "low", "description": null},
                {"tier": "medium", "description": null},
                {"tier": "high", "description": null},
                {"tier": "xhigh", "description": null},
                {"tier": "max", "description": null},
                {"tier": "ultra", "description": null}
            ])
        );
        assert_eq!(value["rows"][1]["display_label"], "codex-hidden");
        assert_eq!(value["rows"][1]["visibility"], "hidden");
        assert!(value["rows"][1]["reasoning_effort_variants"].is_null());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_directories_are_created_safely_and_never_repermissioned() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let app_home = temporary.path().join("muse-codex");
        let directories = StockDirectories {
            config_home: app_home.join("stock-config"),
            data_home: app_home.join("stock-data"),
            app_home: app_home.clone(),
        };
        directories.create_private().unwrap();
        for path in [
            &directories.app_home,
            &directories.config_home,
            &directories.data_home,
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let permissive = temporary.path().join("permissive");
        fs::create_dir(&permissive).unwrap();
        fs::set_permissions(&permissive, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_private_dir(&permissive).is_err());
        assert_eq!(
            fs::metadata(&permissive).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn gateway_readiness_directory_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary_directory = create_gateway_temporary_directory().unwrap();
        assert_eq!(
            fs::metadata(temporary_directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn app_home_must_use_a_dedicated_absolute_leaf() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(validate_app_home_location(Path::new("muse-codex")).is_err());
        assert!(validate_app_home_location(temporary.path()).is_err());
        assert!(validate_app_home_location(&temporary.path().join("muse-codex")).is_ok());

        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            assert!(validate_app_home_location(&home.join(".codex/muse-codex")).is_err());
        }
    }

    #[test]
    fn readiness_contract_requires_loopback_and_a_256_bit_token() {
        let token = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let ready = GatewayReady {
            schema_version: GATEWAY_READY_SCHEMA_VERSION,
            base_url: "http://127.0.0.1:4321".into(),
            token,
            default_model: "gpt-visible".into(),
            models: gateway_models(),
        };
        assert!(ready.validate().is_ok());

        let remote = GatewayReady {
            schema_version: GATEWAY_READY_SCHEMA_VERSION,
            base_url: "https://example.test".into(),
            token: URL_SAFE_NO_PAD.encode([7_u8; 32]),
            default_model: "gpt-visible".into(),
            models: gateway_models(),
        };
        assert!(remote.validate().is_err());

        let invalid_model = GatewayReady {
            schema_version: GATEWAY_READY_SCHEMA_VERSION,
            base_url: "http://127.0.0.1:4321".into(),
            token: URL_SAFE_NO_PAD.encode([7_u8; 32]),
            default_model: "--help".into(),
            models: gateway_models(),
        };
        assert!(invalid_model.validate().is_err());
    }

    #[test]
    fn child_environment_explicitly_removes_every_sensitive_override() {
        let mut command = Command::new("unused");
        scrub_secret_environment(&mut command);
        let removed: Vec<&OsStr> = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect();
        for variable in SECRET_ENVIRONMENT {
            assert!(
                removed.contains(&OsStr::new(variable)),
                "{variable} not removed"
            );
        }
    }

    #[test]
    fn stock_runtime_forces_the_ultra_reasoning_gate_open() {
        let mut command = Command::new("unused");
        command.env(ULTRA_REASONING_GATE, "0");
        enable_ultra_reasoning(&mut command);

        let value = command
            .get_envs()
            .find_map(|(name, value)| (name == ULTRA_REASONING_GATE).then_some(value))
            .flatten();
        assert_eq!(value, Some(OsStr::new("1")));
    }
}
