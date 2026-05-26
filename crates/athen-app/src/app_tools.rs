//! Composite tool registry that extends ShellToolRegistry with app-level tools.
//!
//! Calendar CRUD tools and contact management tools are added here since
//! athen-agent doesn't depend on athen-persistence or athen-contacts.
//! The composition root (athen-app) wires the stores into the registry
//! before handing it to the agent.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use athen_agent::ShellToolRegistry;
use athen_contacts::ContactStore;
use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};
use athen_core::error::{AthenError, Result};
use athen_core::event::AttachmentId;
use athen_core::http_endpoint::{AuthMethod, RegisteredEndpoint};
use athen_core::identity::{IdentityEntry, ProfileTag};
use athen_core::risk::BaseImpact;
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;
use athen_core::traits::http_endpoint::HttpEndpointStore;
use athen_core::traits::identity::IdentityStore;
use athen_core::traits::mcp::McpClient;
use athen_core::traits::memory::{MemoryItem, MemoryStore};
use athen_core::traits::skill::SkillStore;
use athen_core::traits::tool::ToolRegistry;
use athen_core::traits::vault::Vault;
use athen_memory::Memory;
use athen_persistence::attachments::AttachmentStore;
use athen_persistence::calendar::{CalendarEvent, CalendarStore, EventCreator};
use athen_persistence::contacts::SqliteContactStore;
use athen_persistence::http_endpoints::SqliteHttpEndpointStore;
use athen_persistence::identity::SqliteIdentityStore;
use athen_persistence::skills::SqliteSkillStore;

use crate::file_gate::FileGate;
use crate::http_rate_limiter::{HttpRateLimiter, RateCheck};
use crate::vault_creds::endpoint_scope;

/// Prefix MCP-routed tools use to avoid name collisions with built-in tools.
/// `slack__post_message` resolves to mcp_id="slack", tool="post_message".
const MCP_TOOL_SEPARATOR: &str = "__";

/// Wraps [`ShellToolRegistry`] and adds calendar, contact, memory, and MCP tools.
pub struct AppToolRegistry {
    inner: Arc<ShellToolRegistry>,
    calendar: Option<CalendarStore>,
    contacts: Option<SqliteContactStore>,
    memory: Option<Arc<Memory>>,
    mcp: Option<Arc<dyn McpClient>>,
    file_gate: Option<Arc<FileGate>>,
    attachments: Option<AttachmentStore>,
    identity: Option<Arc<SqliteIdentityStore>>,
    skills: Option<Arc<SqliteSkillStore>>,
    http_endpoints: Option<Arc<SqliteHttpEndpointStore>>,
    vault: Option<Arc<dyn Vault>>,
    http_rate_limiter: Option<Arc<HttpRateLimiter>>,
    http_client: Option<reqwest::Client>,
    /// Path to the auto-generated `cloud_apis.md` index. When present,
    /// the `http_request` tool description points the agent at it so
    /// they can read the endpoint catalogue on demand.
    cloud_apis_doc_path: Option<std::path::PathBuf>,
    /// Calendar source config store — used by `calendar_create` to push
    /// agent-authored events to the remote calendar (iCloud/Google/etc.)
    /// alongside the local insert. When `None`, agent calendar writes
    /// stay local-only.
    calendar_source_store: Option<Arc<dyn CalendarSourceConfigStore>>,
    /// When set to `"athen_setup"`, the registry includes the 6 setup_*
    /// tools for interactive onboarding. Other profiles never see them.
    active_profile_id: Option<String>,
    /// Per-arc cache of already-loaded skill slugs. The second call to
    /// `load_skill` with the same slug returns a short "already loaded"
    /// stub instead of the full body, saving tokens. Uses interior
    /// mutability so `call_tool(&self, …)` can update the cache without
    /// requiring `&mut self`.
    loaded_skills: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl AppToolRegistry {
    /// Create a new composite registry.
    pub fn new(
        inner: ShellToolRegistry,
        calendar: Option<CalendarStore>,
        contacts: Option<SqliteContactStore>,
        memory: Option<Arc<Memory>>,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            calendar,
            contacts,
            memory,
            mcp: None,
            file_gate: None,
            attachments: None,
            identity: None,
            skills: None,
            http_endpoints: None,
            vault: None,
            http_rate_limiter: None,
            http_client: None,
            cloud_apis_doc_path: None,
            calendar_source_store: None,
            active_profile_id: None,
            loaded_skills: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn with_active_profile_id(mut self, id: Option<String>) -> Self {
        self.active_profile_id = id;
        self
    }

    pub fn with_vault_standalone(mut self, vault: Arc<dyn Vault>) -> Self {
        if self.vault.is_none() {
            self.vault = Some(vault);
        }
        self
    }

    /// Attach the calendar source config store so `calendar_create` can
    /// push agent-authored events to the user's remote calendar. The
    /// vault attached via `with_http_endpoints` is reused for the
    /// CalDAV credentials — both lookups go through the same `Vault`
    /// trait object.
    pub fn with_calendar_remote(mut self, store: Arc<dyn CalendarSourceConfigStore>) -> Self {
        self.calendar_source_store = Some(store);
        self
    }

    /// Attach the registered-endpoint store + vault + rate limiter so the
    /// agent can call `http_request` against any endpoint the user has
    /// registered. Without all three, the tool refuses with a clear
    /// error and is not advertised in `list_tools`.
    pub fn with_http_endpoints(
        mut self,
        store: Arc<SqliteHttpEndpointStore>,
        vault: Arc<dyn Vault>,
        rate_limiter: Arc<HttpRateLimiter>,
        client: reqwest::Client,
        cloud_apis_doc_path: Option<std::path::PathBuf>,
    ) -> Self {
        self.http_endpoints = Some(store);
        self.vault = Some(vault);
        self.http_rate_limiter = Some(rate_limiter);
        self.http_client = Some(client);
        self.cloud_apis_doc_path = cloud_apis_doc_path;
        self
    }

    /// Attach the identity store so the agent can call `identity_add` to
    /// persist new personality / rules / knowledge / user / team statements
    /// into the user-editable identity prefix. Without this, the tool
    /// refuses with a clear error.
    pub fn with_identity(mut self, identity: Arc<SqliteIdentityStore>) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Attach the skill store so the agent can call `load_skill` to pull
    /// procedural-playbook bodies on demand. The static-prefix listing of
    /// available skills is built by the composition root from the same
    /// store; this wires the *invocation* side. Without this, the tool is
    /// not advertised and `load_skill` calls refuse with a clear error.
    pub fn with_skills(mut self, skills: Arc<SqliteSkillStore>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Attach an MCP client. Tools exposed by enabled MCP servers will appear
    /// alongside the built-in tools, prefixed with `<mcp_id>__`.
    pub fn with_mcp(mut self, mcp: Arc<dyn McpClient>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Attach the path-permission gate. When set, every file-touching tool
    /// call is routed through `FileGate::handle` before reaching the
    /// underlying registry or MCP client.
    pub fn with_file_gate(mut self, gate: Arc<FileGate>) -> Self {
        self.file_gate = Some(gate);
        self
    }

    /// Attach the attachment store so the agent can call
    /// `read_attachment_full` / `fetch_attachment` against rows that
    /// `prepare_attachment_surfacing` already advertised in turn 0.
    pub fn with_attachments(mut self, store: AttachmentStore) -> Self {
        self.attachments = Some(store);
        self
    }

    // ── Schema helpers ───────────────────────────────────────────────

    fn calendar_list_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "start": {
                    "type": "string",
                    "description": "Start of time range (ISO 8601 UTC string, e.g. '2026-04-05T00:00:00Z')"
                },
                "end": {
                    "type": "string",
                    "description": "End of time range (ISO 8601 UTC string, e.g. '2026-04-06T00:00:00Z')"
                }
            },
            "required": ["start", "end"]
        })
    }

