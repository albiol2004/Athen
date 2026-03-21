use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::risk::RiskScore;

pub type TaskId = Uuid;
pub type AgentId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub source_event: Option<Uuid>,
    pub domain: DomainType,
    pub description: String,
    pub priority: TaskPriority,
    pub status: TaskStatus,
    pub risk_score: Option<RiskScore>,
    pub risk_budget: Option<u32>,
    pub risk_used: u32,
    pub assigned_agent: Option<AgentId>,
    pub steps: Vec<TaskStep>,
    pub deadline: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStep {
    pub id: Uuid,
    pub index: u32,
    pub description: String,
    pub status: StepStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub output: Option<serde_json::Value>,
    pub checkpoint: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskPriority {
    Background = 0,
    Low = 1,
    Normal = 2,
    High = 3,
    Critical = 4,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    AwaitingApproval,
    InProgress,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DomainType {
    Base,
    Communication,
    Code,
    Agenda,
    Files,
    Research,
    Custom(String),
}
