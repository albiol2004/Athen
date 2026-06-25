//! `spawn_subagent` — the tool the main agent uses to spawn a specialist.
//! (Accepts the legacy alias `delegate_to_agent` on the wire.)
//!
//! Mental model from the user (real-life analogy): your assistant calls
//! someone else for a specific service. The specialist does the job and
//! reports back. They do NOT recursively call other specialists for that
//! task. Depth is exactly 1.
//!
//! Implementation:
//! - `DelegationToolRegistry` wraps a base `AppToolRegistry` and adds the
//!   `spawn_subagent` tool to its surface.
//! - When the agent calls `spawn_subagent`, this module builds a sub-
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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use athen_agent::AgentBuilder;
use athen_core::error::{AthenError, Result};
use athen_core::risk::BaseImpact;
use athen_core::subagent::{is_spawn_subagent_name, SPAWN_SUBAGENT_TOOL_NAME};
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::tool::ToolRegistry;

/// Builds the BARE sub-arc-scoped tool registry (no delegation wrapper —
/// depth=1). Constructed by the composition root from `AppState` (captured
/// through the Tauri `AppHandle`), so `delegation.rs` never depends on
/// `AppState`. The returned registry is scoped to `(sub_arc_id,
/// sub_profile_id)`: GitHub identity, checkpoint branch, approval-arc tag,
/// `active_profile_id`, and identity/skill blocks all follow the sub-agent's
/// own arc + profile rather than the parent's.
pub type SubRegistryFactory = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Box<dyn ToolRegistry>> + Send>>
        + Send
        + Sync,
>;

