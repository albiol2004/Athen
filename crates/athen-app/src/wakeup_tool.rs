//! `create_wakeup` — the tool the agent uses to schedule its own follow-ups.
//!
//! Mental model: the agent submits something to a platform and wants to
//! come back in a couple of hours; or sets a daily check-in; or recurs a
//! report. The agent describes the future work as a fresh instruction +
//! schedule, the wake-up scheduler fires it, the executor handles the
//! fire exactly like any other wake-up the user could have created.
//!
//! Risk model — the key insight from `project_wakeups_phase5_design.md`:
//! the act of scheduling is cheap; the *future fire* is what carries
//! risk. The declared `tool_allowlist` + `contact_allowlist` get pinned
//! into the wake-up at creation time, and the wake-up wrapper *enforces*
//! them at fire time, so the declaration is a grounded signal of intent.
//!
//! BaseImpact computed dynamically per call:
//!   `BaseImpact = max(base_risk for each tool in declared allowlist)`.
//! `tool_allowlist = None` defaults to `WritePersist` (conservative
//! middle — encourages agents to declare narrow allowlists without
//! making a Read-only check-in feel hostile).
//!
//! Autonomy:
//!   - default `SafeOnly` if the agent doesn't ask
//!   - if the agent explicitly requests `Auto`, escalate the
//!     `create_wakeup` call to a user-approval question on the
//!     ApprovalRouter (Auto is a user-trust call, not an agent-trust
//!     call). User confirms → the wake-up is persisted with `Auto`.
//!     User denies → the call returns `success=false`.
//!
//! `BaseImpact::System` (today's "critical" tier) also forces the
//! approval prompt regardless of autonomy, because the future fire will
//! be doing destructive things even under SafeOnly.
//!
//! Composition: this wrapper sits between `DelegationToolRegistry` and
//! `WakeupRestrictedRegistry` — `delegate_to_agent` is exposed by the
//! inner layer (so a wake-up can declare a sub-agent in its allowlist),
//! and the outer wake-up wrapper can still hide `create_wakeup` from a
//! locked-down wake-up's surface if its allowlist excludes it.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
use athen_core::config::NotificationChannelKind;
use athen_core::contact::ContactId;
use athen_core::error::{AthenError, Result};
use athen_core::notification::{NotificationOrigin, NotificationUrgency};
use athen_core::risk::BaseImpact;
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::tool::ToolRegistry;
use athen_core::traits::wakeup::WakeupStore;
use athen_core::wakeup::{AutonomyBand, Schedule, Wakeup, WakeupOrigin};

const CREATE_WAKEUP_TOOL_NAME: &str = "create_wakeup";

/// Everything the create_wakeup tool needs to persist a wake-up and
/// (when escalation triggers) ask the user for permission. Cloneable so
/// the wrapper can capture it once at composition and re-use it across
/// many calls in a session.
#[derive(Clone)]
pub struct WakeupToolContext {
    pub wakeup_store: Arc<dyn WakeupStore>,
    /// Used when the agent requests `Auto` autonomy or when the dynamic
    /// BaseImpact comes out as `System`. `None` disables both escalation
    /// paths — calls that would have escalated fail closed instead of
    /// silently widening trust. CLI / test builds sit at `None`.
    pub approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    /// The parent arc id this tool call originates from. Stored as
    /// `WakeupOrigin::Agent { authoring_arc_id }` so the UI can link a
    /// fired wake-up back to the conversation that authored it.
    pub parent_arc_id: String,
}

/// Wraps a base tool registry and exposes `create_wakeup` as an
/// additional tool the agent can call to schedule its own follow-ups.
pub struct WakeupAuthoringRegistry {
    inner: Box<dyn ToolRegistry>,
    ctx: WakeupToolContext,
}

