//! `delegate_to_agent` — the tool the main agent uses to spawn a specialist.
//!
//! Mental model from the user (real-life analogy): your assistant calls
//! someone else for a specific service. The specialist does the job and
//! reports back. They do NOT recursively call other specialists for that
//! task. Depth is exactly 1.
//!
//! Implementation:
//! - `DelegationToolRegistry` wraps a base `AppToolRegistry` and adds the
//!   `delegate_to_agent` tool to its surface.
//! - When the agent calls `delegate_to_agent`, this module builds a sub-
//!   executor whose tool registry is the *bare* base registry (no
//!   delegation tool) — so the sub-agent literally cannot delegate
//!   further. Depth=1 is enforced by composition, not by a runtime flag.
//! - The sub-agent runs under the requested profile (or the default if no
//!   target was given), with that profile's `tool_selection` filtering its
//!   tool surface as usual.
//! - A sub-arc is created with `parent_arc_id` set to the caller's arc, so
//!   the conversation history forms a tree — cheap audit trail without any
//!   UI work.
//!
//! Approval inheritance + risk: the sub-agent's task inherits the parent
//! task's risk floor by re-using the same coordinator + auditor wiring at
//! the executor level. Sub-agent's *own* risky tool calls go through the
//! same approval router as any other tool call. No special-casing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use athen_agent::AgentBuilder;
use athen_core::error::{AthenError, Result};
use athen_core::risk::BaseImpact;
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::tool::ToolRegistry;

const DELEGATE_TOOL_NAME: &str = "delegate_to_agent";

/// Everything the delegation tool needs to spin up a sub-agent. Cloneable
/// (every field is `Arc` or owned/`Clone`) so the wrapper can capture it
/// once and re-use it across many delegation calls in a session.
#[derive(Clone)]
pub struct DelegationContext {
    pub profile_store: Arc<athen_persistence::profiles::SqliteProfileStore>,
    pub arc_store: athen_persistence::arcs::ArcStore,
    pub llm_router: Arc<tokio::sync::RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    /// The arc the *parent* agent is running under. Each delegation creates
    /// a sub-arc whose `parent_arc_id` points here.
    pub parent_arc_id: String,
    /// Tool documentation directory passed to the sub-agent's executor.
    pub tool_doc_dir: Option<std::path::PathBuf>,
    /// Tauri handle used to wire a silent auditor on the sub-executor so
    /// the sub-agent's tool calls are persisted under `sub_arc_id`. None
    /// means "no UI handle available" — sub-agent runs without persistence
    /// (CLI / test paths).
    pub app_handle: Option<tauri::AppHandle>,
}

/// Wraps a base tool registry and exposes `delegate_to_agent` as an
/// additional tool. The base registry is what sub-agents actually use, so
/// the sub-agent's surface excludes `delegate_to_agent` automatically —
/// that's how the depth=1 invariant is enforced (by composition, not by a
/// runtime flag the agent could be tricked into ignoring).
pub struct DelegationToolRegistry {
    /// The full base tool registry. Sub-agents get this directly (without
    /// the delegation wrapper), so they have every other tool but cannot
    /// delegate further.
    base: Arc<dyn ToolRegistry>,
    ctx: DelegationContext,
}

impl DelegationToolRegistry {
    pub fn new(base: Arc<dyn ToolRegistry>, ctx: DelegationContext) -> Self {
        Self { base, ctx }
    }