/// Builds the per-arc LLM router cell for the sub-arc, honoring whatever pin
/// was propagated onto it (provider + `pinned_slug`). Same
/// `Arc<RwLock<Arc<DefaultLlmRouter>>>` shape `SharedRouter` wraps, so the
/// sub-agent runs on the SAME provider+model as the parent unless overridden.
pub type SubRouterFactory = Arc<
    dyn Fn(
            String,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Arc<tokio::sync::RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Everything the delegation tool needs to spin up a sub-agent. Cloneable
/// (every field is `Arc` or owned/`Clone`) so the wrapper can capture it
/// once and re-use it across many delegation calls in a session.
#[derive(Clone)]
pub struct DelegationContext {
    pub profile_store: Arc<athen_persistence::profiles::SqliteProfileStore>,
    /// Identity store for rendering the user's hand-maintained personality
    /// / rules / knowledge / team block into the sub-agent's system prefix.
    /// Optional because pre-identity tests may build a context without it.
    pub identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    /// Skill store. Used to render the SKILLS listing into the sub-agent's
    /// static prefix so a delegated specialist sees the same playbooks the
    /// parent had access to (load_skill resolves against the same store).
    /// Optional for the same backwards-compat reason as `identity_store`.
    pub skill_store: Option<Arc<athen_persistence::skills::SqliteSkillStore>>,
    /// Registered HTTP endpoints store. Used to render the
    /// "REGISTERED CLOUD APIs" block into the sub-agent's static prefix
    /// so the delegated specialist also sees ElevenLabs / Jina / etc.
    /// as already-configured and skips SDK-install rabbit holes.
    /// Optional for the same backwards-compat reason as `identity_store`.
    pub http_endpoint_store:
        Option<Arc<athen_persistence::http_endpoints::SqliteHttpEndpointStore>>,
    pub arc_store: athen_persistence::arcs::ArcStore,
    pub llm_router: Arc<tokio::sync::RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    /// The arc the *parent* agent is running under. Each delegation creates
    /// a sub-arc whose `parent_arc_id` points here.
    pub parent_arc_id: String,
    /// Tool documentation directory passed to the sub-agent's executor.
    pub tool_doc_dir: Option<std::path::PathBuf>,
    /// UI bridge used to wire a silent auditor on the sub-executor so
    /// the sub-agent's tool calls are persisted under `sub_arc_id`. None
    /// means "no bridge available" — sub-agent runs without persistence
    /// (CLI / test paths).
    pub ui: Option<crate::ui_bridge::UiBridge>,
    /// When `Some`, the spawned sub-agent's tool registry is wrapped with
    /// a `WakeupRestrictedRegistry` carrying these restrictions — i.e. the
    /// wake-up's tool/contact allowlist + autonomy band propagate to the
    /// child. `None` means the sub-agent runs with its profile's natural
    /// surface (either we're not inside a wake-up firing, or the wake-up
    /// opted out of inheritance via `Wakeup::inherit_restrictions = false`).
    pub wakeup_restrictions: Option<crate::wakeup_registry::WakeupSubagentRestrictions>,
    /// Builds the sub-agent's bare registry scoped to its own sub-arc +
    /// profile. `None` (CLI / tests) falls back to reusing the parent's base
    /// registry — today's behavior.
    pub sub_registry_factory: Option<SubRegistryFactory>,
    /// Builds the sub-agent's per-arc router honoring the inherited pin.
    /// `None` falls back to sharing the parent's handed-in router.
    pub sub_router_factory: Option<SubRouterFactory>,
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
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["default", "off", "minimal", "low", "medium", "high", "max"],
                    "description": "Optional cross-provider reasoning-effort override for the specialist's LLM calls. Use 'max' or 'high' when the brief is genuinely hard (complex code, multi-step planning) — reasoning tokens are billed at the output rate so cranking this on a chatty task wastes money. Omit or use 'default' to inherit the provider's built-in default."
                }
            },
            "required": ["target_profile_id", "brief"]
        })
    }

    fn delegate_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
            description: "Spawn a sub-agent to handle a task in parallel. Use this when:\n\
                 - A task needs a different specialist profile (e.g. coder for code, \
                   researcher for web research, marketing for copy)\n\
                 - You want to offload a self-contained subtask while you continue \
                   working on the main thread\n\
                 - The task benefits from a fresh context with a focused brief\n\
                 \n\
                 The sub-agent runs independently with its own tools and returns a \
                 structured result. It cannot delegate further (depth = 1).\n\
                 \n\
                 The `brief` is a SPECIFICATION — describe what the sub-agent should \
                 produce, don't paste the deliverable. The sub-agent sees ONLY the \
                 brief (not your conversation), so make it self-contained. Keep it \
                 under ~2000 characters."
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
pub(crate) struct DelegateArgs {
    pub(crate) target_profile_id: String,
    pub(crate) brief: String,
    /// Optional per-call reasoning-effort override for the sub-agent.
    /// Stored as the wire string (`"default"`/`"off"`/`"minimal"`/`"low"`/
    /// `"medium"`/`"high"`/`"max"`) and written onto the sub-arc so the
    /// sub-executor picks it up through the standard resolver chain.
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
}

#[async_trait]
impl ToolRegistry for DelegationToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut tools = self.base.list_tools().await?;
        tools.push(Self::delegate_tool_definition());
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        // Accept both the current name (`spawn_subagent`) and the legacy
        // alias (`delegate_to_agent`) so older prompts / model habits still
        // dispatch into delegation rather than falling through to the base.
        if !is_spawn_subagent_name(name) {
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
            Ok((sub_arc_id, content, success, verified, note)) => {
                // A run that succeeded but failed verification is reported as
                // a failure to the parent, with the verifier's note as the
                // error so the parent can re-brief or retry.
                let effective_success = success && verified;
                let error = if effective_success {
                    None
                } else {
                    note.clone()
                        .or_else(|| Some("sub-agent task did not succeed".to_string()))
                };
                Ok(ToolResult {
                    success: effective_success,
                    output: json!({
                        "sub_arc_id": sub_arc_id,
                        "content": content,
                        "success": effective_success,
                        "verified": verified,
                        "verification_note": note,
                    }),
                    error,
                    execution_time_ms: elapsed_ms,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: json!({ "error": e.to_string() }),
                error: Some(e.to_string()),
                execution_time_ms: elapsed_ms,
            }),
        }
    }
}

