use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Write;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir};

pub const SUPPORTED_MUSE_VERSION: &str = "1.0.3-R2198.1";
pub const STOCK_MUSE_BASENAME: &str = "muse-bin-1.0.3-R2198.1";
pub const STOCK_MUSE_COMMAND: &str = "muse";
pub const GATEWAY_BASENAME: &str = "muse-codex-gateway";
pub const DEFAULT_GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(15);

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
    "MUSE_CUSTOM_HEADERS",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    GatewayAuth(Vec<OsString>),
    Muse(Vec<OsString>),
}

/// Validates the public provider and classifies only the auth forms owned by
/// muse-codex. Help and malformed auth invocations otherwise fall through to
/// stock Muse so its parser and help text remain authoritative.
pub fn classify_invocation(args: &[OsString]) -> Result<Invocation> {
    let (provider_selected, without_provider) = validate_and_remove_provider(args)?;

    if without_provider == [OsString::from("login")] {
        return Ok(Invocation::GatewayAuth(os_args(&["auth", "login"])));
    }
    if without_provider == [OsString::from("login"), OsString::from("--device-auth")] {
        return Ok(Invocation::GatewayAuth(os_args(&[
            "auth",
            "login",
            "--device-auth",
        ])));
    }
    if without_provider == [OsString::from("logout")] {
        return Ok(Invocation::GatewayAuth(os_args(&["auth", "logout"])));
    }
    if provider_selected
        && without_provider
            == [
                OsString::from("auth"),
                OsString::from("set"),
                OsString::from("--api-key-stdin"),
            ]
    {
        return Ok(Invocation::GatewayAuth(os_args(&["auth", "set-api-key"])));
    }

    Ok(Invocation::Muse(args.to_vec()))
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
    let mut rewritten = os_args(&["--provider", "meta", "--base-url"]);
    rewritten.push(base_url.into());

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

        rewritten.push(arg.clone());
        index += 1;
    }

    rewritten
}

/// Information-only calls do not need authentication or a running gateway.
/// Keeping their argv untouched preserves stock Muse's exact help/version text.
pub fn is_informational_invocation(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|argument| *argument != "--")
        .any(|argument| {
            argument == "-h" || argument == "--help" || argument == "-V" || argument == "--version"
        })
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
        index += 1;
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

fn reject_symlink_or_non_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "refusing to use non-regular settings file {}",
            path.display()
        );
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
}

impl GatewayReady {
    fn validate(self) -> Result<Self> {
        if self.schema_version != 1 {
            bail!(
                "unsupported gateway readiness schema {}; expected 1",
                self.schema_version
            );
        }
        validate_loopback_base_url(&self.base_url)?;
        validate_gateway_token(&self.token)?;
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
            Ok(Some(Self(value.into_vec())))
        }
        #[cfg(not(unix))]
        {
            let value = value
                .into_string()
                .map_err(|_| anyhow!("{name} is not valid Unicode"))?;
            Ok(Some(Self(value.into_bytes())))
        }
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

impl GatewayProcess {
    fn start(
        executable: &Path,
        upstream_base_url: Option<&OsStr>,
        api_key: Option<&SecretInput>,
    ) -> Result<Self> {
        let temporary_directory = TempBuilder::new()
            .prefix("muse-codex-gateway-")
            .tempdir()
            .context("failed to create gateway readiness directory")?;
        create_private_dir(temporary_directory.path())?;
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
            bail!(
                "gateway exited before becoming ready ({})",
                display_exit_status(status)
            );
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
            bail!("gateway did not become ready before the startup timeout");
        }
        thread::sleep(Duration::from_millis(25));
    }
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
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("gateway readiness path is not a regular file");
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
    }

    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let ready = serde_json::from_slice(&bytes).context("invalid gateway readiness JSON")?;
    Ok(Some(ready))
}

fn scrub_secret_environment(command: &mut Command) {
    for variable in SECRET_ENVIRONMENT {
        command.env_remove(variable);
    }
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
        Invocation::GatewayAuth(gateway_arguments) => {
            let gateway = discover_gateway(&discovery)?;
            run_gateway_auth(&gateway, &gateway_arguments)
        }
        Invocation::Muse(muse_arguments) => {
            let stock_muse = discover_stock_muse(&discovery)?;
            verify_stock_muse_version(&stock_muse)?;
            if is_informational_invocation(&muse_arguments) {
                let stock_arguments = strip_public_provider_args(&muse_arguments)?;
                run_stock_information(&stock_muse, &stock_arguments)
            } else {
                let gateway = discover_gateway(&discovery)?;
                run_stock_muse(&gateway, &stock_muse, &muse_arguments)
            }
        }
    }
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
    let upstream_base_url =
        resolve_upstream_base_url(arguments, env::var_os("OPENAI_BASE_URL").as_deref())?;
    let api_key = SecretInput::from_environment("OPENAI_API_KEY")?;
    let gateway = GatewayProcess::start(
        gateway_executable,
        upstream_base_url.as_deref(),
        api_key.as_ref(),
    )?;
    drop(api_key);

    let directories = StockDirectories::discover()?;
    directories.create_private()?;
    let settings_path = seed_stock_settings(&directories.config_home, &gateway.ready.base_url)?;
    let auth_path = directories.config_home.join("muse").join("auth.json");
    let rewritten = rewrite_muse_args(arguments, &gateway.ready.base_url);

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
    let result = wait_with_signal_forwarding(&mut child);
    drop(gateway);
    result
}

fn run_stock_information(stock_muse: &Path, arguments: &[OsString]) -> Result<i32> {
    let directories = StockDirectories::discover()?;
    directories.create_private()?;
    // The endpoint is deliberately unroutable and remains inside the isolated
    // profile. Information-only calls do not make provider requests.
    seed_stock_settings(&directories.config_home, "http://127.0.0.1:9")?;
    let auth_path = directories.config_home.join("muse").join("auth.json");

    let mut command = Command::new(stock_muse);
    command
        .args(arguments)
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .env("XDG_CONFIG_HOME", &directories.config_home)
        .env("XDG_DATA_HOME", &directories.data_home)
        .env("MUSE_AUTH_PATH", auth_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    scrub_secret_environment(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run stock Muse {}", stock_muse.display()))?;
    wait_with_signal_forwarding(&mut child)
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
    fn auth_help_and_unknown_forms_remain_stock_muse_commands() {
        let help = args(&["login", "--help"]);
        assert_eq!(
            classify_invocation(&help).unwrap(),
            Invocation::Muse(help.clone())
        );
        let incomplete = args(&["auth", "set", "--api-key-stdin"]);
        assert_eq!(
            classify_invocation(&incomplete).unwrap(),
            Invocation::Muse(incomplete.clone())
        );
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
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "exec",
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
                "--provider",
                "meta",
                "--base-url",
                "http://127.0.0.1:4321",
                "exec",
                "--",
                "--provider",
                "openai",
                "--base-url=https://literal.example",
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
            schema_version: 1,
            base_url: "http://127.0.0.1:4321".into(),
            token,
        };
        assert!(ready.validate().is_ok());

        let remote = GatewayReady {
            schema_version: 1,
            base_url: "https://example.test".into(),
            token: URL_SAFE_NO_PAD.encode([7_u8; 32]),
        };
        assert!(remote.validate().is_err());
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
}