    fn calendar_create_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Event title"
                },
                "start_time": {
                    "type": "string",
                    "description": "Start time (ISO 8601 UTC, e.g. '2026-04-05T14:00:00Z')"
                },
                "end_time": {
                    "type": "string",
                    "description": "End time (ISO 8601 UTC, e.g. '2026-04-05T15:00:00Z')"
                },
                "all_day": {
                    "type": "boolean",
                    "description": "Whether this is an all-day event (default false)"
                },
                "description": {
                    "type": "string",
                    "description": "Event description (optional)"
                },
                "location": {
                    "type": "string",
                    "description": "Event location (optional)"
                },
                "category": {
                    "type": "string",
                    "description": "Category for grouping/coloring. Prefer the user's existing calendar names when known (e.g. 'Trabajo', 'Familia', 'Casa'). Otherwise one of: meeting, birthday, deadline, reminder, personal, work, other."
                },
                "reminder_minutes": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Reminder lead times in minutes (e.g. [15, 60] = 15min and 1h before). Omit for no reminder."
                },
                "target_calendar_id": {
                    "type": "string",
                    "description": "Specific calendar id to save into. Omit to use the user's default agent calendar (set in Settings → Calendar)."
                }
            },
            "required": ["title", "start_time", "end_time"]
        })
    }

    fn calendar_update_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The event ID to update"
                },
                "title": {
                    "type": "string",
                    "description": "New title (optional, keeps existing if omitted)"
                },
                "start_time": {
                    "type": "string",
                    "description": "New start time (optional)"
                },
                "end_time": {
                    "type": "string",
                    "description": "New end time (optional)"
                },
                "all_day": {
                    "type": "boolean",
                    "description": "Whether this is an all-day event"
                },
                "description": {
                    "type": "string",
                    "description": "New description"
                },
                "location": {
                    "type": "string",
                    "description": "New location"
                },
                "category": {
                    "type": "string",
                    "description": "New category"
                },
                "color": {
                    "type": "string",
                    "description": "New hex color"
                },
                "reminder_minutes": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "New reminder lead times"
                },
                "recurrence": {
                    "type": "string",
                    "description": "New recurrence (Daily/Weekly/Monthly/Yearly or null)"
                }
            },
            "required": ["id"]
        })
    }

    fn calendar_delete_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The event ID to delete"
                }
            },
            "required": ["id"]
        })
    }

    // ── Tool implementations ─────────────────────────────────────────

    async fn do_calendar_list(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self
            .calendar
            .as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let start = args
            .get("start")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'start' parameter".into()))?;
        let end = args
            .get("end")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'end' parameter".into()))?;

        tracing::info!(
            tool = "calendar_list",
            start,
            end,
            "Listing calendar events"
        );

        let t = Instant::now();
        let events = store.list_events(start, end).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        let events_json: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "title": e.title,
                    "start_time": e.start_time,
                    "end_time": e.end_time,
                    "all_day": e.all_day,
                    "location": e.location,
                    "description": e.description,
                    "category": e.category,
                    "color": e.color,
                    "recurrence": e.recurrence,
                    "reminder_minutes": e.reminder_minutes,
                })
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: json!({ "events": events_json, "count": events_json.len() }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_calendar_create(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self
            .calendar
            .as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'title' parameter".into()))?;
        let start_time = args
            .get("start_time")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'start_time' parameter".into()))?;
        let end_time = args
            .get("end_time")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'end_time' parameter".into()))?;

        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let reminder_minutes: Vec<i64> = args
            .get("reminder_minutes")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();

        let target_calendar_id = args
            .get("target_calendar_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        let mut event = CalendarEvent {
            id: id.clone(),
            title: title.to_string(),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            start_time: start_time.to_string(),
            end_time: end_time.to_string(),
            all_day: args
                .get("all_day")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            location: args
                .get("location")
                .and_then(|v| v.as_str())
                .map(String::from),
            recurrence: None,
            reminder_minutes,
            color: None,
            category: args
                .get("category")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_by: EventCreator::Agent,
            arc_id: None,
            created_at: now.clone(),
            updated_at: now,
            source_id: None,
            remote_id: None,
            remote_etag: None,
            ical_uid: None,
        };

        tracing::info!(
            tool = "calendar_create",
            title,
            start_time,
            "Creating calendar event"
        );

        // Try to push to the remote first so the local row carries the
        // remote_id/etag straight away (and the next sync pass treats it
        // as already-synced, not orphaned).
        let push_outcome = self
            .try_push_agent_create(&event, target_calendar_id.as_deref())
            .await;
        let (pushed_remote, push_target_name, push_error) = match push_outcome {
            Ok(Some((source_id, remote_id, etag, ical_uid, cal_name))) => {
                event.source_id = Some(source_id);
                event.remote_id = Some(remote_id);
                event.remote_etag = etag;
                event.ical_uid = Some(ical_uid);
                (true, Some(cal_name), None)
            }
            Ok(None) => (false, None, None),
            Err(e) => {
                // Remote push failed (auth, network, 403…). Still create
                // the event locally — the agent already told the user it
                // would. Surface the error so the audit trail shows why
                // it didn't land on the phone.
                tracing::warn!(error = %e, "Agent calendar_create remote push failed; falling back to local-only");
                (false, None, Some(e))
            }
        };

        let t = Instant::now();
        store.create_event(&event).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        let message = if pushed_remote {
            format!(
                "Event '{}' created and pushed to '{}'",
                title,
                push_target_name.as_deref().unwrap_or("remote calendar")
            )
        } else if let Some(ref e) = push_error {
            format!("Event '{title}' created locally (remote push failed: {e})")
        } else {
            format!("Event '{title}' created locally (no remote calendar configured)")
        };

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": id,
                "title": title,
                "start_time": start_time,
                "end_time": end_time,
                "pushed_remote": pushed_remote,
                "remote_target": push_target_name,
                "push_error": push_error,
                "message": message,
            }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    /// Resolve a write target + push the event. Priority:
    ///   1. Explicit `target_calendar_id` arg paired with the user-default
    ///      source (most common: user said "use my Familia calendar").
    ///   2. Config `agent_default_source_id` + `agent_default_calendar_id`.
    ///   3. `auto_pick_write_target` — only fires when exactly one source
    ///      is enabled.
    ///   4. None → local-only (`Ok(None)`).
    async fn try_push_agent_create(
        &self,
        event: &CalendarEvent,
        target_calendar_id: Option<&str>,
    ) -> std::result::Result<Option<(String, String, Option<String>, String, String)>, String> {
        let Some(cfg_store) = self.calendar_source_store.clone() else {
            return Ok(None);
        };
        let Some(vault) = self.vault.clone() else {
            return Ok(None);
        };

        let cfg = crate::settings::load_main_config_public();
        let default_source = cfg.calendar.agent_default_source_id.as_deref();
        let default_calendar = cfg.calendar.agent_default_calendar_id.as_deref();
        let default_calendar_name = cfg.calendar.agent_default_calendar_name.as_deref();

        // Resolve target.
        let target: Option<crate::calendar_sources::WriteTarget> = if let Some(cal_id) =
            target_calendar_id
        {
            // Need a source to pair with the calendar id. Prefer the
            // configured default, otherwise auto-pick a sole source.
            let source_id_str = default_source.map(String::from);
            let source = if let Some(sid) = source_id_str {
                let uuid = uuid::Uuid::parse_str(&sid)
                    .map_err(|e| format!("Bad default source id: {e}"))?;
                cfg_store
                    .get(uuid)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Configured default calendar source not found".to_string())?
            } else {
                let sources = cfg_store.list().await.map_err(|e| e.to_string())?;
                let enabled: Vec<_> = sources.into_iter().filter(|s| s.enabled).collect();
                if enabled.len() != 1 {
                    return Err(
                            "Agent supplied target_calendar_id but no default source is set and more than one source is enabled.".into(),
                        );
                }
                enabled.into_iter().next().unwrap()
            };
            Some(crate::calendar_sources::WriteTarget {
                source,
                calendar_id: cal_id.to_string(),
                calendar_name: cal_id.to_string(),
            })
        } else if let (Some(sid), Some(cid)) = (default_source, default_calendar) {
            let uuid =
                uuid::Uuid::parse_str(sid).map_err(|e| format!("Bad default source id: {e}"))?;
            let source = cfg_store
                .get(uuid)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Configured default calendar source not found".to_string())?;
            Some(crate::calendar_sources::WriteTarget {
                source,
                calendar_id: cid.to_string(),
                calendar_name: default_calendar_name
                    .map(String::from)
                    .unwrap_or_else(|| cid.to_string()),
            })
        } else {
            crate::calendar_sources::auto_pick_write_target(&cfg_store, &vault)
                .await
                .map_err(|e| e.to_string())?
        };

        let Some(target) = target else {
            return Ok(None);
        };

        let (remote_id, etag, uid) = crate::calendar_sources::push_create(&target, &vault, event)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some((
            target.source.id.to_string(),
            remote_id,
            etag,
            uid,
            target.calendar_name,
        )))
    }

    async fn do_calendar_update(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self
            .calendar
            .as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;

        tracing::info!(tool = "calendar_update", id, "Updating calendar event");

        // Load the existing event first.
        let existing = store
            .get_event(id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Event '{id}' not found")))?;

        let now = chrono::Utc::now().to_rfc3339();

        let reminder_minutes: Vec<i64> = args
            .get("reminder_minutes")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or(existing.reminder_minutes);

        let recurrence = if args.get("recurrence").is_some() {
            args.get("recurrence")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(json!(s)).ok())
        } else {
            existing.recurrence
        };

        let updated = CalendarEvent {
            id: id.to_string(),
            title: args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing.title)
                .to_string(),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or(existing.description),
            start_time: args
                .get("start_time")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing.start_time)
                .to_string(),
            end_time: args
                .get("end_time")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing.end_time)
                .to_string(),
            all_day: args
                .get("all_day")
                .and_then(|v| v.as_bool())
                .unwrap_or(existing.all_day),
            location: args
                .get("location")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or(existing.location),
            recurrence,
            reminder_minutes,
            color: args
                .get("color")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or(existing.color),
            category: args
                .get("category")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or(existing.category),
            created_by: existing.created_by,
            arc_id: existing.arc_id,
            created_at: existing.created_at,
            updated_at: now,
            source_id: existing.source_id,
            remote_id: existing.remote_id,
            remote_etag: existing.remote_etag,
            ical_uid: existing.ical_uid,
        };

        let t = Instant::now();
        store.update_event(&updated).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": id,
                "title": updated.title,
                "message": format!("Event '{}' updated successfully", updated.title),
            }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_calendar_delete(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self
            .calendar
            .as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;

        tracing::info!(tool = "calendar_delete", id, "Deleting calendar event");

        let t = Instant::now();
        store.delete_event(id).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "id": id, "message": "Event deleted successfully" }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    // ── Contact schema helpers ──────────────────────────────────────

    fn contacts_list_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn contacts_search_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query to match against contact names and identifier values (case-insensitive)"
                }
            },
            "required": ["query"]
        })
    }

    fn contacts_create_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The contact's display name"
                },
                "identifiers": {
                    "type": "array",
                    "description": "List of identifiers for this contact (optional)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "value": {
                                "type": "string",
                                "description": "The identifier value (e.g. email address, phone number, username)"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Identifier type: Email, Phone, Telegram, WhatsApp, IMessage, Signal, Discord, Slack, Twitter, Username, Other"
                            }
                        },
                        "required": ["value", "kind"]
                    }
                },
                "trust_level": {
                    "type": "string",
                    "description": "Initial trust level: Unknown, Neutral, Known, Trusted (default: Neutral)"
                }
            },
            "required": ["name"]
        })
    }

    fn contacts_update_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The contact ID to update"
                },
                "name": {
                    "type": "string",
                    "description": "New name (optional, keeps existing if omitted)"
                },
                "identifiers": {
                    "type": "array",
                    "description": "New identifiers list (optional, REPLACES all existing identifiers if provided)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" },
                            "kind": { "type": "string" }
                        },
                        "required": ["value", "kind"]
                    }
                },
                "trust_level": {
                    "type": "string",
                    "description": "New trust level (optional)"
                }
            },
            "required": ["id"]
        })
    }

    fn contacts_delete_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The contact ID to delete"
                }
            },
            "required": ["id"]
        })
    }

    // ── Contact tool implementations ────────────────────────────────

    fn contact_store(&self) -> Result<&SqliteContactStore> {
        self.contacts
            .as_ref()
            .ok_or_else(|| AthenError::Other("Contact store not available".into()))
    }

    fn parse_identifier_kind(s: &str) -> IdentifierKind {
        match s {
            "Email" => IdentifierKind::Email,
            "Phone" => IdentifierKind::Phone,
            "Telegram" => IdentifierKind::Telegram,
            "WhatsApp" => IdentifierKind::WhatsApp,
            "IMessage" => IdentifierKind::IMessage,
            "Signal" => IdentifierKind::Signal,
            "Discord" => IdentifierKind::Discord,
            "Slack" => IdentifierKind::Slack,
            "Twitter" => IdentifierKind::Twitter,
            "Username" => IdentifierKind::Username,
            _ => IdentifierKind::Other,
        }
    }

    fn parse_trust_level(s: &str) -> TrustLevel {
        match s.to_lowercase().as_str() {
            "unknown" => TrustLevel::Unknown,
            "neutral" => TrustLevel::Neutral,
            "known" => TrustLevel::Known,
            "trusted" => TrustLevel::Trusted,
            "authuser" => TrustLevel::AuthUser,
            _ => TrustLevel::Neutral,
        }
    }

    fn contact_to_json(c: &Contact) -> serde_json::Value {
        json!({
            "id": c.id.to_string(),
            "name": c.name,
            "trust_level": format!("{:?}", c.trust_level),
            "trust_manual_override": c.trust_manual_override,
            "identifiers": c.identifiers.iter().map(|i| json!({
                "value": i.value,
                "kind": format!("{:?}", i.kind),
            })).collect::<Vec<_>>(),
            "interaction_count": c.interaction_count,
            "last_interaction": c.last_interaction.map(|t| t.to_rfc3339()),
            "blocked": c.blocked,
        })
    }

    async fn do_contacts_list(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.contact_store()?;

        tracing::info!(tool = "contacts_list", "Listing all contacts");

        let t = Instant::now();
        let contacts = store.list_all().await?;
        let elapsed = t.elapsed().as_millis() as u64;

        let contacts_json: Vec<serde_json::Value> =
            contacts.iter().map(Self::contact_to_json).collect();

        Ok(ToolResult {
            success: true,
            output: json!({ "contacts": contacts_json, "count": contacts_json.len() }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_contacts_search(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.contact_store()?;

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'query' parameter".into()))?;

        tracing::info!(tool = "contacts_search", query, "Searching contacts");

        let t = Instant::now();
        let contacts = store.list_all().await?;
        // Tokenize the query on whitespace and require ALL tokens to appear
        // as substrings in the per-contact haystack (name + every identifier
        // value, lowercased, joined). This makes "Alex Garcia" match a
        // contact whose name is "Alex" and whose email is "alex.garcia@...".
        let query_tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        let matches: Vec<serde_json::Value> = contacts
            .iter()
            .filter(|c| {
                if query_tokens.is_empty() {
                    return true;
                }
                let mut haystack = c.name.to_lowercase();
                for ident in &c.identifiers {
                    haystack.push(' ');
                    haystack.push_str(&ident.value.to_lowercase());
                }
                query_tokens.iter().all(|tok| haystack.contains(tok))
            })
            .map(Self::contact_to_json)
            .collect();
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "contacts": matches, "count": matches.len() }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_contacts_create(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.contact_store()?;

        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'name' parameter".into()))?;

        let identifiers: Vec<ContactIdentifier> = args
            .get("identifiers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let value = item.get("value")?.as_str()?;
                        let kind = item.get("kind")?.as_str()?;
                        Some(ContactIdentifier {
                            value: value.to_string(),
                            kind: Self::parse_identifier_kind(kind),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let trust_level = args
            .get("trust_level")
            .and_then(|v| v.as_str())
            .map(Self::parse_trust_level)
            .unwrap_or(TrustLevel::Neutral);

        let id = uuid::Uuid::new_v4();
        let contact = Contact {
            id,
            name: name.to_string(),
            trust_level,
            trust_manual_override: false,
            identifiers,
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: false,
        };

        tracing::info!(tool = "contacts_create", name, "Creating contact");

        let t = Instant::now();
        store.save(&contact).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": id.to_string(),
                "name": name,
                "message": format!("Contact '{}' created successfully", name),
            }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_contacts_update(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.contact_store()?;

        let id_str = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;

        let id = uuid::Uuid::parse_str(id_str)
            .map_err(|e| AthenError::Other(format!("Invalid contact ID: {e}")))?;

        tracing::info!(tool = "contacts_update", id = id_str, "Updating contact");

        let mut contact = store
            .load(id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Contact '{id_str}' not found")))?;

        if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
            contact.name = name.to_string();
        }

        if let Some(arr) = args.get("identifiers").and_then(|v| v.as_array()) {
            contact.identifiers = arr
                .iter()
                .filter_map(|item| {
                    let value = item.get("value")?.as_str()?;
                    let kind = item.get("kind")?.as_str()?;
                    Some(ContactIdentifier {
                        value: value.to_string(),
                        kind: Self::parse_identifier_kind(kind),
                    })
                })
                .collect();
        }

        if let Some(level_str) = args.get("trust_level").and_then(|v| v.as_str()) {
            contact.trust_level = Self::parse_trust_level(level_str);
        }

        let t = Instant::now();
        store.save(&contact).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": id_str,
                "name": contact.name,
                "message": format!("Contact '{}' updated successfully", contact.name),
            }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_contacts_delete(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.contact_store()?;

        let id_str = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;

        let id = uuid::Uuid::parse_str(id_str)
            .map_err(|e| AthenError::Other(format!("Invalid contact ID: {e}")))?;

        tracing::info!(tool = "contacts_delete", id = id_str, "Deleting contact");

        let t = Instant::now();
        store.delete(id).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "id": id_str, "message": "Contact deleted successfully" }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    // ── Attachment schema helpers ───────────────────────────────────

    fn read_attachment_full_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Attachment ID (UUID) — exactly as listed in the attachment surfacing message at the top of the conversation."
                }
            },
            "required": ["id"]
        })
    }

    fn fetch_attachment_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Attachment ID (UUID) — exactly as listed in the attachment surfacing message."
                }
            },
            "required": ["id"]
        })
    }

    // ── Attachment tool implementations ─────────────────────────────

    fn attachment_store(&self) -> Result<&AttachmentStore> {
        self.attachments
            .as_ref()
            .ok_or_else(|| AthenError::Other("Attachment store not available".into()))
    }

    fn parse_attachment_id(args: &serde_json::Value) -> Result<AttachmentId> {
        let id_str = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| AthenError::Other(format!("invalid attachment id '{id_str}': {e}")))?;
        Ok(AttachmentId(uuid))
    }

    /// Implementation of `read_attachment_full`. Returns the full
    /// extracted text for an attachment without truncation.
    ///
    /// Resolution order:
    /// 1. If a sidecar already exists, read it.
    /// 2. Else if it's a PDF with bytes still local, lazy-extract and
    ///    persist the new sidecar.
    /// 3. Else if the MIME starts with `text/`, read the local bytes
    ///    directly as UTF-8.
    /// 4. Otherwise return an error explaining what's missing.
    async fn do_read_attachment_full(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.attachment_store()?;
        let id = Self::parse_attachment_id(args)?;
        let start = Instant::now();

        let att = store
            .get(id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("attachment {id} not found")))?;

        tracing::info!(
            attachment_id = %att.id,
            name = %att.name,
            mime = %att.mime_type,
            has_local_path = att.local_path.is_some(),
            has_extracted_text = att.extracted_text_path.is_some(),
            purged = att.is_purged(),
            "read_attachment_full"
        );

        let mime = att.mime_type.to_ascii_lowercase();
        let is_pdf = mime.starts_with("application/pdf");
        let is_text = mime.starts_with("text/");

        // 1. Existing sidecar.
        if let Some(sidecar) = att.extracted_text_path.as_ref() {
            match tokio::fs::read_to_string(sidecar).await {
                Ok(text) => {
                    let elapsed = start.elapsed().as_millis() as u64;
                    return Ok(ToolResult {
                        success: true,
                        output: json!({
                            "id": id.to_string(),
                            "name": att.name,
                            "mime_type": att.mime_type,
                            "source": "sidecar",
                            "chars": text.chars().count(),
                            "text": text,
                        }),
                        error: None,
                        execution_time_ms: elapsed,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        path = %sidecar.display(),
                        error = %e,
                        "Failed to read attachment sidecar"
                    );
                }
            }
        }

        // 2. Lazy PDF extraction.
        if is_pdf {
            if let Some(local) = att.local_path.as_ref() {
                let local_clone = local.clone();
                match tokio::task::spawn_blocking(move || {
                    athen_sentidos::pdf_extract::extract_to_sidecar(&local_clone)
                })
                .await
                {
                    Ok(Ok(sidecar)) => {
                        if let Err(e) = store.record_extracted_text(att.id, sidecar.clone()).await {
                            tracing::warn!(
                                attachment_id = %att.id,
                                error = %e,
                                "Failed to persist lazy extracted_text_path"
                            );
                        }
                        let text = tokio::fs::read_to_string(&sidecar).await.map_err(|e| {
                            AthenError::Other(format!("extracted PDF sidecar unreadable: {e}"))
                        })?;
                        let elapsed = start.elapsed().as_millis() as u64;
                        return Ok(ToolResult {
                            success: true,
                            output: json!({
                                "id": id.to_string(),
                                "name": att.name,
                                "mime_type": att.mime_type,
                                "source": "lazy_extract",
                                "chars": text.chars().count(),
                                "text": text,
                            }),
                            error: None,
                            execution_time_ms: elapsed,
                        });
                    }
                    Ok(Err(e)) => {
                        return Err(AthenError::Other(format!("PDF extraction failed: {e}")));
                    }
                    Err(e) => {
                        return Err(AthenError::Other(format!("PDF extraction join error: {e}")));
                    }
                }
            }
        }

        // 3. Text MIME — just read the bytes.
        if is_text {
            if let Some(local) = att.local_path.as_ref() {
                let text = tokio::fs::read_to_string(local)
                    .await
                    .map_err(|e| AthenError::Other(format!("text attachment unreadable: {e}")))?;
                let elapsed = start.elapsed().as_millis() as u64;
                return Ok(ToolResult {
                    success: true,
                    output: json!({
                        "id": id.to_string(),
                        "name": att.name,
                        "mime_type": att.mime_type,
                        "source": "local_text",
                        "chars": text.chars().count(),
                        "text": text,
                    }),
                    error: None,
                    execution_time_ms: elapsed,
                });
            }
        }

        // 4. Nothing readable.
        let reason = if att.is_purged() {
            "bytes have been purged and no extracted text sidecar exists; \
             call fetch_attachment to redownload"
        } else if att.local_path.is_none() {
            "no bytes on disk and no extracted text sidecar — only metadata \
             was preserved (likely policy refused download); call \
             fetch_attachment to attempt redownload"
        } else if !is_pdf && !is_text {
            "binary attachment (not PDF, not text) — extraction is not \
             supported; the bytes are on disk but cannot be read as text"
        } else {
            "no readable representation available"
        };
        Err(AthenError::Other(format!(
            "read_attachment_full: cannot read '{}' (id={}, mime={}): {}",
            att.name, att.id, att.mime_type, reason
        )))
    }

    /// Implementation of `fetch_attachment`. The actual per-source refetch
    /// (Email IMAP `BODY[part]` / Telegram `getFile`) is wired alongside
    /// the TTL purger; until then this surfaces the row's current state +
    /// source coordinates so the agent can decide whether to ask the user
    /// to forward the file again or proceed without it.
    async fn do_fetch_attachment(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.attachment_store()?;
        let id = Self::parse_attachment_id(args)?;
        let start = Instant::now();

        let att = store
            .get(id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("attachment {id} not found")))?;

        tracing::info!(
            attachment_id = %att.id,
            name = %att.name,
            has_local_path = att.local_path.is_some(),
            purged = att.is_purged(),
            "fetch_attachment"
        );

        let elapsed = start.elapsed().as_millis() as u64;
        if att.is_local() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "id": id.to_string(),
                    "name": att.name,
                    "mime_type": att.mime_type,
                    "size_bytes": att.size_bytes,
                    "status": "already_local",
                    "message": "Bytes are still on disk — call read_attachment_full \
                                to read the text representation, or use the file \
                                tools if the path is needed.",
                }),
                error: None,
                execution_time_ms: elapsed,
            });
        }

        // Purged or never-fetched: surface the source and ask the agent to
        // proceed without the bytes (the sidecar may still satisfy text
        // questions). Per-source auto-refetch lands with #147.
        let source_summary = match att.source.as_ref() {
            Some(athen_core::event::AttachmentSource::Email { mailbox, uid, .. }) => {
                json!({ "kind": "email", "mailbox": mailbox, "uid": uid })
            }
            Some(athen_core::event::AttachmentSource::Telegram {
                chat_id,
                message_id,
                ..
            }) => {
                json!({ "kind": "telegram", "chat_id": chat_id, "message_id": message_id })
            }
            None => serde_json::Value::Null,
        };

        Ok(ToolResult {
            success: false,
            output: json!({
                "id": id.to_string(),
                "name": att.name,
                "mime_type": att.mime_type,
                "size_bytes": att.size_bytes,
                "status": if att.is_purged() { "purged" } else { "metadata_only" },
                "has_extracted_text": att.extracted_text_path.is_some(),
                "source": source_summary,
                "message": "Automatic refetch is not yet wired. If a text sidecar \
                            exists, call read_attachment_full instead. Otherwise \
                            ask the user to forward the file again.",
            }),
            error: Some("fetch_attachment: bytes unavailable, refetch not wired".into()),
            execution_time_ms: elapsed,
        })
    }

    // ── Persistent memory tool implementations ─────────────────────

    async fn do_persistent_memory_store(
        &self,
        memory: &Memory,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'key' parameter".into()))?;
        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'value' parameter".into()))?;

        tracing::info!(tool = "memory_store", key, "Storing in persistent memory");

        let start = Instant::now();
        let lookup = format!("{key}: {value}");

        // Pre-store dedup: skip if a sufficiently similar memory already
        // exists. Recall is gated by the global min-relevance threshold
        // (0.6 cosine), so any hit is a high-confidence overlap. Returning
        // success-with-status lets the LLM see the existing memory and
        // decide whether to retry with a genuinely new fact.
        if let Ok(hits) = memory.recall(&lookup, 1).await {
            if let Some(existing) = hits.first() {
                tracing::info!(
                    tool = "memory_store",
                    key,
                    existing_id = %existing.id,
                    "Skipping duplicate memory_store; similar entry already known"
                );
                return Ok(ToolResult {
                    success: true,
                    output: json!({
                        "status": "skipped",
                        "reason": "already_known",
                        "existing_id": existing.id,
                        "existing_content": existing.content,
                        "hint": "A similar memory is already stored. If you have genuinely NEW information beyond what's shown above, call memory_store again with the new fact only.",
                    }),
                    error: None,
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        let item = MemoryItem {
            id: format!("agent_{key}"),
            content: lookup,
            metadata: json!({
                "key": key,
                "value": value,
                "source": "agent_tool",
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
        };

        memory.remember(item).await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "stored": key, "persistent": true }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    async fn do_persistent_memory_recall(
        &self,
        memory: &Memory,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let key = args.get("key").and_then(|v| v.as_str());

        tracing::info!(
            tool = "memory_recall",
            ?key,
            "Recalling from persistent memory"
        );

        let start = Instant::now();

        let output = if let Some(query) = key {
            // Search for memories matching the key/query.
            match memory.recall(query, 10).await {
                Ok(items) if items.is_empty() => {
                    json!({ "query": query, "found": false, "memories": [] })
                }
                Ok(items) => {
                    let memories: Vec<serde_json::Value> = items
                        .iter()
                        .map(|item| {
                            json!({
                                "id": item.id,
                                "content": item.content,
                            })
                        })
                        .collect();
                    json!({ "query": query, "found": true, "memories": memories })
                }
                Err(e) => {
                    json!({ "query": query, "error": e.to_string() })
                }
            }
        } else {
            // No key: list all stored memories by searching with a very
            // broad query at zero threshold. We temporarily bypass the
            // min_relevance_score by searching for a common token.
            json!({
                "hint": "Please provide a search query to find specific memories. Example: memory_recall with key='Nadia' or key='meeting'.",
                "memories": [],
                "count": 0
            })
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output,
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    fn identity_add_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "description": "Which identity category to file the entry under. Standard categories: 'user' (personal facts about the user — relationships, family, preferences, hobbies; PREFER THIS for personal facts), 'personality' (voice, refusal style), 'rules' (hard constraints, 'never X'/'always Y'), 'knowledge' (general facts and recurring contexts — projects, places), 'team' (org chart, business identity). Custom user-created categories are also accepted.",
                },
                "body": {
                    "type": "string",
                    "description": "The identity statement, plain markdown. Keep it 1–3 sentences. Example: 'The user's girlfriend is Sara.'",
                },
                "applies_to": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Which agent profiles see this entry. Default ['Always'] makes it visible to every profile. Use a profile id (e.g. 'coder') to scope, or '!coder' to exclude one profile.",
                },
            },
            "required": ["category", "body"]
        })
    }

    async fn do_identity_add(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let Some(store) = self.identity.as_ref() else {
            return Err(AthenError::Other(
                "identity_add: identity store is not wired into this agent".to_string(),
            ));
        };

        let category = args
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AthenError::Other("identity_add: 'category' is required".to_string()))?
            .to_string();

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AthenError::Other("identity_add: 'body' is required".to_string()))?
            .to_string();

        let applies_to: Vec<ProfileTag> = match args.get("applies_to") {
            Some(serde_json::Value::Array(items)) if !items.is_empty() => items
                .iter()
                .filter_map(|v| v.as_str())
                .map(parse_applies_to_token)
                .collect(),
            _ => vec![ProfileTag::Always],
        };
        let applies_to = if applies_to.is_empty() {
            vec![ProfileTag::Always]
        } else {
            applies_to
        };

        let now = chrono::Utc::now();
        let entry = IdentityEntry {
            id: uuid::Uuid::new_v4(),
            category: category.clone(),
            body: body.clone(),
            applies_to: applies_to.clone(),
            pinned: false,
            proposed_by_agent: true,
            created_at: now,
            updated_at: now,
        };

        let start = Instant::now();
        store.upsert_entry(&entry).await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let applies_to_strs: Vec<String> = applies_to.iter().map(format_applies_to_tag).collect();

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": entry.id.to_string(),
                "category": category,
                "body": body,
                "applies_to": applies_to_strs,
                "proposed_by_agent": true,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    // ── load_skill ───────────────────────────────────────────────────

    fn load_skill_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The slug of the skill to load — the kebab-case identifier shown in the SKILLS listing in your system prefix (e.g. 'cold-email-outreach')."
                }
            },
            "required": ["slug"]
        })
    }

    async fn do_load_skill(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let Some(store) = self.skills.as_ref() else {
            return Err(AthenError::Other(
                "load_skill: skill store is not wired into this agent".to_string(),
            ));
        };

        let slug = args
            .get("slug")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AthenError::Other("load_skill: 'slug' is required".to_string()))?
            .to_string();

        // Idempotency: if this slug was already loaded earlier in this arc,
        // return a lightweight stub instead of the full body. The Mutex is
        // never contended (one agent per arc, sequential tool calls) so the
        // lock is always uncontested.
        {
            let mut cache = self
                .loaded_skills
                .lock()
                .expect("loaded_skills mutex poisoned");
            if cache.contains(&slug) {
                tracing::debug!(slug, "load_skill: returning already-loaded stub");
                return Ok(ToolResult {
                    success: true,
                    output: json!({
                        "ok": true,
                        "slug": slug,
                        "already_loaded": true,
                        "message": "Skill already loaded in this arc. Refer to the content returned earlier.",
                    }),
                    error: None,
                    execution_time_ms: 0,
                });
            }
            cache.insert(slug.clone());
        }

        let start = Instant::now();
        let body = store.load_body(&slug).await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({
                "slug": slug,
                "body": body,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    // ── athen_docs ────────────────────────────────────────────────

    fn athen_docs_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get"],
                    "description": "Use 'list' to browse all available guide topics, or 'get' to read a specific guide."
                },
                "topic": {
                    "type": "string",
                    "description": "The guide slug to read (required when action is 'get'). Get available slugs from the 'list' action."
                }
            },
            "required": ["action"]
        })
    }

    fn do_athen_docs(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let topic = args.get("topic").and_then(|v| v.as_str());

        let start = Instant::now();
        let body = crate::athen_docs::do_athen_docs(action, topic)?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "content": body }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    // ── http_request ────────────────────────────────────────────────

    fn http_request_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "Registered endpoint name (case-insensitive). The user manages these in Settings → Cloud APIs."
                },
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                    "description": "HTTP method. Defaults to GET. POST/PUT/PATCH/DELETE require user approval by default."
                },
                "path": {
                    "type": "string",
                    "description": "URL path joined to the endpoint's base_url. May start with '/' or not. May include query string; prefer the structured 'query' object for clarity."
                },
                "query": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Query-string parameters. Merged with any defaults configured on the endpoint."
                },
                "body": {
                    "description": "Request body (JSON). Only for POST/PUT/PATCH; ignored on GET/DELETE. Mutually exclusive with `files`/`form` — use those for multipart uploads."
                },
                "form": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Multipart text fields (string→string). Only meaningful when `files` is also set, or when the API expects multipart even with no file. If `files` is empty and `body` is unset, this is sent as application/x-www-form-urlencoded instead."
                },
                "files": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Multipart file fields: `{field_name: \"/abs/path/to/file\"}`. When set, the request becomes multipart/form-data. Use this for Whisper STT (`{file: \"/path/to/voice.ogg\"}`) and similar uploads. The path must already exist; pull voice notes/photos from the attachment_view tool first."
                },
                "save_to": {
                    "type": "string",
                    "description": "Absolute path to write the response BODY to (binary-safe). When set, the JSON response replaces `body` with `{saved_to, body_bytes, content_type}` — no base64, no UTF-8 lossy decode. Use this for ElevenLabs TTS (audio/mpeg) and any other binary download."
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Extra request headers. Merged on top of any defaults configured on the endpoint."
                }
            },
            "required": ["endpoint", "path"]
        })
    }

    async fn do_http_request(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let Some(store) = self.http_endpoints.as_ref() else {
            return Err(AthenError::Other(
                "http_request: registered-endpoint store is not wired into this agent".to_string(),
            ));
        };
        let Some(vault) = self.vault.as_ref() else {
            return Err(AthenError::Other(
                "http_request: vault is not wired into this agent".to_string(),
            ));
        };
        let Some(client) = self.http_client.as_ref() else {
            return Err(AthenError::Other(
                "http_request: HTTP client is not wired into this agent".to_string(),
            ));
        };
        let Some(rl) = self.http_rate_limiter.as_ref() else {
            return Err(AthenError::Other(
                "http_request: rate limiter is not wired into this agent".to_string(),
            ));
        };

        let endpoint_name = args
            .get("endpoint")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AthenError::Other("http_request: 'endpoint' is required".to_string()))?;

        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("http_request: 'path' is required".to_string()))?;

        let method_str = args
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let method = reqwest::Method::from_bytes(method_str.as_bytes()).map_err(|_| {
            AthenError::Other(format!("http_request: invalid method '{method_str}'"))
        })?;

        let endpoint = store.get_by_name(endpoint_name).await?.ok_or_else(|| {
            AthenError::Other(format!(
                "http_request: no registered endpoint named '{endpoint_name}'. \
                     Ask the user to add one in Settings → Cloud APIs, or list known endpoints."
            ))
        })?;

        if !endpoint.enabled {
            return Err(AthenError::Other(format!(
                "http_request: endpoint '{}' is disabled",
                endpoint.name
            )));
        }

        // Rate-limit pre-check: refuse before opening a socket if the
        // configured per-minute cap is exhausted.
        if let Some(rate) = endpoint.rate_limit {
            if let RateCheck::Exceeded {
                recent_calls,
                limit_per_minute,
                retry_in_secs,
            } = rl.check(endpoint.id, rate.requests_per_minute)
            {
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "rate_limited",
                        "endpoint": endpoint.name,
                        "limit_per_minute": limit_per_minute,
                        "recent_calls": recent_calls,
                        "retry_in_secs": retry_in_secs,
                    }),
                    error: Some(format!(
                        "Rate limit {limit_per_minute}/min exceeded ({recent_calls} calls in past 60s). \
                         Try again in {retry_in_secs}s."
                    )),
                    execution_time_ms: 0,
                });
            }
        }

        let url = join_base_and_path(&endpoint.base_url, path)?;

        // Start with the endpoint's default headers, then layer per-call
        // overrides on top so callers can squelch a default by re-setting.
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in &endpoint.default_headers {
            insert_header(&mut header_map, k, v)?;
        }
        if let Some(extra) = args.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    insert_header(&mut header_map, k, s)?;
                }
            }
        }

        // Build the full query: defaults + overrides.
        let mut query: Vec<(String, String)> = endpoint.default_query_params.clone();
        if let Some(extra) = args.get("query").and_then(|v| v.as_object()) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    query.push((k.clone(), s.to_string()));
                }
            }
        }

        // Resolve credentials from the vault and inject them into the
        // appropriate slot (header / query / basic-auth). On failure we
        // surface a precise message so the user can re-save the key.
        let scope = endpoint_scope(endpoint.id);
        let mut basic_auth: Option<(String, String)> = None;
        match &endpoint.auth_method {
            AuthMethod::None => {}
            AuthMethod::BearerToken => {
                let token = vault
                    .get(&scope, "token")
                    .await?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AthenError::Other(format!(
                            "http_request: endpoint '{}' has no bearer token in the vault. \
                             Open Settings → Cloud APIs to set it.",
                            endpoint.name
                        ))
                    })?;
                insert_header(&mut header_map, "Authorization", &format!("Bearer {token}"))?;
            }
            AuthMethod::Header { name } => {
                let value = vault
                    .get(&scope, "value")
                    .await?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AthenError::Other(format!(
                            "http_request: endpoint '{}' has no header credential in the vault.",
                            endpoint.name
                        ))
                    })?;
                insert_header(&mut header_map, name, &value)?;
            }
            AuthMethod::QueryParam { name } => {
                let value = vault
                    .get(&scope, "value")
                    .await?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AthenError::Other(format!(
                            "http_request: endpoint '{}' has no query-param credential in the vault.",
                            endpoint.name
                        ))
                    })?;
                query.push((name.clone(), value));
            }
            AuthMethod::BasicAuth { user } => {
                let pass = vault
                    .get(&scope, "password")
                    .await?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AthenError::Other(format!(
                            "http_request: endpoint '{}' has no basic-auth password in the vault.",
                            endpoint.name
                        ))
                    })?;
                basic_auth = Some((user.clone(), pass));
            }
        }

        let mut builder = client.request(method.clone(), url).headers(header_map);
        if !query.is_empty() {
            builder = builder.query(&query);
        }
        if let Some((u, p)) = basic_auth {
            builder = builder.basic_auth(u, Some(p));
        }
        // Body is only meaningful on write methods; reqwest will happily
        // attach a body to GET, but most APIs ignore or reject it. Three
        // mutually-exclusive shapes are supported:
        //   1. `files` (and optionally `form`)  → multipart/form-data
        //   2. `form` alone                     → application/x-www-form-urlencoded
        //   3. `body`                           → application/json
        // Caller is responsible for not mixing — if they do, multipart wins,
        // form is folded in, json body is silently ignored.
        if matches!(
            method,
            reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
        ) {
            let files_obj = args.get("files").and_then(|v| v.as_object());
            let form_obj = args.get("form").and_then(|v| v.as_object());

            if let Some(files) = files_obj.filter(|m| !m.is_empty()) {
                let mut multipart = reqwest::multipart::Form::new();
                if let Some(form) = form_obj {
                    for (k, v) in form {
                        if let Some(s) = v.as_str() {
                            multipart = multipart.text(k.clone(), s.to_string());
                        }
                    }
                }
                for (field, path_v) in files {
                    let Some(p) = path_v.as_str() else { continue };
                    let path = std::path::Path::new(p);
                    let bytes = match tokio::fs::read(path).await {
                        Ok(b) => b,
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: json!({
                                    "error": "file_read_failed",
                                    "endpoint": endpoint.name,
                                    "path": p,
                                    "detail": e.to_string(),
                                }),
                                error: Some(format!(
                                    "http_request: cannot read file '{p}' for field '{field}': {e}"
                                )),
                                execution_time_ms: 0,
                            });
                        }
                    };
                    let file_name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("upload")
                        .to_string();
                    let mime = guess_mime_for_path(path);
                    let part = reqwest::multipart::Part::bytes(bytes)
                        .file_name(file_name)
                        .mime_str(&mime)
                        .map_err(|e| {
                            AthenError::Other(format!(
                                "http_request: bad mime '{mime}' for field '{field}': {e}"
                            ))
                        })?;
                    multipart = multipart.part(field.clone(), part);
                }
                builder = builder.multipart(multipart);
            } else if let Some(form) = form_obj.filter(|m| !m.is_empty()) {
                let pairs: Vec<(String, String)> = form
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                builder = builder.form(&pairs);
            } else if let Some(body) = args.get("body") {
                if !body.is_null() {
                    builder = builder.json(body);
                }
            }
        }

        let save_to: Option<String> = args
            .get("save_to")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let started = Instant::now();
        let send_result = builder.send().await;
        // Record the call attempt for the rate limiter regardless of
        // outcome — quota reflects pressure on the upstream, including
        // failed attempts.
        rl.record(endpoint.id);

        let response = match send_result {
            Ok(r) => r,
            Err(e) => {
                let scrubbed = scrub_secret_text(&e.to_string(), &endpoint, vault, &scope).await;
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "request_failed",
                        "endpoint": endpoint.name,
                        "detail": scrubbed,
                    }),
                    error: Some(scrubbed),
                    execution_time_ms: started.elapsed().as_millis() as u64,
                });
            }
        };

        let status = response.status();
        let header_pairs: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Pull bytes first so we always have a length, then decode UTF-8
        // with replacement so a single bad sequence doesn't blank the
        // body. The previous `.text().unwrap_or_default()` was the source
        // of empty-body reports — a decompression error or invalid UTF-8
        // silently coerced to "".
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("Body read failed: {e}");
                let scrubbed = scrub_secret_text(&msg, &endpoint, vault, &scope).await;
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "body_read_failed",
                        "endpoint": endpoint.name,
                        "status": status.as_u16(),
                        "detail": scrubbed,
                    }),
                    error: Some(scrubbed),
                    execution_time_ms: started.elapsed().as_millis() as u64,
                });
            }
        };
        let body_bytes = bytes.len();

        // If the caller asked to save the body, write it and report the
        // path — never try to decode the bytes (could be audio, image, PDF).
        let body_value: serde_json::Value = if let Some(out_path) = save_to.as_deref() {
            let path = std::path::Path::new(out_path);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        return Ok(ToolResult {
                            success: false,
                            output: json!({
                                "error": "save_to_mkdir_failed",
                                "endpoint": endpoint.name,
                                "path": out_path,
                                "detail": e.to_string(),
                            }),
                            error: Some(format!(
                                "http_request: cannot create parent dir for '{out_path}': {e}"
                            )),
                            execution_time_ms: started.elapsed().as_millis() as u64,
                        });
                    }
                }
            }
            if let Err(e) = tokio::fs::write(path, &bytes).await {
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "save_to_write_failed",
                        "endpoint": endpoint.name,
                        "path": out_path,
                        "detail": e.to_string(),
                    }),
                    error: Some(format!(
                        "http_request: cannot write response to '{out_path}': {e}"
                    )),
                    execution_time_ms: started.elapsed().as_millis() as u64,
                });
            }
            json!({
                "saved_to": out_path,
                "body_bytes": body_bytes,
                "content_type": content_type,
            })
        } else {
            let raw_text = String::from_utf8_lossy(&bytes).into_owned();
            if content_type.contains("application/json") && !raw_text.is_empty() {
                match serde_json::from_str::<serde_json::Value>(&raw_text) {
                    Ok(v) => v,
                    Err(e) => json!({
                        "parse_error": e.to_string(),
                        "raw_text": raw_text.clone(),
                    }),
                }
            } else {
                json!({ "raw_text": raw_text })
            }
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;
        // Bump the persistent counter — best-effort; a counter blip is
        // never worth failing a successful HTTP call.
        if let Err(e) = store.record_call(endpoint.id).await {
            tracing::warn!(endpoint = %endpoint.name, error = %e, "record_call failed");
        }

        let success = status.is_success();
        Ok(ToolResult {
            success,
            output: json!({
                "endpoint": endpoint.name,
                "status": status.as_u16(),
                "headers": header_pairs,
                "body": body_value,
                "body_bytes": body_bytes,
                "content_type": content_type,
                "latency_ms": elapsed_ms,
            }),
            error: if success {
                None
            } else {
                Some(format!("HTTP {} from '{}'", status.as_u16(), endpoint.name))
            },
            execution_time_ms: elapsed_ms,
        })
    }

    // ── Setup tools (profile-gated) ──────────────────────────────

    async fn dispatch_setup_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let body = match name {
            "setup_email" => {
                let addr = args["address"].as_str().unwrap_or_default();
                let pw = args["password"].as_str().unwrap_or_default();
                let vault = self
                    .vault
                    .as_ref()
                    .ok_or_else(|| AthenError::Other("Vault not available for setup".into()))?;
                crate::setup_tools::do_setup_email(vault, addr, pw).await?
            }
            "setup_calendar_connect" => {
                let provider = args["provider"].as_str().unwrap_or_default();
                let username = args["username"].as_str().unwrap_or_default();
                let password = args["password"].as_str().unwrap_or_default();
                let base_url = args["base_url"].as_str();
                let vault = self
                    .vault
                    .as_ref()
                    .ok_or_else(|| AthenError::Other("Vault not available for setup".into()))?;
                let cstore = self.calendar_source_store.as_ref().ok_or_else(|| {
                    AthenError::Other("Calendar source store not available".into())
                })?;
                crate::setup_tools::do_setup_calendar_connect(
                    vault, &**cstore, provider, username, password, base_url,
                )
                .await?
            }
            "setup_calendar_configure" => {
                let source_id_str = args["source_id"].as_str().unwrap_or_default();
                let source_id = uuid::Uuid::parse_str(source_id_str)
                    .map_err(|e| AthenError::Other(format!("Invalid source_id UUID: {e}")))?;
                let selected: Vec<String> = args["selected_calendars"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let default_cal = args["default_calendar_id"].as_str();
                let cstore = self.calendar_source_store.as_ref().ok_or_else(|| {
                    AthenError::Other("Calendar source store not available".into())
                })?;
                crate::setup_tools::do_setup_calendar_configure(
                    &**cstore,
                    source_id,
                    &selected,
                    default_cal,
                )
                .await?
            }
            "setup_telegram" => {
                let token = args["bot_token"].as_str().unwrap_or_default();
                let vault = self
                    .vault
                    .as_ref()
                    .ok_or_else(|| AthenError::Other("Vault not available for setup".into()))?;
                crate::setup_tools::do_setup_telegram(vault, token).await?
            }
            "setup_owner_info" => {
                let field = args["field"].as_str().unwrap_or_default();
                let value = args["value"].as_str().unwrap_or_default();
                let cstore = self
                    .contacts
                    .as_ref()
                    .ok_or_else(|| AthenError::Other("Contact store not available".into()))?;
                crate::setup_tools::do_setup_owner_info(cstore, field, value).await?
            }
            "setup_search_key" => {
                let provider = args["provider"].as_str().unwrap_or_default();
                let key = args["key"].as_str().unwrap_or_default();
                let vault = self
                    .vault
                    .as_ref()
                    .ok_or_else(|| AthenError::Other("Vault not available for setup".into()))?;
                crate::setup_tools::do_setup_search_key(vault, provider, key).await?
            }
            _ => return Err(AthenError::ToolNotFound(name.to_string())),
        };
        let elapsed = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: serde_json::Value::String(body),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    fn setup_tool_definitions() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "setup_email".into(),
                description: "Set up email (IMAP + SMTP). Autodetects servers from the email address, tests, and saves. Use an app-specific password, not the main account password.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "address": { "type": "string", "description": "Email address" },
                        "password": { "type": "string", "description": "App-specific password" }
                    },
                    "required": ["address", "password"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "setup_calendar_connect".into(),
                description: "Connect a CalDAV calendar source. Tests the connection and returns the list of available calendars so you can ask which to sync.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "provider": { "type": "string", "enum": ["icloud", "google", "fastmail", "yandex", "nextcloud", "custom"] },
                        "username": { "type": "string", "description": "CalDAV username (usually your email)" },
                        "password": { "type": "string", "description": "App-specific password" },
                        "base_url": { "type": "string", "description": "Required only for nextcloud or custom" }
                    },
                    "required": ["provider", "username", "password"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "setup_calendar_configure".into(),
                description: "Finalize calendar setup: select which calendars to sync and optionally set a default for new events.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source_id": { "type": "string" },
                        "selected_calendars": { "type": "array", "items": { "type": "string" } },
                        "default_calendar_id": { "type": "string" }
                    },
                    "required": ["source_id", "selected_calendars"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "setup_telegram".into(),
                description: "Connect a Telegram bot for notifications. Provide the token from @BotFather.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "bot_token": { "type": "string" }
                    },
                    "required": ["bot_token"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "setup_owner_info".into(),
                description: "Set your personal info so Athen knows who you are. Set one field at a time.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "field": { "type": "string", "enum": ["name", "email", "phone", "telegram_user_id"] },
                        "value": { "type": "string" }
                    },
                    "required": ["field", "value"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "setup_search_key".into(),
                description: "Add a web search API key. Brave offers 2,000 free queries/month.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "provider": { "type": "string", "enum": ["brave", "tavily"] },
                        "key": { "type": "string" }
                    },
                    "required": ["provider", "key"]
                }),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            },
        ]
    }
}

