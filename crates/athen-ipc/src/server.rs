//! IPC server and client for the coordinator process.
//!
//! `IpcServer` listens on a Unix socket and manages connections from
//! monitors, agents, and UI processes. `IpcClient` connects to the
//! coordinator and provides send/recv over the transport.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::ipc::{IpcMessage, ProcessId, ProcessTarget, ProcessType};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::transport::{IpcTransport, UnixTransport};

/// IPC server that the coordinator runs to accept connections from
/// monitors, agents, and UI processes.
pub struct IpcServer {
    socket_path: String,
    /// Connected processes indexed by their ProcessId.
    connections: Arc<RwLock<HashMap<ProcessId, Arc<UnixTransport>>>>,
    /// Channel for delivering received messages to the coordinator.
    message_tx: tokio::sync::mpsc::Sender<IpcMessage>,
    /// Receiving end for incoming messages.
    message_rx: Mutex<tokio::sync::mpsc::Receiver<IpcMessage>>,
    /// Handle to the accept loop task so we can abort on shutdown.
    accept_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl IpcServer {
    /// Create a new IPC server bound to the given socket path.
    ///
    /// The server does not start accepting connections until `start()` is called.
    pub fn new(socket_path: &str) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        Self {
            socket_path: socket_path.to_string(),
            connections: Arc::new(RwLock::new(HashMap::new())),
            message_tx: tx,
            message_rx: Mutex::new(rx),
            accept_handle: Mutex::new(None),
        }
    }

    /// Start accepting connections on the socket.
    ///
    /// Spawns a background task that accepts new connections and spawns
    /// a reader task for each one. Received messages are forwarded to
    /// the internal channel and can be consumed via `recv()`.
    pub async fn start(&self) -> Result<()> {
        // Remove stale socket file if it exists
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(path = %self.socket_path, "IPC server listening");

        let connections = Arc::clone(&self.connections);
        let tx = self.message_tx.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let transport = Arc::new(UnixTransport::new(stream));
                        let connections = Arc::clone(&connections);
                        let tx = tx.clone();

                        tokio::spawn(async move {
                            Self::handle_connection(transport, connections, tx).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to accept connection");
                        break;
                    }
                }
            }
        });

        *self.accept_handle.lock().await = Some(handle);
        Ok(())
    }

    /// Handle a single connection: wait for the first message (expected to be
    /// a Registration), then register the process and read subsequent messages.
    async fn handle_connection(
        transport: Arc<UnixTransport>,
        connections: Arc<RwLock<HashMap<ProcessId, Arc<UnixTransport>>>>,
        tx: tokio::sync::mpsc::Sender<IpcMessage>,
    ) {
        // Read the first message to identify the process.
        // Even if it's not a Registration, we still forward it and use the source.
        let first_msg = match transport.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "connection dropped before identification");
                return;
            }
        };

        let process_id = first_msg.source.clone();
        tracing::info!(
            process_type = ?process_id.process_type,
            instance_id = %process_id.instance_id,
            "process connected"
        );

        // Register the connection
        {
            let mut conns = connections.write().await;
            conns.insert(process_id.clone(), Arc::clone(&transport));
        }

        // Forward the first message
        let _ = tx.send(first_msg).await;

        // Read loop
        loop {
            match transport.recv().await {
                Ok(msg) => {
                    if tx.send(msg).await.is_err() {
                        tracing::warn!("message channel closed, stopping reader");
                        break;
                    }
                }
                Err(e) => {
                    tracing::info!(
                        process_type = ?process_id.process_type,
                        instance_id = %process_id.instance_id,
                        error = %e,
                        "process disconnected"
                    );
                    break;
                }
            }
        }

        // Remove the connection
        {
            let mut conns = connections.write().await;
            conns.remove(&process_id);
        }
    }

    /// Receive the next message from any connected process.
    pub async fn recv(&self) -> Result<IpcMessage> {
        let mut rx = self.message_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| AthenError::Ipc("message channel closed".to_string()))
    }

    /// Send a message to a specific process by its `ProcessId`.
    pub async fn send_to(&self, process_id: &ProcessId, message: &IpcMessage) -> Result<()> {
        let conns = self.connections.read().await;
        let transport = conns
            .get(process_id)
            .ok_or_else(|| AthenError::Ipc(format!("process not found: {:?}", process_id)))?;
        transport.send(message).await
    }

    /// Broadcast a message to all connected processes.
    pub async fn broadcast(&self, message: &IpcMessage) -> Result<()> {
        let conns = self.connections.read().await;
        for (pid, transport) in conns.iter() {
            if let Err(e) = transport.send(message).await {
                tracing::warn!(
                    process_type = ?pid.process_type,
                    instance_id = %pid.instance_id,
                    error = %e,
                    "failed to send broadcast to process"
                );
            }
        }
        Ok(())
    }

    /// Broadcast a message to all connected processes of a specific type.
    pub async fn broadcast_to_type(
        &self,
        process_type: &ProcessType,
        message: &IpcMessage,
    ) -> Result<()> {
        let conns = self.connections.read().await;
        for (pid, transport) in conns.iter() {
            if &pid.process_type == process_type {
                if let Err(e) = transport.send(message).await {
                    tracing::warn!(
                        process_type = ?pid.process_type,
                        instance_id = %pid.instance_id,
                        error = %e,
                        "failed to send to process"
                    );
                }
            }
        }
        Ok(())
    }

    /// Route a message based on its `ProcessTarget`.
    pub async fn route(&self, message: &IpcMessage) -> Result<()> {
        match &message.target {
            ProcessTarget::Direct(pid) => self.send_to(pid, message).await,
            ProcessTarget::Coordinator => {
                // Message is for this server (the coordinator); nothing to route.
                Ok(())
            }
            ProcessTarget::Broadcast(ptype) => self.broadcast_to_type(ptype, message).await,
        }
    }

    /// Get the number of currently connected processes.
    pub async fn connected_count(&self) -> usize {
        self.connections.read().await.len()
    }

    /// Shut down the server: abort the accept loop and close all connections.
    pub async fn shutdown(&self) -> Result<()> {
        // Abort the accept loop
        if let Some(handle) = self.accept_handle.lock().await.take() {
            handle.abort();
        }

        // Close all connections
        let conns = self.connections.read().await;
        for (_, transport) in conns.iter() {
            let _ = transport.close().await;
        }

        // Clean up the socket file
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        tracing::info!(path = %self.socket_path, "IPC server shut down");
        Ok(())
    }
}

