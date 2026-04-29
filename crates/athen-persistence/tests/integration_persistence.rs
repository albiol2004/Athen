//! Integration tests for athen-persistence across the full task lifecycle.

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use athen_core::ipc::{IpcMessage, IpcPayload, ProcessId, ProcessTarget, ProcessType};
use athen_core::task::{DomainType, StepStatus, Task, TaskPriority, TaskStatus, TaskStep};
use athen_core::traits::persistence::{PersistentStore, TaskFilter};
use athen_persistence::checkpoint::CheckpointManager;
use athen_persistence::Database;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn make_step(index: u32, description: &str, status: StepStatus) -> TaskStep {
    TaskStep {
        id: Uuid::new_v4(),
        index,
        description: description.to_string(),
        status,
        started_at: if status != StepStatus::Pending {
            Some(Utc::now())
        } else {
            None
        },
        completed_at: if status == StepStatus::Completed {
            Some(Utc::now())
        } else {
            None
        },
        output: if status == StepStatus::Completed {
            Some(json!({"result": "success", "step": index}))
        } else {
            None
        },
        checkpoint: None,
    }
}

fn make_task_with_steps(description: &str, num_steps: u32) -> Task {
    let steps: Vec<TaskStep> = (0..num_steps)
        .map(|i| {
            make_step(
                i,
                &format!("Step {i}: process stage {i}"),
                StepStatus::Pending,
            )
        })
        .collect();

    Task {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        source_event: Some(Uuid::new_v4()),
        domain: DomainType::Code,
        description: description.to_string(),
        priority: TaskPriority::High,
        status: TaskStatus::Pending,
        risk_score: None,
        risk_budget: Some(200),
        risk_used: 0,
        assigned_agent: Some(Uuid::new_v4()),
        steps,
        deadline: Some(Utc::now() + chrono::Duration::hours(2)),
    }
}

fn make_ipc_message(payload: IpcPayload) -> IpcMessage {
    IpcMessage {
        id: Uuid::new_v4(),
        source: ProcessId {
            process_type: ProcessType::Monitor,
            instance_id: Uuid::new_v4(),
        },
        target: ProcessTarget::Coordinator,
        payload,
    }
}

// ---------------------------------------------------------------------------
// Test 1: Complete task lifecycle with real persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_full_task_lifecycle_persisted() {
    let db = Database::in_memory().await.unwrap();
    let store = db.store();

    // Create a realistic task with 3 steps
    let mut task = make_task_with_steps("Refactor authentication module", 3);
    let task_id = task.id;

    // Save it
    store.save_task(&task).await.unwrap();

    // Load it back, verify all fields match including steps
    let loaded = store.load_task(task_id).await.unwrap().unwrap();
    assert_eq!(loaded.id, task_id);
    assert_eq!(loaded.description, "Refactor authentication module");
    assert_eq!(loaded.domain, DomainType::Code);
    assert_eq!(loaded.priority, TaskPriority::High);
    assert_eq!(loaded.status, TaskStatus::Pending);
    assert_eq!(loaded.risk_budget, Some(200));
    assert_eq!(loaded.risk_used, 0);
    assert!(loaded.assigned_agent.is_some());
    assert!(loaded.source_event.is_some());
    assert!(loaded.deadline.is_some());
    assert_eq!(loaded.steps.len(), 3);
    for (i, step) in loaded.steps.iter().enumerate() {
        assert_eq!(step.index, i as u32);
        assert_eq!(step.status, StepStatus::Pending);
    }

    // Update step 0 status to Completed, step 1 to InProgress
    task.steps[0].status = StepStatus::Completed;
    task.steps[0].started_at = Some(Utc::now());
    task.steps[0].completed_at = Some(Utc::now());
    task.steps[0].output = Some(json!({"result": "auth refactored"}));

    task.steps[1].status = StepStatus::InProgress;
    task.steps[1].started_at = Some(Utc::now());

    task.status = TaskStatus::InProgress;
    task.updated_at = Utc::now();

    // Save checkpoint for the task with accumulated data
    let checkpoint_data = json!({
        "accumulated_context": {
            "files_modified": ["src/auth.rs", "src/middleware.rs"],
            "tokens_used": 1500,
            "current_step": 1
        }
    });
    store
        .save_checkpoint(task_id, checkpoint_data.clone())
        .await
        .unwrap();

    // Save again, load again -- verify updated state
    store.save_task(&task).await.unwrap();
    let reloaded = store.load_task(task_id).await.unwrap().unwrap();
    assert_eq!(reloaded.status, TaskStatus::InProgress);
    assert_eq!(reloaded.steps[0].status, StepStatus::Completed);
    assert!(reloaded.steps[0].completed_at.is_some());
    assert_eq!(reloaded.steps[1].status, StepStatus::InProgress);
    assert!(reloaded.steps[1].started_at.is_some());
    assert_eq!(reloaded.steps[2].status, StepStatus::Pending);

    // Verify checkpoint survived
    let loaded_cp = store.load_checkpoint(task_id).await.unwrap().unwrap();
    assert_eq!(loaded_cp, checkpoint_data);

    // Update task status to Completed
    task.status = TaskStatus::Completed;
    task.steps[1].status = StepStatus::Completed;
    task.steps[1].completed_at = Some(Utc::now());
    task.steps[2].status = StepStatus::Completed;
    task.steps[2].started_at = Some(Utc::now());
    task.steps[2].completed_at = Some(Utc::now());
    task.updated_at = Utc::now();
    store.save_task(&task).await.unwrap();

    // List tasks with filter status = Completed -- assert it shows up
    let completed = store
        .list_tasks(TaskFilter {
            status: Some(TaskStatus::Completed),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].id, task_id);

    // List tasks with filter status = Pending -- assert it doesn't show up
    let pending = store
        .list_tasks(TaskFilter {
            status: Some(TaskStatus::Pending),
            limit: None,
        })
        .await
        .unwrap();
    assert!(pending.is_empty());
}