fn join_base_and_path(base: &str, path: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(base)
        .map_err(|e| AthenError::Other(format!("http_request: invalid base_url '{base}': {e}")))?;
    parsed
        .join(path)
        .map_err(|e| AthenError::Other(format!("http_request: invalid path '{path}': {e}")))
}

/// Best-effort MIME guess from file extension. Used to set the Content-Type
/// of a multipart file part — Whisper STT and most upload APIs accept the
/// generic `application/octet-stream` fallback, but signal-rich types like
/// `audio/ogg` and `image/png` improve compatibility with stricter servers.
fn guess_mime_for_path(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "ogg" | "oga" | "opus" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "webm" => "audio/webm",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn insert_header(map: &mut reqwest::header::HeaderMap, name: &str, value: &str) -> Result<()> {
    let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| AthenError::Other(format!("http_request: bad header name '{name}': {e}")))?;
    let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
        AthenError::Other(format!("http_request: bad header value for '{name}': {e}"))
    })?;
    map.insert(header_name, header_value);
    Ok(())
}

/// Replace the stored credential string with `[REDACTED]` anywhere it
/// appears in `text`. Reads the credential lazily via the vault so this
/// path is only paid on error. Best-effort: a missing vault entry just
/// returns the original text.
async fn scrub_secret_text(
    text: &str,
    endpoint: &RegisteredEndpoint,
    vault: &Arc<dyn Vault>,
    scope: &str,
) -> String {
    let key_opt = endpoint.auth_method.vault_key();
    let Some(key) = key_opt else {
        return text.to_string();
    };
    match vault.get(scope, key).await {
        Ok(Some(secret)) if !secret.is_empty() => text.replace(&secret, "[REDACTED]"),
        _ => text.to_string(),
    }
}

