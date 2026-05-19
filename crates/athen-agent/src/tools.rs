//! Built-in tool registry backed by shell execution and filesystem operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use athen_core::contact::TrustLevel;
use athen_core::error::{AthenError, Result};
use athen_core::paths;
use athen_core::risk::{BaseImpact, DataSensitivity, RiskContext, RiskLevel};
use athen_core::sandbox::{SandboxLevel, SandboxProfile};
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::checkpoint::CheckpointStore;
use athen_core::traits::email_sender::{EmailSender, OutboundEmail};
use athen_core::traits::shell::{ShellExecutor, ShellOptions};
use athen_core::traits::telegram_sender::{
    OutboundTelegramMessage, TelegramAttachment, TelegramAttachmentKind, TelegramSender,
};
use athen_core::traits::tool::ToolRegistry;
use athen_risk::rules::RuleEngine;
use athen_sandbox::UnifiedSandbox;
use athen_shell::Shell;

/// Suppress the cmd.exe / console window flash that Windows attaches to GUI
/// parents when they spawn console subprocesses. No-op on non-Windows.
/// `CREATE_NO_WINDOW = 0x0800_0000`.
#[inline]
fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

#[cfg(windows)]
#[inline]
fn hide_console_std(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000)
}

/// Cached identifier of the shell `Shell::execute*` actually routes
/// through. Probed once on first call. Returns `"nushell"` when the
/// embedded nushell is available (bundled sidecar or `nu` on PATH),
/// otherwise the platform native shell name (`"sh"` on Unix, `"cmd"`
/// on Windows). Cheap to call repeatedly after first invocation.
///
/// The agent's system prompt uses this to teach shell-correct syntax:
/// bash idioms (`&&`, `>file 2>&1`, `nohup CMD &`, `export X=Y`)
/// silently fail under nushell or cmd, and conversely.
pub async fn detect_shell_kind() -> &'static str {
    static SHELL_KIND: tokio::sync::OnceCell<&'static str> = tokio::sync::OnceCell::const_new();
    *SHELL_KIND
        .get_or_init(|| async {
            let s = Shell::new().await;
            if s.has_nushell() {
                "nushell"
            } else if cfg!(windows) {
                "cmd"
            } else {
                "sh"
            }
        })
        .await
}
use athen_web::{DuckDuckGoSearch, HybridReader, PageReader, WebSearchProvider};

/// Provider used by the shell tool to discover the per-arc set of writable
/// directories that should be exposed to the sandbox in addition to the
/// hardcoded default writable set (`/tmp` plus the data dir).
///
/// The `app_tools` layer wires this against the `GrantStore`. When no
/// provider is set the shell still works — the agent just won't be able to
/// write outside the default safe locations.
#[async_trait]
pub trait ShellExtraWritableProvider: Send + Sync {
    async fn extra_writable_paths(&self) -> Vec<PathBuf>;
}

/// Permission gate consulted before `install_package` actually invokes
/// pip/npm. Implementations typically surface a confirmation prompt to
/// the user (in-app card + Telegram inline keyboard) and return `true`
/// only on Approve. With no gate wired, the registry fails closed —
/// the agent can `list_installed_packages` but never installs without
/// user consent.
#[async_trait]
pub trait ToolboxApprovalGate: Send + Sync {
    async fn confirm_install(&self, runtime: &str, package: &str, reason: &str) -> bool;
}

/// Summary handed to the email approval gate. Carries everything the
/// user needs to decide without opening another screen — recipients,
/// subject, a short preview of the body, and (for replies) the
/// referenced message-id.
#[derive(Debug, Clone)]
pub struct EmailSendSummary {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    /// First ~200 chars of the plain body for the approval prompt.
    pub body_preview: String,
    /// Set when the agent is replying to an existing thread.
    pub in_reply_to: Option<String>,
}

/// Summary handed to the Telegram approval gate. Carries the
/// information the user needs to decide: destination chat (resolved),
/// message text preview, and attachment metadata (paths + kinds).
#[derive(Debug, Clone)]
pub struct TelegramSendSummary {
    /// Resolved destination chat (after the default-owner-chat fallback).
    pub chat_id: i64,
    /// `true` when the destination is the bot's configured owner chat.
    /// The agent layer auto-approves when this is set.
    pub to_owner: bool,
    /// First ~200 chars of `text`. Empty when the message is
    /// attachments-only.
    pub text_preview: String,
    /// Absolute paths of each attachment, in send order.
    pub attachment_paths: Vec<PathBuf>,
    /// `Photo` / `Document` / `Auto` per attachment, parallel to
    /// `attachment_paths`.
    pub attachment_kinds: Vec<TelegramAttachmentKind>,
}

/// Permission gate consulted before `send_telegram` hands a message to
/// the Bot API. Mirrors [`EmailSendApprovalGate`].
#[async_trait]
pub trait TelegramSendApprovalGate: Send + Sync {
    /// Returns `true` only when the user approves. Any other outcome
    /// (deny, timeout, transport error) returns `false` so sends fail
    /// closed.
    async fn confirm_send(&self, summary: &TelegramSendSummary) -> bool;
}

/// Summary of one outbound Telegram send, handed to
/// [`TelegramOutboundRecorder::record`]. Carries enough context for the
/// composition root to (a) stamp the cross-channel arc-routing hint and
/// (b) append a row to the per-`chat_id` transcript log for future
/// system-context injection.
pub struct OutboundTelegramSummary<'a> {
    pub chat_id: i64,
    /// `None` when the message was attachments-only.
    pub text: Option<&'a str>,
    pub attachment_count: usize,
}

/// Notified after `send_telegram` successfully delivers, so the
/// composition root can record "the last outbound Telegram message
/// belongs to arc X" (cross-channel arc routing) and append the
/// outbound bubble to the per-chat transcript (context injection on the
/// next inbound). Implementations should fail silently — recorder
/// errors must never surface as send failures.
#[async_trait]
pub trait TelegramOutboundRecorder: Send + Sync {
    async fn record(&self, summary: OutboundTelegramSummary<'_>);
}

/// Permission gate consulted before `email_send` hands a message to
/// the SMTP transport. Mirrors [`ToolboxApprovalGate`] in spirit:
/// implementations surface a confirmation prompt (in-app card +
/// Telegram inline keyboard) and return `true` only on Approve.
/// Every other outcome (deny, timeout, transport error) returns
/// `false` so sends fail closed.
#[async_trait]
pub trait EmailSendApprovalGate: Send + Sync {
    /// Returns `true` only when the user approves. Any other outcome
    /// (deny, timeout, transport error) returns `false` so sends fail
    /// closed.
    async fn confirm_send(&self, summary: &EmailSendSummary) -> bool;
}

/// Lightweight check the `email_send` path consults before invoking
/// the approval gate: is this destination email address one of the
/// owner's own identifiers?
///
/// Lives in `athen-agent` (rather than depending on `athen-contacts`)
/// to keep the hexagonal dep graph clean. The composition root in
/// `athen-app` implements this trait for `athen_contacts::OwnerLookup`
/// and injects it via [`ShellToolRegistry::with_owner_check`].
///
/// Semantics: when `email_send` finds the destination set is
/// *non-empty* and *every* address matches the owner, the approval
/// gate is bypassed (the user can't meaningfully "approve sending an
/// email to themselves"). A single non-owner address still routes
/// through the gate — BCC'ing an external while CC'ing the owner does
/// **not** bypass.
#[async_trait]
pub trait OwnerDestinationCheck: Send + Sync {
    /// Compare `email` (already lowercased by the caller) against the
    /// owner contact's email identifiers. Returns `false` when no
    /// owner is configured.
    async fn is_owner_email(&self, email: &str) -> bool;
}

/// Resolves a [`GithubIdentity`] to the env-var bundle injected into
/// `shell_execute` invocations so git/gh commands authenticate and
/// commit as the right account.
///
/// Implementations look up the user's stored PAT + commit-author name +
/// email under `identity.vault_scope()` and return:
/// - `GH_TOKEN` (gh + most git credential helpers pick this up)
/// - `GITHUB_TOKEN` (alias accepted by many tools)
/// - `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL`
/// - `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`
/// - `GH_CONFIG_DIR` (per-identity dir so the bot's `gh` state never
///   collides with the user's `~/.config/gh`)
///
/// Failing to resolve is not an error: implementations return whatever
/// they have (possibly empty) and the agent's git/gh commands run
/// without auth rather than refusing to run. This keeps the agent
/// useful even when only some of the credentials are configured.
#[async_trait]
pub trait GithubIdentityResolver: Send + Sync {
    /// Resolve identity → env-var bundle. Returns an empty Vec when the
    /// identity is `None` or no values are configured. The returned
    /// list is suitable to splice directly into a child-process env.
    async fn resolve_env_vars(
        &self,
        identity: athen_core::agent_profile::GithubIdentity,
    ) -> Vec<(String, String)>;
}

/// Metadata for a process started via `shell_spawn`. Kept in
/// [`ShellToolRegistry::spawned`] so subsequent `shell_logs`/`shell_kill`
/// calls can find the log file and validate the PID is one we own.
#[derive(Clone, Debug)]
pub struct SpawnedProcess {
    pub pid: u32,
    pub command: String,
    pub label: Option<String>,
    pub log_path: PathBuf,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Shared map of spawned processes. Hosted by `AppState` so it survives
/// across the per-message `ShellToolRegistry` instances and the agent can
/// kill in turn N a process it spawned in turn N-1.
pub type SpawnedProcessMap = Arc<Mutex<HashMap<u32, SpawnedProcess>>>;

/// Optional sink notified every time the spawned-process map mutates.
/// The app layer wires this to a JSON pidfile (`spawned_pids.json`) so a
/// crash/power-loss leaves a recoverable record — on next start the
/// reconciler best-effort kills any orphans before fresh wake-ups can
/// re-spawn watchers on top of them.
///
/// Implementations should atomically rewrite their store and swallow IO
/// errors (log, don't panic). The snapshot is the current full map in no
/// particular order — receivers should treat it as the truth, not as a
/// delta.
#[async_trait]
pub trait SpawnPersistenceHook: Send + Sync {
    async fn on_change(&self, snapshot: Vec<SpawnedProcess>);
}

/// Snapshot the map's values without the surrounding lock. Used by the
/// hook firing path so we don't hold the spawned-map lock across an
/// async fs write (which would also need to grab it on the next
/// shell_spawn).
async fn snapshot_spawned(map: &SpawnedProcessMap) -> Vec<SpawnedProcess> {
    map.lock().await.values().cloned().collect()
}

/// Force-kill every process tracked in `map` and clear the map. Used by
/// the auto-updater right before it overwrites the install dir and by
/// the graceful-shutdown coordinator at app exit: any `shell_spawn`'d
/// watcher that's chained into `nu.exe` (cmd /C nu -c …) would
/// otherwise hold the bundled sidecar locked. Post-update / post-exit
/// those processes would be unmanageable orphans anyway — the in-memory
/// PID map is gone — so killing them is the right semantic.
///
/// When `hook` is provided, fires a single final `on_change` with the
/// (empty) snapshot after the clear so the pidfile reflects truth.
pub async fn kill_all_spawned(
    map: &SpawnedProcessMap,
    hook: Option<&Arc<dyn SpawnPersistenceHook>>,
) -> usize {
    // Snapshot + clear under the lock so concurrent shell_spawn calls
    // can't slip a new entry in while we iterate.
    let pids: Vec<u32> = {
        let mut guard = map.lock().await;
        let pids = guard.keys().copied().collect();
        guard.clear();
        pids
    };
    if let Some(h) = hook {
        h.on_change(Vec::new()).await;
    }
    if pids.is_empty() {
        return 0;
    }
    tracing::info!(count = pids.len(), "killing spawned processes");
    for pid in &pids {
        kill_spawned_pid(*pid).await;
    }
    pids.len()
}

/// Best-effort SIGKILL / taskkill for a single PID. Public so the pidfile
/// reconciler can call it on app startup without needing the in-memory
/// `SpawnedProcessMap` (which is, by definition, empty in a fresh process).
#[cfg(unix)]
pub async fn kill_spawned_pid(pid: u32) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    // Same pattern as do_shell_kill's force path: hit the process group
    // first (children of `sh -c`), then the bare PID as a fallback.
    let pgid = Pid::from_raw(-(pid as i32));
    let direct = Pid::from_raw(pid as i32);
    let _ = kill(pgid, Signal::SIGKILL);
    let _ = kill(direct, Signal::SIGKILL);
}

#[cfg(windows)]
pub async fn kill_spawned_pid(pid: u32) {
    let mut cmd = tokio::process::Command::new("taskkill");
    hide_console_tokio(&mut cmd);
    cmd.arg("/PID").arg(pid.to_string()).arg("/T").arg("/F");
    let _ = cmd.output().await;
}

/// A [`ToolRegistry`] that provides built-in tools for shell execution,
/// filesystem operations, and in-session key-value memory,
/// backed by [`athen_shell::Shell`].
pub struct ShellToolRegistry {
    shell: Shell,
    memory: Arc<Mutex<HashMap<String, String>>>,
    sandbox: Option<UnifiedSandbox>,
    rule_engine: RuleEngine,
    extra_writable: Option<Arc<dyn ShellExtraWritableProvider>>,
    /// Per-path blake3 hash of the file contents the agent has most recently
    /// `read`. Used by `edit` and `write` to enforce read-before-modify and
    /// detect external changes between reads and writes.
    read_state: Arc<Mutex<HashMap<PathBuf, blake3::Hash>>>,
    /// Processes started via `shell_spawn`, keyed by PID. We only ever
    /// kill PIDs in this map — refusing to touch arbitrary system PIDs.
    spawned: Arc<Mutex<HashMap<u32, SpawnedProcess>>>,
    /// Backend for the `web_search` tool. Defaults to bundled DuckDuckGo
    /// HTML scraping (no key). Can be swapped for Tavily etc. via
    /// [`Self::with_web_search`].
    web_search: Arc<dyn WebSearchProvider>,
    /// Backend for the `web_fetch` tool. Defaults to a local reqwest-based
    /// reader. Can be swapped for Cloudflare's Browser Rendering via
    /// [`Self::with_page_reader`].
    page_reader: Arc<dyn PageReader>,
    /// Optional permission gate consulted before `install_package`
    /// actually invokes pip/npm. Without it, install requests are
    /// denied with a clear message — failing closed avoids silent
    /// installs in CLI usage where there's no user to ask.
    toolbox_approval: Option<Arc<dyn ToolboxApprovalGate>>,
    /// Outbound SMTP transport for the `email_send` tool. `None` means
    /// SMTP isn't configured; `email_send` refuses with a clear "not
    /// configured" error rather than silently dropping mail.
    email_sender: Option<Arc<dyn EmailSender>>,
    /// Approval gate consulted before every `email_send`. `None` means
    /// no gate is wired — the tool refuses to send so emails can never
    /// leave without user consent.
    email_approval: Option<Arc<dyn EmailSendApprovalGate>>,
    /// Optional owner-destination check. When wired, `email_send`
    /// bypasses the approval gate iff *every* recipient resolves to
    /// the owner's own email identifiers. Unwired (or no owner set) →
    /// behaviour is unchanged: the gate runs unconditionally.
    owner_check: Option<Arc<dyn OwnerDestinationCheck>>,
    /// Outbound Telegram transport for the `send_telegram` tool.
    /// `None` means the bot isn't configured; the tool refuses with a
    /// clear "not configured" error rather than dropping the message.
    telegram_sender: Option<Arc<dyn TelegramSender>>,
    /// Approval gate consulted before every non-owner `send_telegram`.
    /// `None` means no gate is wired and the tool refuses non-owner
    /// sends. Sends to the bot's owner chat bypass the gate (the user
    /// is messaging themselves).
    telegram_approval: Option<Arc<dyn TelegramSendApprovalGate>>,
    /// Best-effort post-send recorder. Fires after every successful
    /// `send_telegram` so the composition root can stamp the outbound
    /// hint — letting the user's next Telegram reply route back to this
    /// arc instead of getting re-triaged.
    telegram_outbound_recorder: Option<Arc<dyn TelegramOutboundRecorder>>,
    /// Optional sink notified every time the spawned-process map mutates.
    /// Wired by the app layer to a JSON pidfile under `<data_dir>/`. Tests
    /// and CLI builds leave it `None` and the registry behaves as before.
    spawn_persistence: Option<Arc<dyn SpawnPersistenceHook>>,
    /// Which GitHub credentials (if any) `shell_execute` should inject
    /// into git/gh commands. Set by the composition root from the
    /// active agent profile. Defaults to `None` so unconfigured builds
    /// (CLI, tests) behave exactly as before.
    github_identity: athen_core::agent_profile::GithubIdentity,
    /// Resolves the GitHub identity (above) to an env-var bundle.
    /// Wired by `athen-app` against the vault. When `None` (CLI/tests),
    /// no env injection happens regardless of `github_identity`.
    github_identity_resolver: Option<Arc<dyn GithubIdentityResolver>>,
    /// Git-backed snapshot store. When wired together with
    /// `checkpoint_arc_id`, the `write`/`edit` tools snapshot pre-state
    /// before mutating so the user can revert. `None` on CLI/tests.
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    /// Arc the snapshot store should associate this registry's actions
    /// with. Set per-arc by the composition root. `None` falls back to
    /// "no snapshot" even if the store is wired.
    checkpoint_arc_id: Option<String>,
}

/// Defense-in-depth: tolerate args delivered as a JSON-encoded string
/// instead of a JSON object. Some LLM providers occasionally emit the
/// `arguments` field as a string when its value contains tricky escapes
/// (large HTML/code with raw newlines, etc.). The provider layer
/// repairs and parses these eagerly with a lenient pass, but if a
/// `Value::String` ever reaches a `do_*` function we attempt the same
/// repair here so the tool still works.
///
/// Returns a reference to either the original `args` (when it's already
/// an object/array) or the parsed-into-`owned` value when it was a
/// JSON-encoded string. If the string can't be parsed even after
/// repair, returns the original — the caller surfaces a clean
/// "missing 'X'" error.
fn coerce_args<'a>(
    args: &'a serde_json::Value,
    owned: &'a mut serde_json::Value,
) -> &'a serde_json::Value {
    if let Some(s) = args.as_str() {
        // Strict parse first.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
            *owned = parsed;
            return owned;
        }
        // Same lenient repair the provider layer uses: re-escape raw
        // control characters inside string literals and try again.
        let repaired = repair_unescaped_control_chars(s);
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&repaired) {
            tracing::warn!(
                "Tool args reached do_* as Value::String; repaired and parsed (len={})",
                s.len()
            );
            *owned = parsed;
            return owned;
        }
    }
    args
}

