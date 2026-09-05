use crate::Error;
use crate::Result;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CLIENT_ID;
use codex_login::ServerOptions;
use secrecy::ExposeSecret;
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use std::env;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const MUSE_CODEX_HOME_ENV: &str = "MUSE_CODEX_HOME";
const MAX_API_KEY_BYTES: usize = 16 * 1024;

/// Stable, isolated storage configuration for Muse Codex credentials.
///
/// The directory is used only as a namespace for the upstream Codex keyring
/// record. Credentials are always persisted with `Keyring`, never `auth.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthConfig {
    home: PathBuf,
}

impl AuthConfig {
    /// Resolves the isolated Muse Codex home.
    ///
    /// `MUSE_CODEX_HOME`, when non-empty, wins. Otherwise the path is
    /// `<platform-local-data>/muse-codex/codex-home`.
    pub fn for_muse_codex() -> Result<Self> {
        if let Some(home) = env::var_os(MUSE_CODEX_HOME_ENV).filter(|value| !value.is_empty()) {
            return Self::with_home(PathBuf::from(home));
        }

        let data_root = dirs::data_local_dir().ok_or(Error::DataDirectoryUnavailable)?;
        // This is a platform-designated user data root, not a caller-selected
        // auth directory. Do not change permissions on it.
        std::fs::create_dir_all(&data_root)?;
        Self::with_home(data_root.join("muse-codex").join("codex-home"))
    }

    /// Uses an explicit isolated home. Valid paths end in `muse-codex` or
    /// `muse-codex/codex-home`. Existing directories must already be private,
    /// owned by the current user, and not symlinks. Permissions are set only as
    /// part of atomically creating new dedicated directories.
    pub fn with_home(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        if home.as_os_str().is_empty() {
            return Err(Error::InvalidAuthHome("path is empty".to_string()));
        }
        if !home.is_absolute() {
            return Err(Error::InvalidAuthHome("path must be absolute".to_string()));
        }
        if home
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(Error::InvalidAuthHome(
                "path must not contain `.` or `..` components".to_string(),
            ));
        }

        let dedicated_root_path = dedicated_root(&home)?;
        reject_known_broad_paths(&home)?;
        let root_parent = dedicated_root_path.parent().ok_or_else(|| {
            Error::InvalidAuthHome("dedicated directory has no parent".to_string())
        })?;
        let parent_metadata = std::fs::metadata(root_parent).map_err(|error| {
            Error::InvalidAuthHome(format!(
                "parent {} must already exist: {error}",
                root_parent.display()
            ))
        })?;
        if !parent_metadata.is_dir() {
            return Err(Error::InvalidAuthHome(format!(
                "parent {} is not a directory",
                root_parent.display()
            )));
        }
        let canonical_parent = std::fs::canonicalize(root_parent).map_err(|error| {
            Error::InvalidAuthHome(format!(
                "could not canonicalize parent {}: {error}",
                root_parent.display()
            ))
        })?;
        let expected_root = canonical_parent.join("muse-codex");
        let expected_home = if home == dedicated_root_path {
            expected_root.clone()
        } else {
            expected_root.join("codex-home")
        };

        ensure_private_directory(dedicated_root_path)?;
        if home != dedicated_root_path {
            ensure_private_directory(&home)?;
        }

        let home = std::fs::canonicalize(&home)
            .map_err(|error| Error::InvalidAuthHome(format!("{}: {error}", home.display())))?;
        if home != expected_home {
            return Err(Error::InvalidAuthHome(
                "auth path changed while it was being validated".to_string(),
            ));
        }
        let canonical_root = dedicated_root(&home)?;
        let root_metadata = std::fs::symlink_metadata(canonical_root)?;
        validate_existing_private_directory(canonical_root, &root_metadata)?;
        if home != canonical_root {
            let home_metadata = std::fs::symlink_metadata(&home)?;
            validate_existing_private_directory(&home, &home_metadata)?;
        }
        Ok(Self { home })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) async fn manager(&self) -> std::sync::Arc<AuthManager> {
        AuthManager::shared(
            self.home.clone(),
            /* enable_codex_api_key_env */ false,
            AuthCredentialsStoreMode::Keyring,
            /* chatgpt_base_url */ None,
        )
        .await
    }

    fn server_options(&self) -> ServerOptions {
        ServerOptions::new(
            self.home.clone(),
            CLIENT_ID.to_string(),
            /* forced_chatgpt_workspace_id */ None,
            AuthCredentialsStoreMode::Keyring,
        )
    }
}