fn parse_applies_to_token(tok: &str) -> ProfileTag {
    let trimmed = tok.trim();
    if trimmed.eq_ignore_ascii_case("always") {
        ProfileTag::Always
    } else if let Some(rest) = trimmed.strip_prefix('!') {
        ProfileTag::NotProfile(rest.trim().to_string())
    } else {
        ProfileTag::Profile(trimmed.to_string())
    }
}

fn format_applies_to_tag(tag: &ProfileTag) -> String {
    match tag {
        ProfileTag::Always => "Always".to_string(),
        ProfileTag::Profile(p) => p.clone(),
        ProfileTag::NotProfile(p) => format!("!{p}"),
    }
}

#[async_trait]
impl ToolRegistry for AppToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut tools = self.inner.list_tools().await?;

        // Override memory tool descriptions when persistent memory is wired.
        if self.memory.is_some() {
            for tool in &mut tools {
                if tool.name == "memory_store" {
                    tool.description = "IMPORTANT: When the user asks you to remember something, call this tool IMMEDIATELY. Stores information permanently across conversations. Use key as a short label and value as the full detail.".to_string();
                } else if tool.name == "memory_recall" {
                    tool.description = "Search your persistent memory. ALWAYS provide a key (search query) — e.g. key='Nadia' or key='meeting'. Returns semantically similar stored memories.".to_string();
                }
            }
        }

        if self.calendar.is_some() {
            tools.push(ToolDefinition {
                name: "calendar_list".to_string(),
                description: "List calendar events within a date range. Returns event details including title, time, location, and category.".to_string(),
                parameters: Self::calendar_list_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::Read,
            });
            tools.push(ToolDefinition {
                name: "calendar_create".to_string(),
                description: "Create a new calendar event with title, time, location, category, reminders, and recurrence.".to_string(),
                parameters: Self::calendar_create_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            });
            tools.push(ToolDefinition {
                name: "calendar_update".to_string(),
                description: "Update an existing calendar event. Only the provided fields are changed; others keep their current values. Requires the event ID.".to_string(),
                parameters: Self::calendar_update_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            });
            tools.push(ToolDefinition {
                name: "calendar_delete".to_string(),
                description: "Delete a calendar event by ID.".to_string(),
                parameters: Self::calendar_delete_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            });
        }

        if let Some(mcp) = &self.mcp {
            match mcp.list_tools().await {
                Ok(mcp_tools) => {
                    for t in mcp_tools {
                        let prefixed_name = format!("{}{MCP_TOOL_SEPARATOR}{}", t.mcp_id, t.name);
                        tools.push(ToolDefinition {
                            name: prefixed_name,
                            description: t
                                .description
                                .unwrap_or_else(|| format!("MCP tool from {}", t.mcp_id)),
                            parameters: t.input_schema,
                            // Generic backend; the dispatch in call_tool routes to MCP.
                            backend: ToolBackend::Shell {
                                command: String::new(),
                                native: false,
                            },
                            // Stamped by the registry from per-server default
                            // + per-tool overrides on `McpCatalogEntry`.
                            base_risk: t.base_risk,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list MCP tools: {e}");
                }
            }
        }

        if self.attachments.is_some() {
            tools.push(ToolDefinition {
                name: "read_attachment_full".to_string(),
                description: "Read the FULL extracted text of an attachment that was already announced in the attachment surfacing message at the top of the conversation. Use this when the inlined snippet was truncated (look for 'PDF text inlined (X of Y chars)' in the surfacing message) or when you need to re-read text after compaction. Returns the entire text, no character budget. Pass the exact UUID from the surfacing message.".to_string(),
                parameters: Self::read_attachment_full_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            });
            tools.push(ToolDefinition {
                name: "fetch_attachment".to_string(),
                description: "Re-confirm or attempt to redownload the bytes of an attachment whose local copy was purged after TTL. Returns the attachment's current state: 'already_local' if the bytes are still on disk (in which case prefer read_attachment_full), or 'purged'/'metadata_only' with the original source coordinates so you can decide how to proceed. Pass the exact UUID from the surfacing message.".to_string(),
                parameters: Self::fetch_attachment_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            });
        }

        if self.http_endpoints.is_some()
            && self.vault.is_some()
            && self.http_client.is_some()
            && self.http_rate_limiter.is_some()
        {
            // Bake the doc path into the description so the agent can
            // discover endpoints on demand without flooding the prompt.
            // Tier-2 chain: this description (in `tools/http.md`) →
            // index at `cloud_apis.md` → per-endpoint detail under
            // `cloud_apis/<name>.md`.
            let doc_pointer = match &self.cloud_apis_doc_path {
                Some(p) => format!(
                    " To discover what endpoints are registered (and the per-endpoint sample paths, auth specifics, free-tier limits, notes), `read` the index at `{}` first — it lists every endpoint with a one-liner and a path to its tiny per-endpoint detail file. Read ONE detail file when you actually need to call that endpoint, not all of them.",
                    p.display()
                ),
                None => String::new(),
            };
            tools.push(ToolDefinition {
                name: "http_request".to_string(),
                description: format!(
                    "Call a registered cloud HTTP API by name. Prefer bespoke tools (web_fetch, email_send, calendar_*, contacts_*) when one exists — they have richer schemas. Use http_request for less-common APIs the user has registered in Settings → Cloud APIs (Hunter, Brave Search, Open-Meteo, etc.). Returns {{endpoint, status, headers, body, body_bytes, content_type, latency_ms}}. Body is parsed JSON when Content-Type is application/json, else {{raw_text: '...'}}.{doc_pointer}"
                ),
                parameters: Self::http_request_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                // Per-call risk derivation by HTTP method + endpoint
                // risk_override is a follow-up. Today every http_request
                // call gets the same conservative WritePersist budget so
                // risk never under-counts.
                base_risk: BaseImpact::WritePersist,
            });
        }

        if self.skills.is_some() {
            tools.push(ToolDefinition {
                name: "load_skill".to_string(),
                description: "Load the full body of one of your registered skills. The static SKILLS section in your system prefix lists each skill's slug + one-line description; call this with the slug when a skill matches the task at hand (drafting an email → cold-email-outreach, formatting release notes → release-notes, etc.). Returns the skill's markdown body. Read-only; no side effects.".to_string(),
                parameters: Self::load_skill_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            });
        }

        tools.push(ToolDefinition {
            name: "athen_docs".to_string(),
            description: "Browse and read Athen's built-in guides for setup, configuration, and troubleshooting. Use action 'list' to see all available topics, or action 'get' with a topic slug to read a specific guide.".to_string(),
            parameters: Self::athen_docs_schema(),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: BaseImpact::Read,
        });

        if self.identity.is_some() {
            tools.push(ToolDefinition {
                name: "identity_add".to_string(),
                description: "Add an identity entry to the user's hand-maintained identity store. Use this when the user shares a personal fact ('I have a girlfriend named Sara'), a hard rule ('never email my boss without checking with me'), or recurring context worth remembering across every conversation. Prefer category='user' for personal facts. The entry is immediately live in every future agent prefix and is shown in Settings → Identity with an 'added by agent' chip the user can dismiss. No approval flow — be selective and only add facts the user clearly wants persisted.".to_string(),
                parameters: Self::identity_add_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            });
        }

        if self.contacts.is_some() {
            tools.push(ToolDefinition {
                name: "contacts_list".to_string(),
                description: "List all contacts with their names, identifiers, and trust levels."
                    .to_string(),
                parameters: Self::contacts_list_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            });
            tools.push(ToolDefinition {
                name: "contacts_search".to_string(),
                description: "Search contacts by name or identifier value (case-insensitive). Returns matching contacts.".to_string(),
                parameters: Self::contacts_search_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::Read,
            });
            tools.push(ToolDefinition {
                name: "contacts_create".to_string(),
                description: "Create a new contact with a name, optional identifiers (email, phone, Telegram, etc.), and optional trust level.".to_string(),
                parameters: Self::contacts_create_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            });
            tools.push(ToolDefinition {
                name: "contacts_update".to_string(),
                description: "Update an existing contact. Only provided fields are changed. If identifiers are provided, they REPLACE all existing identifiers. Requires the contact ID.".to_string(),
                parameters: Self::contacts_update_schema(),
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            });
            tools.push(ToolDefinition {
                name: "contacts_delete".to_string(),
                description: "Delete a contact by ID.".to_string(),
                parameters: Self::contacts_delete_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            });
        }

        // Setup tools: only visible under the "athen_setup" profile.
        if self.active_profile_id.as_deref() == Some("athen_setup") {
            tools.extend(Self::setup_tool_definitions());
        }

        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        // Path permission gate: any file-touching tool is intercepted here.
        // The gate may run the operation directly (for absolute paths
        // outside the sandbox) or hand back control via the closure
        // for paths inside the sandbox / built-in tools.
        if let Some(gate) = self.file_gate.clone() {
            if FileGate::is_file_tool(name) {
                // The gate handles `list_directory` directly via tokio::fs;
                // for `read`/`edit`/`write`/`grep` it calls back through
                // this closure so the inner ShellToolRegistry can apply
                // its stateful read-state (the hash check that prevents
                // blind overwrites).
                let name_owned = name.to_string();
                let inner_for_closure = self.inner.clone();
                let dispatch = move |rewritten: serde_json::Value| {
                    let name = name_owned.clone();
                    let inner_for_closure = inner_for_closure.clone();
                    Box::pin(async move { inner_for_closure.call_tool(&name, rewritten).await })
                        as futures::future::BoxFuture<'static, Result<ToolResult>>
                };
                return gate.handle(name, args, dispatch).await;
            }
        }

        // Route MCP-prefixed tool names (e.g. "slack__post_message") to the registry.
        if let Some(mcp) = &self.mcp {
            if let Some((mcp_id, tool)) = name.split_once(MCP_TOOL_SEPARATOR) {
                let started = Instant::now();
                let outcome = mcp.call_tool(mcp_id, tool, args).await?;
                let elapsed = started.elapsed().as_millis() as u64;
                return Ok(ToolResult {
                    success: outcome.success,
                    output: serde_json::json!({
                        "text": outcome.text,
                        "content": outcome.raw,
                    }),
                    error: if outcome.success {
                        None
                    } else {
                        Some(outcome.text.clone())
                    },
                    execution_time_ms: elapsed,
                });
            }
        }

        // Override memory tools with persistent memory when available.
        if let Some(ref memory) = self.memory {
            match name {
                "memory_store" => return self.do_persistent_memory_store(memory, &args).await,
                "memory_recall" => return self.do_persistent_memory_recall(memory, &args).await,
                _ => {}
            }
        }

        match name {
            "calendar_list" => self.do_calendar_list(&args).await,
            "calendar_create" => self.do_calendar_create(&args).await,
            "calendar_update" => self.do_calendar_update(&args).await,
            "calendar_delete" => self.do_calendar_delete(&args).await,
            "contacts_list" => self.do_contacts_list(&args).await,
            "contacts_search" => self.do_contacts_search(&args).await,
            "contacts_create" => self.do_contacts_create(&args).await,
            "contacts_update" => self.do_contacts_update(&args).await,
            "contacts_delete" => self.do_contacts_delete(&args).await,
            "read_attachment_full" => self.do_read_attachment_full(&args).await,
            "fetch_attachment" => self.do_fetch_attachment(&args).await,
            "identity_add" => self.do_identity_add(&args).await,
            "load_skill" => self.do_load_skill(&args).await,
            "athen_docs" => self.do_athen_docs(&args),
            "http_request" => self.do_http_request(&args).await,
            // Setup tools (gated by profile in list_tools)
            "setup_email" => self.dispatch_setup_tool(name, &args).await,
            "setup_calendar_connect" => self.dispatch_setup_tool(name, &args).await,
            "setup_calendar_configure" => self.dispatch_setup_tool(name, &args).await,
            "setup_telegram" => self.dispatch_setup_tool(name, &args).await,
            "setup_owner_info" => self.dispatch_setup_tool(name, &args).await,
            "setup_search_key" => self.dispatch_setup_tool(name, &args).await,
            _ => self.inner.call_tool(name, args).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_persistence::calendar::EventCreator;
    use athen_persistence::Database;
    use serde_json::json;

    /// Helper: create an in-memory DB + CalendarStore + AppToolRegistry.
    /// Returns the database (must be kept alive) and the registry.
    async fn setup_with_calendar() -> (Database, AppToolRegistry) {
        let db = Database::in_memory().await.unwrap();
        let calendar_store = db.calendar_store();
        let shell = ShellToolRegistry::new().await;
        let registry = AppToolRegistry::new(shell, Some(calendar_store), None, None);
        (db, registry)
    }

    /// Helper: create an AppToolRegistry without a calendar store.
    async fn setup_without_calendar() -> AppToolRegistry {
        let shell = ShellToolRegistry::new().await;
        AppToolRegistry::new(shell, None, None, None)
    }

    /// Helper: create an in-memory DB + ContactStore + AppToolRegistry.
    async fn setup_with_contacts() -> (Database, AppToolRegistry) {
        let db = Database::in_memory().await.unwrap();
        let contact_store = db.contact_store();
        let shell = ShellToolRegistry::new().await;
        let registry = AppToolRegistry::new(shell, None, Some(contact_store), None);
        (db, registry)
    }

    /// Helper: create an in-memory DB + both CalendarStore and ContactStore.
    async fn setup_with_all() -> (Database, AppToolRegistry) {
        let db = Database::in_memory().await.unwrap();
        let calendar_store = db.calendar_store();
        let contact_store = db.contact_store();
        let shell = ShellToolRegistry::new().await;
        let registry = AppToolRegistry::new(shell, Some(calendar_store), Some(contact_store), None);
        (db, registry)
    }

    /// Helper: extract the event ID from a calendar_create result.
    fn extract_id(result: &ToolResult) -> String {
        result.output["id"].as_str().unwrap().to_string()
    }

    // 1. list_tools_includes_calendar_tools
    #[tokio::test]
    async fn list_tools_includes_calendar_tools() {
        let (_db, registry) = setup_with_calendar().await;
        let tools = registry.list_tools().await.unwrap();
        assert_eq!(
            tools.len(),
            23,
            "Expected 18 shell + 4 calendar + 1 athen_docs tools"
        );

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"calendar_list"));
        assert!(names.contains(&"calendar_create"));
        assert!(names.contains(&"calendar_update"));
        assert!(names.contains(&"calendar_delete"));
    }

    // 2. list_tools_without_calendar_has_only_shell
    #[tokio::test]
    async fn list_tools_without_calendar_has_only_shell() {
        let registry = setup_without_calendar().await;
        let tools = registry.list_tools().await.unwrap();
        assert_eq!(
            tools.len(),
            19,
            "Expected 18 shell/web/memory/toolbox/email/telegram + 1 athen_docs tools when calendar is None"
        );

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&"calendar_list"));
        assert!(!names.contains(&"calendar_create"));
    }

    // 3. shell_tools_still_work
    #[tokio::test]
    async fn shell_tools_still_work() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool("shell_execute", json!({ "command": "echo hello" }))
            .await
            .unwrap();
        assert!(result.success);
        let output = result.output.to_string();
        assert!(
            output.contains("hello"),
            "Expected 'hello' in output: {output}"
        );
    }

    // 4. calendar_create_basic
    #[tokio::test]
    async fn calendar_create_basic() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Team Standup",
                    "start_time": "2026-04-05T14:00:00+00:00",
                    "end_time": "2026-04-05T14:30:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output["id"].as_str().is_some());
        assert_eq!(result.output["title"].as_str().unwrap(), "Team Standup");
    }

    // 5. calendar_list_empty
    #[tokio::test]
    async fn calendar_list_empty() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-01T00:00:00+00:00",
                    "end": "2026-04-30T23:59:59+00:00"
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 0);
        assert!(result.output["events"].as_array().unwrap().is_empty());
    }

    // 6. calendar_create_then_list
    #[tokio::test]
    async fn calendar_create_then_list() {
        let (_db, registry) = setup_with_calendar().await;

        // Create an event
        let create_result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Lunch",
                    "start_time": "2026-04-05T12:00:00+00:00",
                    "end_time": "2026-04-05T13:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert!(create_result.success);

        // List events in a range that includes the event
        let list_result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-05T00:00:00+00:00",
                    "end": "2026-04-06T00:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert!(list_result.success);
        assert_eq!(list_result.output["count"].as_u64().unwrap(), 1);

        let events = list_result.output["events"].as_array().unwrap();
        assert_eq!(events[0]["title"].as_str().unwrap(), "Lunch");
    }

    // 7. calendar_create_with_all_fields
    //
    // `color` and `recurrence` were intentionally dropped from the agent
    // schema (color is FE-derived from category; recurrence is the user's
    // job on create). They stay updatable via `calendar_update`. The test
    // asserts the kept fields round-trip and the dropped ones land None.
    #[tokio::test]
    async fn calendar_create_with_all_fields() {
        let (db, registry) = setup_with_calendar().await;
        let calendar_store = db.calendar_store();

        let result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Sprint Planning",
                    "start_time": "2026-04-07T09:00:00+00:00",
                    "end_time": "2026-04-07T10:30:00+00:00",
                    "description": "Plan Q2 sprint",
                    "location": "Conference Room A",
                    "category": "meeting",
                    "reminder_minutes": [15, 60]
                }),
            )
            .await
            .unwrap();
        assert!(result.success);

        let id = extract_id(&result);
        let event = calendar_store.get_event(&id).await.unwrap().unwrap();
        assert_eq!(event.title, "Sprint Planning");
        assert_eq!(event.description, Some("Plan Q2 sprint".to_string()));
        assert_eq!(event.location, Some("Conference Room A".to_string()));
        assert_eq!(event.category, Some("meeting".to_string()));
        assert_eq!(event.color, None);
        assert_eq!(event.reminder_minutes, vec![15, 60]);
        assert_eq!(event.recurrence, None);
    }

    // 8. calendar_update_partial
    #[tokio::test]
    async fn calendar_update_partial() {
        let (db, registry) = setup_with_calendar().await;
        let calendar_store = db.calendar_store();

        // Create with a location
        let create_result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Original Title",
                    "start_time": "2026-04-05T14:00:00+00:00",
                    "end_time": "2026-04-05T15:00:00+00:00",
                    "location": "Room A"
                }),
            )
            .await
            .unwrap();
        let id = extract_id(&create_result);

        // Update only the title
        let update_result = registry
            .call_tool(
                "calendar_update",
                json!({
                    "id": id,
                    "title": "Updated Title"
                }),
            )
            .await
            .unwrap();
        assert!(update_result.success);
        assert_eq!(
            update_result.output["title"].as_str().unwrap(),
            "Updated Title"
        );

        // Verify location is still "Room A"
        let event = calendar_store.get_event(&id).await.unwrap().unwrap();
        assert_eq!(event.title, "Updated Title");
        assert_eq!(event.location, Some("Room A".to_string()));
    }

    // 9. calendar_update_nonexistent
    #[tokio::test]
    async fn calendar_update_nonexistent() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool(
                "calendar_update",
                json!({
                    "id": "nonexistent-id-12345",
                    "title": "Ghost"
                }),
            )
            .await;
        assert!(
            result.is_err(),
            "Updating a nonexistent event should return an error"
        );
    }

    // 10. calendar_delete_basic
    #[tokio::test]
    async fn calendar_delete_basic() {
        let (_db, registry) = setup_with_calendar().await;

        // Create
        let create_result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Ephemeral",
                    "start_time": "2026-04-05T16:00:00+00:00",
                    "end_time": "2026-04-05T17:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        let id = extract_id(&create_result);

        // Delete
        let delete_result = registry
            .call_tool("calendar_delete", json!({ "id": id }))
            .await
            .unwrap();
        assert!(delete_result.success);

        // Verify list is empty
        let list_result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-05T00:00:00+00:00",
                    "end": "2026-04-06T00:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert_eq!(list_result.output["count"].as_u64().unwrap(), 0);
    }

    // 11. calendar_list_date_range
    #[tokio::test]
    async fn calendar_list_date_range() {
        let (_db, registry) = setup_with_calendar().await;

        // Event on April 5
        registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "April 5 event",
                    "start_time": "2026-04-05T10:00:00+00:00",
                    "end_time": "2026-04-05T11:00:00+00:00"
                }),
            )
            .await
            .unwrap();

        // Event on April 10
        registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "April 10 event",
                    "start_time": "2026-04-10T10:00:00+00:00",
                    "end_time": "2026-04-10T11:00:00+00:00"
                }),
            )
            .await
            .unwrap();

        // Event on April 20
        registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "April 20 event",
                    "start_time": "2026-04-20T10:00:00+00:00",
                    "end_time": "2026-04-20T11:00:00+00:00"
                }),
            )
            .await
            .unwrap();

        // Query April 4-11: should get 2 events
        let result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-04T00:00:00+00:00",
                    "end": "2026-04-11T00:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert_eq!(result.output["count"].as_u64().unwrap(), 2);

        // Query April 15-25: should get 1 event
        let result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-15T00:00:00+00:00",
                    "end": "2026-04-25T00:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);
        assert_eq!(
            result.output["events"][0]["title"].as_str().unwrap(),
            "April 20 event"
        );
    }

    // 12. calendar_create_agent_creator
    #[tokio::test]
    async fn calendar_create_agent_creator() {
        let (db, registry) = setup_with_calendar().await;
        let calendar_store = db.calendar_store();

        let result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "title": "Agent-made event",
                    "start_time": "2026-04-05T08:00:00+00:00",
                    "end_time": "2026-04-05T09:00:00+00:00"
                }),
            )
            .await
            .unwrap();
        let id = extract_id(&result);

        let event = calendar_store.get_event(&id).await.unwrap().unwrap();
        assert_eq!(event.created_by, EventCreator::Agent);
    }

    // 13. calendar_create_missing_title
    #[tokio::test]
    async fn calendar_create_missing_title() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool(
                "calendar_create",
                json!({
                    "start_time": "2026-04-05T14:00:00+00:00",
                    "end_time": "2026-04-05T15:00:00+00:00"
                }),
            )
            .await;
        assert!(result.is_err(), "Missing title should produce an error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("title"),
            "Error should mention 'title': {err_msg}"
        );
    }

    // 14. calendar_no_store_returns_error
    #[tokio::test]
    async fn calendar_no_store_returns_error() {
        let registry = setup_without_calendar().await;
        let result = registry
            .call_tool(
                "calendar_list",
                json!({
                    "start": "2026-04-01T00:00:00+00:00",
                    "end": "2026-04-30T00:00:00+00:00"
                }),
            )
            .await;
        assert!(
            result.is_err(),
            "Calendar tool should fail when store is None"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not available"),
            "Error should mention calendar not available: {err_msg}"
        );
    }

    // 15. unknown_tool_delegates_to_inner
    #[tokio::test]
    async fn unknown_tool_delegates_to_inner() {
        let (_db, registry) = setup_with_calendar().await;
        let result = registry
            .call_tool("totally_fake_tool", json!({ "arg": "value" }))
            .await;
        assert!(result.is_err(), "Unknown tool should return an error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("not found")
                || err_msg.to_lowercase().contains("unknown"),
            "Error should indicate tool not found: {err_msg}"
        );
    }

    // ── Contact tool tests ──────────────────────────────────────────

    // 16. list_tools_includes_contact_tools
    #[tokio::test]
    async fn list_tools_includes_contact_tools() {
        let (_db, registry) = setup_with_contacts().await;
        let tools = registry.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"contacts_list"));
        assert!(names.contains(&"contacts_search"));
        assert!(names.contains(&"contacts_create"));
        assert!(names.contains(&"contacts_update"));
        assert!(names.contains(&"contacts_delete"));
        // 18 shell + 5 contact = 23
        assert_eq!(tools.len(), 24);
    }

    // 17. list_tools_with_all_stores
    #[tokio::test]
    async fn list_tools_with_all_stores() {
        let (_db, registry) = setup_with_all().await;
        let tools = registry.list_tools().await.unwrap();
        // 18 shell + 4 calendar + 5 contact = 27
        assert_eq!(tools.len(), 28);
    }

    // 18. contacts_create_basic
    #[tokio::test]
    async fn contacts_create_basic() {
        let (_db, registry) = setup_with_contacts().await;
        let result = registry
            .call_tool(
                "contacts_create",
                json!({
                    "name": "Alice",
                    "identifiers": [
                        { "value": "alice@example.com", "kind": "Email" }
                    ]
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output["id"].as_str().is_some());
        assert_eq!(result.output["name"].as_str().unwrap(), "Alice");
    }

    // 19. contacts_list_after_create
    #[tokio::test]
    async fn contacts_list_after_create() {
        let (_db, registry) = setup_with_contacts().await;

        // Create two contacts
        registry
            .call_tool("contacts_create", json!({ "name": "Alice" }))
            .await
            .unwrap();
        registry
            .call_tool("contacts_create", json!({ "name": "Bob" }))
            .await
            .unwrap();

        let result = registry
            .call_tool("contacts_list", json!({}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 2);
    }

    // 20. contacts_search_by_name
    #[tokio::test]
    async fn contacts_search_by_name() {
        let (_db, registry) = setup_with_contacts().await;

        registry
            .call_tool("contacts_create", json!({ "name": "Alice Smith" }))
            .await
            .unwrap();
        registry
            .call_tool("contacts_create", json!({ "name": "Bob Jones" }))
            .await
            .unwrap();

        let result = registry
            .call_tool("contacts_search", json!({ "query": "alice" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);
        assert_eq!(
            result.output["contacts"][0]["name"].as_str().unwrap(),
            "Alice Smith"
        );
    }

    // 21. contacts_search_by_identifier
    #[tokio::test]
    async fn contacts_search_by_identifier() {
        let (_db, registry) = setup_with_contacts().await;

        registry
            .call_tool(
                "contacts_create",
                json!({
                    "name": "Alice",
                    "identifiers": [{ "value": "alice@test.com", "kind": "Email" }]
                }),
            )
            .await
            .unwrap();

        let result = registry
            .call_tool("contacts_search", json!({ "query": "alice@test" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);
    }

    // 21b. contacts_search multi-token: tokens must match across the
    // name + identifier haystack. "Alex Garcia" should resolve a contact
    // named "Alex" whose email is "alex.garcia@x.com".
    #[tokio::test]
    async fn contacts_search_multi_token_across_name_and_identifier() {
        let (_db, registry) = setup_with_contacts().await;

        registry
            .call_tool(
                "contacts_create",
                json!({
                    "name": "Alex",
                    "identifiers": [{ "value": "alex.garcia@x.com", "kind": "Email" }]
                }),
            )
            .await
            .unwrap();
        registry
            .call_tool("contacts_create", json!({ "name": "Bob" }))
            .await
            .unwrap();

        // Both tokens present (one in name, one in identifier).
        let result = registry
            .call_tool("contacts_search", json!({ "query": "Alex Garcia" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);
        assert_eq!(
            result.output["contacts"][0]["name"].as_str().unwrap(),
            "Alex"
        );

        // Single token in identifier only.
        let result = registry
            .call_tool("contacts_search", json!({ "query": "Garcia" }))
            .await
            .unwrap();
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);

        // No-match query.
        let result = registry
            .call_tool("contacts_search", json!({ "query": "Carol" }))
            .await
            .unwrap();
        assert_eq!(result.output["count"].as_u64().unwrap(), 0);

        // Empty query returns all contacts.
        let result = registry
            .call_tool("contacts_search", json!({ "query": "" }))
            .await
            .unwrap();
        assert_eq!(result.output["count"].as_u64().unwrap(), 2);
    }

    // 21c. contacts_search substring within token: "Alex" against name
    // "Alexandra" still matches (token is a substring, not a full word).
    #[tokio::test]
    async fn contacts_search_token_is_substring() {
        let (_db, registry) = setup_with_contacts().await;

        registry
            .call_tool("contacts_create", json!({ "name": "Alexandra" }))
            .await
            .unwrap();

        let result = registry
            .call_tool("contacts_search", json!({ "query": "Alex" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"].as_u64().unwrap(), 1);
    }

    // 22. contacts_update_name
    #[tokio::test]
    async fn contacts_update_name() {
        let (_db, registry) = setup_with_contacts().await;

        let create_result = registry
            .call_tool(
                "contacts_create",
                json!({
                    "name": "Alice",
                    "identifiers": [{ "value": "alice@test.com", "kind": "Email" }]
                }),
            )
            .await
            .unwrap();
        let id = create_result.output["id"].as_str().unwrap().to_string();

        let update_result = registry
            .call_tool(
                "contacts_update",
                json!({ "id": id, "name": "Alice Updated" }),
            )
            .await
            .unwrap();
        assert!(update_result.success);
        assert_eq!(
            update_result.output["name"].as_str().unwrap(),
            "Alice Updated"
        );

        // Verify identifier was preserved
        let list_result = registry
            .call_tool("contacts_search", json!({ "query": "alice@test" }))
            .await
            .unwrap();
        assert_eq!(list_result.output["count"].as_u64().unwrap(), 1);
        assert_eq!(
            list_result.output["contacts"][0]["name"].as_str().unwrap(),
            "Alice Updated"
        );
    }

    // 23. contacts_delete_basic
    #[tokio::test]
    async fn contacts_delete_basic() {
        let (_db, registry) = setup_with_contacts().await;

        let create_result = registry
            .call_tool("contacts_create", json!({ "name": "Ephemeral" }))
            .await
            .unwrap();
        let id = create_result.output["id"].as_str().unwrap().to_string();

        let delete_result = registry
            .call_tool("contacts_delete", json!({ "id": id }))
            .await
            .unwrap();
        assert!(delete_result.success);

        // Verify empty
        let list_result = registry
            .call_tool("contacts_list", json!({}))
            .await
            .unwrap();
        assert_eq!(list_result.output["count"].as_u64().unwrap(), 0);
    }

    // 24. contacts_update_nonexistent
    #[tokio::test]
    async fn contacts_update_nonexistent() {
        let (_db, registry) = setup_with_contacts().await;
        let result = registry
            .call_tool(
                "contacts_update",
                json!({ "id": "00000000-0000-0000-0000-000000000000", "name": "Ghost" }),
            )
            .await;
        assert!(result.is_err());
    }

    // 25. contacts_no_store_returns_error
    #[tokio::test]
    async fn contacts_no_store_returns_error() {
        let registry = setup_without_calendar().await;
        let result = registry.call_tool("contacts_list", json!({})).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not available"));
    }

    // 26. contacts_create_with_trust_level
    #[tokio::test]
    async fn contacts_create_with_trust_level() {
        let (db, registry) = setup_with_contacts().await;
        let contact_store = db.contact_store();

        let result = registry
            .call_tool(
                "contacts_create",
                json!({
                    "name": "Trusted Alice",
                    "trust_level": "Trusted"
                }),
            )
            .await
            .unwrap();
        assert!(result.success);

        let id_str = result.output["id"].as_str().unwrap();
        let id = uuid::Uuid::parse_str(id_str).unwrap();
        let loaded = contact_store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.trust_level, TrustLevel::Trusted);
    }

    // ── Identity tool tests ─────────────────────────────────────────

    mod identity_tools {
        use super::*;
        use athen_core::traits::identity::IdentityStore as _IdStore;

        async fn setup_with_identity() -> (Database, AppToolRegistry) {
            let db = Database::in_memory().await.unwrap();
            let store = Arc::new(db.identity_store());
            store.init_schema().await.unwrap();
            store.seed_categories_if_empty().await.unwrap();
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None).with_identity(store);
            (db, registry)
        }

        #[tokio::test]
        async fn list_tools_includes_identity_add_when_store_present() {
            let (_db, registry) = setup_with_identity().await;
            let tools = registry.list_tools().await.unwrap();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"identity_add"));
        }

        #[tokio::test]
        async fn list_tools_omits_identity_add_without_store() {
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None);
            let tools = registry.list_tools().await.unwrap();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(!names.contains(&"identity_add"));
        }

        #[tokio::test]
        async fn identity_add_persists_with_proposed_by_agent_true() {
            let (db, registry) = setup_with_identity().await;
            let store = Arc::new(db.identity_store());

            let result = registry
                .call_tool(
                    "identity_add",
                    json!({
                        "category": "user",
                        "body": "The user's girlfriend is Sara.",
                    }),
                )
                .await
                .unwrap();
            assert!(result.success);
            assert_eq!(result.output["category"].as_str().unwrap(), "user");
            assert!(result.output["proposed_by_agent"].as_bool().unwrap());

            let id_str = result.output["id"].as_str().unwrap();
            let id = uuid::Uuid::parse_str(id_str).unwrap();
            let loaded = store.get_entry(id).await.unwrap().unwrap();
            assert_eq!(loaded.body, "The user's girlfriend is Sara.");
            assert!(loaded.proposed_by_agent);
            assert!(matches!(loaded.applies_to.as_slice(), [ProfileTag::Always]));
        }

        #[tokio::test]
        async fn identity_add_resolves_applies_to_strings() {
            let (_db, registry) = setup_with_identity().await;
            let result = registry
                .call_tool(
                    "identity_add",
                    json!({
                        "category": "rules",
                        "body": "Never email legal@ on Fridays.",
                        "applies_to": ["coder", "!outreach"],
                    }),
                )
                .await
                .unwrap();
            assert!(result.success);
            let scopes = result.output["applies_to"].as_array().unwrap();
            let strs: Vec<&str> = scopes.iter().map(|v| v.as_str().unwrap()).collect();
            assert!(strs.contains(&"coder"));
            assert!(strs.contains(&"!outreach"));
        }

        #[tokio::test]
        async fn identity_add_rejects_empty_body() {
            let (_db, registry) = setup_with_identity().await;
            let err = registry
                .call_tool("identity_add", json!({"category": "user", "body": "  "}))
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("'body' is required"));
        }

        #[tokio::test]
        async fn identity_add_without_store_errors() {
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None);
            let err = registry
                .call_tool("identity_add", json!({"category": "user", "body": "hi"}))
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("identity store is not wired"));
        }
    }

    // ── Attachment tool tests ───────────────────────────────────────

    mod attachment_tools {
        use super::*;
        use athen_core::event::{Attachment, AttachmentSource};
        use std::io::Write;

        async fn setup_with_attachments() -> (Database, AppToolRegistry) {
            let db = Database::in_memory().await.unwrap();
            let store = db.attachment_store();
            store.init_schema().await.unwrap();
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None).with_attachments(store);
            (db, registry)
        }

        fn write_temp(name: &str, contents: &[u8]) -> std::path::PathBuf {
            let mut p = std::env::temp_dir();
            p.push(format!("athen-test-{}-{}", uuid::Uuid::new_v4(), name));
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(contents).unwrap();
            p
        }

        #[tokio::test]
        async fn list_tools_includes_attachment_tools_when_store_present() {
            let (_db, registry) = setup_with_attachments().await;
            let tools = registry.list_tools().await.unwrap();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"read_attachment_full"));
            assert!(names.contains(&"fetch_attachment"));
        }

        #[tokio::test]
        async fn list_tools_omits_attachment_tools_without_store() {
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None);
            let tools = registry.list_tools().await.unwrap();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(!names.contains(&"read_attachment_full"));
            assert!(!names.contains(&"fetch_attachment"));
        }

        #[tokio::test]
        async fn read_attachment_full_returns_sidecar_text() {
            let (db, registry) = setup_with_attachments().await;
            let store = db.attachment_store();
            let event_id = uuid::Uuid::new_v4();
            let pdf_path = write_temp("doc.pdf", b"%PDF-1.4\n");
            let sidecar = write_temp(
                "doc.pdf.txt",
                b"Full extracted CV text. Languages: English C1.",
            );
            let mut att = Attachment::new(
                "doc.pdf",
                "application/pdf",
                10,
                Some(pdf_path.clone()),
                None,
            );
            att.extracted_text_path = Some(sidecar.clone());
            store.insert(event_id, &att).await.unwrap();

            let result = registry
                .call_tool("read_attachment_full", json!({ "id": att.id.to_string() }))
                .await
                .unwrap();
            assert!(result.success);
            assert_eq!(result.output["source"].as_str().unwrap(), "sidecar");
            assert!(result.output["text"]
                .as_str()
                .unwrap()
                .contains("English C1"));
        }

        #[tokio::test]
        async fn read_attachment_full_text_mime_reads_local_bytes() {
            let (db, registry) = setup_with_attachments().await;
            let store = db.attachment_store();
            let event_id = uuid::Uuid::new_v4();
            let txt_path = write_temp("note.txt", b"Plain note body.");
            let att = Attachment::new("note.txt", "text/plain", 16, Some(txt_path.clone()), None);
            store.insert(event_id, &att).await.unwrap();

            let result = registry
                .call_tool("read_attachment_full", json!({ "id": att.id.to_string() }))
                .await
                .unwrap();
            assert!(result.success);
            assert_eq!(result.output["source"].as_str().unwrap(), "local_text");
            assert_eq!(result.output["text"].as_str().unwrap(), "Plain note body.");
        }

        #[tokio::test]
        async fn read_attachment_full_unknown_id_errors() {
            let (_db, registry) = setup_with_attachments().await;
            let bogus = uuid::Uuid::new_v4();
            let err = registry
                .call_tool("read_attachment_full", json!({ "id": bogus.to_string() }))
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("not found"));
        }

        #[tokio::test]
        async fn read_attachment_full_invalid_uuid_errors() {
            let (_db, registry) = setup_with_attachments().await;
            let err = registry
                .call_tool("read_attachment_full", json!({ "id": "not-a-uuid" }))
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("invalid attachment id"));
        }

        #[tokio::test]
        async fn read_attachment_full_purged_pdf_no_sidecar_errors_clearly() {
            let (db, registry) = setup_with_attachments().await;
            let store = db.attachment_store();
            let event_id = uuid::Uuid::new_v4();
            let att = Attachment::new(
                "purged.pdf",
                "application/pdf",
                100,
                None,
                Some(AttachmentSource::Telegram {
                    chat_id: 1,
                    message_id: 2,
                    file_id: "abc".into(),
                }),
            );
            store.insert(event_id, &att).await.unwrap();

            let err = registry
                .call_tool("read_attachment_full", json!({ "id": att.id.to_string() }))
                .await
                .unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("only metadata"));
        }

        #[tokio::test]
        async fn fetch_attachment_local_returns_already_local() {
            let (db, registry) = setup_with_attachments().await;
            let store = db.attachment_store();
            let event_id = uuid::Uuid::new_v4();
            let pdf_path = write_temp("doc.pdf", b"%PDF-1.4\n");
            let att = Attachment::new(
                "doc.pdf",
                "application/pdf",
                10,
                Some(pdf_path.clone()),
                None,
            );
            store.insert(event_id, &att).await.unwrap();

            let result = registry
                .call_tool("fetch_attachment", json!({ "id": att.id.to_string() }))
                .await
                .unwrap();
            assert!(result.success);
            assert_eq!(result.output["status"].as_str().unwrap(), "already_local");
        }

        #[tokio::test]
        async fn fetch_attachment_purged_returns_source_metadata() {
            let (db, registry) = setup_with_attachments().await;
            let store = db.attachment_store();
            let event_id = uuid::Uuid::new_v4();
            let mut att = Attachment::new(
                "doc.pdf",
                "application/pdf",
                10,
                None,
                Some(AttachmentSource::Email {
                    account_id: "acct".into(),
                    mailbox: "INBOX".into(),
                    uid_validity: 1,
                    uid: 42,
                    part_path: "2.1".into(),
                }),
            );
            att.purged_at = Some(chrono::Utc::now());
            store.insert(event_id, &att).await.unwrap();

            let result = registry
                .call_tool("fetch_attachment", json!({ "id": att.id.to_string() }))
                .await
                .unwrap();
            assert!(!result.success);
            assert_eq!(result.output["status"].as_str().unwrap(), "purged");
            assert_eq!(result.output["source"]["kind"].as_str().unwrap(), "email");
            assert_eq!(result.output["source"]["uid"].as_u64().unwrap(), 42);
        }

        #[tokio::test]
        async fn read_attachment_full_without_store_errors() {
            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None);
            let err = registry
                .call_tool(
                    "read_attachment_full",
                    json!({ "id": uuid::Uuid::new_v4().to_string() }),
                )
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("Attachment store not available"));
        }
    }

    // ── MCP risk-stamping tests ─────────────────────────────────────
    //
    // Verifies the per-tool risk plumbed by the registry flows through
    // `AppToolRegistry::list_tools()` into each `ToolDefinition.base_risk`.

    mod mcp_risk {
        use super::*;
        use athen_core::traits::mcp::{McpCallOutcome, McpClient, McpToolDescriptor};
        use std::sync::Arc;

        /// Hand-crafted McpClient that hands back a fixed list of
        /// descriptors with mixed risk levels. The risk values we set here
        /// are what we expect to see on the corresponding ToolDefinitions
        /// after `list_tools()`.
        struct FixedRiskMcp;

        #[async_trait::async_trait]
        impl McpClient for FixedRiskMcp {
            async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
                Ok(vec![
                    McpToolDescriptor {
                        mcp_id: "fs".into(),
                        name: "read_file".into(),
                        description: Some("Read a file".into()),
                        input_schema: serde_json::json!({}),
                        base_risk: BaseImpact::Read,
                    },
                    McpToolDescriptor {
                        mcp_id: "fs".into(),
                        name: "delete_file".into(),
                        description: Some("Delete a file".into()),
                        input_schema: serde_json::json!({}),
                        base_risk: BaseImpact::System,
                    },
                    McpToolDescriptor {
                        mcp_id: "fs".into(),
                        name: "write_file".into(),
                        description: None,
                        input_schema: serde_json::json!({}),
                        base_risk: BaseImpact::WritePersist,
                    },
                ])
            }

            async fn call_tool(
                &self,
                _mcp_id: &str,
                _tool: &str,
                _args: serde_json::Value,
            ) -> Result<McpCallOutcome> {
                Ok(McpCallOutcome {
                    success: true,
                    text: String::new(),
                    raw: serde_json::json!([]),
                })
            }
        }

        #[tokio::test]
        async fn list_tools_stamps_per_tool_risk_from_descriptor() {
            let shell = ShellToolRegistry::new().await;
            let registry =
                AppToolRegistry::new(shell, None, None, None).with_mcp(Arc::new(FixedRiskMcp));
            let tools = registry.list_tools().await.unwrap();

            // MCP tools get a prefixed name (`<mcp_id>__<tool>`).
            let by_name: std::collections::HashMap<&str, BaseImpact> = tools
                .iter()
                .map(|t| (t.name.as_str(), t.base_risk))
                .collect();
            assert_eq!(
                by_name.get("fs__read_file").copied(),
                Some(BaseImpact::Read)
            );
            assert_eq!(
                by_name.get("fs__delete_file").copied(),
                Some(BaseImpact::System)
            );
            assert_eq!(
                by_name.get("fs__write_file").copied(),
                Some(BaseImpact::WritePersist)
            );
        }
    }

    // ── load_skill idempotency tests ─────────────────────────────────

    mod load_skill_idempotency {
        use super::*;
        use athen_core::identity::ProfileTag;
        use athen_core::skill::SkillFrontmatter;
        use athen_core::traits::skill::SkillStore;
        use athen_persistence::skills::SqliteSkillStore;
        use rusqlite::Connection;
        use std::sync::Arc;
        use tempfile::TempDir;
        use tokio::sync::Mutex as TokioMutex;

        async fn setup_with_skill(slug: &str, body: &str) -> (AppToolRegistry, TempDir) {
            let dir = TempDir::new().unwrap();
            let conn = Connection::open_in_memory().unwrap();
            let conn = Arc::new(TokioMutex::new(conn));
            athen_persistence::skills::init_schema(&conn).await.unwrap();
            let store = Arc::new(SqliteSkillStore::new(conn, dir.path().to_path_buf()));

            let front = SkillFrontmatter {
                name: slug.to_string(),
                description: "test skill".to_string(),
                applies_to: vec![ProfileTag::Always],
            };
            store.upsert(slug, &front, body).await.unwrap();

            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None).with_skills(store);
            (registry, dir)
        }

        /// First call returns the full body; second call with the same slug
        /// returns the "already_loaded" stub without hitting the store again.
        #[tokio::test]
        async fn second_call_returns_already_loaded_stub() {
            let (registry, _dir) = setup_with_skill("cold-email", "## Steps\n1. Write email.\n").await;

            // First call — must return the full body.
            let first = registry
                .call_tool("load_skill", json!({ "slug": "cold-email" }))
                .await
                .unwrap();
            assert!(first.success);
            assert!(first.output.get("already_loaded").is_none(),
                "First call must NOT have already_loaded key");
            assert!(
                first.output["body"].as_str().unwrap().contains("Write email"),
                "First call must return the full body"
            );

            // Second call — must return the stub.
            let second = registry
                .call_tool("load_skill", json!({ "slug": "cold-email" }))
                .await
                .unwrap();
            assert!(second.success);
            assert_eq!(
                second.output["already_loaded"].as_bool(),
                Some(true),
                "Second call must have already_loaded=true"
            );
            assert!(second.output.get("body").is_none(),
                "Second call must NOT return the full body");
        }

        /// Different slugs on the same registry are each loaded only once.
        #[tokio::test]
        async fn different_slugs_each_load_once() {
            let dir = TempDir::new().unwrap();
            let conn = Connection::open_in_memory().unwrap();
            let conn = Arc::new(TokioMutex::new(conn));
            athen_persistence::skills::init_schema(&conn).await.unwrap();
            let store = Arc::new(SqliteSkillStore::new(conn, dir.path().to_path_buf()));

            let front_a = SkillFrontmatter {
                name: "alpha".to_string(),
                description: "a".to_string(),
                applies_to: vec![ProfileTag::Always],
            };
            let front_b = SkillFrontmatter {
                name: "beta".to_string(),
                description: "b".to_string(),
                applies_to: vec![ProfileTag::Always],
            };
            store.upsert("alpha", &front_a, "alpha body").await.unwrap();
            store.upsert("beta", &front_b, "beta body").await.unwrap();

            let shell = ShellToolRegistry::new().await;
            let registry = AppToolRegistry::new(shell, None, None, None).with_skills(store);

            // Load alpha — first time.
            let r = registry
                .call_tool("load_skill", json!({ "slug": "alpha" }))
                .await
                .unwrap();
            assert!(r.output.get("already_loaded").is_none());
            assert!(r.output["body"].as_str().unwrap().contains("alpha body"));

            // Load beta — first time (different slug).
            let r = registry
                .call_tool("load_skill", json!({ "slug": "beta" }))
                .await
                .unwrap();
            assert!(r.output.get("already_loaded").is_none());
            assert!(r.output["body"].as_str().unwrap().contains("beta body"));

            // Load alpha again — should be stub.
            let r = registry
                .call_tool("load_skill", json!({ "slug": "alpha" }))
                .await
                .unwrap();
            assert_eq!(r.output["already_loaded"].as_bool(), Some(true));
        }
    }
}
