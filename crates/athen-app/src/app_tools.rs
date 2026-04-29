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
use athen_core::risk::BaseImpact;
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::mcp::McpClient;
use athen_core::traits::memory::{MemoryItem, MemoryStore};
use athen_core::traits::tool::ToolRegistry;
use athen_memory::Memory;
use athen_persistence::calendar::{CalendarEvent, CalendarStore, EventCreator};
use athen_persistence::contacts::SqliteContactStore;

use crate::file_gate::FileGate;

/// Prefix MCP-routed tools use to avoid name collisions with built-in tools.
/// `files__read_file` resolves to mcp_id="files", tool="read_file".
const MCP_TOOL_SEPARATOR: &str = "__";

/// Wraps [`ShellToolRegistry`] and adds calendar, contact, memory, and MCP tools.
pub struct AppToolRegistry {
    inner: Arc<ShellToolRegistry>,
    calendar: Option<CalendarStore>,
    contacts: Option<SqliteContactStore>,
    memory: Option<Arc<Memory>>,
    mcp: Option<Arc<dyn McpClient>>,
    file_gate: Option<Arc<FileGate>>,
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
        }
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
                    "description": "Category: meeting, birthday, deadline, reminder, personal, work, other (optional)"
                },
                "color": {
                    "type": "string",
                    "description": "Hex color for the event (optional, e.g. '#7aa2f7')"
                },
                "reminder_minutes": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Reminder lead times in minutes (e.g. [15, 60] = 15min and 1h before)"
                },
                "recurrence": {
                    "type": "string",
                    "description": "Recurrence: Daily, Weekly, Monthly, Yearly (optional)"
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

        let recurrence = args
            .get("recurrence")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_value(json!(s)).ok());

        let event = CalendarEvent {
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
            recurrence,
            reminder_minutes,
            color: args.get("color").and_then(|v| v.as_str()).map(String::from),
            category: args
                .get("category")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_by: EventCreator::Agent,
            arc_id: None,
            created_at: now.clone(),
            updated_at: now,
        };

        tracing::info!(
            tool = "calendar_create",
            title,
            start_time,
            "Creating calendar event"
        );

        let t = Instant::now();
        store.create_event(&event).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({
                "id": id,
                "title": title,
                "start_time": start_time,
                "end_time": end_time,
                "message": format!("Event '{}' created successfully", title),
            }),
            error: None,
            execution_time_ms: elapsed,
        })
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
        let query_lower = query.to_lowercase();
        let matches: Vec<serde_json::Value> = contacts
            .iter()
            .filter(|c| {
                c.name.to_lowercase().contains(&query_lower)
                    || c.identifiers
                        .iter()
                        .any(|i| i.value.to_lowercase().contains(&query_lower))
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
        let item = MemoryItem {
            id: format!("agent_{key}"),
            content: format!("{key}: {value}"),
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
                            base_risk: BaseImpact::WritePersist,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list MCP tools: {e}");
                }
            }
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

        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        // Path permission gate: any file-touching tool is intercepted here.
        // The gate may run the operation directly (for absolute paths
        // outside the sandbox) or hand back control via the closure
        // for paths inside the sandbox / built-in tools.
        if let Some(gate) = self.file_gate.clone() {
            if FileGate::is_file_tool(name) {
                let name_owned = name.to_string();
                let mcp = self.mcp.clone();
                // The `dispatch_inside_sandbox` closure: routes either
                // to the MCP server (for `files__*` inside sandbox) or
                // to the inner ShellToolRegistry (for the new built-in
                // file tools `read`/`edit`/`write`/`grep`, which carry
                // stateful read-state).
                //
                // The gate calls this closure only when it can't (or
                // shouldn't) execute the op directly with `tokio::fs`.
                let inner_clone_name = name_owned.clone();
                // We need the inner registry to dispatch built-in calls.
                // ShellToolRegistry is held inside `self`, but we can't
                // move `&self` into a 'static closure. The gate's
                // `dispatch` is invoked synchronously inside `handle()`,
                // so a raw pointer borrow would be unsound. Instead,
                // since the inner registry is `Send + Sync`, we rebuild
                // the dispatch by running the gate first with a closure
                // that delegates to a *channel*. That's overkill for our
                // needs — a simpler workaround is to capture an Arc.
                //
                // ShellToolRegistry isn't behind an Arc here; rather
                // than restructure the field, we route built-in calls
                // by re-invoking `self.inner.call_tool` after the gate
                // handle returns its own result. To do that cleanly, we
                // package the args and recurse via a helper.
                let mcp_opt_for_closure = mcp.clone();
                let inner_for_closure = self.inner.clone();
                let dispatch = move |rewritten: serde_json::Value| {
                    let name = inner_clone_name.clone();
                    let mcp_opt = mcp_opt_for_closure.clone();
                    let inner_for_closure = inner_for_closure.clone();
                    Box::pin(async move {
                        // MCP path.
                        if let Some((mcp_id, tool)) = name.split_once(MCP_TOOL_SEPARATOR) {
                            if let Some(mcp_client) = mcp_opt {
                                let started = Instant::now();
                                let outcome = mcp_client.call_tool(mcp_id, tool, rewritten).await?;
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
                            return Err(AthenError::Other("MCP client not available".into()));
                        }
                        // Built-in file tool path: delegate to the inner
                        // ShellToolRegistry so stateful behaviors (e.g.
                        // the read-state hash for `edit`/`write`) are
                        // preserved.
                        inner_for_closure.call_tool(&name, rewritten).await
                    })
                        as futures::future::BoxFuture<'static, Result<ToolResult>>
                };
                return gate.handle(name, args, dispatch).await;
            }
        }

        // Route MCP-prefixed tool names (e.g. "files__read_file") to the registry.
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
        assert_eq!(tools.len(), 12, "Expected 8 shell + 4 calendar tools");

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"calendar_list"));
        assert!(names.contains(&"calendar_create"));
        assert!(names.contains(&"calendar_update"));
        assert!(names.contains(&"calendar_delete"));
    }

    // 2. list_tools_without_calendar_has_8
    #[tokio::test]
    async fn list_tools_without_calendar_has_8() {
        let registry = setup_without_calendar().await;
        let tools = registry.list_tools().await.unwrap();
        assert_eq!(
            tools.len(),
            8,
            "Expected only 8 shell tools when calendar is None"
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
                    "color": "#7aa2f7",
                    "reminder_minutes": [15, 60],
                    "recurrence": "Weekly"
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
        assert_eq!(event.color, Some("#7aa2f7".to_string()));
        assert_eq!(event.reminder_minutes, vec![15, 60]);
        assert_eq!(
            event.recurrence,
            Some(athen_persistence::calendar::Recurrence::Weekly)
        );
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
        // 8 shell + 5 contact = 13
        assert_eq!(tools.len(), 13);
    }

    // 17. list_tools_with_all_stores
    #[tokio::test]
    async fn list_tools_with_all_stores() {
        let (_db, registry) = setup_with_all().await;
        let tools = registry.list_tools().await.unwrap();
        // 8 shell + 4 calendar + 5 contact = 17
        assert_eq!(tools.len(), 17);
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
}