fn dedicated_root(home: &Path) -> Result<&Path> {
    if home.file_name().is_some_and(|name| name == "muse-codex") {
        return Ok(home);
    }
    if home.file_name().is_some_and(|name| name == "codex-home")
        && home
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "muse-codex")
    {
        return home
            .parent()
            .ok_or_else(|| Error::InvalidAuthHome("codex-home has no parent".to_string()));
    }
    Err(Error::InvalidAuthHome(
        "path must end in `muse-codex` or `muse-codex/codex-home`".to_string(),
    ))
}

fn reject_known_broad_paths(home: &Path) -> Result<()> {
    if home == Path::new("/") {
        return Err(Error::InvalidAuthHome(
            "filesystem root is forbidden".to_string(),
        ));
    }
    if let Some(user_home) = dirs::home_dir() {
        if home == user_home {
            return Err(Error::InvalidAuthHome("user home is forbidden".to_string()));
        }
        let stock_codex_home = user_home.join(".codex");
        if home.starts_with(&stock_codex_home) {
            return Err(Error::InvalidAuthHome(
                "the standard Codex home and its descendants are forbidden".to_string(),
            ));
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_existing_private_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = std::fs::symlink_metadata(path)?;
                        return validate_existing_private_directory(path, &metadata);
                    }
                    Err(error) => return Err(Error::Io(error)),
                }
            }
            #[cfg(not(unix))]
            std::fs::DirBuilder::new().create(path)?;

            let metadata = std::fs::symlink_metadata(path)?;
            validate_existing_private_directory(path, &metadata)
        }
        Err(error) => Err(Error::Io(error)),
    }
}

