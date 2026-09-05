//! Isolated Z.ai credential storage.
//!
//! Z.ai credentials deliberately live in their own keyring record. The Codex
//! record is written by the pinned upstream login stack and describes an
//! OpenAI credential; sharing it would let one provider's credential satisfy a
//! launch that selected the other. The account name is derived from the same
//! isolated home Codex namespaces with, so `MUSE_CODEX_HOME` keeps both
//! records isolated together.

use crate::AuthConfig;
use crate::Error;
use crate::Result;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use secrecy::ExposeSecret;
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::sync::Arc;

pub(crate) const ZAI_KEYRING_SERVICE: &str = "Muse Codex Z.ai";

/// Persisted credential record. The schema version exists so a future record
/// shape can be rejected instead of silently misread as an API key.
#[derive(Debug, Serialize, Deserialize)]
struct StoredCredential {
    schema_version: u32,
    api_key: String,
}

const STORED_CREDENTIAL_SCHEMA_VERSION: u32 = 1;

/// Reads and writes the isolated Z.ai keyring record.
#[derive(Clone)]
pub struct ZaiCredentials {
    account: String,
    store: Arc<dyn KeyringStore>,
}

impl std::fmt::Debug for ZaiCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ZaiCredentials")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl ZaiCredentials {
    pub fn new(config: &AuthConfig) -> Self {
        Self::with_store(config, Arc::new(DefaultKeyringStore))
    }

    pub(crate) fn with_store(config: &AuthConfig, store: Arc<dyn KeyringStore>) -> Self {
        Self {
            account: store_key(config.home()),
            store,
        }
    }

    pub fn save(&self, api_key: &SecretString) -> Result<()> {
        crate::auth::validate_api_key(api_key.expose_secret())?;
        let record = StoredCredential {
            schema_version: STORED_CREDENTIAL_SCHEMA_VERSION,
            api_key: api_key.expose_secret().to_string(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|error| Error::Keyring(format!("could not encode the credential: {error}")))?;
        self.store
            .save(ZAI_KEYRING_SERVICE, &self.account, &encoded)
            .map_err(|error| Error::Keyring(error.message()))
    }

    pub fn load(&self) -> Result<Option<SecretString>> {
        let Some(encoded) = self
            .store
            .load(ZAI_KEYRING_SERVICE, &self.account)
            .map_err(|error| Error::Keyring(error.message()))?
        else {
            return Ok(None);
        };
        let record: StoredCredential = serde_json::from_str(&encoded).map_err(|_| {
            Error::Keyring("the stored Z.ai credential is unreadable; store it again".to_string())
        })?;
        if record.schema_version != STORED_CREDENTIAL_SCHEMA_VERSION {
            return Err(Error::Keyring(
                "the stored Z.ai credential uses an unsupported schema; store it again".to_string(),
            ));
        }
        // A record written by an older build could predate a validation rule.
        // Reject it here rather than sending an unusable credential upstream.
        crate::auth::validate_api_key(&record.api_key)?;
        Ok(Some(SecretString::from(record.api_key)))
    }

    pub fn delete(&self) -> Result<bool> {
        self.store
            .delete(ZAI_KEYRING_SERVICE, &self.account)
            .map_err(|error| Error::Keyring(error.message()))
    }
}

/// Mirrors the upstream Codex store-key derivation so both records are keyed by
/// the same isolated home while remaining distinct entries.
fn store_key(home: &Path) -> String {
    let canonical = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let truncated = digest.get(..16).unwrap_or(&digest);
    format!("zai|{truncated}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;

    fn config(root: &Path) -> AuthConfig {
        AuthConfig::with_home(root.join("muse-codex")).expect("isolated home")
    }

    #[test]
    fn saves_loads_and_deletes_an_isolated_record() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        let store = MockKeyringStore::default();
        let credentials = ZaiCredentials::with_store(&config, Arc::new(store.clone()));

        assert!(credentials.load().expect("empty load").is_none());
        credentials
            .save(&SecretString::from("zai-fixture-key".to_string()))
            .expect("save");
        assert_eq!(
            credentials
                .load()
                .expect("load")
                .map(|key| key.expose_secret().to_string()),
            Some("zai-fixture-key".to_string())
        );
        assert!(credentials.delete().expect("delete"));
        assert!(credentials.load().expect("load after delete").is_none());
    }

    #[test]
    fn account_is_namespaced_and_derived_from_the_isolated_home() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        let credentials =
            ZaiCredentials::with_store(&config, Arc::new(MockKeyringStore::default()));
        assert!(credentials.account.starts_with("zai|"));
        assert_eq!(credentials.account.len(), "zai|".len() + 16);
    }

    #[test]
    fn rejects_a_credential_that_fails_api_key_validation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        let credentials =
            ZaiCredentials::with_store(&config, Arc::new(MockKeyringStore::default()));
        assert!(
            credentials
                .save(&SecretString::from("has whitespace".to_string()))
                .is_err()
        );
    }

    #[test]
    fn rejects_an_unreadable_stored_record() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        let store = MockKeyringStore::default();
        let credentials = ZaiCredentials::with_store(&config, Arc::new(store.clone()));
        store
            .save(ZAI_KEYRING_SERVICE, &credentials.account, "not json")
            .expect("seed");
        assert!(credentials.load().is_err());
    }
}
