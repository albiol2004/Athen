//! Composite tool registry that extends ShellToolRegistry with app-level tools.
//!
//! Calendar CRUD tools are added here since athen-agent doesn't depend on
//! athen-persistence. The composition root (athen-app) wires the CalendarStore
//! into the registry before handing it to the agent.

use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use athen_core::error::{AthenError, Result};
use athen_core::risk::BaseImpact;
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::tool::ToolRegistry;
use athen_agent::ShellToolRegistry;
use athen_persistence::calendar::{CalendarEvent, CalendarStore, EventCreator};

/// Wraps [`ShellToolRegistry`] and adds calendar tools backed by [`CalendarStore`].
pub struct AppToolRegistry {
    inner: ShellToolRegistry,
    calendar: Option<CalendarStore>,
}

impl AppToolRegistry {
    /// Create a new composite registry.
    pub fn new(inner: ShellToolRegistry, calendar: Option<CalendarStore>) -> Self {
        Self { inner, calendar }
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
        let store = self.calendar.as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let start = args.get("start").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'start' parameter".into()))?;
        let end = args.get("end").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'end' parameter".into()))?;

        tracing::info!(tool = "calendar_list", start, end, "Listing calendar events");

        let t = Instant::now();
        let events = store.list_events(start, end).await?;
        let elapsed = t.elapsed().as_millis() as u64;

        let events_json: Vec<serde_json::Value> = events.iter().map(|e| {
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
        }).collect();

        Ok(ToolResult {
            success: true,
            output: json!({ "events": events_json, "count": events_json.len() }),
            error: None,
            execution_time_ms: elapsed,
        })
    }

    async fn do_calendar_create(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let store = self.calendar.as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let title = args.get("title").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'title' parameter".into()))?;
        let start_time = args.get("start_time").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'start_time' parameter".into()))?;
        let end_time = args.get("end_time").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'end_time' parameter".into()))?;

        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let reminder_minutes: Vec<i64> = args.get("reminder_minutes")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();

        let recurrence = args.get("recurrence")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_value(json!(s)).ok());

        let event = CalendarEvent {
            id: id.clone(),
            title: title.to_string(),
            description: args.get("description").and_then(|v| v.as_str()).map(String::from),
            start_time: start_time.to_string(),
            end_time: end_time.to_string(),
            all_day: args.get("all_day").and_then(|v| v.as_bool()).unwrap_or(false),
            location: args.get("location").and_then(|v| v.as_str()).map(String::from),
            recurrence,
            reminder_minutes,
            color: args.get("color").and_then(|v| v.as_str()).map(String::from),
            category: args.get("category").and_then(|v| v.as_str()).map(String::from),
            created_by: EventCreator::Agent,
            arc_id: None,
            created_at: now.clone(),
            updated_at: now,
        };

        tracing::info!(tool = "calendar_create", title, start_time, "Creating calendar event");

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
        let store = self.calendar.as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let id = args.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'id' parameter".into()))?;

        tracing::info!(tool = "calendar_update", id, "Updating calendar event");

        // Load the existing event first.
        let existing = store.get_event(id).await?
            .ok_or_else(|| AthenError::Other(format!("Event '{id}' not found")))?;

        let now = chrono::Utc::now().to_rfc3339();

        let reminder_minutes: Vec<i64> = args.get("reminder_minutes")
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
            title: args.get("title").and_then(|v| v.as_str())
                .unwrap_or(&existing.title).to_string(),
            description: args.get("description").and_then(|v| v.as_str())
                .map(String::from).or(existing.description),
            start_time: args.get("start_time").and_then(|v| v.as_str())
                .unwrap_or(&existing.start_time).to_string(),
            end_time: args.get("end_time").and_then(|v| v.as_str())
                .unwrap_or(&existing.end_time).to_string(),
            all_day: args.get("all_day").and_then(|v| v.as_bool())
                .unwrap_or(existing.all_day),
            location: args.get("location").and_then(|v| v.as_str())
                .map(String::from).or(existing.location),
            recurrence,
            reminder_minutes,
            color: args.get("color").and_then(|v| v.as_str())
                .map(String::from).or(existing.color),
            category: args.get("category").and_then(|v| v.as_str())
                .map(String::from).or(existing.category),
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
        let store = self.calendar.as_ref()
            .ok_or_else(|| AthenError::Other("Calendar not available".into()))?;

        let id = args.get("id").and_then(|v| v.as_str())
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
}

#[async_trait]
impl ToolRegistry for AppToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut tools = self.inner.list_tools().await?;

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
                backend: ToolBackend::Shell { command: String::new(), native: false },
                base_risk: BaseImpact::WritePersist,
            });
        }

        Ok(tools)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        match name {
            "calendar_list" => self.do_calendar_list(&args).await,
            "calendar_create" => self.do_calendar_create(&args).await,
            "calendar_update" => self.do_calendar_update(&args).await,
            "calendar_delete" => self.do_calendar_delete(&args).await,
            _ => self.inner.call_tool(name, args).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_persistence::Database;
    use athen_persistence::calendar::EventCreator;
    use serde_json::json;

    /// Helper: create an in-memory DB + CalendarStore + AppToolRegistry.
    /// Returns the database (must be kept alive) and the registry.
    async fn setup_with_calendar() -> (Database, AppToolRegistry) {
        let db = Database::in_memory().await.unwrap();
        let calendar_store = db.calendar_store();
        let shell = ShellToolRegistry::new().await;
        let registry = AppToolRegistry::new(shell, Some(calendar_store));
        (db, registry)
    }

    /// Helper: create an AppToolRegistry without a calendar store.
    async fn setup_without_calendar() -> AppToolRegistry {
        let shell = ShellToolRegistry::new().await;
        AppToolRegistry::new(shell, None)
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
        assert_eq!(tools.len(), 10, "Expected 6 shell + 4 calendar tools");

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"calendar_list"));
        assert!(names.contains(&"calendar_create"));
        assert!(names.contains(&"calendar_update"));
        assert!(names.contains(&"calendar_delete"));
    }

    // 2. list_tools_without_calendar_has_6
    #[tokio::test]
    async fn list_tools_without_calendar_has_6() {
        let registry = setup_without_calendar().await;
        let tools = registry.list_tools().await.unwrap();
        assert_eq!(tools.len(), 6, "Expected only 6 shell tools when calendar is None");

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
        assert!(output.contains("hello"), "Expected 'hello' in output: {output}");
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
        assert_eq!(event.recurrence, Some(athen_persistence::calendar::Recurrence::Weekly));
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
        assert_eq!(update_result.output["title"].as_str().unwrap(), "Updated Title");

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
        assert!(result.is_err(), "Updating a nonexistent event should return an error");
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
        assert!(result.is_err(), "Calendar tool should fail when store is None");
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
            err_msg.to_lowercase().contains("not found") || err_msg.to_lowercase().contains("unknown"),
            "Error should indicate tool not found: {err_msg}"
        );
    }
}
