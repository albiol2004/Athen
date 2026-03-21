use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Envelope for all IPC messages between processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcMessage {
    pub id: Uuid,
    pub source: ProcessId,
    pub target: ProcessTarget,
    pub payload: IpcPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProcessId {
    pub process_type: ProcessType,
    pub instance_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ProcessType {
    Coordinator,
    Monitor,
    Agent,
    Ui,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessTarget {
    /// Send to a specific process
    Direct(ProcessId),
    /// Send to the coordinator
    Coordinator,
    /// Broadcast to all processes of a type
    Broadcast(ProcessType),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcPayload {
    // Monitor -> Coordinator
    SenseEvent(crate::event::SenseEvent),

    // Coordinator -> Agent
    TaskAssignment(crate::task::Task),

    // Agent -> Coordinator
    TaskProgress(TaskProgressReport),

    // Coordinator -> Agent
    TaskControl(TaskControlCommand),

    // Any -> Coordinator
    Registration(ProcessRegistration),

    // Coordinator -> Any
    HealthPing,
    HealthPong { status: ProcessHealthStatus },

    // Coordinator -> UI
    StateUpdate(serde_json::Value),

    // UI -> Coordinator
    UserCommand(serde_json::Value),

    // Approval flow
    ApprovalRequest(ApprovalRequest),
    ApprovalResponse(ApprovalResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgressReport {
    pub task_id: crate::task::TaskId,
    pub step_index: u32,
    pub status: crate::task::StepStatus,
    pub output: Option<serde_json::Value>,
    pub risk_used: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskControlCommand {
    pub task_id: crate::task::TaskId,
    pub action: ControlAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlAction {
    Continue,
    Pause,
    Cancel,
    Modify(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRegistration {
    pub process_type: ProcessType,
    pub pid: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessHealthStatus {
    Healthy,
    Busy,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub task_id: crate::task::TaskId,
    pub description: String,
    pub risk_score: crate::risk::RiskScore,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: Uuid,
    pub approved: bool,
    pub feedback: Option<String>,
}