/// Build the LLM-facing error returned when tool args arrive at a
/// `do_*` helper as a `Value::String`. By the time we get here, the
/// provider's strict + control-char + aggressive-quote repair passes
/// have all failed — the overwhelmingly common cause is the LLM
/// hitting the output token limit mid-string, leaving the args
/// truncated. We tell the model exactly that, plus a tool-specific
/// suggestion for how to retry without looping on the same too-large
/// payload.
fn args_truncated_result(tool: &str, raw: &str, suggestion: &str, started: Instant) -> ToolResult {
    let head: String = raw.chars().take(120).collect();
    let tail: String = {
        let s: String = raw.chars().rev().take(120).collect();
        s.chars().rev().collect()
    };
    let msg = format!(
        "{tool}: tool-call arguments could not be parsed as JSON \
         (raw len={}). The most likely cause is that the model output \
         was truncated by the token limit mid-string — the args buffer \
         ends without a closing quote/brace. Suggestion: {suggestion}.",
        raw.len()
    );
    ToolResult {
        success: false,
        output: serde_json::json!({
            "error": msg,
            "raw_len": raw.len(),
            "raw_head": head,
            "raw_tail": tail,
        }),
        error: Some(msg),
        execution_time_ms: started.elapsed().as_millis() as u64,
    }
}

