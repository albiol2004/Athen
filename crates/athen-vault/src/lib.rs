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

/// Backend selection for [`open_vault`], driven by `ATHEN_VAULT_BACKEND`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultBackend {
    /// Try the OS keyring, fall back to the encrypted file (default).
    Auto,
    /// Encrypted file only — never touch the OS keyring. The right choice
    /// for headless / containerized deployments where no secret-service
    /// daemon exists (avoids the D-Bus probe entirely).
    File,
    /// OS keyring only — fail hard instead of falling back, for operators
    /// who must guarantee secrets never land on disk.
    Keyring,
}

impl VaultBackend {
    /// Read `ATHEN_VAULT_BACKEND` (`auto` | `file` | `keyring`,
    /// case-insensitive). Unset, empty, or unrecognized values mean `Auto`;
    /// unrecognized values additionally log a WARN.
    pub fn from_env() -> Self {
        match std::env::var("ATHEN_VAULT_BACKEND") {
            Ok(s) => Self::parse(&s),
            Err(_) => VaultBackend::Auto,
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => VaultBackend::Auto,
            "file" => VaultBackend::File,
            "keyring" => VaultBackend::Keyring,
            other => {
                tracing::warn!(
                    value = other,
                    "ATHEN_VAULT_BACKEND unrecognized (expected auto|file|keyring); using auto"
                );
                VaultBackend::Auto
            }
        }
    }
}

/// Open the best available vault for `data_dir`.
///
/// Backend selection honors the `ATHEN_VAULT_BACKEND` env var (see
/// [`VaultBackend`]). In the default `auto` mode this tries the OS keyring
/// first; on any failure (no daemon, locked, sandboxed runtime) it falls
/// back to an encrypted file under `data_dir`. The fallback path is logged
/// at WARN so operators can spot keyring outages without having to inspect
/// what got chosen.
///
/// `service` identifies the application within the OS keychain — pass a
/// stable string like `"athen"`. The encrypted-file backend ignores it.
pub async fn open_vault(data_dir: &Path, service: &str) -> Result<Box<dyn Vault>> {
    open_vault_with(data_dir, service, VaultBackend::from_env()).await
}

/// [`open_vault`] with an explicit backend choice (env-independent form,
/// used by tests and by callers that already resolved the policy).
pub async fn open_vault_with(
    data_dir: &Path,
    service: &str,
    backend: VaultBackend,
) -> Result<Box<dyn Vault>> {
    match backend {
        VaultBackend::File => {
            let v = EncryptedFileVault::open(data_dir).await?;
            tracing::info!(
                backend = "encrypted_file",
                "Vault: opened encrypted file (forced via ATHEN_VAULT_BACKEND=file)"
            );
            return Ok(Box::new(v));
        }
        VaultBackend::Keyring => {
            let v = KeyringVault::new(data_dir, service).await?;
            v.self_check().await?;
            tracing::info!(
                backend = "keyring",
                "Vault: opened OS keychain (forced via ATHEN_VAULT_BACKEND=keyring)"
            );
            return Ok(Box::new(v));
        }
        VaultBackend::Auto => {}
    }
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
            tracing::info!(
                backend = "encrypted_file",
                "Vault: opened encrypted-file fallback"
            );
            Ok(Box::new(v))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Vault: keyring unavailable; using encrypted file");
            let v = EncryptedFileVault::open(data_dir).await?;
            tracing::info!(
                backend = "encrypted_file",
                "Vault: opened encrypted-file fallback"
            );
            Ok(Box::new(v))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parse_recognizes_all_variants() {
        assert_eq!(VaultBackend::parse("file"), VaultBackend::File);
        assert_eq!(VaultBackend::parse("FILE"), VaultBackend::File);
        assert_eq!(VaultBackend::parse(" keyring "), VaultBackend::Keyring);
        assert_eq!(VaultBackend::parse("auto"), VaultBackend::Auto);
        assert_eq!(VaultBackend::parse(""), VaultBackend::Auto);
        assert_eq!(VaultBackend::parse("nonsense"), VaultBackend::Auto);
    }

    #[tokio::test]
    async fn forced_file_backend_round_trips_without_keyring() {
        let td = tempfile::tempdir().unwrap();
        let v = open_vault_with(td.path(), "athen-test", VaultBackend::File)
            .await
            .unwrap();
        v.set("provider:deepseek", "api_key", "sk-test")
            .await
            .unwrap();
        let got = v.get("provider:deepseek", "api_key").await.unwrap();
        assert_eq!(got.as_deref(), Some("sk-test"));
        // Forced-file mode must materialize the file backend on disk.
        assert!(td.path().join("vault.key").exists());
    }
}