impl WakeupAuthoringRegistry {
    pub fn new(inner: Box<dyn ToolRegistry>, ctx: WakeupToolContext) -> Self {
        Self { inner, ctx }
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: CREATE_WAKEUP_TOOL_NAME.to_string(),
            description:
                "Schedule a future follow-up for yourself. Use when you've kicked off \
                 work that needs checking back later (form submitted, build running, \
                 daily report due) or when the user asked you to remind them. \
                 \n\
                 The future fire spawns a fresh agent run with the `instruction` you \
                 provide. Output destination must be part of the instruction itself \
                 (the future agent picks the right tool — write to a file, send a \
                 message, append to the arc). \
                 \n\
                 Risk: the call's risk is computed from the tools you declare in \
                 `tool_allowlist`. Read-only check-ins fire silently; outbound or \
                 critical tools require approval at create time. Requesting \
                 `autonomy = \"auto\"` always asks the user — Auto is a user-trust \
                 setting only the user can grant.\n\
                 \n\
                 Schedule shape (pick one):\n\
                 - One-shot at an absolute time: `{\"kind\":\"one_shot\",\"at\":\"<RFC3339>\"}`\n\
                 - One-shot relative to now:     `{\"kind\":\"one_shot\",\"in\":\"2h\"}` \
                   (durations: combinations of `Nd`, `Nh`, `Nm`, `Ns`, e.g. \"1d12h\")\n\
                 - Recurring interval:           `{\"kind\":\"interval\",\"every_seconds\":3600}`\n\
                 - Cron:                         `{\"kind\":\"cron\",\"expr\":\"0 8 * * *\",\"tz\":\"UTC\"}`"
                    .to_string(),
            parameters: Self::schema(),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            // The static base_risk has to be conservative because the per-action
            // gate runs *before* call_tool sees args. The real, dynamic risk
            // computation lives inside `call_tool` via `compute_dynamic_impact`,
            // which routes through ApprovalRouter when needed. Read keeps the
            // outer gate quiet so our own dynamic gate is the only one talking
            // to the user.
            base_risk: BaseImpact::Read,
        }
    }

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "What the future agent should do on fire. Self-contained — the future run won't see this conversation. State the goal, the destination, and any context it needs. Example: 'Check whether the form submission at https://example.com/track/123 has been processed; if it has, append the result to this arc.'"
                },
                "schedule": {
                    "type": "object",
                    "description": "When to fire. See the tool description for the four shapes.",
                    "properties": {
                        "kind": { "type": "string", "enum": ["one_shot", "interval", "cron"] },
                        "at":   { "type": "string", "description": "RFC3339 absolute time (one_shot)" },
                        "in":   { "type": "string", "description": "Relative duration like '2h' or '1d12h' (one_shot)" },
                        "every_seconds": { "type": "integer", "description": "Interval period (interval)" },
                        "anchor": { "type": "string", "description": "RFC3339 anchor for the interval; defaults to now" },
                        "expr":   { "type": "string", "description": "5-field cron expression (cron)" },
                        "tz":     { "type": "string", "description": "IANA timezone for the cron (cron)" }
                    },
                    "required": ["kind"]
                },
                "profile":  { "type": "string", "description": "Agent profile to run the future fire under. Defaults to 'assistant'." },
                "autonomy": {
                    "type": "string",
                    "enum": ["safe_only", "auto", "notify_only"],
                    "description": "How autonomous the future fire is. Defaults to 'safe_only'. Requesting 'auto' triggers a user-approval prompt at create time."
                },
                "tool_allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Names of tools the future fire is allowed to invoke. None / empty = use profile defaults. Declaring a narrow list is the cheapest way to keep this wake-up silent at create time."
                },
                "contact_allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Contact UUIDs the future fire is allowed to send to (outbound tools only). None = use profile defaults."
                },
                "inherit_restrictions": {
                    "type": "boolean",
                    "description": "When true (default), sub-agents the wake-up spawns via delegate_to_agent inherit its tool/contact allowlist + autonomy band."
                },
                "preferred_channel": {
                    "type": "string",
                    "enum": ["in_app", "telegram"],
                    "description": "Where to deliver the 'wake-up done' completion ping."
                },
                "arc_id": {
                    "type": "string",
                    "description": "Arc to append the future fire to. Omit to spawn a fresh arc per fire."
                }
            },
            "required": ["instruction", "schedule"]
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CreateWakeupArgs {
    instruction: String,
    schedule: ScheduleArg,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    autonomy: Option<String>,
    #[serde(default)]
    tool_allowlist: Option<Vec<String>>,
    #[serde(default)]
    contact_allowlist: Option<Vec<String>>,
    #[serde(default)]
    inherit_restrictions: Option<bool>,
    #[serde(default)]
    preferred_channel: Option<String>,
    #[serde(default)]
    arc_id: Option<String>,
}