/// IPC client for monitors, agents, and UI to connect to the coordinator.
pub struct IpcClient {
    transport: Arc<UnixTransport>,
    process_id: ProcessId,
}

impl IpcClient {
    /// Connect to the coordinator at the given socket path.
    ///
    /// Sends a registration message immediately upon connecting.
    pub async fn connect(
        socket_path: &str,
        process_type: ProcessType,
        capabilities: Vec<String>,
    ) -> Result<Self> {
        let transport = Arc::new(UnixTransport::connect(socket_path).await?);
        let process_id = ProcessId {
            process_type: process_type.clone(),
            instance_id: Uuid::new_v4(),
        };

        // Send registration message
        let registration = IpcMessage {
            id: Uuid::new_v4(),
            source: process_id.clone(),
            target: ProcessTarget::Coordinator,
            payload: athen_core::ipc::IpcPayload::Registration(
                athen_core::ipc::ProcessRegistration {
                    process_type,
                    pid: std::process::id(),
                    capabilities,
                },
            ),
        };
        transport.send(&registration).await?;

        Ok(Self {
            transport,
            process_id,
        })
    }

    /// Get this client's process ID.
    pub fn process_id(&self) -> &ProcessId {
        &self.process_id
    }
}

#[async_trait]
impl IpcTransport for IpcClient {
    async fn send(&self, message: &IpcMessage) -> Result<()> {
        self.transport.send(message).await
    }

    async fn recv(&self) -> Result<IpcMessage> {
        self.transport.recv().await
    }