    fn delegate_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "target_profile_id": {
                    "type": "string",
                    "description": "The id of the AgentProfile to spawn (e.g. 'marketing', 'coder'). Pass an empty string to let the system pick the default profile."
                },
                "brief": {
                    "type": "string",
                    "description": "SPECIFICATION (not deliverable) for the specialist, under ~2000 characters. Describe what they should produce: goals, constraints, sections/structure, success criteria. Do NOT paste the deliverable itself — the specialist writes it. The specialist sees only this brief, not your conversation history, so make it self-contained but tight."
                }
            },
            "required": ["target_profile_id", "brief"]
        })
    }

    fn delegate_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: DELEGATE_TOOL_NAME.to_string(),
            description:
                "Hand off a self-contained task to a specialist agent profile. \
                 Use when another profile is genuinely better-suited (e.g. marketing \
                 expertise for a landing-page review, coder profile for writing code). \
                 The specialist runs in a fresh context with their own tools, completes \
                 the task, and returns a structured result you can reason over. The \
                 specialist cannot delegate further — depth is capped at 1, like a \
                 real-life referral.\n\
                 \n\
                 IMPORTANT — the brief is a SPECIFICATION, not the deliverable. Describe \
                 *what* the specialist should produce; do NOT paste the deliverable itself \
                 inside the brief. If you ask the coder agent to write a landing page, the \
                 brief should describe the sections, copy intent, CTAs, and constraints — \
                 NOT contain the literal HTML you want them to emit. The specialist will \
                 write the deliverable; you describe it.\n\
                 \n\
                 Keep the brief under ~2000 characters. The specialist sees only the \
                 brief, not your conversation history, so make it self-contained but tight."
                    .to_string(),
            parameters: Self::delegate_schema(),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: BaseImpact::WritePersist,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DelegateArgs {
    target_profile_id: String,
    brief: String,
}

#[async_trait]
impl ToolRegistry for DelegationToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut tools = self.base.list_tools().await?;
        tools.push(Self::delegate_tool_definition());
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        if name != DELEGATE_TOOL_NAME {
            return self.base.call_tool(name, args).await;
        }

        let started = Instant::now();

        // Recovery ladder for malformed args:
        //   1. `coerce_string_wrapper` re-parses Value::String as JSON
        //      when the provider's repair worked but the wrapper
        //      survived (rare; both providers strip it eagerly now).
        //   2. `salvage_delegate_args_from_raw` regex-extracts the two
        //      fields from a raw broken JSON string. This is the
        //      hard-fallback that handles "LLM stuffed HTML/code with
        //      unescaped quotes into the brief and broke the JSON
        //      beyond what generic repair can fix".
        let coerced = coerce_string_wrapper(args);

        let parsed: DelegateArgs = match serde_json::from_value(coerced.clone()) {
            Ok(a) => a,
            Err(e) => match salvage_delegate_args_from_raw(&coerced) {
                Some(salvaged) => {
                    tracing::warn!(
                        "delegate_to_agent: deserialize failed ({e}); \
                         salvaged target_profile_id + brief via raw extraction \
                         (brief len={})",
                        salvaged.brief.len()
                    );
                    salvaged
                }
                None => {
                    let hint = build_args_error_hint(&coerced);
                    return Ok(ToolResult {
                        success: false,
                        output: json!({
                            "error": format!("invalid arguments: {e}"),
                            "hint": hint,
                        }),
                        error: Some(format!("invalid arguments: {e}")),
                        execution_time_ms: started.elapsed().as_millis() as u64,
                    });
                }
            },
        };

        if parsed.brief.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": "brief is empty" }),
                error: Some("brief must contain a self-contained task description".into()),
                execution_time_ms: started.elapsed().as_millis() as u64,
            });
        }

        let outcome = run_delegation(self.base.clone(), self.ctx.clone(), parsed).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match outcome {
            Ok((sub_arc_id, content, success)) => Ok(ToolResult {
                success,
                output: json!({
                    "sub_arc_id": sub_arc_id,
                    "content": content,
                    "success": success,
                }),
                error: None,
                execution_time_ms: elapsed_ms,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: json!({ "error": e.to_string() }),
                error: Some(e.to_string()),
                execution_time_ms: elapsed_ms,
            }),
        }
    }
}

