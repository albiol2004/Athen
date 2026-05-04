//! Integration tests for the athen-ipc crate.
//!
//! These tests exercise real IPC communication between `IpcServer` and
//! `IpcClient` instances over Unix domain sockets, using `tempdir()` to
//! avoid socket-path collisions. Unix-only: `athen-ipc` itself is gated
//! on `cfg(unix)`, so its symbols don't exist on Windows.

#![cfg(unix)]

use std::collections::HashSet;
use std::time::Duration;

use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::ipc::{
    IpcMessage, IpcPayload, ProcessHealthStatus, ProcessId, ProcessTarget, ProcessType,
};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_ipc::{IpcClient, IpcServer, IpcTransport};
use chrono::Utc;
use tokio::time::timeout;
use uuid::Uuid;

/// Default timeout for operations that should complete quickly.
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Short timeout used to assert that a client does NOT receive a message.
const NO_RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// Build a minimal `SenseEvent` for testing.
fn make_sense_event() -> SenseEvent {
    SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::Email,
        kind: EventKind::NewMessage,
        sender: None,
        content: NormalizedContent {
            summary: Some("test email".into()),
            body: serde_json::json!({"text": "hello"}),
            attachments: vec![],
        },
        source_risk: RiskLevel::Safe,
        raw_id: None,
    }
}

/// Build a minimal `Task` for testing.
fn make_task() -> Task {
    let now = Utc::now();
    Task {
        id: Uuid::new_v4(),
        created_at: now,
        updated_at: now,
        source_event: None,
        domain: DomainType::Base,
        description: "integration test task".into(),
        priority: TaskPriority::Normal,
        status: TaskStatus::Pending,
        risk_score: None,
        risk_budget: None,
        risk_used: 0,
        assigned_agent: None,
        steps: vec![],
        deadline: None,
    }
}

/// Helper: create a coordinator `ProcessId` for building server-originated messages.
fn coordinator_pid() -> ProcessId {
    ProcessId {
        process_type: ProcessType::Coordinator,
        instance_id: Uuid::new_v4(),
    }
}

/// Drain `count` messages from the server (typically registration messages).
async fn drain_registrations(server: &IpcServer, count: usize) {
    for _ in 0..count {
        timeout(TEST_TIMEOUT, server.recv())
            .await
            .expect("timed out draining registration")
            .expect("failed to receive registration");
    }
}

