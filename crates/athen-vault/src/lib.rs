//! Encrypted credential vault for Athen.
//!
//! Two backends share the same `Vault` trait from `athen-core`:
//! - [`EncryptedFileVault`] — chacha20poly1305 + a random 32-byte master key
//!   stored alongside the vault DB. Always available.
//! - [`KeyringVault`] — OS keychain (macOS Keychain / Windows Credential
//!   Manager / Linux Secret Service). Available when the platform exposes one
//!   and the daemon is reachable.
//!
//! Production code wires up via [`open_vault`], which prefers the keyring
//! and falls back to the encrypted file when the keyring round-trip fails.

mod encrypted_file;
mod keyring_backend;

use std::path::Path;

use athen_core::error::Result;
use athen_core::traits::vault::Vault;

pub use encrypted_file::EncryptedFileVault;
pub use keyring_backend::KeyringVault;

/// Open the best available vault for `data_dir`.
///
/// Tries the OS keyring first; on any failure (no daemon, locked, sandboxed
/// runtime) falls back to an encrypted file under `data_dir`. The fallback
/// path is logged at WARN so operators can spot keyring outages without
/// having to inspect what got chosen.
///
/// `service` identifies the application within the OS keychain — pass a
/// stable string like `"athen"`. The encrypted-file backend ignores it.
pub async fn open_vault(data_dir: &Path, service: &str) -> Result<Box<dyn Vault>> {
    match KeyringVault::new(data_dir, service).await {
        Ok(v) if v.self_check().await.is_ok() => {
            tracing::info!(
                backend = "keyring",
                "Vault: opened OS keychain (secrets live there; only the key index is on disk)"
            );
            Ok(Box::new(v))
        }
        Ok(_) => {
            tracing::warn!(
                "Vault: keyring opened but self-check failed; falling back to encrypted file"
            );
            let v = EncryptedFileVault::open(data_dir).await?;
            tracing::info!(backend = "encrypted_file", "Vault: opened encrypted-file fallback");
            Ok(Box::new(v))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Vault: keyring unavailable; using encrypted file");
            let v = EncryptedFileVault::open(data_dir).await?;
            tracing::info!(backend = "encrypted_file", "Vault: opened encrypted-file fallback");
            Ok(Box::new(v))
        }
    }
}