/// Tagged schedule shape mirroring `wakeup_commands::ScheduleReq` but
/// with an extra `in` field for relative one-shots — handy when the
/// agent wants "two hours from now" without doing the RFC3339 math.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScheduleArg {
    OneShot {
        #[serde(default)]
        at: Option<String>,
        #[serde(default, rename = "in")]
        in_: Option<String>,
    },
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    Interval {
        every_seconds: u64,
        #[serde(default)]
        anchor: Option<String>,
    },
}

impl ScheduleArg {
    fn into_schedule(self, now: DateTime<Utc>) -> std::result::Result<Schedule, String> {
        match self {
            ScheduleArg::OneShot { at, in_ } => match (at, in_) {
                (Some(s), None) => parse_rfc3339(&s).map(|at| Schedule::OneShot { at }),
                (None, Some(s)) => {
                    let d = parse_duration(&s)?;
                    Ok(Schedule::OneShot { at: now + d })
                }
                (Some(_), Some(_)) => Err("one_shot: pass either `at` or `in`, not both".into()),
                (None, None) => Err("one_shot: missing `at` or `in`".into()),
            },
            ScheduleArg::Cron { expr, tz } => Ok(Schedule::Cron {
                expr,
                tz: tz.unwrap_or_else(|| "UTC".into()),
            }),
            ScheduleArg::Interval {
                every_seconds,
                anchor,
            } => {
                if every_seconds == 0 {
                    return Err("interval: every_seconds must be > 0".into());
                }
                let anchor = match anchor {
                    Some(s) => parse_rfc3339(&s)?,
                    None => now,
                };
                Ok(Schedule::Interval {
                    every_seconds,
                    anchor,
                })
            }
        }
    }
}

fn parse_rfc3339(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| format!("invalid RFC3339 timestamp '{s}': {e}"))
}

/// Parse durations like `2h`, `90m`, `30s`, `1d12h`, `1d2h30m`. Accepts
/// any combination of the unit suffixes `d` (days), `h` (hours),
/// `m` (minutes), `s` (seconds). Returns the total as a chrono Duration.
fn parse_duration(s: &str) -> std::result::Result<chrono::Duration, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("duration string is empty".into());
    }
    let mut total_secs: i64 = 0;
    let mut current_num = String::new();
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else if c.is_ascii_alphabetic() {
            if current_num.is_empty() {
                return Err(format!("unit '{c}' has no number in '{s}'"));
            }
            let n: i64 = current_num
                .parse()
                .map_err(|_| format!("can't parse number in '{s}'"))?;
            current_num.clear();
            let mult: i64 = match c.to_ascii_lowercase() {
                'd' => 24 * 3600,
                'h' => 3600,
                'm' => 60,
                's' => 1,
                other => return Err(format!("unknown duration unit '{other}' in '{s}'")),
            };
            total_secs = total_secs
                .checked_add(
                    n.checked_mul(mult)
                        .ok_or_else(|| format!("duration '{s}' overflows i64 seconds"))?,
                )
                .ok_or_else(|| format!("duration '{s}' overflows i64 seconds"))?;
        } else if !c.is_whitespace() {
            return Err(format!("unexpected character '{c}' in duration '{s}'"));
        }
    }
    if !current_num.is_empty() {
        return Err(format!(
            "trailing number '{current_num}' without a unit in '{s}'"
        ));
    }
    if total_secs <= 0 {
        return Err(format!("duration '{s}' must be > 0"));
    }
    Ok(chrono::Duration::seconds(total_secs))
}

fn parse_channel(s: &str) -> std::result::Result<NotificationChannelKind, String> {
    match s {
        "in_app" | "InApp" => Ok(NotificationChannelKind::InApp),
        "telegram" | "Telegram" => Ok(NotificationChannelKind::Telegram),
        other => Err(format!("unknown notification channel '{other}'")),
    }
}