// ---------------------------------------------------------------------------
// Test 2: Checkpoint survives simulated crash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_checkpoint_survives_simulated_crash() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("crash_test.db");

    let task_id;
    let checkpoint_data = json!({
        "accumulated_context": {
            "conversation_history": [
                {"role": "user", "content": "Summarize recent emails"},
                {"role": "assistant", "content": "Found 3 new emails..."}
            ],
            "partial_results": {
                "emails_processed": 2,
                "emails_remaining": 1
            },
            "last_api_call_timestamp": "2026-03-21T10:00:00Z"
        }
    });

    // Phase 1: create database, save task and checkpoint, then drop
    {
        let db = Database::new(&db_path).await.unwrap();
        let store = db.store();

        let task = make_task_with_steps("Process inbox emails", 3);
        task_id = task.id;

        store.save_task(&task).await.unwrap();
        store
            .save_checkpoint(task_id, checkpoint_data.clone())
            .await
            .unwrap();

        // Drop db here -- simulates crash
    }

    // Phase 2: reopen the database from the same path
    {
        let db = Database::new(&db_path).await.unwrap();
        let store = db.store();

        // Load the task -- it should still be there
        let loaded_task = store.load_task(task_id).await.unwrap();
        assert!(loaded_task.is_some(), "Task should survive crash");
        let loaded_task = loaded_task.unwrap();
        assert_eq!(loaded_task.id, task_id);
        assert_eq!(loaded_task.description, "Process inbox emails");
        assert_eq!(loaded_task.steps.len(), 3);

        // Load the checkpoint -- it should still be there with correct data
        let loaded_cp = store.load_checkpoint(task_id).await.unwrap();
        assert!(loaded_cp.is_some(), "Checkpoint should survive crash");
        let loaded_cp = loaded_cp.unwrap();
        assert_eq!(loaded_cp, checkpoint_data);

        // Verify checkpoint integrity (SHA-256) by re-saving and reloading
        // The load_checkpoint method internally verifies SHA-256 checksum,
        // so successful load already proves integrity. We also verify the
        // nested structure is intact.
        assert_eq!(
            loaded_cp["accumulated_context"]["emails_processed"],
            json!(null),
            "Top-level context should not have emails_processed directly"
        );
        assert_eq!(
            loaded_cp["accumulated_context"]["partial_results"]["emails_processed"],
            json!(2)
        );
        assert_eq!(
            loaded_cp["accumulated_context"]["conversation_history"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Pending messages queue ordering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pending_messages_queue_ordering() {
    let db = Database::in_memory().await.unwrap();
    let store = db.store();

    // Save 5 pending messages with slightly different timestamps
    let mut message_ids = Vec::new();
    for i in 0..5u32 {
        let msg = make_ipc_message(IpcPayload::StateUpdate(
            json!({"update_seq": i, "data": format!("batch_{i}")}),
        ));
        message_ids.push(msg.id);
        store.save_pending_message(&msg).await.unwrap();
        // Small delay to ensure distinct received_at timestamps
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Pop 3 -- should get the 3 oldest
    let popped_3 = store.pop_pending_messages(3).await.unwrap();
    assert_eq!(popped_3.len(), 3, "Should pop exactly 3 messages");
    assert_eq!(popped_3[0].id, message_ids[0]);
    assert_eq!(popped_3[1].id, message_ids[1]);
    assert_eq!(popped_3[2].id, message_ids[2]);

    // Pop 3 more -- should get the remaining 2 and then empty
    let popped_remaining = store.pop_pending_messages(3).await.unwrap();
    assert_eq!(
        popped_remaining.len(),
        2,
        "Should pop only the 2 remaining messages"
    );
    assert_eq!(popped_remaining[0].id, message_ids[3]);
    assert_eq!(popped_remaining[1].id, message_ids[4]);

    // Pop again -- should get empty vec
    let popped_empty = store.pop_pending_messages(3).await.unwrap();
    assert!(
        popped_empty.is_empty(),
        "Should get empty vec when no messages remain"
    );

    // Verify the popped messages can't be popped again (they're marked processed)
    let popped_again = store.pop_pending_messages(100).await.unwrap();
    assert!(
        popped_again.is_empty(),
        "Already processed messages must not be returned again"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Concurrent task operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_task_operations() {
    let db = Database::in_memory().await.unwrap();
    let store = Arc::new(db.store());

    let mut handles = Vec::new();

    for i in 0..10u32 {
        let store = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            let task = Task {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                source_event: None,
                domain: DomainType::Research,
                description: format!("Concurrent task {i}"),
                priority: TaskPriority::Normal,
                status: TaskStatus::Pending,
                risk_score: None,
                risk_budget: Some(50 + i),
                risk_used: 0,
                assigned_agent: None,
                steps: vec![make_step(
                    0,
                    &format!("Only step for task {i}"),
                    StepStatus::Pending,
                )],
                deadline: None,
            };
            let task_id = task.id;
            store.save_task(&task).await.unwrap();
            (task_id, i)
        });
        handles.push(handle);
    }

    // Wait for all to complete
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // List all tasks -- should have exactly 10
    let all_tasks = store.list_tasks(TaskFilter::default()).await.unwrap();
    assert_eq!(all_tasks.len(), 10, "Should have exactly 10 tasks");

    // Each task should have unique data
    let mut descriptions: Vec<String> = all_tasks.iter().map(|t| t.description.clone()).collect();
    descriptions.sort();
    descriptions.dedup();
    assert_eq!(
        descriptions.len(),
        10,
        "Each task should have a unique description"
    );

    // Verify each task can be loaded individually
    for (task_id, i) in &results {
        let loaded = store.load_task(*task_id).await.unwrap().unwrap();
        assert_eq!(loaded.description, format!("Concurrent task {i}"));
        assert_eq!(loaded.risk_budget, Some(50 + *i));
        assert_eq!(loaded.steps.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Test 5: CheckpointManager file atomicity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_checkpoint_manager_file_atomicity() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let checkpoint_dir = tmp_dir.path().join("checkpoints");
    let db_path = tmp_dir.path().join("atomicity_test.db");

    let db = Database::new(&db_path).await.unwrap();
    let store = db.store();

    // Create a CheckpointManager with a temp directory
    let manager = CheckpointManager::with_file_backup(store, &checkpoint_dir).unwrap();

    let task_id = Uuid::new_v4();

    // Save a checkpoint
    let data_v1 = json!({
        "version": 1,
        "context": "initial state",
        "items": [1, 2, 3]
    });
    manager.save(task_id, data_v1.clone()).await.unwrap();

    // Verify the file exists on disk
    let checkpoint_file = checkpoint_dir.join(format!("{task_id}.checkpoint.json"));
    assert!(
        checkpoint_file.exists(),
        "Checkpoint file should exist on disk"
    );

    // Load it back, verify data matches
    let loaded_v1 = manager.load(task_id).await.unwrap().unwrap();
    assert_eq!(loaded_v1, data_v1);

    // Save a different checkpoint for the same task (overwrite)
    let data_v2 = json!({
        "version": 2,
        "context": "updated state after processing",
        "items": [1, 2, 3, 4, 5],
        "extra_field": true
    });
    manager.save(task_id, data_v2.clone()).await.unwrap();

    // Load again -- should get the new data, not the old
    let loaded_v2 = manager.load(task_id).await.unwrap().unwrap();
    assert_eq!(loaded_v2, data_v2);
    assert_ne!(
        loaded_v2, data_v1,
        "Should not return old data after overwrite"
    );

    // Verify the file on disk was updated (not duplicated)
    assert!(checkpoint_file.exists());
    let file_contents = std::fs::read_to_string(&checkpoint_file).unwrap();
    assert!(
        file_contents.contains("updated state after processing"),
        "File should contain the new data"
    );
    assert!(
        !file_contents.contains("initial state"),
        "File should not contain the old data"
    );
}