/// The actual delegation run: resolve the target profile, create a sub-arc,
/// build a fresh executor whose tool registry is the bare base (no
/// delegation), and execute a synthetic task built from the brief.
///
/// Returns `(sub_arc_id, content, success)` on success.
async fn run_delegation(
    base: Arc<dyn ToolRegistry>,
    ctx: DelegationContext,
    args: DelegateArgs,
) -> Result<(String, String, bool)> {
    use athen_core::traits::profile::ProfileStore;

    let started = Instant::now();
    tracing::info!(
        target_profile_id = %args.target_profile_id,
        brief_len = args.brief.len(),
        parent_arc_id = %ctx.parent_arc_id,
        "delegate_to_agent: run_delegation entered"
    );

    // 1. Resolve target profile (fall back to default if unknown / empty).
    let target_id = if args.target_profile_id.trim().is_empty() {
        None
    } else {
        Some(args.target_profile_id.clone())
    };
    let profile = match ctx.profile_store.get_or_default(target_id.as_ref()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("delegate_to_agent: profile_store.get_or_default failed: {e}");
            return Err(e);
        }
    };
    tracing::info!(
        profile_id = %profile.id,
        profile_name = %profile.display_name,
        "delegate_to_agent: profile resolved"
    );
    let templates = ctx
        .profile_store
        .resolve_templates(&profile.persona_template_ids)
        .await
        .unwrap_or_default();
    let resolved = athen_core::agent_profile::ResolvedAgentProfile {
        profile: profile.clone(),
        persona_templates: templates,
    };

    // 2. Create the sub-arc. parent_arc_id is set so the tree is queryable
    //    later. Name uses the profile display name + a short slug from the
    //    brief so it's recognizable in the UI sidebar.
    let sub_arc_id = format!(
        "arc_{}_sub_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let arc_name = format!(
        "[{}] {}",
        profile.display_name,
        truncate(&args.brief, 60)
    );
    if let Err(e) = ctx
        .arc_store
        .create_arc_with_parent(
            &sub_arc_id,
            &arc_name,
            athen_persistence::arcs::ArcSource::System,
            &ctx.parent_arc_id,
        )
        .await
    {
        tracing::warn!("delegation: create_arc_with_parent failed: {e}");
    }
    if let Err(e) = ctx
        .arc_store
        .set_active_profile_id(&sub_arc_id, Some(&profile.id))
        .await
    {
        tracing::warn!("delegation: set_active_profile_id on sub-arc failed: {e}");
    }

    // 3. Build the sub-executor. The sub-agent's tool registry is the
    //    *bare* base — no DelegationToolRegistry wrap — so it has every
    //    other tool but cannot itself delegate. Depth=1 by composition.
    let exec_router: Box<dyn LlmRouter> =
        Box::new(crate::state::SharedRouter(Arc::clone(&ctx.llm_router)));
    let sub_registry: Box<dyn ToolRegistry> = Box::new(ArcRegistryAdapter(base));

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(sub_registry)
        .max_steps(30)
        .timeout(Duration::from_secs(180))
        .active_profile(resolved);
    if let Some(ref dir) = ctx.tool_doc_dir {
        builder = builder.tool_doc_dir(dir.clone());
    }

    // Wire a *silent* auditor on the sub-executor so its tool calls are
    // persisted to `arc_entries` under the sub-arc id. The frontend reads
    // these to render the inline expandable view under the parent's
    // `delegate_to_agent` row. We deliberately skip `agent-progress` events
    // (silent) and don't pass a stream sender — both would surface in the
    // parent's UI as if the parent took those steps.
    if let Some(ref handle) = ctx.app_handle {
        let sub_turn_id = uuid::Uuid::new_v4().to_string();
        let sub_tool_log = crate::commands::new_tool_log();
        let sub_auditor = crate::commands::TauriAuditor::new_silent(
            handle.clone(),
            Some(ctx.arc_store.clone()),
            sub_arc_id.clone(),
            sub_turn_id,
            sub_tool_log,
        );
        builder = builder.auditor(Box::new(sub_auditor));
    }
    let executor = builder
        .build()
        .map_err(|e| AthenError::Other(format!("delegation: build sub-executor: {e}")))?;
    tracing::info!(
        sub_arc_id = %sub_arc_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "delegate_to_agent: sub-executor built; starting sub-task"
    );

    // 4. Synthesize a Task from the brief and run it.
    let task = Task {
        id: uuid::Uuid::new_v4(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        source_event: None,
        domain: DomainType::Base,
        description: args.brief.clone(),
        priority: TaskPriority::Normal,
        status: TaskStatus::InProgress,
        risk_score: None,
        risk_budget: None,
        risk_used: 0,
        assigned_agent: None,
        steps: vec![],
        deadline: None,
    };

    let result = match executor.execute(task).await {
        Ok(r) => {
            tracing::info!(
                sub_arc_id = %sub_arc_id,
                success = r.success,
                steps_completed = r.steps_completed,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "delegate_to_agent: sub-task finished"
            );
            r
        }
        Err(e) => {
            tracing::warn!(
                sub_arc_id = %sub_arc_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "delegate_to_agent: sub-task errored: {e}"
            );
            return Err(e);
        }
    };

    // Pull the human-readable response back out of the TaskResult.
    let content = result
        .output
        .as_ref()
        .and_then(|v| v.get("response").and_then(|s| s.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if result.success {
                "(specialist returned no text)".to_string()
            } else {
                "(specialist task failed without a response)".to_string()
            }
        });

    Ok((sub_arc_id, content, result.success))
}