fn sanitize_tool_allowlist(v: Option<Vec<String>>) -> Option<Vec<String>> {
    let cleaned: Vec<String> = v
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn parse_contact_allowlist(
    v: Option<Vec<String>>,
) -> std::result::Result<Option<Vec<ContactId>>, String> {
    let raw = v.unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let id = Uuid::parse_str(s).map_err(|e| format!("invalid contact id '{s}': {e}"))?;
        out.push(id);
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Compute the effective `BaseImpact` for a `create_wakeup` call by
/// looking up each declared tool in the inner registry's `list_tools`
/// and taking the max of their `base_risk` values. `None` allowlist
/// defaults to `WritePersist` (conservative middle).
pub(crate) fn compute_dynamic_impact(
    tool_allowlist: Option<&[String]>,
    available_tools: &[ToolDefinition],
) -> BaseImpact {
    let Some(names) = tool_allowlist.filter(|v| !v.is_empty()) else {
        return BaseImpact::WritePersist;
    };
    let mut max = BaseImpact::Read;
    for name in names {
        if let Some(def) = available_tools.iter().find(|d| &d.name == name) {
            if (def.base_risk as u8) > (max as u8) {
                max = def.base_risk;
            }
        }
        // Unknown names contribute nothing — the wake-up wrapper will
        // simply reject calls to them at fire time. They don't add risk.
    }
    max
}

/// Returns `true` if creating the wake-up needs to ask the user before
/// persisting. Two reasons trigger an ask:
///   1. Agent explicitly requested `Auto` — that's a user-trust call.
///   2. Dynamic BaseImpact is `System` — the future fire will be doing
///      destructive things; the user should know it was scheduled at
///      all.
pub(crate) fn needs_user_approval(requested: AutonomyBand, impact: BaseImpact) -> bool {
    matches!(impact, BaseImpact::System) || matches!(requested, AutonomyBand::Auto)
}

fn approval_prompt(args: &CreateWakeupArgs, impact: BaseImpact, autonomy: AutonomyBand) -> String {
    let reason = match (impact, autonomy) {
        (BaseImpact::System, AutonomyBand::Auto) => {
            "the agent requested Auto autonomy AND the wake-up may invoke critical tools"
        }
        (BaseImpact::System, _) => "the wake-up may invoke critical tools",
        (_, AutonomyBand::Auto) => "the agent requested Auto autonomy",
        _ => "this wake-up needs your approval",
    };
    let summary = truncate(&args.instruction, 240);
    format!(
        "Approve agent-scheduled wake-up?\n\n\
         Reason: {reason}.\n\n\
         Instruction: {summary}"
    )
}

fn approval_description(args: &CreateWakeupArgs) -> String {
    let mut lines = Vec::new();
    if let Some(p) = &args.profile {
        lines.push(format!("Profile: {p}"));
    }
    if let Some(a) = &args.autonomy {
        lines.push(format!("Autonomy: {a}"));
    }
    if let Some(tools) = &args.tool_allowlist {
        if !tools.is_empty() {
            lines.push(format!("Tools: {}", tools.join(", ")));
        }
    }
    if let Some(contacts) = &args.contact_allowlist {
        if !contacts.is_empty() {
            lines.push(format!("Contacts: {}", contacts.join(", ")));
        }
    }
    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[async_trait]
impl ToolRegistry for WakeupAuthoringRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut tools = self.inner.list_tools().await?;
        tools.push(Self::tool_definition());
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        if name != CREATE_WAKEUP_TOOL_NAME {
            return self.inner.call_tool(name, args).await;
        }

        let started = Instant::now();
        let parsed: CreateWakeupArgs = match serde_json::from_value(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": format!("invalid arguments: {e}") }),
                    error: Some(format!("invalid arguments: {e}")),
                    execution_time_ms: started.elapsed().as_millis() as u64,
                });
            }
        };

        match self.handle_create(parsed, started).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(ToolResult {
                success: false,
                output: json!({ "error": e.to_string() }),
                error: Some(e.to_string()),
                execution_time_ms: started.elapsed().as_millis() as u64,
            }),
        }
    }
}

