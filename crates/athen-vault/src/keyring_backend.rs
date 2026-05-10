//! OS keychain vault backend (`keyring` crate).
//!
//! The `keyring` crate hides macOS Keychain / Windows Credential Manager /
//! Linux Secret Service behind a single `Entry::set/get/delete_password`
//! API. None of those backends offer enumeration in a portable way, so we
//! keep a SQLite index of `(scope, key)` tuples next to the vault dir; the
//! actual values live in the OS keychain.
//!
//! The keychain key is `"<scope>::<key>"` so that every secret has a stable
//! addressable name within the `service`. `service` is set once per app
//! install (typically `"athen"`).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use keyring::Entry;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::traits::vault::Vault;

const INDEX_FILENAME: &str = "vault_index.db";

const INDEX_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS secret_index (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    PRIMARY KEY (scope, key)
);
"#;

const SELF_CHECK_SCOPE: &str = "__athen_vault__";
const SELF_CHECK_KEY: &str = "self_check";
const SELF_CHECK_VALUE: &str = "ok";

pub struct KeyringVault {
    service: String,
    index: Arc<Mutex<Connection>>,
}

impl KeyringVault {
    /// Open a keyring-backed vault. Creates `vault_index.db` next to the
    /// other vault files. Does NOT verify keychain reachability — call
    /// [`KeyringVault::self_check`] for that (the factory in `lib.rs`
    /// already does).
    pub async fn new(data_dir: &Path, service: &str) -> Result<Self> {
        let data_dir = data_dir.to_path_buf();
        let service = service.to_string();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)
                .map_err(|e| AthenError::Vault(format!("Create vault dir: {e}")))?;
            let conn = Connection::open(data_dir.join(INDEX_FILENAME))
                .map_err(|e| AthenError::Vault(format!("Open vault index: {e}")))?;
            conn.execute_batch(INDEX_SCHEMA_SQL)
                .map_err(|e| AthenError::Vault(format!("Init vault index: {e}")))?;
            Ok(KeyringVault {
                service,
                index: Arc::new(Mutex::new(conn)),
            })
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn keyring open: {e}")))?
    }

    /// Round-trip a sentinel value to confirm the keychain is actually
    /// reachable. Used by the factory to decide whether to fall back.
    pub async fn self_check(&self) -> Result<()> {
        self.set(SELF_CHECK_SCOPE, SELF_CHECK_KEY, SELF_CHECK_VALUE)
            .await?;
        let got = self.get(SELF_CHECK_SCOPE, SELF_CHECK_KEY).await?;
        if got.as_deref() != Some(SELF_CHECK_VALUE) {
            return Err(AthenError::Vault(
                "Keyring self-check round-trip mismatch".into(),
            ));
        }
        // Leave the sentinel in place; deleting on every open thrashes the
        // keychain prompt on macOS without any benefit.
        Ok(())
    }

    fn entry(&self, scope: &str, key: &str) -> Result<Entry> {
        let user = format!("{scope}::{key}");
        Entry::new(&self.service, &user)
            .map_err(|e| AthenError::Vault(format!("Keyring entry: {e}")))
    }
}

#[async_trait]
impl Vault for KeyringVault {
    async fn set(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        let entry = self.entry(scope, key)?;
        let value = value.to_string();
        let scope_owned = scope.to_string();
        let key_owned = key.to_string();
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            entry
                .set_password(&value)
                .map_err(|e| AthenError::Vault(format!("Keyring set: {e}")))?;
            let conn = index.blocking_lock();
            conn.execute(
                "INSERT OR REPLACE INTO secret_index (scope, key) VALUES (?1, ?2)",
                params![scope_owned, key_owned],
            )
            .map_err(|e| AthenError::Vault(format!("Index insert: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn keyring set: {e}")))?
    }

    async fn get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let entry = self.entry(scope, key)?;
        tokio::task::spawn_blocking(move || match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AthenError::Vault(format!("Keyring get: {e}"))),
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn keyring get: {e}")))?
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<()> {
        let entry = self.entry(scope, key)?;
        let scope_owned = scope.to_string();
        let key_owned = key.to_string();
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => return Err(AthenError::Vault(format!("Keyring delete: {e}"))),
            }
            let conn = index.blocking_lock();
            conn.execute(
                "DELETE FROM secret_index WHERE scope = ?1 AND key = ?2",
                params![scope_owned, key_owned],
            )
            .map_err(|e| AthenError::Vault(format!("Index delete: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn keyring delete: {e}")))?
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        let scope = scope.to_string();
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            let conn = index.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT key FROM secret_index WHERE scope = ?1")
                .map_err(|e| AthenError::Vault(format!("Prepare index list: {e}")))?;
            let rows = stmt
                .query_map(params![scope], |r| r.get::<_, String>(0))
                .map_err(|e| AthenError::Vault(format!("Query index list: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Vault(format!("Index row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn keyring list: {e}")))?
    }
}

// Keyring tests need a real OS keychain daemon, which CI typically lacks.
// Cover the index/SQL bookkeeping and trust the upstream `keyring` crate's
// own coverage for the platform backends. End-to-end keyring use is
// exercised in dev when `open_vault` selects this backend.