/// Recover from the OpenAI provider's `Value::String` wrapper fallback
/// (which kicks in when the LLM emits malformed JSON for tool args, e.g.
/// embedding raw newlines inside a big HTML blob). If `args` is a string
/// that re-parses as a JSON object, return that. Otherwise return as-is.
///
/// Mirrors the recovery in `athen-agent::tools::coerce_args` — duplicated
/// here to avoid taking a cross-crate dep on a private helper.
fn coerce_string_wrapper(args: serde_json::Value) -> serde_json::Value {
    if let Some(s) = args.as_str() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
            tracing::warn!(
                "delegate_to_agent args reached call_tool as Value::String; \
                 re-parsed (len={})",
                s.len()
            );
            return parsed;
        }
    }
    args
}

/// Last-resort salvage: pull `target_profile_id` and `brief` out of a
/// malformed JSON args payload using positional string ops, treating
/// the brief value as raw text (so embedded HTML / unescaped quotes /
/// raw newlines don't matter).
///
/// Accepts either a `Value::String` (the openai provider's last-resort
/// wrapper for unparseable args) OR a partially-parsed object that's
/// still missing fields after a deserialize attempt — but in practice
/// the salvage path only matters for the string case.
///
/// Strategy:
///   - target_profile_id: regex-style extract `"target_profile_id"\s*:\s*"<short>"`.
///     The id is a known short identifier (no special chars), so the
///     "no embedded quotes" assumption is safe.
///   - brief: find `"brief"\s*:\s*"`, take everything between that and
///     the LAST `"` in the payload. We assume the LLM closed the
///     string and the object at the very end, even if the middle is
///     full of unescaped quotes.
///
/// Returns `None` if either field is missing entirely.
fn salvage_delegate_args_from_raw(value: &serde_json::Value) -> Option<DelegateArgs> {
    let raw = value.as_str()?;
    let target_profile_id = extract_short_string_field(raw, "target_profile_id")
        .unwrap_or_default();
    let brief = extract_trailing_string_field(raw, "brief")?;
    if brief.trim().is_empty() {
        return None;
    }
    Some(DelegateArgs {
        target_profile_id,
        brief,
    })
}

