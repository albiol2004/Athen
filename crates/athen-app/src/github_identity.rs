//! Resolver that turns an `AgentProfile.github_identity` into the env-var
//! bundle injected into every `shell_execute` invocation, so the agent's
//! git/gh commands authenticate and commit as the right account.
//!
//! Two vault scopes — `github:bot` and `github:user` — hold the same three
//! keys: `token`, `user_name`, `user_email`. Identity is a property of the
//! agent profile (set once in Settings); the resolver does no command
//! parsing or cwd heuristics — every `shell_execute` from a profile that
//! opts in gets the same env, period.
//!
//! Vars set:
//! - `GH_TOKEN` + `GITHUB_TOKEN` (gh CLI + most git credential helpers)
//! - `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL`
//! - `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`
//! - `GH_CONFIG_DIR` — per-identity dir under `<data_dir>/github/<bot|user>`
//!   so the bot's `gh auth status` never collides with the user's `~/.config/gh`.
//!
//! Token-leak posture: env vars are visible to any child process. That's
//! the same posture as `gh auth login` or any other secret-in-env CLI; the
//! relevant threat is the agent itself echoing the token into its own
//! tool output or commit message, not a foreign process inspecting our env.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use athen_agent::tools::GithubIdentityResolver;
use athen_core::agent_profile::GithubIdentity;
use athen_core::traits::vault::Vault;

/// Vault-backed implementation of [`GithubIdentityResolver`].
pub struct VaultGithubIdentityResolver {
    vault: Arc<dyn Vault>,
    /// Base dir for per-identity `GH_CONFIG_DIR`. Usually `<data_dir>/github`.
    /// Subdirs `bot/` and `user/` are created on demand by the gh CLI when
    /// it first writes to them.
    base_dir: Option<PathBuf>,
}

impl VaultGithubIdentityResolver {
    pub fn new(vault: Arc<dyn Vault>, base_dir: Option<PathBuf>) -> Self {
        Self { vault, base_dir }
    }
}

#[async_trait]
impl GithubIdentityResolver for VaultGithubIdentityResolver {
    async fn resolve_env_vars(&self, identity: GithubIdentity) -> Vec<(String, String)> {
        let Some(scope) = identity.vault_scope() else {
            return Vec::new();
        };
        let mut env: Vec<(String, String)> = Vec::with_capacity(7);

        if let Ok(Some(token)) = self.vault.get(scope, "token").await {
            if !token.is_empty() {
                // Both vars are recognised across the ecosystem. `GH_TOKEN`
                // is the gh CLI's primary name; `GITHUB_TOKEN` is the
                // alias most git credential helpers and tools accept.
                env.push(("GH_TOKEN".into(), token.clone()));
                env.push(("GITHUB_TOKEN".into(), token));
            }
        }

        if let Ok(Some(name)) = self.vault.get(scope, "user_name").await {
            if !name.is_empty() {
                env.push(("GIT_AUTHOR_NAME".into(), name.clone()));
                env.push(("GIT_COMMITTER_NAME".into(), name));
            }
        }

        if let Ok(Some(email)) = self.vault.get(scope, "user_email").await {
            if !email.is_empty() {
                env.push(("GIT_AUTHOR_EMAIL".into(), email.clone()));
                env.push(("GIT_COMMITTER_EMAIL".into(), email));
            }
        }

        // Per-identity gh config dir — keeps the bot's stored state out
        // of the user's `~/.config/gh`. The dir is created lazily by gh
        // itself when it first writes a config; we just point the var.
        if let Some(ref base) = self.base_dir {
            let sub = match identity {
                GithubIdentity::Bot => "bot",
                GithubIdentity::User => "user",
                GithubIdentity::None => return env,
            };
            let dir = base.join(sub);
            // Best-effort create — gh will also create on first write,
            // but doing it here avoids a confusing "no such file"
            // first-time error if a tool reads the dir directly.
            let _ = std::fs::create_dir_all(&dir);
            env.push(("GH_CONFIG_DIR".into(), dir.to_string_lossy().into_owned()));
        }

        env
    }
}