// ---------------------------------------------------------------------------
// Test 1: Monitor sends a SenseEvent to the coordinator via IPC
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_monitor_sends_event_to_coordinator_via_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("monitor.sock");
    let sock_str = sock.to_str().unwrap();

    // Start server
    let server = IpcServer::new(sock_str);
    server.start().await.unwrap();
    tokio::task::yield_now().await;

    // Connect a Monitor client
    let client = IpcClient::connect(sock_str, ProcessType::Monitor, vec!["email".into()])
        .await
        .unwrap();

    // Drain registration message
    drain_registrations(&server, 1).await;

    // Build and send a SenseEvent
    let event = make_sense_event();
    let event_id = event.id;

    let msg = IpcMessage {
        id: Uuid::new_v4(),
        source: client.process_id().clone(),
        target: ProcessTarget::Coordinator,
        payload: IpcPayload::SenseEvent(event),
    };
    let msg_id = msg.id;

    timeout(TEST_TIMEOUT, client.send(&msg))
        .await
        .expect("send timed out")
        .expect("send failed");

    // Server receives the message
    let received = timeout(TEST_TIMEOUT, server.recv())
        .await
        .expect("recv timed out")
        .expect("recv failed");

    assert_eq!(received.id, msg_id);
    assert_eq!(received.source, *client.process_id());

    match &received.payload {
        IpcPayload::SenseEvent(evt) => {
            assert_eq!(evt.id, event_id);
            assert_eq!(evt.source, EventSource::Email);
            assert_eq!(evt.content.summary.as_deref(), Some("test email"));
        }
        other => panic!("expected SenseEvent, got {:?}", other),
    }

    // Cleanup
    client.close().await.unwrap();
    server.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 2: Coordinator broadcasts HealthPing to multiple agents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_coordinator_broadcasts_to_multiple_agents() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("broadcast.sock");
    let sock_str = sock.to_str().unwrap();

    let server = IpcServer::new(sock_str);
    server.start().await.unwrap();
    tokio::task::yield_now().await;

    // Connect 3 Agent clients
    let mut clients = Vec::new();
    for _ in 0..3 {
        let c = IpcClient::connect(sock_str, ProcessType::Agent, vec![])
            .await
            .unwrap();
        clients.push(c);
    }

    // Drain all 3 registration messages
    drain_registrations(&server, 3).await;

    // Small yield so all connections are fully registered
    tokio::task::yield_now().await;
    assert_eq!(server.connected_count().await, 3);

    // Broadcast a HealthPing
    let ping = IpcMessage {
        id: Uuid::new_v4(),
        source: coordinator_pid(),
        target: ProcessTarget::Broadcast(ProcessType::Agent),
        payload: IpcPayload::HealthPing,
    };
    let ping_id = ping.id;

    server
        .broadcast_to_type(&ProcessType::Agent, &ping)
        .await
        .unwrap();

    // Each client receives the ping and sends back a HealthPong
    for client in &clients {
        let received = timeout(TEST_TIMEOUT, client.recv())
            .await
            .expect("client recv timed out")
            .expect("client recv failed");

        assert_eq!(received.id, ping_id);
        match &received.payload {
            IpcPayload::HealthPing => {}
            other => panic!("expected HealthPing, got {:?}", other),
        }

        // Reply with HealthPong
        let pong = IpcMessage {
            id: Uuid::new_v4(),
            source: client.process_id().clone(),
            target: ProcessTarget::Coordinator,
            payload: IpcPayload::HealthPong {
                status: ProcessHealthStatus::Healthy,
            },
        };
        timeout(TEST_TIMEOUT, client.send(&pong))
            .await
            .expect("pong send timed out")
            .expect("pong send failed");
    }

    // Server receives all 3 pongs
    let mut pong_sources = HashSet::new();
    for _ in 0..3 {
        let msg = timeout(TEST_TIMEOUT, server.recv())
            .await
            .expect("server recv timed out")
            .expect("server recv failed");

        match &msg.payload {
            IpcPayload::HealthPong { status } => {
                assert_eq!(*status, ProcessHealthStatus::Healthy);
            }
            other => panic!("expected HealthPong, got {:?}", other),
        }
        pong_sources.insert(msg.source.instance_id);
    }
    assert_eq!(pong_sources.len(), 3, "should have 3 distinct pong sources");

    // Cleanup
    for c in &clients {
        c.close().await.unwrap();
    }
    server.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 3: Coordinator routes a TaskAssignment to a specific agent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_coordinator_routes_to_specific_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("route.sock");
    let sock_str = sock.to_str().unwrap();

    let server = IpcServer::new(sock_str);
    server.start().await.unwrap();
    tokio::task::yield_now().await;

    // Connect 2 agent clients
    let agent1 = IpcClient::connect(sock_str, ProcessType::Agent, vec![])
        .await
        .unwrap();
    let agent2 = IpcClient::connect(sock_str, ProcessType::Agent, vec![])
        .await
        .unwrap();

    drain_registrations(&server, 2).await;
    tokio::task::yield_now().await;

    // Send a TaskAssignment to agent1 only
    let task = make_task();
    let task_id = task.id;

    let assignment = IpcMessage {
        id: Uuid::new_v4(),
        source: coordinator_pid(),
        target: ProcessTarget::Direct(agent1.process_id().clone()),
        payload: IpcPayload::TaskAssignment(task),
    };
    let assignment_id = assignment.id;

    server
        .send_to(agent1.process_id(), &assignment)
        .await
        .unwrap();

    // Agent1 receives the assignment
    let received = timeout(TEST_TIMEOUT, agent1.recv())
        .await
        .expect("agent1 recv timed out")
        .expect("agent1 recv failed");

    assert_eq!(received.id, assignment_id);
    match &received.payload {
        IpcPayload::TaskAssignment(t) => {
            assert_eq!(t.id, task_id);
            assert_eq!(t.description, "integration test task");
        }
        other => panic!("expected TaskAssignment, got {:?}", other),
    }

    // Agent2 should NOT receive anything (short timeout to verify)
    let agent2_result = timeout(NO_RECV_TIMEOUT, agent2.recv()).await;
    assert!(
        agent2_result.is_err(),
        "agent2 should not have received a message"
    );

    // Cleanup
    agent1.close().await.unwrap();
    agent2.close().await.unwrap();
    server.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 4: Client reconnection after disconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_client_reconnection_after_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("reconnect.sock");
    let sock_str = sock.to_str().unwrap();

    let server = IpcServer::new(sock_str);
    server.start().await.unwrap();
    tokio::task::yield_now().await;

    // Connect first client
    let client1 = IpcClient::connect(sock_str, ProcessType::Monitor, vec!["cal".into()])
        .await
        .unwrap();

    // Drain registration
    drain_registrations(&server, 1).await;

    // Client sends a message, server receives it
    let msg1 = IpcMessage {
        id: Uuid::new_v4(),
        source: client1.process_id().clone(),
        target: ProcessTarget::Coordinator,
        payload: IpcPayload::HealthPing,
    };
    let msg1_id = msg1.id;
    client1.send(&msg1).await.unwrap();

    let received = timeout(TEST_TIMEOUT, server.recv())
        .await
        .expect("recv timed out")
        .expect("recv failed");
    assert_eq!(received.id, msg1_id);

    // Disconnect the first client
    client1.close().await.unwrap();

    // Brief pause to let the server detect the disconnect
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect a new client with the same ProcessType
    let client2 = IpcClient::connect(sock_str, ProcessType::Monitor, vec!["cal".into()])
        .await
        .unwrap();

    // Drain the new registration
    drain_registrations(&server, 1).await;
    tokio::task::yield_now().await;

    // New client sends a message, server receives it
    let msg2 = IpcMessage {
        id: Uuid::new_v4(),
        source: client2.process_id().clone(),
        target: ProcessTarget::Coordinator,
        payload: IpcPayload::HealthPing,
    };
    let msg2_id = msg2.id;
    client2.send(&msg2).await.unwrap();

    let received2 = timeout(TEST_TIMEOUT, server.recv())
        .await
        .expect("recv timed out after reconnect")
        .expect("recv failed after reconnect");
    assert_eq!(received2.id, msg2_id);

    // Server can also send to the new client
    let pong = IpcMessage {
        id: Uuid::new_v4(),
        source: coordinator_pid(),
        target: ProcessTarget::Direct(client2.process_id().clone()),
        payload: IpcPayload::HealthPong {
            status: ProcessHealthStatus::Healthy,
        },
    };
    let pong_id = pong.id;
    server.send_to(client2.process_id(), &pong).await.unwrap();

    let reply = timeout(TEST_TIMEOUT, client2.recv())
        .await
        .expect("client2 recv timed out")
        .expect("client2 recv failed");
    assert_eq!(reply.id, pong_id);

    // Cleanup
    client2.close().await.unwrap();
    server.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 5: Concurrent message sending from multiple clients
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_message_sending() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("concurrent.sock");
    let sock_str = sock.to_str().unwrap();

    let server = IpcServer::new(sock_str);
    server.start().await.unwrap();
    tokio::task::yield_now().await;

    // Spawn 5 tasks, each connecting a fresh client and sending a message.
    // Each task returns the message ID it sent so we can verify on the server.
    let mut join_handles = Vec::new();
    let mut expected_ids: HashSet<Uuid> = HashSet::new();

    // Use a barrier to synchronize all 5 clients so they send at the same time.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(5));

    for _ in 0..5 {
        let path = sock_str.to_string();
        let barrier = barrier.clone();
        let msg_id = Uuid::new_v4();
        expected_ids.insert(msg_id);

        let handle = tokio::spawn(async move {
            let client = IpcClient::connect(&path, ProcessType::Agent, vec![])
                .await
                .expect("client connect failed");

            let msg = IpcMessage {
                id: msg_id,
                source: client.process_id().clone(),
                target: ProcessTarget::Coordinator,
                payload: IpcPayload::HealthPing,
            };

            // Wait until all 5 clients are connected and ready
            barrier.wait().await;

            client.send(&msg).await.expect("concurrent send failed");
            client.close().await.unwrap();
            msg_id
        });
        join_handles.push(handle);
    }

    // Wait for all spawned tasks to finish sending
    for handle in join_handles {
        timeout(TEST_TIMEOUT, handle)
            .await
            .expect("task timed out")
            .expect("task panicked");
    }

    // Server receives 5 registration messages + 5 data messages = 10 total.
    // Registrations come first (per connection), but order across connections
    // is nondeterministic, so we just collect everything and filter.
    let mut received_ids: HashSet<Uuid> = HashSet::new();
    let mut registration_count = 0;

    for _ in 0..10 {
        let msg = timeout(TEST_TIMEOUT, server.recv())
            .await
            .expect("server recv timed out during concurrent test")
            .expect("server recv failed during concurrent test");

        match &msg.payload {
            IpcPayload::Registration(_) => {
                registration_count += 1;
            }
            IpcPayload::HealthPing => {
                received_ids.insert(msg.id);
            }
            other => panic!("unexpected payload: {:?}", other),
        }
    }

    assert_eq!(
        registration_count, 5,
        "should have received 5 registrations"
    );
    assert_eq!(
        received_ids, expected_ids,
        "all 5 HealthPing message IDs should be received"
    );

    server.shutdown().await.unwrap();
}
