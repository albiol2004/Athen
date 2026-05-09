//! Tool registry wrapper that enforces a wake-up's tool + contact
//! allowlists at fire time.
//!
//! Design — `docs/WAKEUPS.md` §"Risk model":
//!
//! - `tool_allowlist`: when `Some(list)`, every other tool is hidden from
//!   `list_tools` and rejected from `call_tool`. The agent cannot call
//!   what it cannot see — and even if a model tries, the gate rejects.
//! - `contact_allowlist`: when `Some(list)`, outbound tools must target
//!   only contacts whose identifiers are in the allowlist. The check
//!   runs in `call_tool` against the resolved contact identifiers
//!   (today: email recipients).
//! - `AutonomyBand::NotifyOnly`: outbound tools (currently `email_send`)
//!   are stripped from the surface unconditionally — the wake-up can
//!   read and summarize but cannot act on the world.
//! - `AutonomyBand::Auto` and `SafeOnly`: no extra surface stripping;
//!   per-action risk gate continues to run normally and the LLM is
//!   reminded of its band via the system suffix directive.
//!
//! The wrapper sits *outermost* — after the file gate, app tools, MCP,
//! and delegation registries are already composed. Composition order
//! matters: stripping happens last so a tool that would otherwise be
//! exposed by any of those layers can still be hidden from the agent.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use athen_contacts::ContactStore;
use athen_core::contact::{ContactId, IdentifierKind};
use athen_core::error::{AthenError, Result};
use athen_core::tool::{ToolDefinition, ToolResult};
use athen_core::traits::tool::ToolRegistry;
use athen_core::wakeup::AutonomyBand;

/// Outbound tool names — stripped under `NotifyOnly` and gated by the
/// contact allowlist. Keep tight: only tools that actually contact a
/// person belong here. `shell_execute` is *not* outbound; the file/email
/// gate already protects it.
const OUTBOUND_TOOL_NAMES: &[&str] = &["email_send"];

pub struct WakeupRestrictedRegistry {
    inner: Box<dyn ToolRegistry>,
    tool_allowlist: Option<HashSet<String>>,
    /// Resolved identifiers of allowlisted contacts (email addresses,
    /// phone numbers, Telegram usernames). Pre-resolved at construction
    /// so `call_tool` doesn't hit the contact store on every call.
    /// Lower-cased for case-insensitive comparison. `Some(empty)` means
    /// "configured but every entry failed to resolve" → fail closed.
    allowed_identifiers: Option<HashSet<String>>,
    autonomy: AutonomyBand,
}

impl WakeupRestrictedRegistry {
    /// Build a wrapper. `contact_store` is consulted up-front to expand
    /// `contact_allowlist` (UUIDs) into the concrete identifier strings
    /// the outbound tools actually receive.
    pub async fn new(
        inner: Box<dyn ToolRegistry>,
        tool_allowlist: Option<Vec<String>>,
        contact_allowlist: Option<Vec<ContactId>>,
        autonomy: AutonomyBand,
        contact_store: Option<Arc<dyn ContactStore>>,
    ) -> Self {
        let tool_allowlist = tool_allowlist
            .filter(|v| !v.is_empty())
            .map(|v| v.into_iter().collect::<HashSet<_>>());

        let allowed_identifiers = match (&contact_allowlist, &contact_store) {
            (Some(ids), Some(store)) if !ids.is_empty() => {
                let mut out = HashSet::new();
                for id in ids {
                    match store.load(*id).await {
                        Ok(Some(c)) => {
                            for ident in &c.identifiers {
                                out.insert(ident.value.trim().to_ascii_lowercase());
                            }
                        }
                        _ => {
                            tracing::warn!(
                                contact = %id,
                                "Wake-up contact_allowlist references unknown contact; entry skipped"
                            );
                        }
                    }
                }
                Some(out)
            }
            _ => None,
        };

        Self {
            inner,
            tool_allowlist,
            allowed_identifiers,
            autonomy,
        }
    }

    fn tool_allowed(&self, name: &str) -> bool {
        if matches!(self.autonomy, AutonomyBand::NotifyOnly) && OUTBOUND_TOOL_NAMES.contains(&name)
        {
            return false;
        }
        match &self.tool_allowlist {
            Some(set) => set.contains(name),
            None => true,
        }
    }

