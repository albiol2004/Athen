//! Encrypted-file vault backend.
//!
//! Layout under `data_dir`:
//! - `vault.key` — 32 random bytes, file mode `0600` on Unix.
//! - `vault.db` — SQLite with one table `secrets(scope, key, nonce, ciphertext)`.
//!
//! Each value is sealed with ChaCha20-Poly1305 using a fresh 12-byte random
//! nonce. The AAD binds the ciphertext to its `(scope, key)` location, so a
//! row swap on the DB file invalidates the auth tag. The master key file is
//! generated on first launch and never rotated automatically — that would
//! make every existing secret unreadable; rotation is a deliberate operation
//! left for a later release.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use zeroize::Zeroize;

use athen_core::error::{AthenError, Result};
use athen_core::traits::vault::Vault;

const KEY_FILENAME: &str = "vault.key";
const DB_FILENAME: &str = "vault.db";
const MASTER_KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS secrets (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    PRIMARY KEY (scope, key)
);
"#;

pub struct EncryptedFileVault {
    cipher: ChaCha20Poly1305,
    conn: Arc<Mutex<Connection>>,
}

impl EncryptedFileVault {
    /// Open or create the vault rooted at `data_dir`. Creates the directory
    /// if missing. Generates a fresh master key on first run.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let data_dir = data_dir.to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_blocking(&data_dir))
            .await
            .map_err(|e| AthenError::Vault(format!("Spawn open: {e}")))?
    }

    fn open_blocking(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| AthenError::Vault(format!("Create vault dir: {e}")))?;

        let key_path = data_dir.join(KEY_FILENAME);
        let mut key_bytes = load_or_create_master_key(&key_path)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
        key_bytes.zeroize();

        let db_path = data_dir.join(DB_FILENAME);
        let conn = Connection::open(&db_path)
            .map_err(|e| AthenError::Vault(format!("Open vault DB: {e}")))?;
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| AthenError::Vault(format!("Init vault schema: {e}")))?;

        Ok(Self {
            cipher,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn aad(scope: &str, key: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(scope.len() + 1 + key.len());
        out.extend_from_slice(scope.as_bytes());
        out.push(0);
        out.extend_from_slice(key.as_bytes());
        out
    }
}

fn load_or_create_master_key(path: &PathBuf) -> Result<[u8; MASTER_KEY_BYTES]> {
    if path.exists() {
        let bytes =
            std::fs::read(path).map_err(|e| AthenError::Vault(format!("Read master key: {e}")))?;
        if bytes.len() != MASTER_KEY_BYTES {
            return Err(AthenError::Vault(format!(
                "Master key file has wrong length: {} (expected {})",
                bytes.len(),
                MASTER_KEY_BYTES
            )));
        }
        let mut out = [0u8; MASTER_KEY_BYTES];
        out.copy_from_slice(&bytes);
        Ok(out)
    } else {
        let mut out = [0u8; MASTER_KEY_BYTES];
        OsRng.fill_bytes(&mut out);
        write_master_key(path, &out)?;
        Ok(out)
    }
}

#[cfg(unix)]
fn write_master_key(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| AthenError::Vault(format!("Create master key file: {e}")))?;
    use std::io::Write;
    f.write_all(bytes)
        .map_err(|e| AthenError::Vault(format!("Write master key: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_master_key(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    // On Windows the user data dir already lives under the user profile;
    // additional ACL hardening is left for a follow-up.
    std::fs::write(path, bytes).map_err(|e| AthenError::Vault(format!("Write master key: {e}")))?;
    Ok(())
}

#[async_trait]
impl Vault for EncryptedFileVault {
    async fn set(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce_bytes);
        let aad = Self::aad(scope, key);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: value.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| AthenError::Vault("Encrypt failed".into()))?;
        let conn = self.conn.clone();
        let scope = scope.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR REPLACE INTO secrets (scope, key, nonce, ciphertext) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![scope, key, nonce_bytes.to_vec(), ciphertext],
            )
            .map_err(|e| AthenError::Vault(format!("Insert secret: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn set: {e}")))?
    }

    async fn get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn.clone();
        let scope_owned = scope.to_string();
        let key_owned = key.to_string();
        let row: Option<(Vec<u8>, Vec<u8>)> = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.query_row(
                "SELECT nonce, ciphertext FROM secrets WHERE scope = ?1 AND key = ?2",
                params![scope_owned, key_owned],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(AthenError::Vault(format!("Query secret: {other}"))),
            })
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn get: {e}")))??;

        let Some((nonce_bytes, ciphertext)) = row else {
            return Ok(None);
        };
        if nonce_bytes.len() != NONCE_BYTES {
            return Err(AthenError::Vault("Stored nonce has wrong length".into()));
        }
        let aad = Self::aad(scope, key);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| AthenError::Vault("Decrypt failed (tampered or wrong key)".into()))?;
        let s = String::from_utf8(plaintext)
            .map_err(|e| AthenError::Vault(format!("Plaintext not UTF-8: {e}")))?;
        Ok(Some(s))
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<()> {
        let conn = self.conn.clone();
        let scope = scope.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM secrets WHERE scope = ?1 AND key = ?2",
                params![scope, key],
            )
            .map_err(|e| AthenError::Vault(format!("Delete secret: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn delete: {e}")))?
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        let scope = scope.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT key FROM secrets WHERE scope = ?1")
                .map_err(|e| AthenError::Vault(format!("Prepare list: {e}")))?;
            let rows = stmt
                .query_map(params![scope], |r| r.get::<_, String>(0))
                .map_err(|e| AthenError::Vault(format!("Query list: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Vault(format!("List row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Vault(format!("Spawn list: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn fresh_vault() -> (TempDir, EncryptedFileVault) {
        let dir = TempDir::new().unwrap();
        let v = EncryptedFileVault::open(dir.path()).await.unwrap();
        (dir, v)
    }

    #[tokio::test]
    async fn round_trip_set_get() {
        let (_dir, v) = fresh_vault().await;
        v.set("endpoint:jina", "api_key", "jina_secret_xyz")
            .await
            .unwrap();
        let got = v.get("endpoint:jina", "api_key").await.unwrap();
        assert_eq!(got.as_deref(), Some("jina_secret_xyz"));
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let (_dir, v) = fresh_vault().await;
        assert!(v.get("nope", "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn replace_overwrites() {
        let (_dir, v) = fresh_vault().await;
        v.set("s", "k", "v1").await.unwrap();
        v.set("s", "k", "v2").await.unwrap();
        assert_eq!(v.get("s", "k").await.unwrap().as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn delete_removes() {
        let (_dir, v) = fresh_vault().await;
        v.set("s", "k", "x").await.unwrap();
        v.delete("s", "k").await.unwrap();
        assert!(v.get("s", "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_missing_is_idempotent() {
        let (_dir, v) = fresh_vault().await;
        v.delete("never_set", "nope").await.unwrap();
    }

    #[tokio::test]
    async fn list_scoped_only() {
        let (_dir, v) = fresh_vault().await;
        v.set("a", "k1", "v").await.unwrap();
        v.set("a", "k2", "v").await.unwrap();
        v.set("b", "k3", "v").await.unwrap();
        let mut a = v.list("a").await.unwrap();
        a.sort();
        assert_eq!(a, vec!["k1".to_string(), "k2".to_string()]);
        let b = v.list("b").await.unwrap();
        assert_eq!(b, vec!["k3".to_string()]);
        assert!(v.list("c").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn persists_across_instances() {
        let dir = TempDir::new().unwrap();
        {
            let v = EncryptedFileVault::open(dir.path()).await.unwrap();
            v.set("persist", "k", "value-1").await.unwrap();
        }
        let v2 = EncryptedFileVault::open(dir.path()).await.unwrap();
        assert_eq!(
            v2.get("persist", "k").await.unwrap().as_deref(),
            Some("value-1")
        );
    }

    #[tokio::test]
    async fn empty_value_round_trips() {
        let (_dir, v) = fresh_vault().await;
        v.set("s", "blank", "").await.unwrap();
        assert_eq!(v.get("s", "blank").await.unwrap().as_deref(), Some(""));
    }

    #[tokio::test]
    async fn aad_binds_ciphertext_to_location() {
        // Move a ciphertext from (scope1,key1) to (scope2,key2) at the SQL
        // level. Decryption should fail because the AAD no longer matches.
        let dir = TempDir::new().unwrap();
        let v = EncryptedFileVault::open(dir.path()).await.unwrap();
        v.set("scope1", "k", "secret-A").await.unwrap();
        v.set("scope2", "k", "secret-B").await.unwrap();

        let conn = v.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Steal scope1's row and rewrite scope2's row to use the same
            // ciphertext+nonce. The nonce is fine; AAD differs → tag fails.
            let (nonce, ct): (Vec<u8>, Vec<u8>) = conn
                .query_row(
                    "SELECT nonce, ciphertext FROM secrets WHERE scope='scope1' AND key='k'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            conn.execute(
                "UPDATE secrets SET nonce=?1, ciphertext=?2 WHERE scope='scope2' AND key='k'",
                params![nonce, ct],
            )
            .unwrap();
        })
        .await
        .unwrap();

        let err = v.get("scope2", "k").await.unwrap_err();
        assert!(err.to_string().contains("Decrypt failed"), "got: {err}");
    }

    #[tokio::test]
    async fn corrupt_master_key_fails_decrypt() {
        let dir = TempDir::new().unwrap();
        {
            let v = EncryptedFileVault::open(dir.path()).await.unwrap();
            v.set("s", "k", "value").await.unwrap();
        }
        // Replace the master key with random bytes — old ciphertext is now
        // unreadable, but the vault should still open (no panic) and surface
        // a Decrypt error rather than returning the wrong plaintext.
        let mut new_key = [0u8; MASTER_KEY_BYTES];
        OsRng.fill_bytes(&mut new_key);
        std::fs::write(dir.path().join(KEY_FILENAME), new_key).unwrap();

        let v2 = EncryptedFileVault::open(dir.path()).await.unwrap();
        let err = v2.get("s", "k").await.unwrap_err();
        assert!(err.to_string().contains("Decrypt failed"), "got: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn master_key_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _v) = fresh_vault().await;
        let meta = std::fs::metadata(dir.path().join(KEY_FILENAME)).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "master key should be 0600, got {mode:o}");
    }
}