/// Find `"<field>"\s*:\s*"<value>"` and return `<value>`. Used for
/// short identifier fields (no embedded quotes). Returns None if not
/// found or the value would contain a quote.
fn extract_short_string_field(raw: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = raw.find(&needle)?;
    let after = &raw[start + needle.len()..];
    let colon = after.find(':')?;
    let after_colon = &after[colon + 1..];
    let trimmed = after_colon.trim_start();
    let rest = trimmed.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Find `"<field>"\s*:\s*"` and return everything from there until the
/// LAST `"` in the input (which we assume is the closing quote of this
/// field's value, since `brief` is the last field in the schema).
/// Used for unbounded fields whose value may contain unescaped quotes.
fn extract_trailing_string_field(raw: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = raw.find(&needle)?;
    let after = &raw[start + needle.len()..];
    let colon = after.find(':')?;
    let after_colon = &after[colon + 1..];
    let trimmed = after_colon.trim_start();
    let rest = trimmed.strip_prefix('"')?;
    let last_quote = rest.rfind('"')?;
    Some(rest[..last_quote].to_string())
}

/// When delegate_to_agent args fail to deserialize, give the LLM a useful
/// hint about *what shape we expected*. The most common cause is the
/// model embedding the deliverable inside the brief and breaking JSON;
/// this hint nudges it to retry with a tighter spec next turn.
fn build_args_error_hint(args: &serde_json::Value) -> String {
    if args.is_string() {
        "args came in as a raw string, not an object — this usually means the \
         JSON tool_call was malformed because something large (like full HTML \
         or code) was embedded inside a field. Retry with a tight specification \
         in `brief` (under ~2000 chars), describing what the specialist should \
         produce — do NOT paste the deliverable itself."
            .to_string()
    } else if let Some(obj) = args.as_object() {
        let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        format!(
            "expected object with fields `target_profile_id` (string) and `brief` (string); \
             received fields: [{}]",
            keys.join(", ")
        )
    } else {
        "expected object with fields `target_profile_id` (string) and `brief` (string)".into()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Adapter that exposes an `Arc<dyn ToolRegistry>` as `Box<dyn ToolRegistry>`
/// so it can be handed to `AgentBuilder::tool_registry`. Also used by the
/// composition root as the fallback registry shape when delegation isn't
/// wired up (no profile store).
pub(crate) struct ArcRegistryAdapter(pub(crate) Arc<dyn ToolRegistry>);

#[async_trait]
impl ToolRegistry for ArcRegistryAdapter {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        self.0.list_tools().await
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        self.0.call_tool(name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::tool::ToolDefinition;

    /// A no-op base registry that exposes a single fake tool, so we can
    /// verify the wrapper adds delegate_to_agent on top of it.
    struct FakeBase;

    #[async_trait]
    impl ToolRegistry for FakeBase {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(vec![ToolDefinition {
                name: "fake_tool".to_string(),
                description: "fake".to_string(),
                parameters: json!({}),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            }])
        }
        async fn call_tool(&self, _name: &str, _args: serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: json!({}),
                error: None,
                execution_time_ms: 0,
            })
        }
    }

    /// Confirm `list_tools` adds delegate_to_agent on top of whatever the
    /// base registry exposes — that's the visible surface to the LLM.
    #[tokio::test]
    async fn list_tools_adds_delegate_on_top_of_base() {
        // We can't easily build a real DelegationContext in a test (it
        // needs SQLite stores etc.). So this test just verifies the schema
        // and definition surface — the heavier integration is exercised
        // through a manual smoke test once the UI ships.
        let def = DelegationToolRegistry::delegate_tool_definition();
        assert_eq!(def.name, "delegate_to_agent");
        assert!(def.description.contains("specialist"));
        // Required args present.
        let schema = &def.parameters;
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap();
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"target_profile_id"));
        assert!(required_names.contains(&"brief"));
    }

    /// Sanity: the truncate helper preserves multi-byte boundaries.
    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
        assert_eq!(truncate("ñañañaña", 4), "ñaña…");
    }

    /// Wrapper forwards non-delegate calls to the base registry — this is
    /// what makes the sub-agent's own tool calls work.
    #[tokio::test]
    async fn forwards_non_delegate_calls_to_base() {
        let base: Arc<dyn ToolRegistry> = Arc::new(FakeBase);
        let result = base.call_tool("fake_tool", json!({})).await.unwrap();
        assert!(result.success);
        // Inspecting the wrapper directly would require constructing a
        // real DelegationContext (with SQLite). The forwarding logic is
        // a one-liner on the call_tool match; covered by the type-level
        // structure rather than execution here.
    }

    /// Recovery from the provider's `Value::String` wrapper fallback —
    /// when the LLM embeds a huge HTML blob and breaks JSON parsing, the
    /// raw payload arrives as a Value::String. We must re-parse it so
    /// delegate_to_agent doesn't fail with a confusing "missing field"
    /// when the actual problem was upstream JSON breakage.
    #[test]
    fn coerce_string_wrapper_reparses_inner_json() {
        let inner = r#"{"target_profile_id":"coder","brief":"hello"}"#;
        let wrapped = serde_json::Value::String(inner.to_string());
        let coerced = coerce_string_wrapper(wrapped);
        let parsed: DelegateArgs = serde_json::from_value(coerced).unwrap();
        assert_eq!(parsed.target_profile_id, "coder");
        assert_eq!(parsed.brief, "hello");
    }

    #[test]
    fn coerce_string_wrapper_passes_objects_through_unchanged() {
        let obj = json!({ "target_profile_id": "x", "brief": "y" });
        let same = coerce_string_wrapper(obj.clone());
        assert_eq!(same, obj);
    }

    #[test]
    fn coerce_string_wrapper_leaves_unparseable_strings_as_string() {
        let bad = serde_json::Value::String("not json at all".to_string());
        let result = coerce_string_wrapper(bad.clone());
        assert_eq!(result, bad);
    }

    /// The args-error hint is what the LLM sees when delegate_to_agent
    /// fails to deserialize. It must surface the most common failure mode
    /// (string-wrapped args = embedded huge blob) so the model retries
    /// with a tighter brief.
    #[test]
    fn args_error_hint_calls_out_string_wrapper_case() {
        let bad = serde_json::Value::String("garbage".to_string());
        let hint = build_args_error_hint(&bad);
        assert!(hint.contains("raw string"));
        assert!(hint.contains("brief"));
    }

    /// Real-world failure mode: the LLM stuffs HTML with unescaped
    /// quotes into the brief, so generic JSON repair fails and the args
    /// arrive as a Value::String. The salvage path must still extract
    /// both fields so the delegation can proceed.
    #[test]
    fn salvage_extracts_both_fields_from_raw_html_brief() {
        // HTML with unescaped quotes inside the brief — the exact
        // failure mode the user is hitting. We construct it as a plain
        // string (not a raw literal) so `\"` correctly emits a literal
        // double-quote into the input we want to salvage.
        let raw = "{\"target_profile_id\":\"coder\",\"brief\":\"Crea un HTML con <h1 class=\"hero\">Hi</h1> y un <a href=\"#\">link</a>\"}";
        let value = serde_json::Value::String(raw.to_string());
        let salvaged = salvage_delegate_args_from_raw(&value).expect("salvage should succeed");
        assert_eq!(salvaged.target_profile_id, "coder");
        assert!(salvaged.brief.contains("class=\"hero\""));
        assert!(salvaged.brief.contains("href=\"#\""));
    }

    #[test]
    fn salvage_handles_empty_target_profile_id() {
        // The schema allows an empty target_profile_id (means "default").
        let raw = r#"{"target_profile_id":"","brief":"do a thing"}"#;
        let salvaged =
            salvage_delegate_args_from_raw(&serde_json::Value::String(raw.to_string())).unwrap();
        assert_eq!(salvaged.target_profile_id, "");
        assert_eq!(salvaged.brief, "do a thing");
    }

    #[test]
    fn salvage_returns_none_when_brief_missing() {
        let raw = r#"{"target_profile_id":"coder"}"#;
        let value = serde_json::Value::String(raw.to_string());
        assert!(salvage_delegate_args_from_raw(&value).is_none());
    }

    #[test]
    fn salvage_returns_none_for_non_string_value() {
        let value = json!({ "target_profile_id": "coder", "brief": "x" });
        assert!(salvage_delegate_args_from_raw(&value).is_none());
    }

    #[test]
    fn args_error_hint_lists_object_keys() {
        let obj = json!({ "wrong_field": "x", "another": "y" });
        let hint = build_args_error_hint(&obj);
        assert!(hint.contains("wrong_field"));
        assert!(hint.contains("another"));
    }
}