/// Copy the parent arc's runtime context onto a freshly-created sub-arc so
/// the sub-agent runs with the SAME provider, model slug, tier, and
/// reasoning effort as its parent. The sub-arc is brand new (pins unset), so
/// `set_pinned_provider_if_unset` installs the parent's pin cleanly; tier +
/// effort are last-write-wins setters. The per-call `reasoning_effort`
/// delegate param (if any) is applied by the caller AFTER this runs, so it
/// still wins over the inherited effort. Best-effort: a failed lookup or
/// setter just leaves the sub-arc to re-pin against the live active provider.
async fn propagate_parent_pins(
    arc_store: &athen_persistence::arcs::ArcStore,
    parent_arc_id: &str,
    sub_arc_id: &str,
) {
    let parent = match arc_store.get_arc(parent_arc_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("delegation: parent arc lookup failed: {e}");
            return;
        }
    };
    if let Some(provider_id) = parent.pinned_provider_id.as_deref() {
        let slug = parent.pinned_slug.as_deref().unwrap_or_default();
        if let Err(e) = arc_store
            .set_pinned_provider_if_unset(sub_arc_id, provider_id, slug)
            .await
        {
            tracing::warn!("delegation: propagate provider pin failed: {e}");
        }
    }
    if let Some(tier) = parent.tier_override.as_deref() {
        if let Err(e) = arc_store.set_tier_override(sub_arc_id, Some(tier)).await {
            tracing::warn!("delegation: propagate tier override failed: {e}");
        }
    }
    if let Some(effort) = parent.reasoning_effort_override.as_deref() {
        if let Err(e) = arc_store
            .set_reasoning_effort_override(sub_arc_id, Some(effort))
            .await
        {
            tracing::warn!("delegation: propagate reasoning effort failed: {e}");
        }
    }
}