    /// Check whether outbound recipients in `args` are all in the
    /// resolved identifier allowlist. Returns `Ok(())` if the allowlist
    /// isn't set or if every recipient is permitted; `Err(...)` with a
    /// user-readable reason otherwise.
    fn check_recipients(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        let Some(allowed) = self.allowed_identifiers.as_ref() else {
            return Ok(());
        };
        // Empty resolved set means "configured but every contact failed
        // to resolve" — treat as deny-all so we fail closed.
        if allowed.is_empty() {
            return Err(format!(
                "Wake-up contact allowlist is configured but no allowed identifiers \
                 resolved; refusing outbound `{name}` call. Edit the wake-up's contact \
                 allowlist or remove unknown contacts."
            ));
        }
        let recipients = collect_recipients(name, args);
        let bad: Vec<_> = recipients
            .iter()
            .filter(|r| !allowed.contains(r.trim().to_ascii_lowercase().as_str()))
            .cloned()
            .collect();
        if bad.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Wake-up contact allowlist blocks `{name}` to: {}. Allowed identifiers: \
                 {}.",
                bad.join(", "),
                allowed.iter().cloned().collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

/// Pull recipient strings out of `args` for a given outbound tool. New
/// outbound tools that ship later add their own arm here.
fn collect_recipients(name: &str, args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if name == "email_send" {
        for key in ["to", "cc", "bcc"] {
            if let Some(arr) = args.get(key).and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    out
}

#[async_trait]
impl ToolRegistry for WakeupRestrictedRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let inner = self.inner.list_tools().await?;
        Ok(inner
            .into_iter()
            .filter(|t| self.tool_allowed(&t.name))
            .collect())
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        if !self.tool_allowed(name) {
            return Err(AthenError::Other(format!(
                "Wake-up restriction: tool `{name}` is not in this wake-up's \
                 allowlist (or is blocked under autonomy=notify_only)."
            )));
        }
        if OUTBOUND_TOOL_NAMES.contains(&name) {
            if let Err(reason) = self.check_recipients(name, &args) {
                return Err(AthenError::Other(reason));
            }
        }
        // `_` indicates `OUTBOUND_TOOL_NAMES` is identifier kind agnostic
        // for now (only email). When Telegram-send lands, add an arm
        // that resolves chat ids against `IdentifierKind::Telegram`
        // entries — `IdentifierKind` is imported above for that reason.
        let _ = IdentifierKind::Email;
        self.inner.call_tool(name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use athen_core::risk::BaseImpact;
    use athen_core::tool::ToolBackend;

    #[derive(Default)]
    struct FakeRegistry {
        calls: tokio::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl ToolRegistry for FakeRegistry {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(vec![
                ToolDefinition {
                    name: "read".into(),
                    description: "Read a file".into(),
                    parameters: json!({}),
                    backend: ToolBackend::Shell {
                        command: String::new(),
                        native: false,
                    },
                    base_risk: BaseImpact::Read,
                },
                ToolDefinition {
                    name: "email_send".into(),
                    description: "Send mail".into(),
                    parameters: json!({}),
                    backend: ToolBackend::Shell {
                        command: String::new(),
                        native: false,
                    },
                    base_risk: BaseImpact::WritePersist,
                },
            ])
        }

        async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
            self.calls.lock().await.push((name.into(), args.clone()));
            Ok(ToolResult {
                success: true,
                output: serde_json::json!({"ok": true}),
                error: None,
                execution_time_ms: 0,
            })
        }
    }

    #[tokio::test]
    async fn allowlist_filters_list_and_blocks_call() {
        let inner: Box<dyn ToolRegistry> = Box::new(FakeRegistry::default());
        let r = WakeupRestrictedRegistry::new(
            inner,
            Some(vec!["read".into()]),
            None,
            AutonomyBand::SafeOnly,
            None,
        )
        .await;
        let listed = r.list_tools().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "read");

        let err = r
            .call_tool("email_send", json!({"to": ["x@example.com"]}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not in this wake-up's"));
    }

    #[tokio::test]
    async fn notify_only_strips_outbound_tools() {
        let inner: Box<dyn ToolRegistry> = Box::new(FakeRegistry::default());
        let r =
            WakeupRestrictedRegistry::new(inner, None, None, AutonomyBand::NotifyOnly, None).await;
        let listed = r.list_tools().await.unwrap();
        assert!(listed.iter().any(|t| t.name == "read"));
        assert!(!listed.iter().any(|t| t.name == "email_send"));

        let err = r
            .call_tool("email_send", json!({"to": ["x@example.com"]}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("notify_only"));
    }

    #[tokio::test]
    async fn no_allowlists_passes_through() {
        let inner: Box<dyn ToolRegistry> = Box::new(FakeRegistry::default());
        let r = WakeupRestrictedRegistry::new(inner, None, None, AutonomyBand::Auto, None).await;
        let listed = r.list_tools().await.unwrap();
        assert_eq!(listed.len(), 2);
        let res = r
            .call_tool("email_send", json!({"to": ["x@example.com"]}))
            .await
            .unwrap();
        assert!(res.success);
    }
}
