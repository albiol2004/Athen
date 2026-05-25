//! Built-in self-help documentation served by the `athen_docs` agent tool.
//!
//! Content is embedded at compile time via `include_str!` from
//! `skills/system/<slug>.md`. The tool exposes two actions:
//!
//! - `list` — returns all available guide topics with descriptions.
//! - `get`  — returns the full markdown body of a specific guide.
//!
//! This is Layer 2 of the self-support system (see
//! `docs/SELF_SUPPORT_SKILLS.md`). The agent discovers these guides through
//! a builtin `athen-docs` skill listed in its prefix, then calls this tool
//! to browse and read specific topics.

use athen_core::error::{AthenError, Result};

struct DocEntry {
    slug: &'static str,
    title: &'static str,
    description: &'static str,
    content: &'static str,
}

const DOCS: &[DocEntry] = &[
    DocEntry {
        slug: "setup-calendar-source",
        title: "Connect a Calendar Source",
        description: "Walk the user through connecting iCloud, Google, Fastmail, Yandex, or Nextcloud calendar via CalDAV.",
        content: include_str!("../../../skills/system/setup-calendar-source.md"),
    },
    DocEntry {
        slug: "setup-email",
        title: "Connect Email (IMAP/SMTP)",
        description: "Walk the user through connecting their email account with autodetect, app-specific passwords, and manual override.",
        content: include_str!("../../../skills/system/setup-email.md"),
    },
    DocEntry {
        slug: "setup-mcp-server",
        title: "Add an MCP Server",
        description: "Explain what MCP servers are and walk through adding one in Settings — stdio or SSE transport, per-tool risk overrides.",
        content: include_str!("../../../skills/system/setup-mcp-server.md"),
    },
    DocEntry {
        slug: "setup-cloud-api-endpoint",
        title: "Register a Cloud API Endpoint",
        description: "Walk through picking from the 15 presets or adding a custom HTTP API endpoint the agent can call.",
        content: include_str!("../../../skills/system/setup-cloud-api-endpoint.md"),
    },
    DocEntry {
        slug: "setup-github-identity",
        title: "Connect GitHub Identity",
        description: "Explain Bot vs User identity modes, creating a PAT, and connecting it so the agent can commit and push.",
        content: include_str!("../../../skills/system/setup-github-identity.md"),
    },
    DocEntry {
        slug: "setup-skill",
        title: "Create a Skill",
        description: "Explain what skills are and walk through creating a reusable playbook in Settings → Skills.",
        content: include_str!("../../../skills/system/setup-skill.md"),
    },
    DocEntry {
        slug: "setup-wakeup",
        title: "Set Up Scheduled Tasks (Wake-ups)",
        description: "Explain wake-ups (scheduled/recurring tasks), autonomy bands, recurrence patterns, and how to create them.",
        content: include_str!("../../../skills/system/setup-wakeup.md"),
    },
    DocEntry {
        slug: "pick-local-model",
        title: "Choose a Local Model",
        description: "Hardware requirements table, installing Ollama or llama.cpp, model recommendations by RAM/VRAM, family selection.",
        content: include_str!("../../../skills/system/pick-local-model.md"),
    },
    DocEntry {
        slug: "understand-risk-system",
        title: "How the Risk System Works",
        description: "Explain the four risk bands (Auto/NotifyAndProceed/HumanConfirm/HardBlock), base impact levels, and contact trust.",
        content: include_str!("../../../skills/system/understand-risk-system.md"),
    },
    DocEntry {
        slug: "understand-profiles",
        title: "Agent Profiles",
        description: "Explain what profiles are, how to create specialized agents, primary tool groups, and profile routing.",
        content: include_str!("../../../skills/system/understand-profiles.md"),
    },
    DocEntry {
        slug: "troubleshoot-no-llm-response",
        title: "Troubleshoot: No LLM Response",
        description: "Diagnostic checklist when Athen isn't responding — key validation, network, model slug, family, quota.",
        content: include_str!("../../../skills/system/troubleshoot-no-llm-response.md"),
    },
];

pub fn do_athen_docs(action: &str, topic: Option<&str>) -> Result<String> {
    match action {
        "list" => {
            let mut out = String::with_capacity(DOCS.len() * 120);
            out.push_str("Available Athen guides:\n\n");
            for (i, doc) in DOCS.iter().enumerate() {
                out.push_str(&format!(
                    "{}. **{}** (`{}`)\n   {}\n\n",
                    i + 1,
                    doc.title,
                    doc.slug,
                    doc.description,
                ));
            }
            out.push_str("Call `athen_docs` with action \"get\" and topic set to the slug (e.g. \"setup-email\") to read the full guide.");
            Ok(out)
        }
        "get" => {
            let slug = topic.ok_or_else(|| {
                AthenError::Other(
                    "athen_docs: 'topic' is required when action is 'get'. Call with action 'list' to see available topics.".into(),
                )
            })?;
            let entry = DOCS.iter().find(|d| d.slug == slug).ok_or_else(|| {
                let available: Vec<&str> = DOCS.iter().map(|d| d.slug).collect();
                AthenError::Other(format!(
                    "athen_docs: unknown topic '{}'. Available: {}",
                    slug,
                    available.join(", ")
                ))
            })?;
            Ok(entry.content.to_string())
        }
        other => Err(AthenError::Other(format!(
            "athen_docs: unknown action '{}'. Use 'list' or 'get'.",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_all_docs() {
        let result = do_athen_docs("list", None).unwrap();
        assert!(result.contains("setup-email"));
        assert!(result.contains("pick-local-model"));
        assert!(result.contains("troubleshoot-no-llm-response"));
        for doc in DOCS {
            assert!(result.contains(doc.slug), "missing slug: {}", doc.slug);
        }
    }

    #[test]
    fn get_returns_content() {
        let result = do_athen_docs("get", Some("setup-email")).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn get_unknown_topic_errors() {
        let err = do_athen_docs("get", Some("nonexistent")).unwrap_err();
        assert!(err.to_string().contains("unknown topic"));
        assert!(err.to_string().contains("setup-email"));
    }

    #[test]
    fn get_without_topic_errors() {
        let err = do_athen_docs("get", None).unwrap_err();
        assert!(err.to_string().contains("topic"));
    }

    #[test]
    fn unknown_action_errors() {
        let err = do_athen_docs("delete", None).unwrap_err();
        assert!(err.to_string().contains("unknown action"));
    }
}