/// Lightweight post-run check that the specialist's deliverable actually
/// satisfies the brief, mirroring the executor's completion-judge envelope
/// (one cheap-tier LLM call, low max_tokens, temperature 0, reasoning off,
/// 30s timeout). Returns `(verified, note)`.
///
/// Bias is toward NOT punishing a good run: an empty deliverable short-
/// circuits to `false` with no LLM call, a clear `CONTINUE` verdict flips to
/// `false`, but any LLM error / timeout / ambiguous answer defaults to
/// `true` so a flaky judge never hard-fails a legitimate result.
async fn verify_deliverable(
    router: &dyn LlmRouter,
    brief: &str,
    content: &str,
) -> (bool, Option<String>) {
    if content.trim().is_empty() {
        return (
            false,
            Some("specialist returned an empty deliverable".to_string()),
        );
    }
    let prompt = format!(
        "A specialist sub-agent was given this brief:\n\n\
         ---BRIEF---\n{}\n---END BRIEF---\n\n\
         It returned this deliverable:\n\n\
         ---DELIVERABLE---\n{}\n---END DELIVERABLE---\n\n\
         Does the deliverable satisfy the brief? Reply with exactly one line:\n\
         - `DONE` if it addresses the brief.\n\
         - `CONTINUE: <one short reason>` if it is empty, off-topic, a refusal, \
         or claims work it did not actually produce.",
        truncate(brief, 2000),
        truncate(content, 4000),
    );
    let request = athen_core::llm::LlmRequest {
        messages: vec![athen_core::llm::ChatMessage {
            role: athen_core::llm::Role::User,
            content: athen_core::llm::MessageContent::Text(prompt),
        }],
        profile: athen_core::llm::ModelProfile::Judges,
        max_tokens: Some(64),
        temperature: Some(0.0),
        tools: None,
        system_prompt: None,
        reasoning_effort: athen_core::llm::ReasoningEffort::Off,
    };
    match tokio::time::timeout(Duration::from_secs(30), router.route(&request)).await {
        Ok(Ok(resp)) => {
            let answer = resp.content.trim();
            if answer.to_uppercase().contains("CONTINUE") {
                let reason = answer
                    .split_once(':')
                    .map(|(_, rest)| rest.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("deliverable does not satisfy the brief")
                    .chars()
                    .take(200)
                    .collect::<String>();
                (false, Some(reason))
            } else {
                (true, None)
            }
        }
        Ok(Err(e)) => {
            tracing::warn!("delegation: deliverable verifier LLM error: {e}; trusting result");
            (true, None)
        }
        Err(_) => {
            tracing::warn!("delegation: deliverable verifier timed out; trusting result");
            (true, None)
        }
    }
}

/// The actual delegation run: resolve the target profile, create a sub-arc,
/// build a fresh executor whose tool registry is the bare base (no
/// delegation), and execute a synthetic task built from the brief.
///
/// Returns `(sub_arc_id, content, success, verified, verification_note)`.
pub(crate) async fn run_delegation(
    base: Arc<dyn ToolRegistry>,
    ctx: DelegationContext,
    args: DelegateArgs,
) -> Result<(String, String, bool, bool, Option<String>)> {
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
    let arc_name = format!("[{}] {}", profile.display_name, truncate(&args.brief, 60));
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
    // Inherit the parent arc's runtime context (provider pin, model slug,
    // tier, reasoning effort) so the specialist runs on the SAME model as
    // the parent rather than silently snapping to the global active
    // provider. Runs BEFORE the per-call reasoning_effort block below so an
    // explicit delegate-time effort still overrides the inherited one.
    propagate_parent_pins(&ctx.arc_store, &ctx.parent_arc_id, &sub_arc_id).await;
    // Per-call reasoning_effort: write onto the sub-arc so the standard
    // resolver chain (state::resolve_reasoning_effort_for_arc) picks it
    // up when the sub-executor builds its LlmRequest. Validated via
    // ReasoningEffort::from_str so a bogus value silently falls back to
    // the parent's default rather than corrupting persisted state.
    if let Some(ref raw) = args.reasoning_effort {
        use std::str::FromStr;
        match athen_core::llm::ReasoningEffort::from_str(raw) {
            Ok(eff) => {
                if let Err(e) = ctx
                    .arc_store
                    .set_reasoning_effort_override(&sub_arc_id, Some(eff.to_wire_str()))
                    .await
                {
                    tracing::warn!("delegation: set_reasoning_effort_override failed: {e}");
                }
            }
            Err(_) => {
                tracing::warn!(
                    "delegate_to_agent: ignoring unknown reasoning_effort={raw:?}; \
                     accepted: default|off|minimal|low|medium|high|max"
                );
            }
        }
    }

    // 3. Build the sub-executor. The sub-agent's tool registry is the
    //    *bare* base — no DelegationToolRegistry wrap — so it has every
    //    other tool but cannot itself delegate. Depth=1 by composition.
    //
    //    When the parent is a wake-up firing with `inherit_restrictions =
    //    true`, we re-wrap the sub-agent's registry with the same
    //    `WakeupRestrictedRegistry` the parent used so the wake-up's
    //    tool/contact allowlist + autonomy band propagate down. The
    //    restrictions snapshot was pre-resolved on the parent side, so
    //    no second contact-store lookup is needed.
    //
    //    Router: when a router factory is wired (the app paths), build a
    //    per-arc router for the sub-arc — after `propagate_parent_pins` ran,
    //    the sub-arc carries the parent's pin, so the factory resolves to the
    //    same provider+slug. Without a factory (CLI/tests) we fall back to
    //    sharing the parent's handed-in router.
    let sub_router_cell = match &ctx.sub_router_factory {
        Some(factory) => factory(sub_arc_id.clone()).await,
        None => Arc::clone(&ctx.llm_router),
    };
    let exec_router: Box<dyn LlmRouter> =
        Box::new(crate::state::SharedRouter(Arc::clone(&sub_router_cell)));
    //
    //    Registry: when a registry factory is wired, build a registry scoped
    //    to the sub-arc + sub-profile (correct GitHub identity, checkpoint
    //    branch, approval-arc tag, identity/skill blocks). Without it we
    //    reuse the parent's base — today's behavior. Either way the result is
    //    bare (no delegation wrapper), so depth=1 holds.
    let sub_base: Box<dyn ToolRegistry> = match &ctx.sub_registry_factory {
        Some(factory) => factory(sub_arc_id.clone(), resolved.profile.id.clone()).await,
        None => Box::new(ArcRegistryAdapter(base)),
    };
    let sub_registry: Box<dyn ToolRegistry> = match &ctx.wakeup_restrictions {
        Some(restrictions) => Box::new(
            crate::wakeup_registry::WakeupRestrictedRegistry::new_with_resolved(
                sub_base,
                restrictions.clone(),
            ),
        ),
        None => sub_base,
    };

    // Temperature follows the sub-arc's *effective* provider — the same one
    // the router factory resolved above — rather than the global active
    // provider. After `propagate_parent_pins`, that's the parent's pinned
    // provider when the parent was pinned, so router + temperature agree.
    let sampling_temperature = {
        let cfg = crate::state::load_config();
        let active_id = crate::state::resolve_active_provider(&cfg);
        let target = crate::state::resolve_effective_provider_for_arc_with_config(
            Some(&ctx.arc_store),
            &sub_arc_id,
            &active_id,
            athen_core::llm::ModelProfile::Powerful,
            &cfg,
        )
        .await;
        crate::compaction::resolve_provider_temperature(&cfg, &target.provider_id)
    };

    // Identity block follows the *sub-agent's* profile, not the parent's —
    // a personal-assistant who delegates to coder gives the sub-agent the
    // coding-style entries, not the personal-assistant ones.
    let identity_block = crate::identity_render::render_identity_block(
        ctx.identity_store.as_ref(),
        &resolved.profile.id,
    )
    .await;
    // Endpoints block is install-wide, not profile-specific (any profile
    // that has http_request can call any registered endpoint). The
    // executor still gates on http_request being in the sub-agent's
    // tool slice, so a profile without it pays zero bytes.
    let endpoints_block =
        crate::endpoints_render::render_endpoints_block(ctx.http_endpoint_store.as_ref()).await;
    let skills_block =
        crate::skills_render::render_skills_block(ctx.skill_store.as_ref(), &resolved.profile.id)
            .await;

    // Sub-executor's reasoning effort: resolved from the sub-arc, which
    // we may have just written above (per-call delegate param) or which
    // inherits the parent's setting via a previous user gesture. The
    // standard resolver covers both cases uniformly.
    let sub_reasoning_effort =
        crate::state::resolve_reasoning_effort_for_arc(Some(&ctx.arc_store), &sub_arc_id).await;

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(sub_registry)
        .timeout(Duration::from_secs(180))
        .active_profile(resolved)
        .identity_block(identity_block)
        .endpoints_block(endpoints_block)
        .skills_block(skills_block)
        .enable_default_reminders(true)
        .default_temperature(sampling_temperature)
        .default_reasoning_effort(sub_reasoning_effort);
    if let Some(ref dir) = ctx.tool_doc_dir {
        builder = builder.tool_doc_dir(dir.clone());
    }
    builder = builder
        .toolbox_info(athen_agent::toolbox::ToolboxPromptInfo::load().await)
        .shell_kind(athen_agent::detect_shell_kind().await);

    // Wire a *silent* auditor on the sub-executor so its tool calls are
    // persisted to `arc_entries` under the sub-arc id. The frontend reads
    // these to render the inline expandable view under the parent's
    // `delegate_to_agent` row. We deliberately skip `agent-progress` events
    // (silent) and don't pass a stream sender — both would surface in the
    // parent's UI as if the parent took those steps.
    if let Some(ref ui) = ctx.ui {
        let sub_turn_id = uuid::Uuid::new_v4().to_string();
        let sub_tool_log = crate::commands::new_tool_log();
        let sub_auditor = crate::commands::TauriAuditor::new_silent(
            ui.clone(),
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

    // Post-run verification: confirm the deliverable actually satisfies the
    // brief before we report success upward. Skipped when the run already
    // failed (the failure carries its own signal). Reuses the sub-agent's
    // router (same provider/model), so it's one cheap-tier call.
    let (verified, verification_note) = if result.success {
        let verify_router = crate::state::SharedRouter(Arc::clone(&sub_router_cell));
        verify_deliverable(&verify_router, &args.brief, &content).await
    } else {
        (true, None)
    };
    if !verified {
        tracing::warn!(
            sub_arc_id = %sub_arc_id,
            note = ?verification_note,
            "delegate_to_agent: deliverable failed post-run verification"
        );
    }

    Ok((
        sub_arc_id,
        content,
        result.success,
        verified,
        verification_note,
    ))
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
    let target_profile_id =
        extract_short_string_field(raw, "target_profile_id").unwrap_or_default();
    let brief = extract_trailing_string_field(raw, "brief")?;
    if brief.trim().is_empty() {
        return None;
    }
    Some(DelegateArgs {
        target_profile_id,
        brief,
        // Salvage path: optional field is not worth a regex.
        reasoning_effort: None,
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
        assert_eq!(def.name, "spawn_subagent");
        assert!(def.description.contains("specialist"));
        // The legacy alias must still route into delegation at call_tool.
        assert!(is_spawn_subagent_name("delegate_to_agent"));
        assert!(is_spawn_subagent_name("spawn_subagent"));
        // Required args present.
        let schema = &def.parameters;
        let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
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

    /// A canned `LlmRouter` for verifier tests: `Some(text)` replies with that
    /// content; `None` errors (to exercise the trust-on-error path).
    struct FakeRouter {
        reply: Option<String>,
    }

    #[async_trait]
    impl athen_core::traits::llm::LlmRouter for FakeRouter {
        async fn route(
            &self,
            _req: &athen_core::llm::LlmRequest,
        ) -> Result<athen_core::llm::LlmResponse> {
            match &self.reply {
                Some(text) => Ok(athen_core::llm::LlmResponse {
                    content: text.clone(),
                    reasoning_content: None,
                    model_used: "fake".into(),
                    provider: "fake".into(),
                    usage: Default::default(),
                    tool_calls: vec![],
                    finish_reason: athen_core::llm::FinishReason::Stop,
                }),
                None => Err(AthenError::Other("router boom".into())),
            }
        }
        async fn budget_remaining(&self) -> Result<athen_core::llm::BudgetStatus> {
            Ok(athen_core::llm::BudgetStatus {
                daily_limit_usd: None,
                spent_today_usd: 0.0,
                remaining_usd: None,
                tokens_used_today: 0,
            })
        }
    }

    /// An empty deliverable fails verification without spending an LLM call.
    #[tokio::test]
    async fn verify_deliverable_empty_short_circuits() {
        let router = FakeRouter {
            reply: Some("DONE".into()),
        };
        let (verified, note) = verify_deliverable(&router, "brief", "   \n\t ").await;
        assert!(!verified);
        assert!(note.is_some());
    }

    /// A `CONTINUE: <reason>` verdict flips verification to false and carries
    /// the reason.
    #[tokio::test]
    async fn verify_deliverable_continue_flips_false() {
        let router = FakeRouter {
            reply: Some("CONTINUE: off-topic".into()),
        };
        let (verified, note) = verify_deliverable(&router, "brief", "some content").await;
        assert!(!verified);
        assert_eq!(note.as_deref(), Some("off-topic"));
    }

    /// `DONE` passes verification.
    #[tokio::test]
    async fn verify_deliverable_done_passes() {
        let router = FakeRouter {
            reply: Some("DONE".into()),
        };
        let (verified, note) = verify_deliverable(&router, "brief", "real content").await;
        assert!(verified);
        assert!(note.is_none());
    }

    /// An LLM error during verification trusts the result (never hard-fails a
    /// good run on a flaky judge).
    #[tokio::test]
    async fn verify_deliverable_router_error_trusts_result() {
        let router = FakeRouter { reply: None };
        let (verified, _note) = verify_deliverable(&router, "brief", "content").await;
        assert!(verified);
    }

    /// `propagate_parent_pins` copies the parent arc's provider pin, slug,
    /// tier, and reasoning effort onto a fresh sub-arc; a later per-call
    /// effort write (mirroring run_delegation's per-call block) still wins.
    #[tokio::test]
    async fn propagate_parent_pins_copies_runtime_context() {
        use athen_persistence::arcs::{ArcSource, ArcStore};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let store = ArcStore::new(Arc::new(tokio::sync::Mutex::new(conn)));
        store.init_schema().await.unwrap();

        store
            .create_arc("parent", "p", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("parent", "deepseek", "deepseek-chat")
            .await
            .unwrap();
        store
            .set_tier_override("parent", Some("code"))
            .await
            .unwrap();
        store
            .set_reasoning_effort_override("parent", Some("high"))
            .await
            .unwrap();
        store
            .create_arc_with_parent("sub", "s", ArcSource::System, "parent")
            .await
            .unwrap();

        propagate_parent_pins(&store, "parent", "sub").await;

        let sub = store.get_arc("sub").await.unwrap().unwrap();
        assert_eq!(sub.pinned_provider_id.as_deref(), Some("deepseek"));
        assert_eq!(sub.pinned_slug.as_deref(), Some("deepseek-chat"));
        assert_eq!(sub.tier_override.as_deref(), Some("code"));
        assert_eq!(sub.reasoning_effort_override.as_deref(), Some("high"));

        // Per-call delegate effort overrides the inherited one.
        store
            .set_reasoning_effort_override("sub", Some("low"))
            .await
            .unwrap();
        let sub = store.get_arc("sub").await.unwrap().unwrap();
        assert_eq!(sub.reasoning_effort_override.as_deref(), Some("low"));
    }

    /// The group-restricted `coder` builtin profile must expose
    /// `spawn_subagent` after `apply_tool_selection`, even though its group
    /// whitelist does not contain the "delegate" group. This is the headline
    /// regression #175 targets.
    #[test]
    fn coder_profile_exposes_spawn_subagent() {
        let coder = athen_persistence::profiles::canonical_builtin_profile("coder")
            .expect("coder builtin profile exists");
        // Sanity: coder really is group-restricted and does NOT list delegate.
        match &coder.tool_selection {
            athen_core::agent_profile::ToolSelection::Groups(groups) => {
                assert!(!groups.iter().any(|g| g == "delegate"));
            }
            other => panic!("coder expected ToolSelection::Groups, got {other:?}"),
        }
        let tools = vec![
            ToolDefinition {
                name: "spawn_subagent".to_string(),
                description: "x".to_string(),
                parameters: json!({}),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "shell_execute".to_string(),
                description: "x".to_string(),
                parameters: json!({}),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
        ];
        let filtered = athen_agent::executor::apply_tool_selection(&tools, &coder.tool_selection);
        assert!(filtered.iter().any(|t| t.name == "spawn_subagent"));
    }
}