    async fn close(&self) -> Result<()> {
        self.transport.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::ipc::{IpcPayload, ProcessId, ProcessTarget, ProcessType};

    fn make_ping(source: &ProcessId) -> IpcMessage {
        IpcMessage {
            id: Uuid::new_v4(),
            source: source.clone(),
            target: ProcessTarget::Coordinator,
            payload: IpcPayload::HealthPing,
        }
    }

    #[tokio::test]
    async fn test_server_client_communication() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("server.sock");
        let sock_str = sock_path.to_str().unwrap();

        let server = IpcServer::new(sock_str);
        server.start().await.unwrap();

        // Give the server a moment to bind
        tokio::task::yield_now().await;

        // Connect a client
        let client = IpcClient::connect(sock_str, ProcessType::Monitor, vec!["email".into()])
            .await
            .unwrap();

        // Server should receive the registration message
        let reg_msg = server.recv().await.unwrap();
        match &reg_msg.payload {
            IpcPayload::Registration(reg) => {
                assert_eq!(reg.process_type, ProcessType::Monitor);
                assert!(reg.capabilities.contains(&"email".to_string()));
            }
            _ => panic!("expected Registration message"),
        }

        // Client sends a ping
        let ping = make_ping(client.process_id());
        let ping_id = ping.id;
        client.send(&ping).await.unwrap();

        // Server receives the ping
        let received = server.recv().await.unwrap();
        assert_eq!(received.id, ping_id);

        // Server sends a pong back to the client
        let pong = IpcMessage {
            id: Uuid::new_v4(),
            source: ProcessId {
                process_type: ProcessType::Coordinator,
                instance_id: Uuid::new_v4(),
            },
            target: ProcessTarget::Direct(client.process_id().clone()),
            payload: IpcPayload::HealthPong {
                status: athen_core::ipc::ProcessHealthStatus::Healthy,
            },
        };
        let pong_id = pong.id;
        server.send_to(client.process_id(), &pong).await.unwrap();

        let reply = client.recv().await.unwrap();
        assert_eq!(reply.id, pong_id);

        // Clean up
        client.close().await.unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_server_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("broadcast.sock");
        let sock_str = sock_path.to_str().unwrap();

        let server = IpcServer::new(sock_str);
        server.start().await.unwrap();
        tokio::task::yield_now().await;

        // Connect two clients
        let client1 = IpcClient::connect(sock_str, ProcessType::Agent, vec![])
            .await
            .unwrap();
        let client2 = IpcClient::connect(sock_str, ProcessType::Agent, vec![])
            .await
            .unwrap();

        // Drain the registration messages
        let _ = server.recv().await.unwrap();
        let _ = server.recv().await.unwrap();

        // Give connections time to register
        tokio::task::yield_now().await;

        assert_eq!(server.connected_count().await, 2);

        // Broadcast a message
        let broadcast_msg = IpcMessage {
            id: Uuid::new_v4(),
            source: ProcessId {
                process_type: ProcessType::Coordinator,
                instance_id: Uuid::new_v4(),
            },
            target: ProcessTarget::Broadcast(ProcessType::Agent),
            payload: IpcPayload::HealthPing,
        };
        let broadcast_id = broadcast_msg.id;
        server.broadcast(&broadcast_msg).await.unwrap();

        // Both clients should receive it
        let r1 = client1.recv().await.unwrap();
        let r2 = client2.recv().await.unwrap();
        assert_eq!(r1.id, broadcast_id);
        assert_eq!(r2.id, broadcast_id);

        // Clean up
        client1.close().await.unwrap();
        client2.close().await.unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_server_send_to_unknown_process() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("unknown.sock");
        let sock_str = sock_path.to_str().unwrap();

        let server = IpcServer::new(sock_str);
        server.start().await.unwrap();

        let unknown_pid = ProcessId {
            process_type: ProcessType::Agent,
            instance_id: Uuid::new_v4(),
        };
        let msg = IpcMessage {
            id: Uuid::new_v4(),
            source: ProcessId {
                process_type: ProcessType::Coordinator,
                instance_id: Uuid::new_v4(),
            },
            target: ProcessTarget::Direct(unknown_pid.clone()),
            payload: IpcPayload::HealthPing,
        };

        let result = server.send_to(&unknown_pid, &msg).await;
        assert!(result.is_err());

        server.shutdown().await.unwrap();
    }
}