fn validate_existing_private_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(Error::InvalidAuthHome(format!(
            "{} is a symlink",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(Error::InvalidAuthHome(format!(
            "{} is not a directory",
            path.display()
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `geteuid` takes no pointers and has no preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(Error::InvalidAuthHome(format!(
                "{} is not owned by the current user",
                path.display()
            )));
        }
        let mode = metadata.mode() & 0o777;
        if mode != 0o700 {
            return Err(Error::InvalidAuthHome(format!(
                "{} must already have mode 0700 (found {mode:04o})",
                path.display()
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    Browser,
    Device,
}

/// User-facing information produced by an interactive login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LoginPrompt {
    Browser {
        auth_url: String,
    },
    Device {
        verification_url: String,
        user_code: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthStatus {
    SignedOut,
    ApiKey,
    ChatGpt {
        email: Option<String>,
        plan: Option<String>,
    },
}

/// Runs upstream Codex login with its default UI behavior.
pub async fn login(config: &AuthConfig, mode: LoginMode) -> Result<AuthStatus> {
    match mode {
        LoginMode::Browser => {
            let options = config.server_options();
            let server = codex_login::run_login_server(options)?;
            server.block_until_done().await?;
        }
        LoginMode::Device => {
            codex_login::run_device_code_login(config.server_options()).await?;
        }
    }
    status(config).await
}

/// Runs login without printing. The gateway receives exactly one prompt and
/// owns all user-facing output (and opening the browser, if desired).
pub async fn login_with_prompt_handler<F>(
    config: &AuthConfig,
    mode: LoginMode,
    prompt_handler: F,
) -> Result<AuthStatus>
where
    F: FnOnce(LoginPrompt) + Send,
{
    match mode {
        LoginMode::Browser => {
            let mut options = config.server_options();
            options.open_browser = false;
            let server = codex_login::run_login_server(options)?;
            prompt_handler(LoginPrompt::Browser {
                auth_url: server.auth_url.clone(),
            });
            server.block_until_done().await?;
        }
        LoginMode::Device => {
            let options = config.server_options();
            let device_code = codex_login::request_device_code(&options).await?;
            prompt_handler(LoginPrompt::Device {
                verification_url: device_code.verification_url.clone(),
                user_code: device_code.user_code.clone(),
            });
            codex_login::complete_device_code_login(options, device_code).await?;
        }
    }
    status(config).await
}

pub async fn logout(config: &AuthConfig) -> Result<bool> {
    codex_login::logout_with_revoke(config.home(), AuthCredentialsStoreMode::Keyring)
        .await
        .map_err(Error::from)
}

pub fn set_api_key(config: &AuthConfig, api_key: SecretString) -> Result<()> {
    validate_api_key(api_key.expose_secret())?;
    codex_login::login_with_api_key(
        config.home(),
        api_key.expose_secret(),
        AuthCredentialsStoreMode::Keyring,
    )
    .map_err(Error::from)
}

pub fn set_api_key_from_reader(config: &AuthConfig, reader: &mut dyn Read) -> Result<()> {
    let api_key = read_api_key(reader)?;
    set_api_key(config, api_key)
}

pub async fn status(config: &AuthConfig) -> Result<AuthStatus> {
    let manager = config.manager().await;
    let Some(auth) = manager.auth().await else {
        return Ok(AuthStatus::SignedOut);
    };

    if auth.is_api_key_auth() {
        return Ok(AuthStatus::ApiKey);
    }
    if !auth.is_chatgpt_auth() {
        return Err(Error::UnsupportedAuthMode);
    }

    let plan = auth.account_plan_type().and_then(|plan| {
        serde_json::to_value(plan)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
    });
    Ok(AuthStatus::ChatGpt {
        email: auth.get_account_email(),
        plan,
    })
}

fn read_api_key(reader: &mut dyn Read) -> Result<SecretString> {
    let mut bytes = Vec::with_capacity(256);
    reader
        .take((MAX_API_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_API_KEY_BYTES {
        return Err(Error::ApiKeyTooLong(MAX_API_KEY_BYTES));
    }
    let raw = String::from_utf8(bytes).map_err(|_| Error::ApiKeyNotUtf8)?;
    // A terminal contributes line endings, but spaces and tabs may be part of
    // an accidental paste. Do not silently alter those characters into a
    // different credential.
    let without_line_ending = raw.trim_end_matches(['\r', '\n']);
    validate_api_key(without_line_ending)?;
    Ok(SecretString::from(without_line_ending.to_owned()))
}

pub(crate) fn validate_api_key(api_key: &str) -> Result<()> {
    if api_key.is_empty() {
        return Err(Error::EmptyApiKey);
    }
    if api_key.len() > MAX_API_KEY_BYTES {
        return Err(Error::ApiKeyTooLong(MAX_API_KEY_BYTES));
    }
    if api_key.chars().any(char::is_whitespace) {
        return Err(Error::ApiKeyContainsWhitespace);
    }
    if api_key.chars().any(char::is_control) {
        return Err(Error::ApiKeyContainsControlCharacters);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn reader_trims_a_single_stdin_line() {
        let mut input = &b"sk-fixture\n"[..];
        let key = read_api_key(&mut input).expect("valid key");
        assert_eq!(key.expose_secret(), "sk-fixture");
    }

    #[test]
    fn reader_rejects_internal_whitespace() {
        let mut input = &b"sk bad\n"[..];
        assert!(matches!(
            read_api_key(&mut input),
            Err(Error::ApiKeyContainsWhitespace)
        ));
    }

    #[test]
    fn reader_does_not_silently_trim_spaces_or_tabs() {
        for bytes in [
            &b" sk-fixture\n"[..],
            &b"sk-fixture \n"[..],
            &b"sk-fixture\t\n"[..],
        ] {
            let mut input = bytes;
            assert!(matches!(
                read_api_key(&mut input),
                Err(Error::ApiKeyContainsWhitespace)
            ));
        }
    }

    #[test]
    fn direct_validation_enforces_size_and_control_character_limits() {
        assert!(matches!(
            validate_api_key(&"x".repeat(MAX_API_KEY_BYTES + 1)),
            Err(Error::ApiKeyTooLong(MAX_API_KEY_BYTES))
        ));
        assert!(matches!(
            validate_api_key("sk-fixture\0suffix"),
            Err(Error::ApiKeyContainsControlCharacters)
        ));
    }

    #[test]
    fn explicit_home_is_canonical_and_private() {
        let temp = tempfile::tempdir().expect("tempdir");
        let requested = temp.path().join("muse-codex").join("codex-home");
        let config = AuthConfig::with_home(&requested).expect("auth config");
        assert!(config.home().is_absolute());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(config.home())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn arbitrary_and_broad_paths_are_rejected_without_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let arbitrary = temp.path().join("auth");
        assert!(matches!(
            AuthConfig::with_home(&arbitrary),
            Err(Error::InvalidAuthHome(_))
        ));
        assert!(!arbitrary.exists());
        assert!(AuthConfig::with_home(Path::new("/")).is_err());
        if let Some(user_home) = dirs::home_dir() {
            assert!(AuthConfig::with_home(&user_home).is_err());
            assert!(AuthConfig::with_home(user_home.join(".codex")).is_err());
            assert!(AuthConfig::with_home(user_home.join(".codex/muse-codex/codex-home")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_broad_directory_is_rejected_not_chmodded() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let requested = temp.path().join("muse-codex");
        std::fs::create_dir(&requested).expect("create auth dir");
        std::fs::set_permissions(&requested, std::fs::Permissions::from_mode(0o755))
            .expect("set fixture mode");
        assert!(AuthConfig::with_home(&requested).is_err());
        let mode = std::fs::metadata(&requested)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_auth_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        std::fs::create_dir(&target).expect("create target");
        let requested = temp.path().join("muse-codex");
        symlink(&target, &requested).expect("create symlink");
        assert!(matches!(
            AuthConfig::with_home(&requested),
            Err(Error::InvalidAuthHome(_))
        ));
    }
}