impl WakeupAuthoringRegistry {
    async fn handle_create(&self, args: CreateWakeupArgs, started: Instant) -> Result<ToolResult> {
        let now = Utc::now();

        // 1. Validate + assemble.
        let instruction = args.instruction.trim().to_string();
        if instruction.is_empty() {
            return Ok(bad_args(started, "instruction is empty"));
        }
        let autonomy = args
            .autonomy
            .as_deref()
            .map(AutonomyBand::from_str_lossy)
            .unwrap_or(AutonomyBand::SafeOnly);
        let preferred_channel = match args.preferred_channel.as_deref() {
            Some(s) => match parse_channel(s) {
                Ok(c) => Some(c),
                Err(e) => return Ok(bad_args(started, &e)),
            },
            None => None,
        };
        let tool_allowlist = sanitize_tool_allowlist(args.tool_allowlist.clone());
        let contact_allowlist = match parse_contact_allowlist(args.contact_allowlist.clone()) {
            Ok(v) => v,
            Err(e) => return Ok(bad_args(started, &e)),
        };
        let inherit_restrictions = args.inherit_restrictions.unwrap_or(true);
        let profile = args
            .profile
            .clone()
            .unwrap_or_else(|| "assistant".to_string());

        // Schedule conversion (consumes a clone of the schedule field
        // so the original `args` stays intact for prompt-building below).
        let schedule = match args.schedule.clone().into_schedule(now) {
            Ok(s) => s,
            Err(e) => return Ok(bad_args(started, &e)),
        };
        let next_fire_at = athen_scheduler::compute_next_fire(&schedule, now);
        if next_fire_at.is_none() {
            return Ok(bad_args(
                started,
                "schedule produced no next fire time (one-shot in the past or invalid cron)",
            ));
        }

        // 2. Dynamic BaseImpact: max of declared allowlist's base_risk.
        let available = self.inner.list_tools().await?;
        let impact = compute_dynamic_impact(tool_allowlist.as_deref(), &available);

        // 3. Decide whether to ask. Auto-request OR System-tier impact
        //    triggers an approval prompt. Other impacts fall through and
        //    persist directly — Read silently, WritePersist with the
        //    persisted-arc-entry that the executor already records.
        if needs_user_approval(autonomy, impact) {
            let Some(router) = self.ctx.approval_router.as_ref() else {
                // Fail closed: we'd otherwise silently widen autonomy or
                // schedule a System-tier fire.
                return Ok(deny(
                    started,
                    "create_wakeup requires user approval but no approval router is configured; \
                     refusing to schedule. Re-run after the approval router is wired, or pick a \
                     narrower allowlist that avoids critical tools and a non-Auto autonomy.",
                ));
            };
            let q = ApprovalQuestion {
                id: Uuid::new_v4(),
                prompt: approval_prompt(&args, impact, autonomy),
                description: Some(approval_description(&args)),
                choices: vec![ApprovalChoice::approve(), ApprovalChoice::deny()],
                arc_id: Some(self.ctx.parent_arc_id.clone()),
                task_id: None,
                origin: NotificationOrigin::RiskSystem,
                urgency: NotificationUrgency::High,
                created_at: Utc::now(),
            };
            let primary = router.pick_primary(Some(&self.ctx.parent_arc_id)).await;
            let answer = router
                .ask_with_escalation(q, primary)
                .await
                .map_err(|e| AthenError::Other(format!("create_wakeup approval ask: {e}")))?;
            // ApprovalChoice::approve()/deny() use the literal keys
            // "approve" / "deny", same as ApprovalChoiceKind::Approve.
            // Treat anything that isn't "approve" as deny (Cancel etc.).
            if answer.choice_key != "approve" {
                return Ok(deny(started, "user denied the wake-up creation"));
            }
        }

        // 4. Persist. Origin tags the authoring arc so the UI links the
        //    fired wake-up back to the conversation that authored it.
        let authoring_arc_id = crate::file_gate::arc_uuid(&self.ctx.parent_arc_id);
        let w = Wakeup {
            id: Uuid::new_v4(),
            schedule,
            instruction,
            autonomy,
            preferred_channel,
            tool_allowlist,
            contact_allowlist,
            inherit_restrictions,
            profile,
            arc_id: args.arc_id,
            origin: WakeupOrigin::Agent { authoring_arc_id },
            created_at: now,
            last_fired_at: None,
            next_fire_at,
            enabled: true,
        };
        self.ctx.wakeup_store.create(&w).await?;

        let elapsed = started.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "wakeup_id": w.id.to_string(),
                "next_fire_at": w.next_fire_at.map(|d| d.to_rfc3339()),
                "autonomy": w.autonomy.as_str(),
                "computed_impact": match impact {
                    BaseImpact::Read => "read",
                    BaseImpact::WriteTemp => "write_temp",
                    BaseImpact::WritePersist => "write_persist",
                    BaseImpact::System => "system",
                },
            }),
            error: None,
            execution_time_ms: elapsed,
        })
    }
}

