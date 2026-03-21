//! Event routing logic.
//!
//! Maps incoming `SenseEvent`s to `Task`s with appropriate domain and priority.

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::error::Result;
use athen_core::event::{EventSource, SenseEvent};
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::traits::coordinator::EventRouter;

/// Default router that maps event sources to domains and priorities.
pub struct DefaultRouter;

impl DefaultRouter {
    pub fn new() -> Self {
        Self
    }

    /// Map an `EventSource` to the appropriate `DomainType`.
    fn domain_for_source(source: &EventSource) -> DomainType {
        match source {
            EventSource::Email => DomainType::Communication,
            EventSource::Calendar => DomainType::Agenda,
            EventSource::Messaging => DomainType::Communication,
            EventSource::UserInput => DomainType::Base,
            EventSource::System => DomainType::Base,
        }
    }

    /// Determine task priority based on sense priority order.
    fn priority_for_source(source: &EventSource) -> TaskPriority {
        match source {
            EventSource::UserInput => TaskPriority::High,
            EventSource::Calendar => TaskPriority::High,
            EventSource::Messaging => TaskPriority::Normal,
            EventSource::Email => TaskPriority::Normal,
            EventSource::System => TaskPriority::Low,
        }
    }
}

impl Default for DefaultRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventRouter for DefaultRouter {
    async fn route(&self, event: SenseEvent) -> Result<Vec<Task>> {
        let domain = Self::domain_for_source(&event.source);
        let priority = Self::priority_for_source(&event.source);
        let now = Utc::now();

        let description = event
            .content
            .summary
            .clone()
            .unwrap_or_else(|| format!("Task from {:?} event", event.source));

        let task = Task {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            source_event: Some(event.id),
            domain,
            description,
            priority,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: Vec::new(),
            deadline: None,
        };

        Ok(vec![task])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::event::{EventKind, NormalizedContent};
    use athen_core::risk::RiskLevel;

    fn make_event(source: EventSource) -> SenseEvent {
        SenseEvent {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            source,
            kind: EventKind::NewMessage,
            sender: None,
            content: NormalizedContent {
                summary: Some("Test event".to_string()),
                body: serde_json::Value::Null,
                attachments: Vec::new(),
            },
            source_risk: RiskLevel::Safe,
            raw_id: None,
        }
    }

    #[test]
    fn test_domain_mapping_email() {
        assert_eq!(
            DefaultRouter::domain_for_source(&EventSource::Email),
            DomainType::Communication
        );
    }

    #[test]
    fn test_domain_mapping_calendar() {
        assert_eq!(
            DefaultRouter::domain_for_source(&EventSource::Calendar),
            DomainType::Agenda
        );
    }

    #[test]
    fn test_domain_mapping_messaging() {
        assert_eq!(
            DefaultRouter::domain_for_source(&EventSource::Messaging),
            DomainType::Communication
        );
    }

    #[test]
    fn test_domain_mapping_user_input() {
        assert_eq!(
            DefaultRouter::domain_for_source(&EventSource::UserInput),
            DomainType::Base
        );
    }

    #[test]
    fn test_domain_mapping_system() {
        assert_eq!(
            DefaultRouter::domain_for_source(&EventSource::System),
            DomainType::Base
        );
    }

    #[test]
    fn test_priority_user_input() {
        assert_eq!(
            DefaultRouter::priority_for_source(&EventSource::UserInput),
            TaskPriority::High
        );
    }

    #[test]
    fn test_priority_calendar() {
        assert_eq!(
            DefaultRouter::priority_for_source(&EventSource::Calendar),
            TaskPriority::High
        );
    }

    #[test]
    fn test_priority_messaging() {
        assert_eq!(
            DefaultRouter::priority_for_source(&EventSource::Messaging),
            TaskPriority::Normal
        );
    }

    #[test]
    fn test_priority_email() {
        assert_eq!(
            DefaultRouter::priority_for_source(&EventSource::Email),
            TaskPriority::Normal
        );
    }

    #[test]
    fn test_priority_system() {
        assert_eq!(
            DefaultRouter::priority_for_source(&EventSource::System),
            TaskPriority::Low
        );
    }

    #[tokio::test]
    async fn test_route_creates_task_with_correct_fields() {
        let router = DefaultRouter::new();
        let event = make_event(EventSource::Email);
        let event_id = event.id;

        let tasks = router.route(event).await.unwrap();
        assert_eq!(tasks.len(), 1);

        let task = &tasks[0];
        assert_eq!(task.domain, DomainType::Communication);
        assert_eq!(task.priority, TaskPriority::Normal);
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.source_event, Some(event_id));
        assert_eq!(task.description, "Test event");
    }
}