/// Single-quote a path/value for safe inclusion in a `sh -c` command.
/// Mirrors what `shell-words` would do: every embedded `'` becomes
/// `'\''`. Cheap, allocation-once.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Walk the input and escape unescaped control characters inside JSON
/// string literals only. Mirrors `escape_control_chars_in_strings` in
/// the provider layer; duplicated here to avoid an inter-crate
/// dependency on a private helper.
fn repair_unescaped_control_chars(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut in_string = false;
    let mut prev_backslash = false;
    for ch in input.chars() {
        if in_string {
            if prev_backslash {
                out.push(ch);
                prev_backslash = false;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    prev_backslash = true;
                }
                '"' => {
                    out.push(ch);
                    in_string = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Returns `Some(reason)` when `path` is a credential / data-store file the
/// agent should not be able to read. Centralises the policy so `read`,
/// `grep`, and `list_directory` enforce it consistently.
///
/// Blocklist over allowlist: `tools/`, `toolbox/`, `workspace/`, `files/`
/// stay readable so the agent can still discover its toolset, list installed
/// packages, and inspect its own workspace. Only the credential-bearing
/// surfaces (`config.toml`, `athen.db` + WAL/SHM siblings) and the large
/// binary `runtimes/` tree are off-limits.
fn forbidden_data_path(path: &Path) -> Option<&'static str> {
    let data = paths::athen_data_dir()?;
    forbidden_data_path_in(path, &data)
}

/// Same as [`forbidden_data_path`] but takes the data dir explicitly so
/// tests can inject a tempdir without mutating process env vars.
fn forbidden_data_path_in(path: &Path, data: &Path) -> Option<&'static str> {
    if path == data.join("config.toml") {
        return Some("config.toml contains credentials and is not readable from tools");
    }
    for db_name in [
        "athen.db",
        "athen.db-wal",
        "athen.db-shm",
        "athen.db-journal",
    ] {
        if path == data.join(db_name) {
            return Some("athen.db is not readable from tools");
        }
    }
    if path.starts_with(data.join("runtimes")) {
        return Some("~/.athen/runtimes is not readable from tools");
    }
    None
}

impl ShellToolRegistry {
    /// Compose the shell wrapper that the bwrap-sandboxed branch runs
    /// around every `shell_execute` command. bwrap is Linux-only and the
    /// inner command is always invoked through `sh -c`, so bash syntax is
    /// safe here.
    ///
    /// 1. `cd <workspace>` so relative paths land in the agent workspace.
    /// 2. Export `PYTHONPATH=<toolbox/python>:<existing>` so `pip
    ///    install --target` modules import without env juggling.
    /// 3. Prepend `<toolbox/node>/bin` onto `PATH` so npm-installed
    ///    bins are first in lookup order.
    /// 4. Run the user's command inside `( ... )` so embedded `;`
    ///    doesn't escape any of the above.
    ///
    /// For the unsandboxed path, prefer [`build_shell_env`] — it expresses
    /// the same intent through the OS process API (`Command::env`/`cwd`)
    /// and works on any shell (sh, bash, zsh, nushell, cmd, pwsh) on any
    /// OS without embedding shell-specific syntax in the command string.
    fn build_shell_wrapper(command: &str) -> String {
        let mut out = String::new();
        if let Some(ws) = paths::athen_workspace_dir() {
            out.push_str(&format!("cd {} && ", sh_quote(&ws.to_string_lossy())));
        }
        if let Some(pydir) = paths::athen_toolbox_python_dir() {
            let existing = std::env::var("PYTHONPATH").unwrap_or_default();
            let combined = if existing.is_empty() {
                pydir.to_string_lossy().into_owned()
            } else {
                format!("{}:{}", pydir.to_string_lossy(), existing)
            };
            out.push_str(&format!("export PYTHONPATH={} && ", sh_quote(&combined)));
        }
        if let Some(nodedir) = paths::athen_toolbox_node_dir() {
            let bin = nodedir.join("bin");
            let existing = std::env::var("PATH").unwrap_or_default();
            let combined = if existing.is_empty() {
                bin.to_string_lossy().into_owned()
            } else {
                format!("{}:{}", bin.to_string_lossy(), existing)
            };
            out.push_str(&format!("export PATH={} && ", sh_quote(&combined)));
        }
        if out.is_empty() {
            command.to_string()
        } else {
            out.push_str(&format!("( {command} )"));
            out
        }
    }

    /// Compute the env-var overrides and cwd to apply to a `shell_execute`
    /// command via the OS process API. This is the cross-shell, cross-OS
    /// counterpart to [`build_shell_wrapper`]: we never embed `export …`
    /// or `cd …` in the command text, so the same options work for sh,
    /// bash, zsh, nushell, cmd, and pwsh.
    ///
    /// PATH is composed with the platform-correct separator
    /// (`std::env::join_paths`); on Windows that's `;` and Windows path
    /// elements containing `:` (drive letters) survive unmangled. The npm
    /// `--prefix` install target lives at `<toolbox>/node/bin` on Unix
    /// and directly at `<toolbox>/node` on Windows, so we adapt the bin
    /// path accordingly.
    fn build_shell_env() -> (Vec<(String, String)>, Option<PathBuf>) {
        let mut env: Vec<(String, String)> = Vec::new();

        if let Some(pydir) = paths::athen_toolbox_python_dir() {
            let existing = std::env::var_os("PYTHONPATH");
            let mut parts: Vec<PathBuf> = vec![pydir];
            if let Some(ref ex) = existing {
                parts.extend(std::env::split_paths(ex));
            }
            if let Ok(joined) = std::env::join_paths(parts) {
                env.push((
                    "PYTHONPATH".to_string(),
                    joined.to_string_lossy().into_owned(),
                ));
            }
        }

        if let Some(nodedir) = paths::athen_toolbox_node_dir() {
            let node_bin = if cfg!(windows) {
                // npm on Windows installs binaries directly under the
                // prefix (no `bin/` subdir).
                nodedir
            } else {
                nodedir.join("bin")
            };
            let existing = std::env::var_os("PATH");
            let mut parts: Vec<PathBuf> = vec![node_bin];
            if let Some(ref ex) = existing {
                parts.extend(std::env::split_paths(ex));
            }
            if let Ok(joined) = std::env::join_paths(parts) {
                env.push(("PATH".to_string(), joined.to_string_lossy().into_owned()));
            }
        }

        let cwd = paths::athen_workspace_dir();
        (env, cwd)
    }

    /// Best-effort: create the agent workspace dir so relative paths in
    /// `read`/`edit`/`write`/`grep` and shell `cd <workspace>` wrapping have
    /// a real directory to land in. Failure is logged but not fatal — we'll
    /// surface a real error on first use if the path is genuinely unwritable.
    async fn ensure_workspace_dir() {
        if let Some(ws) = paths::athen_workspace_dir() {
            if let Err(e) = tokio::fs::create_dir_all(&ws).await {
                tracing::warn!(
                    workspace = %ws.display(),
                    error = %e,
                    "Failed to create agent workspace dir"
                );
            }
        }
    }

    /// Create a new registry, auto-detecting the available shell backend
    /// and sandbox capabilities.
    pub async fn new() -> Self {
        Self::ensure_workspace_dir().await;
        let sandbox = match UnifiedSandbox::new().await {
            Ok(sb) => {
                let caps = sb.capabilities();
                if caps.bubblewrap || caps.landlock || caps.macos_sandbox || caps.windows_sandbox {
                    tracing::info!("Sandbox available for shell tool execution");
                    Some(sb)
                } else {
                    tracing::info!(
                        "No OS-native sandbox capabilities detected, \
                         shell commands will run unsandboxed"
                    );
                    None
                }
            }
            Err(e) => {
                tracing::warn!("Failed to initialize sandbox, proceeding without: {e}");
                None
            }
        };

        // Pre-create the persistent toolbox dirs so the first
        // `pip install --target` doesn't have to. Failure is logged
        // but never fatal — install_*_package retries dir creation on
        // its own.
        crate::toolbox::ensure_toolbox_dirs().await;

        Self {
            shell: Shell::new().await,
            memory: Arc::new(Mutex::new(HashMap::new())),
            sandbox,
            rule_engine: RuleEngine::new(),
            extra_writable: None,
            read_state: Arc::new(Mutex::new(HashMap::new())),
            spawned: Arc::new(Mutex::new(HashMap::new())),
            web_search: Arc::new(DuckDuckGoSearch::new()),
            // HybridReader chains Local → Jina → Wayback so SPAs and
            // paywalled/blocked pages still produce content without any
            // user configuration.
            page_reader: Arc::new(HybridReader::new()),
            toolbox_approval: None,
            email_sender: None,
            email_approval: None,
            owner_check: None,
            telegram_sender: None,
            telegram_approval: None,
            telegram_outbound_recorder: None,
            spawn_persistence: None,
            github_identity: athen_core::agent_profile::GithubIdentity::None,
            github_identity_resolver: None,
            checkpoint_store: None,
            checkpoint_arc_id: None,
        }
    }

    /// Set which GitHub identity this registry's `shell_execute`
    /// invocations should authenticate as. Must be paired with
    /// [`Self::with_github_identity_resolver`] for env vars to actually
    /// land in the child process; without a resolver the identity is
    /// captured but ignored.
    pub fn with_github_identity(
        mut self,
        identity: athen_core::agent_profile::GithubIdentity,
    ) -> Self {
        self.github_identity = identity;
        self
    }

    /// Inject the resolver that maps the captured `github_identity` to
    /// an env-var bundle (PAT + author/committer names). Without it,
    /// no env injection happens — the agent's git/gh commands run with
    /// whatever the host shell would see.
    pub fn with_github_identity_resolver(
        mut self,
        resolver: Arc<dyn GithubIdentityResolver>,
    ) -> Self {
        self.github_identity_resolver = Some(resolver);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_github_identity_resolver`].
    pub fn with_github_identity_resolver_opt(
        self,
        resolver: Option<Arc<dyn GithubIdentityResolver>>,
    ) -> Self {
        match resolver {
            Some(r) => self.with_github_identity_resolver(r),
            None => self,
        }
    }

    /// Wire the git-backed snapshot store. Pair with
    /// [`Self::with_checkpoint_arc_id`] — both are required before any
    /// snapshot actually fires. Without them, `write`/`edit` behave
    /// exactly as before.
    pub fn with_checkpoint_store(mut self, store: Arc<dyn CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    /// `Option`-flavoured variant.
    pub fn with_checkpoint_store_opt(self, store: Option<Arc<dyn CheckpointStore>>) -> Self {
        match store {
            Some(s) => self.with_checkpoint_store(s),
            None => self,
        }
    }

    /// Tell the registry which arc its snapshots should attach to. The
    /// composition root (athen-app) sets this from the per-arc
    /// registry build.
    pub fn with_checkpoint_arc_id(mut self, arc_id: impl Into<String>) -> Self {
        self.checkpoint_arc_id = Some(arc_id.into());
        self
    }

    /// Snapshot the given paths if the store + arc_id are both wired,
    /// otherwise no-op. Returns the action_id so the caller can stash
    /// it in the tool's output JSON for the auditor to lift onto the
    /// arc-entry metadata. Failures are logged but never block the
    /// tool — a missing snapshot is degraded UX, not a hard error.
    async fn maybe_snapshot(
        &self,
        tool_name: &str,
        args_summary: &str,
        paths: &[PathBuf],
    ) -> Option<String> {
        let store = self.checkpoint_store.as_ref()?;
        let arc_id = self.checkpoint_arc_id.as_deref()?;
        let entry_id = uuid::Uuid::new_v4().to_string();
        match store
            .snapshot_paths(arc_id, &entry_id, None, tool_name, args_summary, paths)
            .await
        {
            Ok(Some(id)) => {
                tracing::info!(
                    arc_id = arc_id,
                    action_id = %id,
                    tool = tool_name,
                    "checkpoint: snapshot recorded"
                );
                Some(id)
            }
            Ok(None) => None, // every path filtered out — nothing to record
            Err(e) => {
                tracing::warn!(
                    arc_id = arc_id,
                    tool = tool_name,
                    error = %e,
                    "checkpoint: snapshot failed (revert won't be available)"
                );
                None
            }
        }
    }

    /// Resolve the GitHub identity env vars for this registry. Empty
    /// Vec when no identity is configured or no resolver is wired.
    /// Called from `do_shell_execute` and `do_shell_spawn`; merging
    /// happens at the call site.
    async fn github_env_overrides(&self) -> Vec<(String, String)> {
        let Some(resolver) = self.github_identity_resolver.as_ref() else {
            return Vec::new();
        };
        if matches!(
            self.github_identity,
            athen_core::agent_profile::GithubIdentity::None
        ) {
            return Vec::new();
        }
        resolver.resolve_env_vars(self.github_identity).await
    }

    /// Inject the toolbox approval gate so `install_package` can ask
    /// the user before running pip/npm. Without a gate the registry
    /// fails closed: install requests return a clear "no gate
    /// configured" error.
    pub fn with_toolbox_approval(mut self, gate: Arc<dyn ToolboxApprovalGate>) -> Self {
        self.toolbox_approval = Some(gate);
        self
    }

    /// Inject an SMTP sender for the `email_send` tool. Without this,
    /// the tool refuses with a "not configured" error so unconfigured
    /// builds (CLI, tests) can't accidentally drop mail.
    pub fn with_email_sender(mut self, sender: Arc<dyn EmailSender>) -> Self {
        self.email_sender = Some(sender);
        self
    }

    /// `Option`-flavoured variant for composition roots that build the
    /// sender conditionally. A `None` keeps the registry unchanged.
    pub fn with_email_sender_opt(self, sender: Option<Arc<dyn EmailSender>>) -> Self {
        match sender {
            Some(s) => self.with_email_sender(s),
            None => self,
        }
    }

    /// Inject the email approval gate so `email_send` can ask the user
    /// before each send. Without a gate the tool fails closed.
    pub fn with_email_approval(mut self, gate: Arc<dyn EmailSendApprovalGate>) -> Self {
        self.email_approval = Some(gate);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_email_sender_opt`].
    pub fn with_email_approval_opt(self, gate: Option<Arc<dyn EmailSendApprovalGate>>) -> Self {
        match gate {
            Some(g) => self.with_email_approval(g),
            None => self,
        }
    }

    /// Inject the owner-destination check so `email_send` can skip the
    /// approval gate when the user is mailing themselves. Without this,
    /// every send goes through the gate — preserving today's behaviour.
    pub fn with_owner_check(mut self, check: Arc<dyn OwnerDestinationCheck>) -> Self {
        self.owner_check = Some(check);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_email_sender_opt`].
    /// Lets the composition root pass `state.owner_destination_check_opt()`
    /// without a manual `if let Some`.
    pub fn with_owner_check_opt(self, check: Option<Arc<dyn OwnerDestinationCheck>>) -> Self {
        match check {
            Some(c) => self.with_owner_check(c),
            None => self,
        }
    }

    /// Inject the Telegram outbound sender for the `send_telegram`
    /// tool. Without it, the tool refuses with a "not configured" error.
    pub fn with_telegram_sender(mut self, sender: Arc<dyn TelegramSender>) -> Self {
        self.telegram_sender = Some(sender);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_telegram_sender`].
    pub fn with_telegram_sender_opt(self, sender: Option<Arc<dyn TelegramSender>>) -> Self {
        match sender {
            Some(s) => self.with_telegram_sender(s),
            None => self,
        }
    }

    /// Inject the Telegram approval gate so non-owner sends ask the
    /// user first. Sends to the configured owner chat bypass the gate.
    pub fn with_telegram_approval(mut self, gate: Arc<dyn TelegramSendApprovalGate>) -> Self {
        self.telegram_approval = Some(gate);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_telegram_approval`].
    pub fn with_telegram_approval_opt(
        self,
        gate: Option<Arc<dyn TelegramSendApprovalGate>>,
    ) -> Self {
        match gate {
            Some(g) => self.with_telegram_approval(g),
            None => self,
        }
    }

    /// Inject a post-send recorder that gets notified after every
    /// successful `send_telegram`. Used by the composition root to stamp
    /// the cross-channel arc-routing hint with the registry's arc id.
    pub fn with_telegram_outbound_recorder(
        mut self,
        recorder: Arc<dyn TelegramOutboundRecorder>,
    ) -> Self {
        self.telegram_outbound_recorder = Some(recorder);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_telegram_outbound_recorder`].
    pub fn with_telegram_outbound_recorder_opt(
        self,
        recorder: Option<Arc<dyn TelegramOutboundRecorder>>,
    ) -> Self {
        match recorder {
            Some(r) => self.with_telegram_outbound_recorder(r),
            None => self,
        }
    }

    /// Replace the default DuckDuckGo search backend (e.g. with Tavily).
    pub fn with_web_search(mut self, provider: Arc<dyn WebSearchProvider>) -> Self {
        self.web_search = provider;
        self
    }

    /// Replace the default local page reader (e.g. with Cloudflare's
    /// Browser Rendering for JS-heavy sites).
    pub fn with_page_reader(mut self, reader: Arc<dyn PageReader>) -> Self {
        self.page_reader = reader;
        self
    }

    /// Inject a provider that supplies additional writable paths for the
    /// shell sandbox, typically derived from the active arc's grants.
    pub fn with_extra_writable(mut self, provider: Arc<dyn ShellExtraWritableProvider>) -> Self {
        self.extra_writable = Some(provider);
        self
    }

    /// Share the map of `shell_spawn`-tracked processes with another owner
    /// (typically `AppState`). Without this, each per-message registry has
    /// its own empty map and the agent can't kill in turn N+1 a server it
    /// spawned in turn N.
    pub fn with_spawned_processes(mut self, spawned: SpawnedProcessMap) -> Self {
        self.spawned = spawned;
        self
    }

    /// Attach a persistence hook fired every time the spawned-process map
    /// mutates (insert / remove / bulk-clear). The app layer wires this to
    /// a JSON pidfile so a crash leaves behind a recoverable record of
    /// orphans for the next startup to reap.
    pub fn with_spawn_persistence_hook(mut self, hook: Arc<dyn SpawnPersistenceHook>) -> Self {
        self.spawn_persistence = Some(hook);
        self
    }

    /// `Option`-flavoured variant — see [`Self::with_email_sender_opt`].
    /// Lets the composition root pass `state.spawn_persistence.clone()`
    /// without a manual `if let Some`.
    pub fn with_spawn_persistence_hook_opt(
        self,
        hook: Option<Arc<dyn SpawnPersistenceHook>>,
    ) -> Self {
        match hook {
            Some(h) => self.with_spawn_persistence_hook(h),
            None => self,
        }
    }

    /// Fire the persistence hook with the current map snapshot. Called
    /// after every `self.spawned` mutation. No-op when no hook is wired.
    async fn fire_spawn_persistence(&self) {
        if let Some(hook) = self.spawn_persistence.as_ref() {
            let snap = snapshot_spawned(&self.spawned).await;
            hook.on_change(snap).await;
        }
    }

    /// Build the JSON Schema for the `shell_execute` tool parameters.
    fn shell_execute_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute. For long-lived servers/daemons \
                                    use shell_spawn instead — bare `&` leaves stdio inherited and \
                                    hangs this call until timeout."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Hard timeout in milliseconds (default 60000, max 600000). \
                                    On timeout the process is killed."
                }
            },
            "required": ["command"]
        })
    }

    /// Build the JSON Schema for the `read` tool parameters.
    fn read_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based starting line (default 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000)"
                }
            },
            "required": ["path"]
        })
    }

    /// Build the JSON Schema for the `edit` tool parameters.
    fn edit_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace (must be unique unless replace_all=true)"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    /// Build the JSON Schema for the `write` tool parameters.
    fn write_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "content": {
                    "type": "string",
                    "description": "Full file contents"
                }
            },
            "required": ["path", "content"]
        })
    }

    /// Build the JSON Schema for the `grep` tool parameters.
    fn grep_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default '.')"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob filter, e.g. '*.rs'"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive match"
                },
                "line_numbers": {
                    "type": "boolean",
                    "description": "Include line numbers in content mode (default true)"
                },
                "context_before": {
                    "type": "integer",
                    "description": "Lines of context before each match"
                },
                "context_after": {
                    "type": "integer",
                    "description": "Lines of context after each match"
                },
                "output_mode": {
                    "type": "string",
                    "description": "'files_with_matches' (default), 'content', or 'count'"
                },
                "max_count": {
                    "type": "integer",
                    "description": "Cap on number of results (default 100)"
                }
            },
            "required": ["pattern"]
        })
    }

    /// Build the JSON Schema for the `list_directory` tool parameters.
    fn list_directory_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the directory to list"
                }
            },
            "required": ["path"]
        })
    }

    fn install_package_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "runtime": {
                    "type": "string",
                    "enum": ["python", "node"],
                    "description": "Package ecosystem: 'python' (pip3 --target) or 'node' (npm --prefix)."
                },
                "package": {
                    "type": "string",
                    "description": "Package spec, e.g. 'fpdf2', 'fpdf2>=2.7', '@scope/foo@1.0'."
                },
                "reason": {
                    "type": "string",
                    "description": "Short justification shown to the user during the approval prompt — explain what the package is for."
                }
            },
            "required": ["runtime", "package", "reason"]
        })
    }

    /// JSON Schema for the `email_send` tool. Required: `to`, `subject`,
    /// `body_text`. Optional: `cc`, `bcc`, `body_html`, `in_reply_to`.
    fn email_send_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Recipient email addresses. Must be non-empty."
                },
                "cc": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional Cc recipients."
                },
                "bcc": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional Bcc recipients."
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject. Shown in the approval prompt — keep it clear and specific."
                },
                "body_text": {
                    "type": "string",
                    "description": "Plain-text body. Always required (HTML alone is bad for accessibility/clients)."
                },
                "body_html": {
                    "type": "string",
                    "description": "Optional HTML body. When set the message is sent multipart/alternative with body_text as the plain fallback."
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Message-ID of the email you're replying to. Sets In-Reply-To/References for proper threading in the recipient's client."
                }
            },
            "required": ["to", "subject", "body_text"]
        })
    }

    /// JSON Schema for the `send_telegram` tool. At least one of
    /// `text` / `attachments` must be present.
    fn send_telegram_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "chat_id": {
                    "type": "integer",
                    "description": "Destination Telegram chat ID. Omit to send to the bot's configured owner chat (the typical case — that's the user you talk to over Telegram). Non-owner chats trigger an approval prompt."
                },
                "text": {
                    "type": "string",
                    "description": "Plain-text message body. Optional only when `attachments` is non-empty. Telegram caps text at 4096 chars; longer messages are auto-split at line/word boundaries."
                },
                "attachments": {
                    "type": "array",
                    "description": "Files to attach. Each entry is an object with: `path` (absolute path to the file, required), `kind` (`photo`|`document`|`auto`, default `auto` — `photo` re-compresses, `document` preserves bytes), and optional `caption` (per-attachment caption, max 1024 chars).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute path to the file to upload."
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["photo", "document", "auto"],
                                "description": "How Telegram should render this file. `photo` for images you're OK losing compression on; `document` for anything that must arrive byte-identical (PDFs, archives, code); `auto` picks by extension."
                            },
                            "caption": {
                                "type": "string",
                                "description": "Optional per-attachment caption (max 1024 chars in the Telegram client)."
                            }
                        },
                        "required": ["path"]
                    }
                },
                "reply_to_message_id": {
                    "type": "integer",
                    "description": "Optional Telegram message_id to thread this message as a reply to. Useful when the user just messaged you and you want the reply to attach to their message."
                }
            }
        })
    }

    fn list_installed_packages_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn uninstall_package_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "runtime": {
                    "type": "string",
                    "enum": ["python", "node"],
                    "description": "Package ecosystem: 'python' or 'node'."
                },
                "package": {
                    "type": "string",
                    "description": "Package name as recorded in the manifest, e.g. 'fpdf2'."
                }
            },
            "required": ["runtime", "package"]
        })
    }

    /// Execute a shell command and return the result.
    ///
    /// When a sandbox is available, the command is executed inside an
    /// OS-native sandbox with a read-only filesystem profile. If sandbox
    /// execution fails, the method falls back to unsandboxed shell
    /// execution so that functionality is never broken.
    async fn do_shell_execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'command' parameter".to_string()))?;

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(60_000)
            .min(600_000);

        // Default-cwd to the agent workspace. Risk evaluation and the
        // sandbox-allowed paths see the original command — the wrapper is
        // only what the bwrap-sandboxed (Linux) branch actually runs. The
        // unsandboxed path uses structured env+cwd via the OS process API
        // ([`build_shell_env`]) so it works on every shell on every OS.
        let wrapped_command = Self::build_shell_wrapper(command);
        let (mut env_overrides, cwd) = Self::build_shell_env();
        // Splice GitHub identity env vars (PAT + author/committer
        // name+email) so git/gh commands authenticate as the configured
        // bot or the user's own account — set on the agent profile,
        // never per-command. No-op when identity is None or no resolver
        // is wired.
        env_overrides.extend(self.github_env_overrides().await);

        tracing::info!(
            tool = "shell_execute",
            command,
            timeout_ms,
            "Executing shell command"
        );

        // Pre-execution risk check: evaluate the ACTUAL command (not user's
        // natural language) through the rule engine. This catches dangerous
        // commands like `rm -rf` regardless of what language the user spoke.
        let risk_ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        if let Some(score) = self.rule_engine.evaluate(command, &risk_ctx) {
            if score.level == RiskLevel::Danger || score.level == RiskLevel::Critical {
                tracing::warn!(
                    tool = "shell_execute",
                    command,
                    risk_score = score.total,
                    risk_level = ?score.level,
                    "Command blocked by risk evaluation"
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "Command blocked by safety system",
                        "reason": format!(
                            "This command was classified as {:?} risk (score: {:.0}). \
                             It cannot be executed without explicit user approval.",
                            score.level, score.total
                        ),
                        "command": command,
                    }),
                    error: Some(format!(
                        "Blocked: {:?} risk command (score {:.0})",
                        score.level, score.total
                    )),
                    execution_time_ms: 0,
                });
            }
        }

        let start = Instant::now();

        // Try sandboxed execution first, fall back to unsandboxed shell.
        // The whole exec is wrapped below in `tokio::time::timeout`; bwrap
        // and NativeShell both set `kill_on_drop`, so dropping the future
        // SIGKILLs the spawned process group.
        let exec_future = async {
            let (stdout, stderr, exit_code) = if let Some(ref sandbox) = self.sandbox {
                // Default writable set: /tmp, the Athen data dir, and cwd.
                // $HOME is intentionally NOT included by default — explicit
                // grants via the GrantStore push entries through `extra_writable`.
                let mut allowed: Vec<PathBuf> = vec![PathBuf::from("/tmp")];
                if let Some(data) = paths::athen_data_dir() {
                    allowed.push(data);
                }
                if let Some(tb) = paths::athen_toolbox_dir() {
                    // Redundant if data_dir is allowed (toolbox is under
                    // it) but explicit here so behavior survives a
                    // future change to data-dir scoping.
                    allowed.push(tb);
                }
                if let Ok(cwd) = std::env::current_dir() {
                    allowed.push(cwd);
                }
                if let Some(provider) = self.extra_writable.as_ref() {
                    for p in provider.extra_writable_paths().await {
                        if !paths::is_system_path(&p) {
                            allowed.push(p);
                        }
                    }
                }
                let level = SandboxLevel::OsNative {
                    profile: SandboxProfile::RestrictedWrite {
                        allowed_paths: allowed,
                    },
                };
                match sandbox
                    .execute_sandboxed("sh", &["-c", &wrapped_command], &level)
                    .await
                {
                    Ok(output) => {
                        // Detect sandbox infrastructure failures (e.g. bwrap can't
                        // create namespaces on restricted CI runners). If stderr
                        // contains sandbox-specific errors, fall back to unsandboxed.
                        let is_sandbox_failure = output.exit_code != 0
                            && (output.stderr.contains("bwrap:")
                                || output.stderr.contains("sandbox-exec:")
                                || output.stderr.contains("creating new namespace"));
                        if is_sandbox_failure {
                            tracing::warn!(
                                tool = "shell_execute",
                                stderr = %output.stderr.trim(),
                                "Sandbox infrastructure failed, falling back to unsandboxed shell"
                            );
                            let output = self
                                .shell
                                .execute_with(
                                    command,
                                    ShellOptions {
                                        env: &env_overrides,
                                        cwd: cwd.as_deref(),
                                    },
                                )
                                .await?;
                            (output.stdout, output.stderr, output.exit_code)
                        } else {
                            tracing::debug!(
                                tool = "shell_execute",
                                "Command executed inside sandbox"
                            );
                            (output.stdout, output.stderr, output.exit_code)
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            tool = "shell_execute",
                            error = %e,
                            "Sandbox execution failed, falling back to unsandboxed shell"
                        );
                        let output = self
                            .shell
                            .execute_with(
                                command,
                                ShellOptions {
                                    env: &env_overrides,
                                    cwd: cwd.as_deref(),
                                },
                            )
                            .await?;
                        (output.stdout, output.stderr, output.exit_code)
                    }
                }
            } else {
                tracing::trace!(
                    tool = "shell_execute",
                    "No sandbox available, executing unsandboxed"
                );
                let output = self
                    .shell
                    .execute_with(
                        command,
                        ShellOptions {
                            env: &env_overrides,
                            cwd: cwd.as_deref(),
                        },
                    )
                    .await?;
                (output.stdout, output.stderr, output.exit_code)
            };
            Ok::<_, AthenError>((stdout, stderr, exit_code))
        };

        let (stdout, stderr, exit_code) =
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), exec_future)
                .await
            {
                Ok(inner) => inner?,
                Err(_) => {
                    tracing::warn!(
                        tool = "shell_execute",
                        command,
                        timeout_ms,
                        "Command timed out — process killed"
                    );
                    let msg = format!(
                        "command timed out after {}ms and was killed. \
                     For long-lived servers/daemons use shell_spawn (returns a PID + log file). \
                     For commands that legitimately need longer, pass `timeout_ms` (max 600000).",
                        timeout_ms
                    );
                    return Ok(ToolResult {
                        success: false,
                        output: json!({ "error": &msg, "timed_out": true }),
                        error: Some(msg),
                        execution_time_ms: start.elapsed().as_millis() as u64,
                    });
                }
            };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let success = exit_code == 0;
        let result_output = json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        });

        Ok(ToolResult {
            success,
            output: result_output,
            error: if success {
                None
            } else {
                Some(format!("exit code {}: {}", exit_code, stderr.trim()))
            },
            execution_time_ms: elapsed_ms,
        })
    }

    /// Canonicalize a path for read-state map keys. Falls back to a
    /// workspace-anchored absolute path when the file doesn't yet exist.
    fn canonical_key(p: &Path) -> PathBuf {
        if let Ok(c) = std::fs::canonicalize(p) {
            return c;
        }
        paths::resolve_in_workspace(p)
    }

    /// Record the blake3 hash of `content` for `path`. Always hashes the
    /// whole file regardless of which slice the caller used.
    pub async fn record_hash(&self, path: &Path, content: &[u8]) {
        let key = Self::canonical_key(path);
        let hash = blake3::hash(content);
        self.read_state.lock().await.insert(key, hash);
    }

    /// Verify that `current` matches the stored hash for `path`. Returns
    /// a clear error when the file was never read or has been modified
    /// externally since the last read.
    pub async fn check_hash(&self, path: &Path, current: &[u8]) -> Result<()> {
        let key = Self::canonical_key(path);
        let map = self.read_state.lock().await;
        match map.get(&key) {
            None => Err(AthenError::Other(format!(
                "must Read this file first before editing or overwriting: {}",
                path.display()
            ))),
            Some(stored) => {
                let now = blake3::hash(current);
                if *stored == now {
                    Ok(())
                } else {
                    Err(AthenError::Other(format!(
                        "file changed on disk since last read: {} — re-read it before editing",
                        path.display()
                    )))
                }
            }
        }
    }

    /// Read a file (or a slice of it) and return numbered lines.
    async fn do_read(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000);
        let offset = offset.max(1) as usize;
        let limit = limit.max(1) as usize;

        // Relative paths resolve against the agent workspace, not the
        // process cwd — that's where the agent's own files live.
        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        tracing::info!(tool = "read", path, offset, limit, "Reading file");
        let start = Instant::now();

        if let Some(reason) = forbidden_data_path(&resolved) {
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": reason, "path": resolved.display().to_string() }),
                error: Some(reason.to_string()),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }
        let p = resolved.as_path();

        // Reject directories early with a clear message.
        match tokio::fs::metadata(p).await {
            Ok(m) if m.is_dir() => {
                let msg = format!("'{path}' is a directory, not a file");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
            Ok(_) => {}
            Err(e) => {
                let msg = format!("cannot read '{path}': {e}");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        let bytes = match tokio::fs::read(p).await {
            Ok(b) => b,
            Err(e) => {
                let msg = e.to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        // Binary-file detection: any NUL byte and we bail. Cheap and
        // matches what most editors and tools do.
        if bytes.contains(&0u8) {
            let msg = format!("'{path}' appears to be a binary file (contains NUL bytes)");
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let content = String::from_utf8_lossy(&bytes).to_string();

        // Slice the requested line range, then number lines `cat -n` style.
        let total_lines = content.lines().count();
        let mut out = String::new();
        let mut returned = 0usize;
        for (i, line) in content.lines().enumerate() {
            let line_no = i + 1;
            if line_no < offset {
                continue;
            }
            if returned >= limit {
                break;
            }
            out.push_str(&format!("{line_no}\t{line}\n"));
            returned += 1;
        }

        // Always hash the *whole* file content so a partial read still
        // protects subsequent edits.
        self.record_hash(p, &bytes).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "content": out,
                "lines_returned": returned,
                "total_lines": total_lines,
                "offset": offset,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Edit a file by exact-string replacement.
    async fn do_edit(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        // Same truncation guard as do_write: if args remained a string
        // after coercion, the JSON tool call was cut off mid-payload.
        if let Some(raw) = args.as_str() {
            return Ok(args_truncated_result(
                "edit",
                raw,
                "shrink the new_string (or chunk replacements across \
                 multiple edit calls) so the JSON tool call fits the \
                 model output budget",
                Instant::now(),
            ));
        }
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'old_string' parameter".to_string()))?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'new_string' parameter".to_string()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Err(AthenError::Other(
                "old_string and new_string are identical (no-op)".to_string(),
            ));
        }

        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        tracing::info!(tool = "edit", path, replace_all, "Editing file");
        let start = Instant::now();
        let p = resolved.as_path();

        let current = tokio::fs::read(p)
            .await
            .map_err(|e| AthenError::Other(format!("cannot read '{path}': {e}")))?;

        // Read-before-write guard. Returns its own clear error message.
        self.check_hash(p, &current).await?;

        let text = String::from_utf8_lossy(&current).to_string();
        let count = text.matches(old_string).count();
        if count == 0 {
            return Err(AthenError::Other(format!(
                "old_string not found in '{path}'"
            )));
        }
        let new_text = if replace_all {
            text.replace(old_string, new_string)
        } else {
            if count > 1 {
                return Err(AthenError::Other(format!(
                    "old_string matches {count} times in '{path}'; \
                     add surrounding context to make it unique, or pass replace_all=true"
                )));
            }
            text.replacen(old_string, new_string, 1)
        };

        // Snapshot pre-mutation state so the action is revertable.
        let snapshot_action_id = self
            .maybe_snapshot(
                "edit",
                &format!("edit {path}"),
                std::slice::from_ref(&resolved),
            )
            .await;

        atomic_write(p, new_text.as_bytes()).await?;
        self.record_hash(p, new_text.as_bytes()).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let mut output = json!({
            "path": path,
            "replacements": if replace_all { count } else { 1 },
            "bytes_written": new_text.len(),
        });
        if let Some(id) = snapshot_action_id {
            output["_snapshot_action_id"] = serde_json::Value::String(id);
        }
        Ok(ToolResult {
            success: true,
            output,
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Write the full contents of a file. New paths skip the read-required
    /// check; existing paths must have been Read first.
    async fn do_write(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        // If args is still a Value::String here, the provider could not
        // parse the JSON tool call — most often because the LLM hit the
        // output token limit mid-content and the args buffer is
        // truncated. Surface that diagnosis so the model retries by
        // chunking instead of looping on the same too-large write.
        if let Some(raw) = args.as_str() {
            return Ok(args_truncated_result(
                "write",
                raw,
                "split the file into ~3-5KB chunks and emit them across \
                 multiple write/edit calls (write the first chunk, then \
                 use edit with replace_all=false to append the rest)",
                Instant::now(),
            ));
        }
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'content' parameter".to_string()))?;

        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        // Make sure the parent dir exists — relative paths often land in a
        // freshly-resolved workspace subdir that doesn't exist yet.
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }

        tracing::info!(tool = "write", path, "Writing file");
        let start = Instant::now();
        let p = resolved.as_path();

        if let Ok(existing) = tokio::fs::read(p).await {
            self.check_hash(p, &existing).await?;
        }
        // (else: new file — allowed without prior read.)

        // Snapshot pre-mutation state. For a brand-new file this records
        // an `absent_paths` entry so revert can delete it.
        let snapshot_action_id = self
            .maybe_snapshot(
                "write",
                &format!("write {path}"),
                std::slice::from_ref(&resolved),
            )
            .await;

        atomic_write(p, content.as_bytes()).await?;
        self.record_hash(p, content.as_bytes()).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let mut output = json!({
            "path": path,
            "bytes_written": content.len(),
        });
        if let Some(id) = snapshot_action_id {
            output["_snapshot_action_id"] = serde_json::Value::String(id);
        }
        Ok(ToolResult {
            success: true,
            output,
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Search files using ripgrep.
    async fn do_grep(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'pattern' parameter".to_string()))?;
        // Resolve the search target against the workspace dir so a bare
        // `grep { pattern: "foo" }` searches the agent's own files, not
        // whatever the host process happened to be launched from.
        let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let resolved_path = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved_path.to_string_lossy().to_string();
        let path = path.as_str();
        let glob = args.get("glob").and_then(|v| v.as_str());
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let line_numbers = args
            .get("line_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let context_before = args.get("context_before").and_then(|v| v.as_u64());
        let context_after = args.get("context_after").and_then(|v| v.as_u64());
        let output_mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let max_count = args
            .get("max_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(100);

        tracing::info!(tool = "grep", pattern, path, output_mode, "ripgrep search");
        let start = Instant::now();

        if let Some(reason) = forbidden_data_path(&resolved_path) {
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": reason, "path": resolved_path.display().to_string() }),
                error: Some(reason.to_string()),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let mut cmd = tokio::process::Command::new("rg");
        hide_console_tokio(&mut cmd);
        cmd.arg("--color=never");
        if case_insensitive {
            cmd.arg("-i");
        }
        match output_mode {
            "files_with_matches" => {
                cmd.arg("-l");
            }
            "count" => {
                cmd.arg("-c");
            }
            "content" => {
                if line_numbers {
                    cmd.arg("-n");
                }
                if let Some(b) = context_before {
                    cmd.arg("-B").arg(b.to_string());
                }
                if let Some(a) = context_after {
                    cmd.arg("-A").arg(a.to_string());
                }
                // Per-file cap on individual matches is friendlier than a
                // hard global cap when scanning many files.
                cmd.arg("-m").arg(max_count.to_string());
            }
            other => {
                return Err(AthenError::Other(format!(
                    "invalid output_mode '{other}': use files_with_matches, content, or count"
                )));
            }
        }
        if let Some(g) = glob {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg("--").arg(pattern).arg(path);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    "ripgrep ('rg') not found on PATH — install it from \
                     https://github.com/BurntSushi/ripgrep#installation"
                        .to_string()
                } else {
                    format!("failed to invoke rg: {e}")
                };
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // rg exits 1 when there are no matches — that's not a failure.
        let exit = output.status.code().unwrap_or(-1);
        let success = exit == 0 || exit == 1;

        // Post-truncate the output to `max_count` lines so non-content
        // modes also respect the cap.
        let truncated: String = stdout
            .lines()
            .take(max_count as usize)
            .collect::<Vec<_>>()
            .join("\n");
        let display = if truncated.trim().is_empty() {
            "No matches".to_string()
        } else {
            truncated
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success,
            output: json!({
                "matches": display,
                "exit_code": exit,
            }),
            error: if success {
                None
            } else {
                Some(format!("rg failed (exit {exit}): {}", stderr.trim()))
            },
            execution_time_ms: elapsed_ms,
        })
    }

    /// Build the JSON Schema for the `memory_store` tool parameters.
    fn memory_store_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to store the value under"
                },
                "value": {
                    "type": "string",
                    "description": "The value to store"
                }
            },
            "required": ["key", "value"]
        })
    }

    /// Build the JSON Schema for the `memory_recall` tool parameters.
    fn memory_recall_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to recall. If omitted, returns all stored keys."
                }
            },
            "required": []
        })
    }

    /// Store a key-value pair in in-session memory.
    async fn do_memory_store(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'key' parameter".to_string()))?;

        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'value' parameter".to_string()))?;

        tracing::info!(tool = "memory_store", key, "Storing value in memory");

        let start = Instant::now();
        self.memory
            .lock()
            .await
            .insert(key.to_string(), value.to_string());
        let elapsed_ms = start.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "stored": key }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Recall a value by key, or list all keys if no key is provided.
    async fn do_memory_recall(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let key = args.get("key").and_then(|v| v.as_str());

        tracing::info!(tool = "memory_recall", ?key, "Recalling from memory");

        let start = Instant::now();
        let memory = self.memory.lock().await;

        let output = match key {
            Some(k) => match memory.get(k) {
                Some(v) => json!({ "key": k, "value": v }),
                None => json!({ "key": k, "found": false }),
            },
            None => {
                let keys: Vec<&String> = memory.keys().collect();
                json!({ "keys": keys })
            }
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output,
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Build the JSON Schema for the `web_search` tool parameters.
    fn web_search_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language search query."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of hits to return (default 5, max 20)."
                }
            },
            "required": ["query"]
        })
    }

    /// Build the JSON Schema for the `web_fetch` tool parameters.
    fn web_fetch_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch and convert to markdown."
                }
            },
            "required": ["url"]
        })
    }

    /// Run a web search via the configured provider.
    async fn do_web_search(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'query' parameter".to_string()))?;
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;

        tracing::info!(
            tool = "web_search",
            provider = self.web_search.name(),
            query,
            max_results,
            "Running web search"
        );

        let start = Instant::now();
        match self.web_search.search(query, max_results).await {
            Ok(results) => {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "title": r.title,
                            "url": r.url,
                            "snippet": r.snippet,
                        })
                    })
                    .collect();
                // `last_used()` returns the underlying provider that
                // actually answered (e.g. "brave"); for plain providers
                // it falls back to `name()`. Surfacing both makes
                // multi-provider chains debuggable from the JSON output.
                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "provider": self.web_search.name(),
                        "answered_by": self.web_search.last_used(),
                        "query": query,
                        "results": json_results,
                    }),
                    error: None,
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": msg,
                        "provider": self.web_search.name(),
                        "answered_by": self.web_search.last_used(),
                    }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }

    /// Fetch a URL and convert it to clean markdown.
    async fn do_web_fetch(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'url' parameter".to_string()))?;

        // Reject obviously bad inputs early so we don't burn a network round-trip.
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            let msg = format!("web_fetch requires an http(s) URL, got: {url}");
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: 0,
            });
        }

        tracing::info!(
            tool = "web_fetch",
            reader = self.page_reader.name(),
            url,
            "Fetching URL"
        );

        let start = Instant::now();
        match self.page_reader.fetch(url).await {
            Ok(result) => Ok(ToolResult {
                success: true,
                output: json!({
                    "url": result.url,
                    "title": result.title,
                    "content": result.content,
                    "source": result.source,
                    "content_chars": result.content.chars().count(),
                }),
                error: None,
                execution_time_ms: start.elapsed().as_millis() as u64,
            }),
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg, "reader": self.page_reader.name() }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }

    /// Build the JSON Schema for the `shell_spawn` tool parameters.
    fn shell_spawn_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to spawn detached. stdout+stderr are captured to a log file. The process survives this call."
                },
                "label": {
                    "type": "string",
                    "description": "Optional human label (e.g. 'http-server') used in the log file name."
                }
            },
            "required": ["command"]
        })
    }

    /// Build the JSON Schema for the `shell_kill` tool parameters.
    fn shell_kill_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "PID returned by shell_spawn"
                },
                "force": {
                    "type": "boolean",
                    "description": "Use SIGKILL immediately instead of SIGTERM (default false)"
                }
            },
            "required": ["pid"]
        })
    }

    /// Build the JSON Schema for the `shell_logs` tool parameters.
    fn shell_logs_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "PID returned by shell_spawn"
                },
                "tail": {
                    "type": "integer",
                    "description": "Number of lines from the end (default 100, max 5000)"
                }
            },
            "required": ["pid"]
        })
    }

    /// Spawn a detached shell command. Returns the PID and a log file path.
    /// The child process outlives this call; use `shell_kill` to stop it
    /// and `shell_logs` to inspect its output.
    ///
    /// NOT routed through the sandbox — bwrap's lifetime is bound to its
    /// child, so sandboxed daemons don't really work. The rule engine still
    /// blocks dangerous commands.
    async fn do_shell_spawn(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'command' parameter".to_string()))?;
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        tracing::info!(
            tool = "shell_spawn",
            command,
            ?label,
            "Spawning detached process"
        );
        let start = Instant::now();

        // Same risk check as shell_execute — a long-lived daemon can do as
        // much damage as a one-shot command.
        let risk_ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        if let Some(score) = self.rule_engine.evaluate(command, &risk_ctx) {
            if score.level == RiskLevel::Danger || score.level == RiskLevel::Critical {
                tracing::warn!(
                    tool = "shell_spawn",
                    command,
                    risk_score = score.total,
                    risk_level = ?score.level,
                    "Spawn blocked by risk evaluation"
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "Command blocked by safety system",
                        "reason": format!(
                            "This command was classified as {:?} risk (score: {:.0}). \
                             It cannot be spawned without explicit user approval.",
                            score.level, score.total
                        ),
                        "command": command,
                    }),
                    error: Some(format!(
                        "Blocked: {:?} risk command (score {:.0})",
                        score.level, score.total
                    )),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        // Pick a log directory under ~/.athen/spawn-logs. Falls back to
        // /tmp/athen-spawn-logs when home isn't resolvable.
        let log_dir = paths::athen_data_dir()
            .map(|p| p.join("spawn-logs"))
            .or_else(|| paths::home_dir().map(|h| h.join(".athen").join("spawn-logs")))
            .unwrap_or_else(|| PathBuf::from("/tmp/athen-spawn-logs"));

        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            let msg = format!("failed to create log dir '{}': {e}", log_dir.display());
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%f").to_string();
        let stem = label.clone().unwrap_or_else(|| "spawn".to_string());
        // Sanitize label for filesystem use.
        let safe_stem: String = stem
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let log_path = log_dir.join(format!("{safe_stem}-{timestamp}.log"));

        // Open the log file for stdout, then open it again (append) for
        // stderr so both streams land in the same file. Two distinct OS
        // file handles are simpler/safer than dup'ing.
        let stdout_file = match std::fs::File::create(&log_path) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("failed to create log file '{}': {e}", log_path.display());
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };
        let stderr_file = match std::fs::OpenOptions::new().append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!(
                    "failed to open log file for stderr '{}': {e}",
                    log_path.display()
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        // Pick the platform shell wrapper. On Unix we route through `sh
        // -c`; on Windows through `cmd /C`. Hardcoding `sh` here used to
        // break Windows entirely (no `sh` on PATH).
        #[cfg(unix)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(command);
            hide_console_tokio(&mut c);
            c
        };

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout_file))
            .stderr(std::process::Stdio::from(stderr_file));

        // Apply the same toolbox env (PYTHONPATH / PATH) and cwd as
        // `shell_execute`, but via the OS process API so the wrapper
        // works on every shell on every OS.
        let (mut env_overrides, cwd) = Self::build_shell_env();
        env_overrides.extend(self.github_env_overrides().await);
        for (k, v) in &env_overrides {
            cmd.env(k, v);
        }
        if let Some(ref ws) = cwd {
            cmd.current_dir(ws);
        }
        // On Unix, put the child in its own process group so signals to us
        // don't propagate, and so `shell_kill` can target the whole tree.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to spawn '{command}': {e}");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let pid = match child.id() {
            Some(p) => p,
            None => {
                let msg = "spawned process has no PID (already exited?)".to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        // Drop the Child handle so the OS keeps the process alive after
        // this function returns. We track it via PID + log path instead.
        std::mem::drop(child);

        let entry = SpawnedProcess {
            pid,
            command: command.to_string(),
            label,
            log_path: log_path.clone(),
            started_at: chrono::Utc::now(),
        };
        self.spawned.lock().await.insert(pid, entry);
        self.fire_spawn_persistence().await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "pid": pid,
                "log_path": log_path.to_string_lossy(),
                "command": command,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Kill a process previously started via `shell_spawn`. Refuses to
    /// touch PIDs that aren't in our map.
    async fn do_shell_kill(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pid_i64 = args
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AthenError::Other("missing 'pid' parameter".to_string()))?;
        if pid_i64 <= 0 || pid_i64 > u32::MAX as i64 {
            return Err(AthenError::Other(format!("invalid pid: {pid_i64}")));
        }
        let pid: u32 = pid_i64 as u32;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        tracing::info!(tool = "shell_kill", pid, force, "Killing spawned process");
        let start = Instant::now();

        // Safety boundary: we only kill PIDs we spawned ourselves.
        {
            let map = self.spawned.lock().await;
            if !map.contains_key(&pid) {
                let msg = "PID not managed by shell_spawn; refusing to kill arbitrary processes"
                    .to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": &msg, "pid": pid }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        #[cfg(unix)]
        let signal_used = {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            // Negative PID = process group, since we set process_group(0).
            let pgid = Pid::from_raw(-(pid as i32));
            let direct = Pid::from_raw(pid as i32);

            let initial_sig = if force {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            // Best-effort: try the group first, then the bare PID as a
            // fallback (group send fails if we're not the group leader).
            let _ = kill(pgid, initial_sig);
            let _ = kill(direct, initial_sig);

            if force {
                "SIGKILL"
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // Probe with signal 0 (no-op) to see if it's still alive.
                let alive = kill(direct, None).is_ok();
                if alive {
                    let _ = kill(pgid, Signal::SIGKILL);
                    let _ = kill(direct, Signal::SIGKILL);
                    "SIGKILL"
                } else {
                    "SIGTERM"
                }
            }
        };

        #[cfg(windows)]
        let signal_used = {
            let mut cmd = tokio::process::Command::new("taskkill");
            hide_console_tokio(&mut cmd);
            cmd.arg("/PID").arg(pid.to_string()).arg("/T");
            if force {
                cmd.arg("/F");
            }
            let _ = cmd.output().await;
            "taskkill"
        };

        self.spawned.lock().await.remove(&pid);
        self.fire_spawn_persistence().await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({ "pid": pid, "signal": signal_used }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Read recent stdout/stderr from a process spawned via `shell_spawn`.
    async fn do_shell_logs(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pid_i64 = args
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AthenError::Other("missing 'pid' parameter".to_string()))?;
        if pid_i64 <= 0 || pid_i64 > u32::MAX as i64 {
            return Err(AthenError::Other(format!("invalid pid: {pid_i64}")));
        }
        let pid: u32 = pid_i64 as u32;
        let tail = args
            .get("tail")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .min(5000) as usize;

        tracing::info!(tool = "shell_logs", pid, tail, "Reading spawn logs");
        let start = Instant::now();

        let entry = {
            let map = self.spawned.lock().await;
            match map.get(&pid) {
                Some(e) => e.clone(),
                None => {
                    let msg = "PID not managed by shell_spawn".to_string();
                    return Ok(ToolResult {
                        success: false,
                        output: json!({ "error": &msg, "pid": pid }),
                        error: Some(msg),
                        execution_time_ms: start.elapsed().as_millis() as u64,
                    });
                }
            }
        };

        let contents = match tokio::fs::read_to_string(&entry.log_path).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!(
                    "failed to read log file '{}': {e}",
                    entry.log_path.display()
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg, "log_path": entry.log_path.to_string_lossy() }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let all_lines: Vec<&str> = contents.lines().collect();
        let total = all_lines.len();
        let take = total.min(tail);
        let logs: String = all_lines[total - take..].join("\n");

        let alive = process_alive(pid);

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "logs": logs,
                "log_path": entry.log_path.to_string_lossy(),
                "alive": alive,
                "lines_returned": take,
                "total_lines": total,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// List entries in a directory.
    async fn do_list_directory(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;

        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        tracing::info!(tool = "list_directory", path, "Listing directory");
        tracing::trace!(
            tool = "list_directory",
            "Filesystem tools use tokio::fs directly (unsandboxed)"
        );

        let start = Instant::now();
        if let Some(reason) = forbidden_data_path(&resolved) {
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": reason, "path": resolved.display().to_string() }),
                error: Some(reason.to_string()),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }
        match tokio::fs::read_dir(&resolved).await {
            Ok(mut reader) => {
                let mut entries = Vec::new();
                while let Ok(Some(entry)) = reader.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let file_type = match entry.file_type().await {
                        Ok(ft) => {
                            if ft.is_dir() {
                                "directory"
                            } else if ft.is_symlink() {
                                "symlink"
                            } else {
                                "file"
                            }
                        }
                        Err(_) => "unknown",
                    };
                    entries.push(json!({
                        "name": name,
                        "type": file_type,
                    }));
                }
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: true,
                    output: json!({ "entries": entries, "count": entries.len() }),
                    error: None,
                    execution_time_ms: elapsed_ms,
                })
            }
            Err(e) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": e.to_string() }),
                    error: Some(e.to_string()),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }

    /// Ask the user (via the configured approval gate) and, on
    /// approval, install a Python or Node package into the persistent
    /// `~/.athen/toolbox/` so subsequent `shell_execute` calls can
    /// import/run it.
    async fn do_install_package(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let runtime_str = args
            .get("runtime")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'runtime' parameter".to_string()))?;
        let package = args
            .get("package")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'package' parameter".to_string()))?;
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'reason' parameter".to_string()))?;

        let runtime = crate::toolbox::Runtime::parse(runtime_str).ok_or_else(|| {
            AthenError::Other(format!(
                "unknown runtime '{runtime_str}' (expected 'python' or 'node')"
            ))
        })?;

        let start = Instant::now();
        let Some(gate) = self.toolbox_approval.clone() else {
            let msg = "install_package: no toolbox approval gate is wired in this context, \
                       so installs cannot be approved. The desktop UI surfaces this gate; \
                       CLI/headless agents need explicit user-side configuration."
                .to_string();
            tracing::warn!("install_package called without approval gate");
            return Ok(ToolResult {
                success: false,
                output: json!({
                    "error": msg,
                    "runtime": runtime.as_str(),
                    "package": package,
                }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        };

        let approved = gate
            .confirm_install(runtime.as_str(), package, reason)
            .await;
        if !approved {
            let msg = format!(
                "install denied by user: {} package '{}' was not installed",
                runtime.as_str(),
                package
            );
            return Ok(ToolResult {
                success: false,
                output: json!({
                    "error": msg,
                    "runtime": runtime.as_str(),
                    "package": package,
                    "reason_offered": reason,
                }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let result = match runtime {
            crate::toolbox::Runtime::Python => {
                crate::toolbox::install_python_package(package, reason).await
            }
            crate::toolbox::Runtime::Node => {
                crate::toolbox::install_node_package(package, reason).await
            }
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(pkg) => Ok(ToolResult {
                success: true,
                output: json!({
                    "runtime": pkg.runtime.as_str(),
                    "package": pkg.package,
                    "version_spec": pkg.version_spec,
                    "installed_version": pkg.installed_version,
                    "reason": pkg.reason,
                    "installed_at": pkg.installed_at,
                    "runtime_version": pkg.runtime_version,
                }),
                error: None,
                execution_time_ms: elapsed_ms,
            }),
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": msg,
                        "runtime": runtime.as_str(),
                        "package": package,
                    }),
                    error: Some(msg),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }

    /// Read the toolbox manifest and return the recorded installs as
    /// JSON so the agent can decide whether a package is already
    /// installed before asking to add it.
    async fn do_list_installed_packages(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let m = crate::toolbox::load_manifest().await;
        let installs: Vec<serde_json::Value> = m
            .installs
            .iter()
            .map(|p| {
                json!({
                    "runtime": p.runtime.as_str(),
                    "package": p.package,
                    "version_spec": p.version_spec,
                    "installed_version": p.installed_version,
                    "reason": p.reason,
                    "installed_at": p.installed_at,
                    "runtime_version": p.runtime_version,
                })
            })
            .collect();
        Ok(ToolResult {
            success: true,
            output: json!({ "installs": installs, "count": m.installs.len() }),
            error: None,
            execution_time_ms: start.elapsed().as_millis() as u64,
        })
    }

    /// Remove a package from the toolbox: drops the on-disk files and
    /// the manifest entry. Not gated by approval — uninstalling is
    /// safe and reversible (just reinstall).
    async fn do_uninstall_package(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let runtime_raw = args
            .get("runtime")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'runtime' parameter".into()))?;
        let package = args
            .get("package")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'package' parameter".into()))?
            .trim();
        if package.is_empty() {
            return Err(AthenError::Other("'package' must be non-empty".into()));
        }
        let runtime = crate::toolbox::Runtime::parse(runtime_raw).ok_or_else(|| {
            AthenError::Other(format!(
                "unknown runtime '{runtime_raw}'; expected 'python' or 'node'"
            ))
        })?;

        let result = match runtime {
            crate::toolbox::Runtime::Python => {
                crate::toolbox::uninstall_python_package(package).await
            }
            crate::toolbox::Runtime::Node => crate::toolbox::uninstall_node_package(package).await,
        };
        let elapsed_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(removed) => Ok(ToolResult {
                success: true,
                output: json!({
                    "runtime": runtime.as_str(),
                    "package": removed.package,
                    "previous_version": removed.installed_version,
                    "removed": true,
                }),
                error: None,
                execution_time_ms: elapsed_ms,
            }),
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": msg,
                        "runtime": runtime.as_str(),
                        "package": package,
                    }),
                    error: Some(msg),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }

    /// Build an outbound email, ask the user (via the configured gate),
    /// and on approval hand it to the SMTP sender. Fails closed when
    /// either the sender or the gate is missing — the agent should
    /// `list_*` instead of mailing into the void.
    async fn do_email_send(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let start = Instant::now();

        let Some(sender) = self.email_sender.clone() else {
            let msg = "SMTP not configured. Configure email settings first.".to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        };
        let Some(gate) = self.email_approval.clone() else {
            let msg = "Email approval gate not wired; refusing to send.".to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        };

        // Recipients: required, non-empty, all strings.
        let to: Vec<String> = match args.get("to").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => {
                let msg = "to must be a non-empty array of recipient addresses".to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };
        if to.is_empty() {
            let msg = "to must be a non-empty array of recipient addresses".to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }
        let cc: Vec<String> = args
            .get("cc")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let bcc: Vec<String> = args
            .get("bcc")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let subject = args
            .get("subject")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'subject' parameter".to_string()))?
            .to_string();
        let body_text = args
            .get("body_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'body_text' parameter".to_string()))?
            .to_string();
        let body_html = args
            .get("body_html")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let in_reply_to = args
            .get("in_reply_to")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // UTF-8 safe slice — `take(200)` walks chars, never bytes.
        let body_preview: String = body_text.chars().take(200).collect();

        let summary = EmailSendSummary {
            to: to.clone(),
            cc: cc.clone(),
            bcc: bcc.clone(),
            subject: subject.clone(),
            body_preview,
            in_reply_to: in_reply_to.clone(),
        };

        tracing::info!(
            tool = "email_send",
            to = ?to,
            subject = %subject,
            "sending email"
        );

        // Owner-self-send bypass: if every destination address (to + cc +
        // bcc) matches one of the owner contact's email identifiers, the
        // approval gate is meaningless ("approve sending mail to
        // yourself?") — skip it. Any non-owner destination falls through
        // to the gate, even if other destinations are owner. The check
        // also requires a non-empty destination set, but `to` is already
        // validated above so this branch is guaranteed to evaluate at
        // least one address.
        let mut auto_approved = false;
        if let Some(check) = self.owner_check.as_ref() {
            let mut all_owner = true;
            'outer: for slot in [&to, &cc, &bcc] {
                for addr in slot.iter() {
                    let lc = addr.trim().to_ascii_lowercase();
                    if lc.is_empty() {
                        continue;
                    }
                    if !check.is_owner_email(&lc).await {
                        all_owner = false;
                        break 'outer;
                    }
                }
            }
            if all_owner {
                tracing::info!(
                    tool = "email_send",
                    "all destinations are owner — auto-approved (bypassing gate)"
                );
                auto_approved = true;
            }
        }

        if !auto_approved && !gate.confirm_send(&summary).await {
            let msg = "User declined the send".to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let email = OutboundEmail {
            to: to.clone(),
            cc,
            bcc,
            subject: subject.clone(),
            body_text,
            body_html,
            in_reply_to,
        };

        match sender.send(&email).await {
            Ok(sent) => {
                tracing::info!(
                    tool = "email_send",
                    to = ?to,
                    subject = %subject,
                    message_id = %sent.message_id,
                    "email sent"
                );
                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "message_id": sent.message_id,
                        "accepted_recipients": sent.accepted_recipients,
                        "to": to,
                        "subject": subject,
                    }),
                    error: None,
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }

    /// Implementation of `send_telegram`. Validates that at least one
    /// of text/attachments is present, asks the approval gate (unless
    /// the destination is the bot's owner chat), and forwards to the
    /// `TelegramSender` adapter which handles Bot API multipart uploads
    /// and text chunking.
    async fn do_send_telegram(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let start = Instant::now();

        let Some(sender) = self.telegram_sender.clone() else {
            let msg =
                "Telegram bot not configured. Set the bot token in Settings → Telegram first."
                    .to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        };

        // Parse args.
        let chat_id_arg = args.get("chat_id").and_then(|v| v.as_i64());
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let reply_to = args.get("reply_to_message_id").and_then(|v| v.as_i64());

        let mut attachments: Vec<TelegramAttachment> = Vec::new();
        if let Some(arr) = args.get("attachments").and_then(|v| v.as_array()) {
            for entry in arr {
                let Some(path) = entry.get("path").and_then(|v| v.as_str()) else {
                    let msg = "each attachment must include `path` (absolute path to the file)"
                        .to_string();
                    return Ok(ToolResult {
                        success: false,
                        output: json!({ "error": msg }),
                        error: Some(msg),
                        execution_time_ms: start.elapsed().as_millis() as u64,
                    });
                };
                let kind = match entry.get("kind").and_then(|v| v.as_str()) {
                    Some("photo") => TelegramAttachmentKind::Photo,
                    Some("document") => TelegramAttachmentKind::Document,
                    None | Some("auto") => TelegramAttachmentKind::Auto,
                    Some(other) => {
                        let msg =
                            format!("attachment kind '{other}' is not one of photo/document/auto");
                        return Ok(ToolResult {
                            success: false,
                            output: json!({ "error": msg }),
                            error: Some(msg),
                            execution_time_ms: start.elapsed().as_millis() as u64,
                        });
                    }
                };
                let caption = entry
                    .get("caption")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                attachments.push(TelegramAttachment {
                    path: PathBuf::from(path),
                    kind,
                    caption,
                });
            }
        }

        if text.is_none() && attachments.is_empty() {
            let msg = "send_telegram needs at least one of `text` or `attachments`".to_string();
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        // Resolve the destination chat for the approval prompt.
        let resolved_chat = match chat_id_arg.or_else(|| sender.default_chat_id()) {
            Some(c) => c,
            None => {
                let msg =
                    "no chat_id given and no owner default configured — set Telegram owner in Settings"
                        .to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };
        let to_owner = sender
            .default_chat_id()
            .map(|d| d == resolved_chat)
            .unwrap_or(false);

        let text_preview: String = text
            .as_deref()
            .map(|t| t.chars().take(200).collect())
            .unwrap_or_default();
        let summary = TelegramSendSummary {
            chat_id: resolved_chat,
            to_owner,
            text_preview,
            attachment_paths: attachments.iter().map(|a| a.path.clone()).collect(),
            attachment_kinds: attachments.iter().map(|a| a.kind).collect(),
        };

        tracing::info!(
            tool = "send_telegram",
            chat_id = resolved_chat,
            to_owner,
            attachments = attachments.len(),
            text_chars = text.as_deref().map(|t| t.chars().count()).unwrap_or(0),
            "sending telegram message"
        );

        // Owner-chat auto-approve: messaging yourself doesn't need a prompt.
        if !to_owner {
            let Some(gate) = self.telegram_approval.clone() else {
                let msg = "Telegram approval gate not wired; refusing to send to non-owner chat"
                    .to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            };
            if !gate.confirm_send(&summary).await {
                let msg = "User declined the send".to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        let outbound = OutboundTelegramMessage {
            chat_id: Some(resolved_chat),
            text,
            attachments,
            reply_to_message_id: reply_to,
        };

        match sender.send(&outbound).await {
            Ok(sent) => {
                // Stamp the cross-channel routing hint AND append to
                // the per-chat transcript so the owner's next Telegram
                // reply lands back in this arc and the agent has
                // recent-context continuity even when arc routing fails.
                if let Some(rec) = self.telegram_outbound_recorder.as_ref() {
                    let outbound_text_for_log = outbound.text.as_deref();
                    rec.record(OutboundTelegramSummary {
                        chat_id: sent.chat_id,
                        text: outbound_text_for_log,
                        attachment_count: outbound.attachments.len(),
                    })
                    .await;
                }
                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "chat_id": sent.chat_id,
                        "message_ids": sent.message_ids,
                        "to_owner": to_owner,
                    }),
                    error: None,
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }
}

#[async_trait]
impl ToolRegistry for ShellToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        Ok(vec![
            ToolDefinition {
                name: "shell_execute".to_string(),
                description: "Run a shell command and return its output (stdout, stderr, exit code). For fetching web pages or searching the web do NOT use curl/wget/lynx — use web_fetch and web_search, which return clean markdown/snippets. For GitHub work, prefer `git` and `gh` here over the REST API: the active agent profile may inject a GitHub identity (PAT + commit author name/email) so commits and PRs land under the configured account automatically.".to_string(),
                parameters: Self::shell_execute_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "read".to_string(),
                description: "Read a file with optional offset/limit. Returns lines numbered cat -n style. ALWAYS use this instead of cat/head/tail.".to_string(),
                parameters: Self::read_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "edit".to_string(),
                description: "Replace exact text in a file. Requires a prior read. Prefer this over sed/awk.".to_string(),
                parameters: Self::edit_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "write".to_string(),
                description: "Overwrite or create a file. For partial changes prefer edit. Existing files require a prior read.".to_string(),
                parameters: Self::write_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "grep".to_string(),
                description: "Search files with ripgrep. Use this instead of grep/find for code search.".to_string(),
                parameters: Self::grep_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "list_directory".to_string(),
                description: "List all files and directories in the given directory path".to_string(),
                parameters: Self::list_directory_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "shell_spawn".to_string(),
                description: "Spawn a long-lived shell command detached (servers, watchers). Returns a PID and a log file path. The process outlives this call. NOT sandboxed; the rule engine still blocks dangerous commands. Use shell_kill to stop it and shell_logs to inspect output.".to_string(),
                parameters: Self::shell_spawn_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "shell_kill".to_string(),
                description: "Kill a process previously started by shell_spawn. Refuses unmanaged PIDs. Sends SIGTERM, then SIGKILL if it doesn't exit (or SIGKILL immediately when force=true).".to_string(),
                parameters: Self::shell_kill_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "shell_logs".to_string(),
                description: "Read the last N lines (default 100, max 5000) of stdout/stderr from a process spawned via shell_spawn. Also reports whether the process is still alive.".to_string(),
                parameters: Self::shell_logs_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "memory_store".to_string(),
                description: "Store a key-value pair in in-session memory for later recall".to_string(),
                parameters: Self::memory_store_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "memory_recall".to_string(),
                description: "Recall a value by key from in-session memory, or list all stored keys if no key is given".to_string(),
                parameters: Self::memory_recall_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search the web and return ranked hits (title, url, snippet). Use this for current/factual info the model may not know, or when web_fetch returns near-empty content on a JS-heavy site — the snippets are often enough to answer.".to_string(),
                parameters: Self::web_search_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "web_fetch".to_string(),
                description: "Fetch a URL and return its readable content as markdown. Use this instead of curl/wget for any web page. Auto-falls-back through a static fetch → JS-rendering reader → web archive, so JS-heavy SPAs and paywalled/blocked pages are usually still readable. The `source` field in the result tells you which tier produced the content (`local-*`, `jina`, or `wayback`).".to_string(),
                parameters: Self::web_fetch_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "email_send".to_string(),
                description: "Send an email via the user's configured SMTP account. The user is asked for explicit approval before each send — the prompt shows recipients, subject, and a body preview, so write a clear subject and a body that makes sense out of context. Required: `to` (array of addresses), `subject`, `body_text`. Optional: `cc`, `bcc`, `body_html`, `in_reply_to` (Message-ID of the email you're replying to — sets In-Reply-To/References for proper threading). Returns the new message-id on success.".to_string(),
                parameters: Self::email_send_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "send_telegram".to_string(),
                description: "Send a Telegram message via the user's configured bot — text and/or file attachments. Omit `chat_id` to message the bot's owner (the typical case — that's the user); non-owner chats trigger an approval prompt. `attachments` is an array of `{path, kind?, caption?}` where `kind` is `photo`|`document`|`auto` (default `auto` — `.png`/`.jpg`/`.webp`/`.gif` go as photo, everything else as document). When a single attachment ships with text ≤ 1024 chars, the text is sent as the file caption (one bubble); otherwise text leads and files follow. Long text auto-splits at the 4096-char limit on line/word boundaries. Common uses: ping the user with a status update, send a generated PDF/screenshot, forward a file from your workspace.".to_string(),
                parameters: Self::send_telegram_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "install_package".to_string(),
                description: "Install a Python package (via pip --target) or Node package (via npm --prefix) into Athen's persistent toolbox at ~/.athen/toolbox. PYTHONPATH and PATH are automatically configured so subsequent shell_execute calls can import/run them. The user will be asked for permission, so write a clear `reason` explaining what the package is for and why you need it.".to_string(),
                parameters: Self::install_package_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "list_installed_packages".to_string(),
                description: "List packages already installed in the toolbox so you don't reinstall them. Returns runtime, name, version, install date, and the reason given when installed.".to_string(),
                parameters: Self::list_installed_packages_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "uninstall_package".to_string(),
                description: "Remove a previously-installed package from the toolbox. Drops the on-disk files and the manifest entry. No approval prompt — uninstalling is reversible (just reinstall).".to_string(),
                parameters: Self::uninstall_package_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
        ])
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        match name {
            "shell_execute" => self.do_shell_execute(&args).await,
            "shell_spawn" => self.do_shell_spawn(&args).await,
            "shell_kill" => self.do_shell_kill(&args).await,
            "shell_logs" => self.do_shell_logs(&args).await,
            "read" => self.do_read(&args).await,
            "edit" => self.do_edit(&args).await,
            "write" => self.do_write(&args).await,
            "grep" => self.do_grep(&args).await,
            "list_directory" => self.do_list_directory(&args).await,
            "memory_store" => self.do_memory_store(&args).await,
            "memory_recall" => self.do_memory_recall(&args).await,
            "web_search" => self.do_web_search(&args).await,
            "web_fetch" => self.do_web_fetch(&args).await,
            "email_send" => self.do_email_send(&args).await,
            "send_telegram" => self.do_send_telegram(&args).await,
            "install_package" => self.do_install_package(&args).await,
            "list_installed_packages" => self.do_list_installed_packages(&args).await,
            "uninstall_package" => self.do_uninstall_package(&args).await,
            _ => Err(AthenError::ToolNotFound(name.to_string())),
        }
    }
}

/// Best-effort liveness check for a PID. On Unix this uses `kill(pid, 0)`
/// which doesn't actually send a signal — it just probes whether the
/// kernel still has a process by that PID we're allowed to signal.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    // Cheap probe via tasklist; sync std::process to keep this helper sync.
    let mut cmd = std::process::Command::new("tasklist");
    hide_console_std(&mut cmd);
    let out = cmd.args(["/FI", &format!("PID eq {pid}"), "/NH"]).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

/// Write `bytes` to `path` atomically: write to a sibling tmp file then
/// rename. Falls back to a direct write when the rename can't happen
/// (e.g. tmp dir on a different filesystem). Used by `edit` and `write`.
async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = match path.file_name() {
        Some(name) => {
            let mut tmp_name = std::ffi::OsString::from(".athen-tmp-");
            tmp_name.push(name);
            tmp_name.push(format!(".{}", std::process::id()));
            path.with_file_name(tmp_name)
        }
        None => {
            return Err(AthenError::Other(format!(
                "invalid file path '{}'",
                path.display()
            )))
        }
    };

    if let Err(e) = tokio::fs::write(&tmp, bytes).await {
        // If the directory doesn't exist, surface that directly.
        return Err(AthenError::Other(format!(
            "failed to write tmp file '{}': {}",
            tmp.display(),
            e
        )));
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        // Best-effort cleanup, then fall back to a direct write so
        // callers on quirky filesystems aren't dead in the water.
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::write(path, bytes).await.map_err(|e2| {
            AthenError::Other(format!(
                "atomic rename failed ({e}); fallback write also failed: {e2}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod coerce_args_tests {
    use super::*;

    #[test]
    fn coerce_args_passes_through_objects_unchanged() {
        let v = serde_json::json!({"path": "/tmp/x"});
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        assert_eq!(coerced["path"], "/tmp/x");
    }

    #[test]
    fn coerce_args_parses_json_strings() {
        let v = serde_json::Value::String(r#"{"path":"/tmp/x","content":"hi"}"#.into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        assert_eq!(coerced["path"], "/tmp/x");
        assert_eq!(coerced["content"], "hi");
    }

    #[test]
    fn coerce_args_returns_original_when_string_is_not_json() {
        let v = serde_json::Value::String("just a string".into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        // Still a string — caller will surface a "missing 'path'" error.
        assert!(coerced.is_string());
    }

    #[test]
    fn do_write_via_coerce_extracts_path_from_stringified_args() {
        // Simulates the bad path: provider couldn't parse args, fell
        // back to Value::String. The coerce helper must still let us
        // pull `path` and `content` out.
        let stringified =
            serde_json::Value::String(r#"{"path":"/tmp/coerce_x.html","content":"hello"}"#.into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&stringified, &mut owned);
        assert_eq!(
            coerced.get("path").and_then(|v| v.as_str()),
            Some("/tmp/coerce_x.html")
        );
        assert_eq!(
            coerced.get("content").and_then(|v| v.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn args_truncated_result_emits_actionable_message() {
        // Truncated raw payload (no closing quote/brace) — exactly what
        // the user hit when DeepSeek ran out of output tokens mid-write.
        let raw = "{\"path\":\"/tmp/x.html\",\"content\":\"<!DOCTYPE html>\n<html>...mid string";
        let result = args_truncated_result("write", raw, "chunk via edit", Instant::now());
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("write"));
        assert!(err.contains("token limit") || err.contains("truncated"));
        assert!(err.contains("chunk via edit"));
        // Output JSON exposes raw_len + head/tail for the LLM to see
        // the cut-off shape and reason about how to retry.
        assert!(result.output.get("raw_len").is_some());
        assert!(result.output.get("raw_head").is_some());
        assert!(result.output.get("raw_tail").is_some());
    }

    #[test]
    fn coerce_repairs_raw_control_chars_in_stringified_args() {
        // The actual production failure mode: DeepSeek emits args as a
        // string with literal newlines inside the content value.
        // serde_json::from_str rejects per spec; coerce_args must
        // re-escape and parse.
        let raw =
            "{\"path\":\"/tmp/coerce_y.html\",\"content\":\"<html>\nhi\n</html>\"}".to_string();
        let stringified = serde_json::Value::String(raw);
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&stringified, &mut owned);
        assert_eq!(
            coerced.get("path").and_then(|v| v.as_str()),
            Some("/tmp/coerce_y.html")
        );
        assert_eq!(
            coerced.get("content").and_then(|v| v.as_str()),
            Some("<html>\nhi\n</html>")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[tokio::test]
    async fn test_list_tools_returns_expected_tools() {
        let registry = ShellToolRegistry::new().await;
        let tools = registry.list_tools().await.unwrap();

        // shell_execute, shell_spawn, shell_kill, shell_logs, read, edit,
        // write, grep, list_directory, memory_store, memory_recall,
        // web_search, web_fetch, email_send, send_telegram,
        // install_package, list_installed_packages, uninstall_package
        assert_eq!(tools.len(), 18);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell_execute"));
        assert!(names.contains(&"shell_spawn"));
        assert!(names.contains(&"shell_kill"));
        assert!(names.contains(&"shell_logs"));
        assert!(names.contains(&"read"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"memory_recall"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"email_send"));
        assert!(names.contains(&"send_telegram"));
        assert!(names.contains(&"install_package"));
        assert!(names.contains(&"list_installed_packages"));
        assert!(names.contains(&"uninstall_package"));

        // Each tool should have a non-empty description and valid parameters schema.
        for tool in &tools {
            assert!(!tool.description.is_empty());
            assert!(tool.parameters.is_object());
            assert!(tool.parameters.get("properties").is_some());
        }
    }

    #[tokio::test]
    async fn test_shell_execute() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("shell_execute", json!({"command": "echo hello"}))
            .await
            .unwrap();

        assert!(result.success);
        let stdout = result.output["stdout"].as_str().unwrap();
        assert!(stdout.trim().contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_execute_failure() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("shell_execute", json!({"command": "exit 42"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.output["exit_code"], 42);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_read_basic_numbers_lines() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "alpha").unwrap();
        writeln!(tmp, "beta").unwrap();
        writeln!(tmp, "gamma").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("1\talpha"));
        assert!(content.contains("2\tbeta"));
        assert!(content.contains("3\tgamma"));
        assert_eq!(result.output["lines_returned"], 3);
        assert_eq!(result.output["total_lines"], 3);
    }

    #[tokio::test]
    async fn test_read_with_offset_and_limit() {
        let mut tmp = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let path = tmp.path().to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path, "offset": 4, "limit": 2}))
            .await
            .unwrap();

        assert!(result.success);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("4\tline4"));
        assert!(content.contains("5\tline5"));
        assert!(!content.contains("3\tline3"));
        assert!(!content.contains("6\tline6"));
        assert_eq!(result.output["lines_returned"], 2);
    }

    #[tokio::test]
    async fn test_read_rejects_directory() {
        let dir = TempDir::new().unwrap();
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("directory"));
    }

    #[tokio::test]
    async fn test_read_rejects_binary_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, [0x48, 0x00, 0x49]).unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("binary"));
    }

    #[tokio::test]
    async fn test_read_not_found() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": "/tmp/__athen_nonexistent_file__"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_edit_rejects_without_prior_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "hello world").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "hello",
                    "new_string": "goodbye",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.to_lowercase().contains("read this file first"));
    }

    #[tokio::test]
    async fn test_edit_rejects_on_external_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "version1").unwrap();

        let registry = ShellToolRegistry::new().await;
        // Read first to record the hash.
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        // External modification.
        std::fs::write(&path, "version2").unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "version2",
                    "new_string": "version3",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("changed on disk"));
    }

    #[tokio::test]
    async fn test_edit_rejects_ambiguous_old_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "foo bar foo bar foo").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("matches 3 times"));
    }

    #[tokio::test]
    async fn test_edit_replace_all() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "foo bar foo bar foo").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                    "replace_all": true,
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["replacements"], 3);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "qux bar qux bar qux");
    }

    #[tokio::test]
    async fn test_edit_unique_replace() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha beta gamma").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "beta",
                    "new_string": "BETA",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "alpha BETA gamma");
    }

    #[tokio::test]
    async fn test_edit_then_edit_uses_updated_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "v1").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "v1",
                    "new_string": "v2",
                }),
            )
            .await
            .unwrap();
        // Second edit without re-reading — should still succeed because
        // the first edit updated the recorded hash.
        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "v2",
                    "new_string": "v3",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_write_new_file_no_read_required() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("brand_new.txt");

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "hi"}),
            )
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hi");
    }

    #[tokio::test]
    async fn test_write_existing_file_requires_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "overwritten"}),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.to_lowercase().contains("read this file first"));
    }

    #[tokio::test]
    async fn test_write_existing_file_after_read_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "overwritten"}),
            )
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "overwritten");
    }

    #[tokio::test]
    async fn test_grep_basic_match() {
        if which::which("rg").is_err() {
            // Skip if ripgrep isn't installed in this environment.
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            "needle in a haystack\nother line\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "grep",
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_str().unwrap(),
                    "output_mode": "files_with_matches",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        let matches = result.output["matches"].as_str().unwrap();
        assert!(matches.contains("a.txt"));
        assert!(!matches.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_grep_no_matches_returns_text() {
        if which::which("rg").is_err() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "grep",
                json!({
                    "pattern": "zzznotthere",
                    "path": dir.path().to_str().unwrap(),
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["matches"].as_str().unwrap(), "No matches");
    }

    #[tokio::test]
    async fn test_list_directory() {
        let dir = TempDir::new().unwrap();
        // Create a couple of files.
        std::fs::write(dir.path().join("alpha.txt"), "a").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "b").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "list_directory",
                json!({"path": dir.path().to_str().unwrap()}),
            )
            .await
            .unwrap();

        assert!(result.success);
        let entries = result.output["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(result.output["count"], 3);

        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));
        assert!(names.contains(&"subdir"));
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let registry = ShellToolRegistry::new().await;
        let result = registry.call_tool("nonexistent_tool", json!({})).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            AthenError::ToolNotFound(name) => assert_eq!(name, "nonexistent_tool"),
            other => panic!("Expected ToolNotFound, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_shell_execute_missing_param() {
        let registry = ShellToolRegistry::new().await;
        let result = registry.call_tool("shell_execute", json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_memory_store_and_recall() {
        let registry = ShellToolRegistry::new().await;

        let store_result = registry
            .call_tool("memory_store", json!({"key": "color", "value": "blue"}))
            .await
            .unwrap();
        assert!(store_result.success);
        assert_eq!(store_result.output["stored"], "color");

        let recall_result = registry
            .call_tool("memory_recall", json!({"key": "color"}))
            .await
            .unwrap();
        assert!(recall_result.success);
        assert_eq!(recall_result.output["key"], "color");
        assert_eq!(recall_result.output["value"], "blue");
    }

    #[tokio::test]
    async fn test_memory_recall_missing_key() {
        let registry = ShellToolRegistry::new().await;

        let result = registry
            .call_tool("memory_recall", json!({"key": "nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["key"], "nonexistent");
        assert_eq!(result.output["found"], false);
    }

    #[tokio::test]
    async fn test_shell_execute_default_timeout_is_60s() {
        // Schema description should advertise the new default.
        let schema = ShellToolRegistry::shell_execute_schema();
        let desc = schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("60000"),
            "schema desc should mention 60000ms default: {desc}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_spawn_returns_pid_and_log_path() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 5", "label": "test-sleep"}),
            )
            .await
            .unwrap();
        assert!(result.success, "spawn failed: {:?}", result.error);
        let pid = result.output["pid"].as_u64().unwrap();
        assert!(pid > 0);
        let log_path = result.output["log_path"].as_str().unwrap();
        assert!(std::path::Path::new(log_path).exists());

        // Cleanup.
        let _ = registry
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_kill_rejects_unmanaged_pid() {
        let registry = ShellToolRegistry::new().await;
        // PID 1 is init — definitely not in our map.
        let result = registry
            .call_tool("shell_kill", json!({"pid": 1}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.to_lowercase().contains("not managed"),
            "expected refusal, got: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_kill_terminates_spawned_process() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 30", "label": "kill-target"}),
            )
            .await
            .unwrap();
        assert!(spawn.success);
        let pid = spawn.output["pid"].as_u64().unwrap() as u32;

        // Confirm it's alive before kill.
        assert!(kill(Pid::from_raw(pid as i32), None).is_ok());

        let kill_result = registry
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await
            .unwrap();
        assert!(kill_result.success, "kill failed: {:?}", kill_result.error);

        // Give the kernel a moment to reap.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            kill(Pid::from_raw(pid as i32), None).is_err(),
            "process {pid} should be gone after SIGKILL"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_logs_returns_content() {
        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sh -c 'echo hi; sleep 1'", "label": "log-test"}),
            )
            .await
            .unwrap();
        assert!(spawn.success);
        let pid = spawn.output["pid"].as_u64().unwrap();

        // Wait for the echo + sleep to finish.
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;

        let logs = registry
            .call_tool("shell_logs", json!({"pid": pid, "tail": 50}))
            .await
            .unwrap();
        assert!(logs.success);
        let log_text = logs.output["logs"].as_str().unwrap();
        assert!(log_text.contains("hi"), "expected 'hi' in logs: {log_text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_logs_reports_alive_false_after_exit() {
        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sh -c 'echo done'", "label": "exit-fast"}),
            )
            .await
            .unwrap();
        let pid = spawn.output["pid"].as_u64().unwrap();

        // Give it time to exit and be reaped by the OS. Spawn dropped the
        // Child handle, so the process becomes a zombie until init reaps it
        // — but `kill(pid, 0)` returns ESRCH once the entry is fully gone.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let logs = registry
            .call_tool("shell_logs", json!({"pid": pid}))
            .await
            .unwrap();
        assert!(logs.success);
        // Note: a freshly-exited child might briefly still appear "alive" as
        // a zombie. Poll a bit before asserting.
        let mut alive = logs.output["alive"].as_bool().unwrap();
        for _ in 0..10 {
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let again = registry
                .call_tool("shell_logs", json!({"pid": pid}))
                .await
                .unwrap();
            alive = again.output["alive"].as_bool().unwrap();
        }
        assert!(!alive, "process should not be alive after exit");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawned_map_is_shareable_across_registries() {
        // Simulates the bug fix: turn 1's registry spawns a process,
        // turn 2's registry — built fresh but with the same shared map —
        // can find it and kill it. Without `with_spawned_processes`, the
        // second registry would have an empty map and refuse the kill.
        let shared: SpawnedProcessMap = Arc::new(Mutex::new(HashMap::new()));

        let reg1 = ShellToolRegistry::new()
            .await
            .with_spawned_processes(shared.clone());
        let reg2 = ShellToolRegistry::new()
            .await
            .with_spawned_processes(shared.clone());

        let spawn = reg1
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 30", "label": "cross-registry"}),
            )
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let pid = spawn.output["pid"].as_u64().unwrap() as u32;

        // The shared map is visible to both.
        assert!(
            shared.lock().await.contains_key(&pid),
            "shared map should contain spawned PID"
        );

        // reg2 (a *different* registry instance) can kill the process
        // spawned by reg1 — proving the map is actually shared, not just
        // that an Arc was stored.
        let kill_result = reg2
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await
            .unwrap();
        assert!(
            kill_result.success,
            "second registry could not kill PID spawned by first: {:?}",
            kill_result.error
        );

        // After kill, both views agree the entry is gone.
        assert!(!shared.lock().await.contains_key(&pid));
    }

    #[tokio::test]
    async fn test_memory_recall_all_keys() {
        let registry = ShellToolRegistry::new().await;

        // Store multiple values.
        registry
            .call_tool("memory_store", json!({"key": "a", "value": "1"}))
            .await
            .unwrap();
        registry
            .call_tool("memory_store", json!({"key": "b", "value": "2"}))
            .await
            .unwrap();

        // Recall without a key to list all keys.
        let result = registry
            .call_tool("memory_recall", json!({}))
            .await
            .unwrap();
        assert!(result.success);

        let keys = result.output["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);

        let key_strs: Vec<&str> = keys.iter().map(|k| k.as_str().unwrap()).collect();
        assert!(key_strs.contains(&"a"));
        assert!(key_strs.contains(&"b"));
    }

    // ── Web tool wiring ────────────────────────────────────────────────────
    //
    // These stubs let us verify dispatch + output envelope shape without any
    // network access. The real backends (DuckDuckGo, HybridReader, ...) have
    // their own coverage in `athen-web`.

    use athen_core::error::{AthenError, Result as CoreResult};
    use athen_web::{PageReader, ReadResult, SearchResult, WebSearchProvider};

    struct StubSearch {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl WebSearchProvider for StubSearch {
        fn name(&self) -> &'static str {
            "stub-search"
        }
        async fn search(&self, _query: &str, max_results: usize) -> CoreResult<Vec<SearchResult>> {
            Ok(self.results.iter().take(max_results).cloned().collect())
        }
    }

    struct StubReader {
        outcome: std::result::Result<ReadResult, String>,
    }

    #[async_trait]
    impl PageReader for StubReader {
        fn name(&self) -> &'static str {
            "stub-reader"
        }
        async fn fetch(&self, _url: &str) -> CoreResult<ReadResult> {
            self.outcome.clone().map_err(AthenError::Other)
        }
    }

    #[tokio::test]
    async fn web_search_dispatches_to_provider_and_caps_results() {
        let stub = StubSearch {
            results: vec![
                SearchResult {
                    title: "First".into(),
                    url: "https://a.example/".into(),
                    snippet: "snippet a".into(),
                },
                SearchResult {
                    title: "Second".into(),
                    url: "https://b.example/".into(),
                    snippet: "snippet b".into(),
                },
                SearchResult {
                    title: "Third".into(),
                    url: "https://c.example/".into(),
                    snippet: "snippet c".into(),
                },
            ],
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_web_search(Arc::new(stub));

        let result = registry
            .call_tool("web_search", json!({ "query": "rust", "max_results": 2 }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["provider"], "stub-search");
        let arr = result.output["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "max_results must be honored");
        assert_eq!(arr[0]["title"], "First");
        assert_eq!(arr[0]["url"], "https://a.example/");
        assert_eq!(arr[0]["snippet"], "snippet a");
    }

    #[tokio::test]
    async fn web_search_clamps_max_results_to_safe_range() {
        let stub = StubSearch {
            results: Vec::new(),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_web_search(Arc::new(stub));

        // 0 must be raised to >=1, 999 must be clamped to <=20. The stub
        // returns empty either way; success is what we're checking — a
        // panic from `clamp` on an out-of-range max would surface here.
        let r = registry
            .call_tool("web_search", json!({ "query": "x", "max_results": 0 }))
            .await
            .unwrap();
        assert!(r.success);

        let r = registry
            .call_tool("web_search", json!({ "query": "x", "max_results": 999 }))
            .await
            .unwrap();
        assert!(r.success);
    }

    #[tokio::test]
    async fn web_fetch_dispatches_to_reader_and_emits_full_envelope() {
        let stub = StubReader {
            outcome: Ok(ReadResult {
                url: "https://example.com/".into(),
                title: Some("Hello".into()),
                content: "# Hello\n\nbody text".into(),
                source: "stub-reader".into(),
            }),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "https://example.com/" }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["url"], "https://example.com/");
        assert_eq!(result.output["title"], "Hello");
        assert_eq!(result.output["source"], "stub-reader");
        assert_eq!(result.output["content"], "# Hello\n\nbody text");
        assert_eq!(result.output["content_chars"], 18);
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_url_without_calling_reader() {
        // Reader would error if invoked — proves the URL guard short-circuits
        // before any network round-trip.
        let stub = StubReader {
            outcome: Err("reader must not be called".into()),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "ftp://nope/" }))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("http(s)"),
            "expected scheme guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn web_fetch_surfaces_reader_error_as_soft_failure() {
        let stub = StubReader {
            outcome: Err("simulated reader failure".into()),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "https://example.com/" }))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("simulated reader failure"));
        // The reader name is reported so the agent can see which tier failed.
        assert_eq!(result.output["reader"], "stub-reader");
    }

    /// Stub gate that records the call and returns a configured answer.
    /// Used to assert install_package's approval-deny path doesn't
    /// invoke pip/npm and that approve forwards args correctly.
    struct StubApprovalGate {
        answer: bool,
        calls: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    #[async_trait]
    impl ToolboxApprovalGate for StubApprovalGate {
        async fn confirm_install(&self, runtime: &str, package: &str, reason: &str) -> bool {
            self.calls.lock().await.push((
                runtime.to_string(),
                package.to_string(),
                reason.to_string(),
            ));
            self.answer
        }
    }

    #[tokio::test]
    async fn install_package_without_gate_fails_closed() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "install_package",
                json!({
                    "runtime": "python",
                    "package": "fpdf2",
                    "reason": "PDF generation",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success, "install must fail closed without a gate");
        let err = result.error.unwrap_or_default();
        assert!(
            err.to_lowercase().contains("approval gate"),
            "error should mention the missing gate, got: {err}"
        );
    }

    #[tokio::test]
    async fn install_package_denied_does_not_invoke_pip() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(StubApprovalGate {
            answer: false,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_toolbox_approval(gate as Arc<dyn ToolboxApprovalGate>);

        let result = registry
            .call_tool(
                "install_package",
                json!({
                    "runtime": "python",
                    "package": "definitely-not-a-real-pkg-xyz123",
                    "reason": "denial-path test",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.to_lowercase().contains("denied"), "got: {err}");
        // The gate was consulted exactly once with the right args. We
        // never reached pip — verified indirectly by the fast,
        // pip-free response (the pkg name is bogus; pip would have
        // produced a network/resolver error instead).
        let recorded = calls.lock().await.clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "python");
        assert_eq!(recorded[0].1, "definitely-not-a-real-pkg-xyz123");
        assert_eq!(recorded[0].2, "denial-path test");
    }

    #[tokio::test]
    async fn list_installed_packages_returns_empty_envelope_with_no_manifest() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("list_installed_packages", json!({}))
            .await
            .unwrap();
        assert!(result.success);
        // count is always present even when the manifest doesn't exist
        // — agent code can rely on the field shape.
        assert!(result.output.get("installs").is_some());
        assert!(result.output.get("count").is_some());
    }

    #[test]
    fn build_shell_wrapper_includes_pythonpath_when_dir_available() {
        let wrapped = ShellToolRegistry::build_shell_wrapper("echo hi");
        // Either the toolbox dir is available (PYTHONPATH export
        // appears) or no home is set and the bare command is
        // returned. Both are valid; we just want to make sure the
        // wrapped form, when present, ends with the user's command in
        // a subshell.
        if wrapped != "echo hi" {
            assert!(wrapped.ends_with("( echo hi )"), "got: {wrapped}");
        }
    }

    /// Mock SMTP sender. Records every `send` call so the tests can
    /// assert the wire-level `OutboundEmail` round-trips correctly,
    /// and lets each test toggle success vs. failure via `should_fail`.
    struct MockEmailSender {
        sent: Arc<Mutex<Vec<OutboundEmail>>>,
        should_fail: bool,
        message_id: String,
    }

    #[async_trait]
    impl EmailSender for MockEmailSender {
        async fn send(
            &self,
            email: &OutboundEmail,
        ) -> athen_core::error::Result<athen_core::traits::email_sender::SentEmail> {
            self.sent.lock().await.push(email.clone());
            if self.should_fail {
                Err(athen_core::error::AthenError::Other(
                    "smtp transport boom".into(),
                ))
            } else {
                Ok(athen_core::traits::email_sender::SentEmail {
                    message_id: self.message_id.clone(),
                    accepted_recipients: email.to.clone(),
                })
            }
        }

        async fn test_connection(&self) -> athen_core::error::Result<()> {
            Ok(())
        }

        fn name(&self) -> &'static str {
            "mock-smtp"
        }
    }

    /// Mock approval gate. Records every consultation so tests can
    /// assert the gate was (or wasn't) called.
    struct MockEmailApprovalGate {
        answer: bool,
        calls: Arc<Mutex<Vec<EmailSendSummary>>>,
    }

    #[async_trait]
    impl EmailSendApprovalGate for MockEmailApprovalGate {
        async fn confirm_send(&self, summary: &EmailSendSummary) -> bool {
            self.calls.lock().await.push(summary.clone());
            self.answer
        }
    }

    fn well_formed_email_args() -> serde_json::Value {
        json!({
            "to": ["alice@example.com"],
            "subject": "Hi",
            "body_text": "hello there",
        })
    }

    #[tokio::test]
    async fn email_send_without_sender_fails_closed() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);
        let result = registry
            .call_tool("email_send", well_formed_email_args())
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.to_lowercase().contains("not configured"), "got: {err}");
        // Gate must NOT be consulted when the transport is missing.
        assert!(calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn email_send_without_gate_fails_closed() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<test@id>".into(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>);
        let result = registry
            .call_tool("email_send", well_formed_email_args())
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.to_lowercase().contains("gate"), "got: {err}");
        assert!(sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn email_send_empty_to_fails_without_calls() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<id@x>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);

        let args = json!({
            "to": [],
            "subject": "Hi",
            "body_text": "hello",
        });
        let result = registry.call_tool("email_send", args).await.unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("non-empty"), "got: {err}");
        assert!(calls.lock().await.is_empty());
        assert!(sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn email_send_gate_denies_skips_transport() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<id@x>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: false,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);

        let result = registry
            .call_tool("email_send", well_formed_email_args())
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.to_lowercase().contains("declined"), "got: {err}");
        assert_eq!(calls.lock().await.len(), 1);
        assert!(sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn email_send_gate_approves_calls_sender_with_full_payload() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<msg@athen>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);

        let args = json!({
            "to": ["alice@example.com", "bob@example.com"],
            "cc": ["carol@example.com"],
            "bcc": ["dave@example.com"],
            "subject": "Project update",
            "body_text": "Plain body",
            "body_html": "<p>HTML body</p>",
            "in_reply_to": "<prev@thread>",
        });
        let result = registry.call_tool("email_send", args).await.unwrap();
        assert!(result.success, "expected success, got {result:?}");
        assert_eq!(result.output["message_id"], "<msg@athen>");
        let accepted = result.output["accepted_recipients"].as_array().unwrap();
        assert_eq!(accepted.len(), 2);

        // Sender called exactly once with the full payload round-tripped.
        let sent = sent.lock().await.clone();
        assert_eq!(sent.len(), 1);
        let e = &sent[0];
        assert_eq!(e.to, vec!["alice@example.com", "bob@example.com"]);
        assert_eq!(e.cc, vec!["carol@example.com"]);
        assert_eq!(e.bcc, vec!["dave@example.com"]);
        assert_eq!(e.subject, "Project update");
        assert_eq!(e.body_text, "Plain body");
        assert_eq!(e.body_html.as_deref(), Some("<p>HTML body</p>"));
        assert_eq!(e.in_reply_to.as_deref(), Some("<prev@thread>"));

        // Gate consulted once with a body preview matching body_text.
        let calls = calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].subject, "Project update");
        assert_eq!(calls[0].body_preview, "Plain body");
    }

    #[tokio::test]
    async fn email_send_surfaces_transport_error() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: true,
            message_id: "<unused@x>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);

        let result = registry
            .call_tool("email_send", well_formed_email_args())
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("smtp transport boom"), "got: {err}");
        // Sender was reached (so the gate approved), but the transport
        // error propagated as a structured failure rather than a panic.
        assert_eq!(sent.lock().await.len(), 1);
    }

    // ---- Owner self-send bypass ----

    /// Mock owner-destination check. Returns `true` for any address in
    /// `owner_emails` (compared lowercased), `false` otherwise.
    struct MockOwnerCheck {
        owner_emails: Vec<String>,
    }

    #[async_trait]
    impl OwnerDestinationCheck for MockOwnerCheck {
        async fn is_owner_email(&self, email: &str) -> bool {
            let lc = email.to_ascii_lowercase();
            self.owner_emails.iter().any(|o| o == &lc)
        }
    }

    #[tokio::test]
    async fn email_send_auto_approves_when_all_destinations_are_owner() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<self@athen>".into(),
        });
        // Gate would deny if asked — so observing a successful send
        // proves the gate was bypassed.
        let gate = Arc::new(MockEmailApprovalGate {
            answer: false,
            calls: calls.clone(),
        });
        let owner = Arc::new(MockOwnerCheck {
            owner_emails: vec!["me@example.com".into(), "alias@example.com".into()],
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>)
            .with_owner_check(owner as Arc<dyn OwnerDestinationCheck>);

        let args = json!({
            "to": ["ME@example.com"], // case-insensitive
            "cc": ["alias@example.com"],
            "subject": "note to self",
            "body_text": "remember the milk",
        });
        let result = registry.call_tool("email_send", args).await.unwrap();
        assert!(result.success, "expected success, got {result:?}");
        // Gate must NOT have been consulted.
        assert!(
            calls.lock().await.is_empty(),
            "approval gate was consulted on owner self-send"
        );
        // Send did happen.
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn email_send_routes_through_gate_when_any_destination_is_non_owner() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<mixed@athen>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        let owner = Arc::new(MockOwnerCheck {
            owner_emails: vec!["me@example.com".into()],
        });
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>)
            .with_owner_check(owner as Arc<dyn OwnerDestinationCheck>);

        // Owner in to:, stranger in bcc:. Mixed destinations must go
        // through the gate — BCC'ing an external while CC'ing the
        // owner is exactly the case the spec rejects.
        let args = json!({
            "to": ["me@example.com"],
            "bcc": ["stranger@example.com"],
            "subject": "leaky",
            "body_text": "...",
        });
        let result = registry.call_tool("email_send", args).await.unwrap();
        assert!(result.success);
        // Gate WAS consulted.
        assert_eq!(calls.lock().await.len(), 1);
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn email_send_routes_through_gate_when_owner_not_configured() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(MockEmailSender {
            sent: sent.clone(),
            should_fail: false,
            message_id: "<no-owner@athen>".into(),
        });
        let gate = Arc::new(MockEmailApprovalGate {
            answer: true,
            calls: calls.clone(),
        });
        // No owner check wired at all — should be identical to today's
        // behaviour: every send hits the gate.
        let registry = ShellToolRegistry::new()
            .await
            .with_email_sender(sender as Arc<dyn EmailSender>)
            .with_email_approval(gate as Arc<dyn EmailSendApprovalGate>);

        let result = registry
            .call_tool("email_send", well_formed_email_args())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(calls.lock().await.len(), 1);
        assert_eq!(sent.lock().await.len(), 1);
    }

    // ---- Layer 1: forbidden_data_path policy ----

    #[test]
    fn forbidden_data_path_blocks_config_toml() {
        let data = Path::new("/home/u/.athen");
        let r = forbidden_data_path_in(&data.join("config.toml"), data);
        assert_eq!(
            r,
            Some("config.toml contains credentials and is not readable from tools")
        );
    }

    #[test]
    fn forbidden_data_path_blocks_athen_db_and_wal() {
        let data = Path::new("/home/u/.athen");
        for name in [
            "athen.db",
            "athen.db-wal",
            "athen.db-shm",
            "athen.db-journal",
        ] {
            let r = forbidden_data_path_in(&data.join(name), data);
            assert_eq!(
                r,
                Some("athen.db is not readable from tools"),
                "expected block for {name}"
            );
        }
    }

    #[test]
    fn forbidden_data_path_blocks_runtimes_tree() {
        let data = Path::new("/home/u/.athen");
        assert_eq!(
            forbidden_data_path_in(&data.join("runtimes"), data),
            Some("~/.athen/runtimes is not readable from tools")
        );
        assert_eq!(
            forbidden_data_path_in(&data.join("runtimes/python/bin/python3"), data),
            Some("~/.athen/runtimes is not readable from tools")
        );
    }

    #[test]
    fn forbidden_data_path_allows_tools_subdir() {
        let data = Path::new("/home/u/.athen");
        assert_eq!(forbidden_data_path_in(&data.join("tools"), data), None);
        assert_eq!(
            forbidden_data_path_in(&data.join("tools/foo.json"), data),
            None
        );
    }

    #[test]
    fn forbidden_data_path_allows_toolbox_manifest() {
        let data = Path::new("/home/u/.athen");
        assert_eq!(
            forbidden_data_path_in(&data.join("toolbox/manifest.json"), data),
            None
        );
        assert_eq!(
            forbidden_data_path_in(&data.join("toolbox/python/foo"), data),
            None
        );
    }

    #[test]
    fn forbidden_data_path_allows_workspace_files() {
        let data = Path::new("/home/u/.athen");
        assert_eq!(
            forbidden_data_path_in(&data.join("workspace/notes.md"), data),
            None
        );
        assert_eq!(
            forbidden_data_path_in(&data.join("files/x.txt"), data),
            None
        );
        // Paths entirely outside the data dir are unaffected.
        assert_eq!(
            forbidden_data_path_in(Path::new("/tmp/something.toml"), data),
            None
        );
    }
}
