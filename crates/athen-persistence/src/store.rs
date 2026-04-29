//! Task store and pending message queue backed by SQLite.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::ipc::IpcMessage;
use athen_core::task::{Task, TaskId, TaskStatus, TaskStep};
use athen_core::traits::persistence::{PersistentStore, TaskFilter};

/// SQLite-backed implementation of `PersistentStore`.
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Create a new `SqliteStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Run the schema migrations on the database.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to initialize schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    domain TEXT NOT NULL,
    description TEXT NOT NULL,
    priority INTEGER NOT NULL,
    status TEXT NOT NULL,
    risk_score_json TEXT,
    risk_budget INTEGER,
    risk_used INTEGER NOT NULL DEFAULT 0,
    assigned_agent TEXT,
    source_event TEXT,
    deadline TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_steps (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    step_index INTEGER NOT NULL,
    description TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    output_json TEXT,
    checkpoint_json TEXT
);

CREATE TABLE IF NOT EXISTS checkpoints (
    task_id TEXT PRIMARY KEY,
    data_json TEXT NOT NULL,
    checksum TEXT NOT NULL,
    saved_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pending_messages (
    id TEXT PRIMARY KEY,
    message_json TEXT NOT NULL,
    received_at TEXT NOT NULL,
    processed INTEGER NOT NULL DEFAULT 0
);
"#;

/// Serialize a `TaskStatus` to its string representation for storage.
fn status_to_str(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "Pending",
        TaskStatus::AwaitingApproval => "AwaitingApproval",
        TaskStatus::InProgress => "InProgress",
        TaskStatus::Paused => "Paused",
        TaskStatus::Completed => "Completed",
        TaskStatus::Failed => "Failed",
        TaskStatus::Cancelled => "Cancelled",
    }
}

/// Deserialize a `TaskStatus` from its stored string.
fn str_to_status(s: &str) -> Result<TaskStatus> {
    match s {
        "Pending" => Ok(TaskStatus::Pending),
        "AwaitingApproval" => Ok(TaskStatus::AwaitingApproval),
        "InProgress" => Ok(TaskStatus::InProgress),
        "Paused" => Ok(TaskStatus::Paused),
        "Completed" => Ok(TaskStatus::Completed),
        "Failed" => Ok(TaskStatus::Failed),
        "Cancelled" => Ok(TaskStatus::Cancelled),
        other => Err(AthenError::Other(format!("Unknown TaskStatus: {other}"))),
    }
}

/// Insert or replace a task and all of its steps inside a single transaction.
fn save_task_sync(conn: &Connection, task: &Task) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| AthenError::Other(format!("Transaction start: {e}")))?;

    let domain_json = serde_json::to_string(&task.domain).map_err(AthenError::Serialization)?;
    let risk_score_json = task
        .risk_score
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(AthenError::Serialization)?;

    tx.execute(
        "INSERT OR REPLACE INTO tasks \
         (id, domain, description, priority, status, risk_score_json, risk_budget, \
          risk_used, assigned_agent, source_event, deadline, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            task.id.to_string(),
            domain_json,
            task.description,
            task.priority as i32,
            status_to_str(&task.status),
            risk_score_json,
            task.risk_budget.map(|b| b as i64),
            task.risk_used as i64,
            task.assigned_agent.map(|a| a.to_string()),
            task.source_event.map(|e| e.to_string()),
            task.deadline.map(|d| d.to_rfc3339()),
            task.created_at.to_rfc3339(),
            task.updated_at.to_rfc3339(),
        ],
    )
    .map_err(|e| AthenError::Other(format!("Insert task: {e}")))?;

    // Delete old steps for this task and re-insert
    tx.execute(
        "DELETE FROM task_steps WHERE task_id = ?1",
        params![task.id.to_string()],
    )
    .map_err(|e| AthenError::Other(format!("Delete steps: {e}")))?;

    for step in &task.steps {
        let output_json = step
            .output
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(AthenError::Serialization)?;
        let checkpoint_json = step
            .checkpoint
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(AthenError::Serialization)?;

        tx.execute(
            "INSERT INTO task_steps \
             (id, task_id, step_index, description, status, started_at, completed_at, \
              output_json, checkpoint_json) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                step.id.to_string(),
                task.id.to_string(),
                step.index as i64,
                step.description,
                serde_json::to_string(&step.status).map_err(AthenError::Serialization)?,
                step.started_at.map(|t| t.to_rfc3339()),
                step.completed_at.map(|t| t.to_rfc3339()),
                output_json,
                checkpoint_json,
            ],
        )
        .map_err(|e| AthenError::Other(format!("Insert step: {e}")))?;
    }

    tx.commit()
        .map_err(|e| AthenError::Other(format!("Commit: {e}")))?;
    Ok(())
}

/// Load a task and its steps by id.
fn load_task_sync(conn: &Connection, id: TaskId) -> Result<Option<Task>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, domain, description, priority, status, risk_score_json, risk_budget, \
             risk_used, assigned_agent, source_event, deadline, created_at, updated_at \
             FROM tasks WHERE id = ?1",
        )
        .map_err(|e| AthenError::Other(format!("Prepare: {e}")))?;

    let task_opt = stmt
        .query_row(params![id.to_string()], |row| {
            Ok(TaskRow {
                id: row.get::<_, String>(0)?,
                domain: row.get::<_, String>(1)?,
                description: row.get::<_, String>(2)?,
                priority: row.get::<_, i32>(3)?,
                status: row.get::<_, String>(4)?,
                risk_score_json: row.get::<_, Option<String>>(5)?,
                risk_budget: row.get::<_, Option<i64>>(6)?,
                risk_used: row.get::<_, i64>(7)?,
                assigned_agent: row.get::<_, Option<String>>(8)?,
                source_event: row.get::<_, Option<String>>(9)?,
                deadline: row.get::<_, Option<String>>(10)?,
                created_at: row.get::<_, String>(11)?,
                updated_at: row.get::<_, String>(12)?,
            })
        })
        .optional()
        .map_err(|e| AthenError::Other(format!("Query task: {e}")))?;

    match task_opt {
        None => Ok(None),
        Some(row) => {
            let steps = load_steps_sync(conn, &row.id)?;
            let task = row_to_task(row, steps)?;
            Ok(Some(task))
        }
    }
}

/// Load steps for a given task, ordered by step_index.
fn load_steps_sync(conn: &Connection, task_id: &str) -> Result<Vec<TaskStep>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, step_index, description, status, started_at, completed_at, \
             output_json, checkpoint_json \
             FROM task_steps WHERE task_id = ?1 ORDER BY step_index",
        )
        .map_err(|e| AthenError::Other(format!("Prepare steps: {e}")))?;

    let rows = stmt
        .query_map(params![task_id], |row| {
            Ok(StepRow {
                id: row.get::<_, String>(0)?,
                index: row.get::<_, i64>(1)?,
                description: row.get::<_, String>(2)?,
                status: row.get::<_, String>(3)?,
                started_at: row.get::<_, Option<String>>(4)?,
                completed_at: row.get::<_, Option<String>>(5)?,
                output_json: row.get::<_, Option<String>>(6)?,
                checkpoint_json: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|e| AthenError::Other(format!("Query steps: {e}")))?;

    let mut steps = Vec::new();
    for row_result in rows {
        let row = row_result.map_err(|e| AthenError::Other(format!("Step row: {e}")))?;
        steps.push(row_to_step(row)?);
    }
    Ok(steps)
}

struct TaskRow {
    id: String,
    domain: String,
    description: String,
    priority: i32,
    status: String,
    risk_score_json: Option<String>,
    risk_budget: Option<i64>,
    risk_used: i64,
    assigned_agent: Option<String>,
    source_event: Option<String>,
    deadline: Option<String>,
    created_at: String,
    updated_at: String,
}

struct StepRow {
    id: String,
    index: i64,
    description: String,
    status: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    output_json: Option<String>,
    checkpoint_json: Option<String>,
}

fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| AthenError::Other(format!("Invalid UUID '{s}': {e}")))
}

fn parse_datetime(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| AthenError::Other(format!("Invalid datetime '{s}': {e}")))
}

fn priority_from_i32(v: i32) -> Result<athen_core::task::TaskPriority> {
    use athen_core::task::TaskPriority;
    match v {
        0 => Ok(TaskPriority::Background),
        1 => Ok(TaskPriority::Low),
        2 => Ok(TaskPriority::Normal),
        3 => Ok(TaskPriority::High),
        4 => Ok(TaskPriority::Critical),
        other => Err(AthenError::Other(format!("Unknown priority: {other}"))),
    }
}

fn row_to_task(row: TaskRow, steps: Vec<TaskStep>) -> Result<Task> {
    use athen_core::task::DomainType;

    let domain: DomainType =
        serde_json::from_str(&row.domain).map_err(AthenError::Serialization)?;
    let risk_score = row
        .risk_score_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(AthenError::Serialization)?;

    Ok(Task {
        id: parse_uuid(&row.id)?,
        domain,
        description: row.description,
        priority: priority_from_i32(row.priority)?,
        status: str_to_status(&row.status)?,
        risk_score,
        risk_budget: row.risk_budget.map(|b| b as u32),
        risk_used: row.risk_used as u32,
        assigned_agent: row.assigned_agent.as_deref().map(parse_uuid).transpose()?,
        source_event: row.source_event.as_deref().map(parse_uuid).transpose()?,
        deadline: row.deadline.as_deref().map(parse_datetime).transpose()?,
        created_at: parse_datetime(&row.created_at)?,
        updated_at: parse_datetime(&row.updated_at)?,
        steps,
    })
}

fn row_to_step(row: StepRow) -> Result<TaskStep> {
    use athen_core::task::StepStatus;

    let status: StepStatus =
        serde_json::from_str(&row.status).map_err(AthenError::Serialization)?;
    let output = row
        .output_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(AthenError::Serialization)?;
    let checkpoint = row
        .checkpoint_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(AthenError::Serialization)?;

    Ok(TaskStep {
        id: parse_uuid(&row.id)?,
        index: row.index as u32,
        description: row.description,
        status,
        started_at: row.started_at.as_deref().map(parse_datetime).transpose()?,
        completed_at: row
            .completed_at
            .as_deref()
            .map(parse_datetime)
            .transpose()?,
        output,
        checkpoint,
    })
}

/// Extension trait to convert rusqlite's optional query results.
trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl PersistentStore for SqliteStore {
    async fn save_task(&self, task: &Task) -> Result<()> {
        let conn = self.conn.clone();
        let task = task.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            save_task_sync(&conn, &task)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn load_task(&self, id: TaskId) -> Result<Option<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            load_task_sync(&conn, id)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_tasks(&self, filter: TaskFilter) -> Result<Vec<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let mut sql = String::from(
                "SELECT id, domain, description, priority, status, risk_score_json, \
                 risk_budget, risk_used, assigned_agent, source_event, deadline, \
                 created_at, updated_at FROM tasks",
            );
            let mut sql_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(ref status) = filter.status {
                sql.push_str(" WHERE status = ?1");
                sql_params.push(Box::new(status_to_str(status).to_string()));
            }

            sql.push_str(" ORDER BY priority DESC, created_at ASC");

            if let Some(limit) = filter.limit {
                sql.push_str(&format!(" LIMIT {limit}"));
            }

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list: {e}")))?;

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                sql_params.iter().map(|p| p.as_ref()).collect();

            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok(TaskRow {
                        id: row.get::<_, String>(0)?,
                        domain: row.get::<_, String>(1)?,
                        description: row.get::<_, String>(2)?,
                        priority: row.get::<_, i32>(3)?,
                        status: row.get::<_, String>(4)?,
                        risk_score_json: row.get::<_, Option<String>>(5)?,
                        risk_budget: row.get::<_, Option<i64>>(6)?,
                        risk_used: row.get::<_, i64>(7)?,
                        assigned_agent: row.get::<_, Option<String>>(8)?,
                        source_event: row.get::<_, Option<String>>(9)?,
                        deadline: row.get::<_, Option<String>>(10)?,
                        created_at: row.get::<_, String>(11)?,
                        updated_at: row.get::<_, String>(12)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query list: {e}")))?;

            let mut tasks = Vec::new();
            for row_result in rows {
                let row = row_result.map_err(|e| AthenError::Other(format!("Row: {e}")))?;
                let task_id_str = row.id.clone();
                let steps = load_steps_sync(&conn, &task_id_str)?;
                tasks.push(row_to_task(row, steps)?);
            }
            Ok(tasks)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn save_checkpoint(&self, task_id: TaskId, data: Value) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let data_json = serde_json::to_string(&data).map_err(AthenError::Serialization)?;

            use sha2::{Digest, Sha256};
            let checksum = format!("{:x}", Sha256::digest(data_json.as_bytes()));

            conn.execute(
                "INSERT OR REPLACE INTO checkpoints (task_id, data_json, checksum, saved_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task_id.to_string(),
                    data_json,
                    checksum,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|e| AthenError::Other(format!("Save checkpoint: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn load_checkpoint(&self, task_id: TaskId) -> Result<Option<Value>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let result: Option<(String, String)> = conn
                .query_row(
                    "SELECT data_json, checksum FROM checkpoints WHERE task_id = ?1",
                    params![task_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|e| AthenError::Other(format!("Load checkpoint: {e}")))?;

            match result {
                None => Ok(None),
                Some((data_json, stored_checksum)) => {
                    // Verify integrity
                    use sha2::{Digest, Sha256};
                    let computed = format!("{:x}", Sha256::digest(data_json.as_bytes()));
                    if computed != stored_checksum {
                        return Err(AthenError::Other(format!(
                            "Checkpoint integrity check failed for task {task_id}: \
                             expected {stored_checksum}, got {computed}"
                        )));
                    }
                    let value: Value =
                        serde_json::from_str(&data_json).map_err(AthenError::Serialization)?;
                    Ok(Some(value))
                }
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn save_pending_message(&self, msg: &IpcMessage) -> Result<()> {
        let conn = self.conn.clone();
        let msg = msg.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let message_json = serde_json::to_string(&msg).map_err(AthenError::Serialization)?;
            conn.execute(
                "INSERT INTO pending_messages (id, message_json, received_at, processed) \
                 VALUES (?1, ?2, ?3, 0)",
                params![msg.id.to_string(), message_json, Utc::now().to_rfc3339(),],
            )
            .map_err(|e| AthenError::Other(format!("Save pending: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn pop_pending_messages(&self, limit: usize) -> Result<Vec<IpcMessage>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Atomically select and mark as processed inside a transaction.
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| AthenError::Other(format!("Transaction: {e}")))?;

            let rows: Vec<(String, String)> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id, message_json FROM pending_messages \
                         WHERE processed = 0 ORDER BY received_at ASC LIMIT ?1",
                    )
                    .map_err(|e| AthenError::Other(format!("Prepare pop: {e}")))?;

                let mapped = stmt
                    .query_map(params![limit as i64], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| AthenError::Other(format!("Query pop: {e}")))?;
                let collected = mapped
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| AthenError::Other(format!("Collect pop: {e}")))?;
                collected
            };

            let mut messages = Vec::with_capacity(rows.len());
            for (id, json) in &rows {
                tx.execute(
                    "UPDATE pending_messages SET processed = 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| AthenError::Other(format!("Mark processed: {e}")))?;

                let msg: IpcMessage =
                    serde_json::from_str(json).map_err(AthenError::Serialization)?;
                messages.push(msg);
            }

            tx.commit()
                .map_err(|e| AthenError::Other(format!("Commit pop: {e}")))?;
            Ok(messages)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::ipc::{IpcPayload, ProcessId, ProcessTarget, ProcessType};
    use athen_core::task::{DomainType, TaskPriority, TaskStatus};
    use chrono::Utc;
    use uuid::Uuid;

    fn make_test_task(status: TaskStatus) -> Task {
        Task {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_event: None,
            domain: DomainType::Base,
            description: "Test task".to_string(),
            priority: TaskPriority::Normal,
            status,
            risk_score: None,
            risk_budget: Some(100),
            risk_used: 0,
            assigned_agent: None,
            steps: vec![],
            deadline: None,
        }
    }

    fn make_test_task_with_steps() -> Task {
        use athen_core::task::StepStatus;
        let mut task = make_test_task(TaskStatus::InProgress);
        task.steps = vec![
            TaskStep {
                id: Uuid::new_v4(),
                index: 0,
                description: "Step one".to_string(),
                status: StepStatus::Completed,
                started_at: Some(Utc::now()),
                completed_at: Some(Utc::now()),
                output: Some(serde_json::json!({"result": "ok"})),
                checkpoint: None,
            },
            TaskStep {
                id: Uuid::new_v4(),
                index: 1,
                description: "Step two".to_string(),
                status: StepStatus::Pending,
                started_at: None,
                completed_at: None,
                output: None,
                checkpoint: None,
            },
        ];
        task
    }

    fn make_ipc_message() -> IpcMessage {
        IpcMessage {
            id: Uuid::new_v4(),
            source: ProcessId {
                process_type: ProcessType::Monitor,
                instance_id: Uuid::new_v4(),
            },
            target: ProcessTarget::Coordinator,
            payload: IpcPayload::HealthPing,
        }
    }

    async fn setup_store() -> SqliteStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = SqliteStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init schema");
        store
    }

    #[tokio::test]
    async fn test_save_and_load_task() {
        let store = setup_store().await;
        let task = make_test_task(TaskStatus::Pending);
        let id = task.id;

        store.save_task(&task).await.unwrap();
        let loaded = store.load_task(id).await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.description, "Test task");
        assert_eq!(loaded.status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn test_save_and_load_task_with_steps() {
        let store = setup_store().await;
        let task = make_test_task_with_steps();
        let id = task.id;

        store.save_task(&task).await.unwrap();
        let loaded = store.load_task(id).await.unwrap().unwrap();

        assert_eq!(loaded.steps.len(), 2);
        assert_eq!(loaded.steps[0].description, "Step one");
        assert_eq!(loaded.steps[1].description, "Step two");
    }

    #[tokio::test]
    async fn test_load_nonexistent_task() {
        let store = setup_store().await;
        let result = store.load_task(Uuid::new_v4()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_update_task() {
        let store = setup_store().await;
        let mut task = make_test_task(TaskStatus::Pending);
        let id = task.id;

        store.save_task(&task).await.unwrap();

        task.status = TaskStatus::InProgress;
        task.description = "Updated".to_string();
        store.save_task(&task).await.unwrap();

        let loaded = store.load_task(id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.description, "Updated");
    }

    #[tokio::test]
    async fn test_list_tasks_no_filter() {
        let store = setup_store().await;

        let t1 = make_test_task(TaskStatus::Pending);
        let t2 = make_test_task(TaskStatus::InProgress);
        store.save_task(&t1).await.unwrap();
        store.save_task(&t2).await.unwrap();

        let all = store.list_tasks(TaskFilter::default()).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_list_tasks_filter_by_status() {
        let store = setup_store().await;

        store
            .save_task(&make_test_task(TaskStatus::Pending))
            .await
            .unwrap();
        store
            .save_task(&make_test_task(TaskStatus::Pending))
            .await
            .unwrap();
        store
            .save_task(&make_test_task(TaskStatus::Completed))
            .await
            .unwrap();

        let pending = store
            .list_tasks(TaskFilter {
                status: Some(TaskStatus::Pending),
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);

        let completed = store
            .list_tasks(TaskFilter {
                status: Some(TaskStatus::Completed),
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
    }

    #[tokio::test]
    async fn test_list_tasks_with_limit() {
        let store = setup_store().await;

        for _ in 0..5 {
            store
                .save_task(&make_test_task(TaskStatus::Pending))
                .await
                .unwrap();
        }

        let limited = store
            .list_tasks(TaskFilter {
                status: None,
                limit: Some(3),
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 3);
    }

    #[tokio::test]
    async fn test_checkpoint_save_and_load() {
        let store = setup_store().await;
        let task = make_test_task(TaskStatus::Pending);
        store.save_task(&task).await.unwrap();

        let data = serde_json::json!({"progress": 50, "state": "halfway"});
        store.save_checkpoint(task.id, data.clone()).await.unwrap();

        let loaded = store.load_checkpoint(task.id).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap(), data);
    }

    #[tokio::test]
    async fn test_checkpoint_overwrite() {
        let store = setup_store().await;
        let task = make_test_task(TaskStatus::Pending);
        store.save_task(&task).await.unwrap();

        let data1 = serde_json::json!({"v": 1});
        let data2 = serde_json::json!({"v": 2});
        store.save_checkpoint(task.id, data1).await.unwrap();
        store.save_checkpoint(task.id, data2.clone()).await.unwrap();

        let loaded = store.load_checkpoint(task.id).await.unwrap().unwrap();
        assert_eq!(loaded, data2);
    }

    #[tokio::test]
    async fn test_checkpoint_nonexistent() {
        let store = setup_store().await;
        let loaded = store.load_checkpoint(Uuid::new_v4()).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_pending_messages_save_and_pop() {
        let store = setup_store().await;

        let m1 = make_ipc_message();
        let m2 = make_ipc_message();
        let m3 = make_ipc_message();

        store.save_pending_message(&m1).await.unwrap();
        store.save_pending_message(&m2).await.unwrap();
        store.save_pending_message(&m3).await.unwrap();

        // Pop 2
        let popped = store.pop_pending_messages(2).await.unwrap();
        assert_eq!(popped.len(), 2);
        assert_eq!(popped[0].id, m1.id);
        assert_eq!(popped[1].id, m2.id);

        // Pop remaining
        let popped2 = store.pop_pending_messages(10).await.unwrap();
        assert_eq!(popped2.len(), 1);
        assert_eq!(popped2[0].id, m3.id);

        // Pop again - empty
        let popped3 = store.pop_pending_messages(10).await.unwrap();
        assert!(popped3.is_empty());
    }

    #[tokio::test]
    async fn test_pending_messages_pop_atomicity() {
        let store = setup_store().await;

        for _ in 0..5 {
            store
                .save_pending_message(&make_ipc_message())
                .await
                .unwrap();
        }

        // Two concurrent pops should not return the same messages
        let pop1 = store.pop_pending_messages(3).await.unwrap();
        let pop2 = store.pop_pending_messages(3).await.unwrap();

        assert_eq!(pop1.len(), 3);
        assert_eq!(pop2.len(), 2);

        // No overlap
        for m in &pop1 {
            assert!(!pop2.iter().any(|m2| m2.id == m.id));
        }
    }
}