fn bad_args(started: Instant, msg: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: json!({ "error": msg }),
        error: Some(msg.to_string()),
        execution_time_ms: started.elapsed().as_millis() as u64,
    }
}

fn deny(started: Instant, msg: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: json!({ "error": msg, "denied": true }),
        error: Some(msg.to_string()),
        execution_time_ms: started.elapsed().as_millis() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str, risk: BaseImpact) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: json!({}),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: risk,
        }
    }

    #[test]
    fn dynamic_impact_defaults_to_write_persist_when_no_allowlist() {
        let tools = vec![
            def("read", BaseImpact::Read),
            def("send", BaseImpact::WritePersist),
        ];
        assert_eq!(
            compute_dynamic_impact(None, &tools),
            BaseImpact::WritePersist
        );
        // Empty allowlist treated the same as None.
        let empty: Vec<String> = vec![];
        assert_eq!(
            compute_dynamic_impact(Some(&empty), &tools),
            BaseImpact::WritePersist
        );
    }

    #[test]
    fn dynamic_impact_takes_max_across_declared_tools() {
        let tools = vec![
            def("read", BaseImpact::Read),
            def("write_tmp", BaseImpact::WriteTemp),
            def("send_email", BaseImpact::WritePersist),
            def("submit_trade", BaseImpact::System),
        ];
        // Read-only check-back wake-up: silent.
        let read_only = vec!["read".to_string()];
        assert_eq!(
            compute_dynamic_impact(Some(&read_only), &tools),
            BaseImpact::Read
        );
        // Outbound wake-up: notify.
        let outbound = vec!["send_email".to_string()];
        assert_eq!(
            compute_dynamic_impact(Some(&outbound), &tools),
            BaseImpact::WritePersist
        );
        // Trading wake-up: critical.
        let trading = vec!["read".to_string(), "submit_trade".to_string()];
        assert_eq!(
            compute_dynamic_impact(Some(&trading), &tools),
            BaseImpact::System
        );
    }

    #[test]
    fn dynamic_impact_ignores_unknown_tool_names() {
        let tools = vec![def("read", BaseImpact::Read)];
        let names = vec!["read".to_string(), "made_up_tool".to_string()];
        // Unknown name doesn't count toward the max — the wake-up
        // wrapper will reject it at fire time anyway.
        assert_eq!(
            compute_dynamic_impact(Some(&names), &tools),
            BaseImpact::Read
        );
    }

    #[test]
    fn approval_required_when_agent_requests_auto() {
        assert!(needs_user_approval(AutonomyBand::Auto, BaseImpact::Read));
        assert!(needs_user_approval(
            AutonomyBand::Auto,
            BaseImpact::WritePersist
        ));
    }

    #[test]
    fn approval_required_when_impact_is_system_regardless_of_autonomy() {
        assert!(needs_user_approval(
            AutonomyBand::SafeOnly,
            BaseImpact::System
        ));
        assert!(needs_user_approval(
            AutonomyBand::NotifyOnly,
            BaseImpact::System
        ));
    }

    #[test]
    fn approval_not_required_for_safe_only_low_impact() {
        assert!(!needs_user_approval(
            AutonomyBand::SafeOnly,
            BaseImpact::Read
        ));
        assert!(!needs_user_approval(
            AutonomyBand::SafeOnly,
            BaseImpact::WriteTemp
        ));
        assert!(!needs_user_approval(
            AutonomyBand::SafeOnly,
            BaseImpact::WritePersist
        ));
        assert!(!needs_user_approval(
            AutonomyBand::NotifyOnly,
            BaseImpact::WritePersist
        ));
    }

    #[test]
    fn parse_duration_accepts_simple_units() {
        assert_eq!(
            parse_duration("30s").unwrap(),
            chrono::Duration::seconds(30)
        );
        assert_eq!(parse_duration("5m").unwrap(), chrono::Duration::minutes(5));
        assert_eq!(parse_duration("2h").unwrap(), chrono::Duration::hours(2));
        assert_eq!(parse_duration("1d").unwrap(), chrono::Duration::days(1));
    }

    #[test]
    fn parse_duration_combines_units() {
        assert_eq!(
            parse_duration("1d12h").unwrap(),
            chrono::Duration::hours(36)
        );
        assert_eq!(
            parse_duration("1d2h30m").unwrap(),
            chrono::Duration::days(1) + chrono::Duration::hours(2) + chrono::Duration::minutes(30)
        );
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err()); // unit with no number
        assert!(parse_duration("5").is_err()); // number with no unit
        assert!(parse_duration("3z").is_err()); // unknown unit
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("0s").is_err()); // must be > 0
    }

    #[test]
    fn schedule_one_shot_in_resolves_relative_to_now() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let s = ScheduleArg::OneShot {
            at: None,
            in_: Some("2h".into()),
        };
        let resolved = s.into_schedule(now).unwrap();
        match resolved {
            Schedule::OneShot { at } => assert_eq!(at, now + chrono::Duration::hours(2)),
            other => panic!("expected OneShot, got {other:?}"),
        }
    }

    #[test]
    fn schedule_one_shot_at_uses_absolute_timestamp() {
        let now = Utc::now();
        let s = ScheduleArg::OneShot {
            at: Some("2026-06-01T08:00:00Z".into()),
            in_: None,
        };
        let resolved = s.into_schedule(now).unwrap();
        match resolved {
            Schedule::OneShot { at } => {
                assert_eq!(at.to_rfc3339(), "2026-06-01T08:00:00+00:00");
            }
            other => panic!("expected OneShot, got {other:?}"),
        }
    }

    #[test]
    fn schedule_one_shot_rejects_both_at_and_in() {
        let s = ScheduleArg::OneShot {
            at: Some("2026-06-01T08:00:00Z".into()),
            in_: Some("2h".into()),
        };
        assert!(s.into_schedule(Utc::now()).is_err());
    }

    #[test]
    fn schedule_one_shot_rejects_neither_at_nor_in() {
        let s = ScheduleArg::OneShot {
            at: None,
            in_: None,
        };
        assert!(s.into_schedule(Utc::now()).is_err());
    }

    #[test]
    fn schedule_interval_zero_period_is_rejected() {
        let s = ScheduleArg::Interval {
            every_seconds: 0,
            anchor: None,
        };
        assert!(s.into_schedule(Utc::now()).is_err());
    }

    // -- Integration tests against a tiny in-memory WakeupStore to exercise
    // -- the wrapper end-to-end: schema surface, args validation, persistence,
    // -- approval-router fail-closed when System-tier impact is requested.

    #[derive(Default)]
    struct InMemWakeupStore {
        rows: tokio::sync::Mutex<Vec<Wakeup>>,
    }

    #[async_trait]
    impl WakeupStore for InMemWakeupStore {
        async fn create(&self, w: &Wakeup) -> Result<()> {
            self.rows.lock().await.push(w.clone());
            Ok(())
        }
        async fn update(&self, _w: &Wakeup) -> Result<()> {
            unreachable!("not used in tests")
        }
        async fn delete(&self, _id: Uuid) -> Result<()> {
            unreachable!()
        }
        async fn get(&self, _id: Uuid) -> Result<Option<Wakeup>> {
            Ok(None)
        }
        async fn list_all(&self) -> Result<Vec<Wakeup>> {
            Ok(self.rows.lock().await.clone())
        }
        async fn list_due(&self, _now: DateTime<Utc>) -> Result<Vec<Wakeup>> {
            Ok(vec![])
        }
        async fn mark_fired(
            &self,
            _id: Uuid,
            _fired_at: DateTime<Utc>,
            _next_fire_at: Option<DateTime<Utc>>,
        ) -> Result<()> {
            unreachable!()
        }
        async fn set_enabled(&self, _id: Uuid, _enabled: bool) -> Result<()> {
            unreachable!()
        }
    }

    /// Minimal inner registry exposing one Read tool, so `create_wakeup`
    /// has something to look up when computing dynamic BaseImpact.
    struct OneReadToolRegistry;

    #[async_trait]
    impl ToolRegistry for OneReadToolRegistry {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(vec![def("read", BaseImpact::Read)])
        }
        async fn call_tool(&self, _name: &str, _args: serde_json::Value) -> Result<ToolResult> {
            unreachable!("the wrapper handles create_wakeup itself; no inner call expected")
        }
    }

    fn mk_wrapper(store: Arc<dyn WakeupStore>) -> WakeupAuthoringRegistry {
        WakeupAuthoringRegistry::new(
            Box::new(OneReadToolRegistry),
            WakeupToolContext {
                wakeup_store: store,
                approval_router: None,
                parent_arc_id: "arc_test".to_string(),
            },
        )
    }

    #[tokio::test]
    async fn list_tools_exposes_create_wakeup_alongside_inner() {
        let store: Arc<dyn WakeupStore> = Arc::new(InMemWakeupStore::default());
        let r = mk_wrapper(store);
        let tools = r.list_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == CREATE_WAKEUP_TOOL_NAME));
        assert!(tools.iter().any(|t| t.name == "read"));
    }

    #[tokio::test]
    async fn call_create_wakeup_persists_with_agent_origin_and_safeonly_default() {
        let store_concrete = Arc::new(InMemWakeupStore::default());
        let store: Arc<dyn WakeupStore> = store_concrete.clone();
        let r = mk_wrapper(store);
        let args = json!({
            "instruction": "Check back on the form submission",
            "schedule": { "kind": "one_shot", "in": "2h" },
            "tool_allowlist": ["read"]
        });
        let res = r.call_tool(CREATE_WAKEUP_TOOL_NAME, args).await.unwrap();
        assert!(res.success, "expected success, got {res:?}");

        let rows = store_concrete.rows.lock().await;
        assert_eq!(rows.len(), 1);
        let w = &rows[0];
        // Origin must mark this as agent-authored, not user.
        match &w.origin {
            WakeupOrigin::Agent { .. } => {}
            other => panic!("expected Agent origin, got {other:?}"),
        }
        // Default autonomy is SafeOnly when the agent doesn't ask for more.
        assert_eq!(w.autonomy, AutonomyBand::SafeOnly);
        // inherit_restrictions defaults to true (safer).
        assert!(w.inherit_restrictions);
        assert!(w.enabled);
    }

    #[tokio::test]
    async fn call_create_wakeup_rejects_empty_instruction() {
        let store: Arc<dyn WakeupStore> = Arc::new(InMemWakeupStore::default());
        let r = mk_wrapper(store);
        let res = r
            .call_tool(
                CREATE_WAKEUP_TOOL_NAME,
                json!({ "instruction": "   ", "schedule": { "kind": "one_shot", "in": "1h" } }),
            )
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn call_create_wakeup_fails_closed_when_auto_requested_without_router() {
        // Auto autonomy needs the approval router; with router=None the
        // tool must refuse, not silently widen trust.
        let store_concrete = Arc::new(InMemWakeupStore::default());
        let store: Arc<dyn WakeupStore> = store_concrete.clone();
        let r = mk_wrapper(store);
        let res = r
            .call_tool(
                CREATE_WAKEUP_TOOL_NAME,
                json!({
                    "instruction": "Submit and revisit",
                    "schedule": { "kind": "one_shot", "in": "1h" },
                    "autonomy": "auto",
                    "tool_allowlist": ["read"]
                }),
            )
            .await
            .unwrap();
        assert!(!res.success);
        // And the row must NOT have been persisted.
        assert_eq!(store_concrete.rows.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn non_create_wakeup_calls_pass_through_to_inner() {
        // The wrapper must forward calls for tools it doesn't own — this
        // is what makes the rest of the registry actually work.
        struct EchoRegistry;
        #[async_trait]
        impl ToolRegistry for EchoRegistry {
            async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
                Ok(vec![def("read", BaseImpact::Read)])
            }
            async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: json!({ "echoed_name": name, "echoed_args": args }),
                    error: None,
                    execution_time_ms: 0,
                })
            }
        }
        let store: Arc<dyn WakeupStore> = Arc::new(InMemWakeupStore::default());
        let r = WakeupAuthoringRegistry::new(
            Box::new(EchoRegistry),
            WakeupToolContext {
                wakeup_store: store,
                approval_router: None,
                parent_arc_id: "arc_test".into(),
            },
        );
        let res = r
            .call_tool("read", json!({ "path": "/tmp/x" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["echoed_name"], "read");
    }
}
