//! Platform-abstracted IPC transport.
//!
//! Implements `IpcTransport` over Unix domain sockets using length-prefixed
//! JSON framing. Each message is sent as a 4-byte big-endian length prefix
//! followed by the JSON payload bytes.

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::ipc::IpcMessage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::codec;

/// Bidirectional IPC channel between processes.
#[async_trait]
pub trait IpcTransport: Send + Sync {
    async fn send(&self, message: &IpcMessage) -> Result<()>;
    async fn recv(&self) -> Result<IpcMessage>;
    async fn close(&self) -> Result<()>;
}

/// Unix domain socket transport implementing `IpcTransport`.
///
/// Wraps a `tokio::net::UnixStream` and provides length-prefixed JSON
/// message framing. The read and write halves are independently locked
/// so send and recv can proceed concurrently.
pub struct UnixTransport {
    reader: Mutex<tokio::net::unix::OwnedReadHalf>,
    writer: Mutex<tokio::net::unix::OwnedWriteHalf>,
}

impl UnixTransport {
    /// Create a transport from an already-connected `UnixStream`.
    pub fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
        }
    }

    /// Connect to a Unix socket at the given path.
    pub async fn connect(path: &str) -> Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self::new(stream))
    }
}

#[async_trait]
impl IpcTransport for UnixTransport {
    async fn send(&self, message: &IpcMessage) -> Result<()> {
        let data = codec::encode(message)?;
        let mut writer = self.writer.lock().await;
        writer.write_all(&data).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn recv(&self) -> Result<IpcMessage> {
        let mut reader = self.reader.lock().await;

        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                AthenError::Ipc("connection closed".to_string())
            } else {
                AthenError::from(e)
            }
        })?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len == 0 {
            return Err(AthenError::Ipc("received zero-length message".to_string()));
        }

        // Guard against unreasonably large messages (16 MiB limit)
        const MAX_MSG_SIZE: usize = 16 * 1024 * 1024;
        if len > MAX_MSG_SIZE {
            return Err(AthenError::Ipc(format!(
                "message too large: {len} bytes (max {MAX_MSG_SIZE})"
            )));
        }

        // Read the JSON payload
        let mut payload_buf = vec![0u8; len];
        reader.read_exact(&mut payload_buf).await?;

        codec::decode(&payload_buf)
    }

    async fn close(&self) -> Result<()> {
        // Shutting down the writer signals EOF to the peer
        let mut writer = self.writer.lock().await;
        writer.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::ipc::{IpcPayload, ProcessId, ProcessTarget, ProcessType};
    use uuid::Uuid;

    fn make_test_message() -> IpcMessage {
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

    #[tokio::test]
    async fn test_transport_send_recv_paired_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");
        let sock_path_str = sock_path.to_str().unwrap().to_string();

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let client_path = sock_path_str.clone();
        let client_handle = tokio::spawn(async move {
            let transport = UnixTransport::connect(&client_path).await.unwrap();
            let msg = make_test_message();
            let msg_id = msg.id;
            transport.send(&msg).await.unwrap();
            msg_id
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let server_transport = UnixTransport::new(server_stream);

        let sent_id = client_handle.await.unwrap();
        let received = server_transport.recv().await.unwrap();
        assert_eq!(received.id, sent_id);
    }

    #[tokio::test]
    async fn test_transport_bidirectional() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("bidir.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let path_str = sock_path.to_str().unwrap().to_string();
        let client_handle = tokio::spawn(async move {
            let transport = UnixTransport::connect(&path_str).await.unwrap();

            // Send a message
            let msg = make_test_message();
            let sent_id = msg.id;
            transport.send(&msg).await.unwrap();

            // Receive a reply
            let reply = transport.recv().await.unwrap();
            (sent_id, reply.id)
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let server_transport = UnixTransport::new(server_stream);

        // Receive from client
        let received = server_transport.recv().await.unwrap();

        // Send a reply
        let reply = make_test_message();
        let reply_id = reply.id;
        server_transport.send(&reply).await.unwrap();

        let (sent_id, received_reply_id) = client_handle.await.unwrap();
        assert_eq!(received.id, sent_id);
        assert_eq!(received_reply_id, reply_id);
    }

    #[tokio::test]
    async fn test_transport_multiple_messages() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("multi.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let path_str = sock_path.to_str().unwrap().to_string();
        let client_handle = tokio::spawn(async move {
            let transport = UnixTransport::connect(&path_str).await.unwrap();
            let mut ids = Vec::new();
            for _ in 0..5 {
                let msg = make_test_message();
                ids.push(msg.id);
                transport.send(&msg).await.unwrap();
            }
            ids
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let server_transport = UnixTransport::new(server_stream);

        let sent_ids = client_handle.await.unwrap();
        for expected_id in sent_ids {
            let received = server_transport.recv().await.unwrap();
            assert_eq!(received.id, expected_id);
        }
    }

    #[tokio::test]
    async fn test_transport_close() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("close.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let path_str = sock_path.to_str().unwrap().to_string();
        let client_handle = tokio::spawn(async move {
            let transport = UnixTransport::connect(&path_str).await.unwrap();
            transport.send(&make_test_message()).await.unwrap();
            transport.close().await.unwrap();
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let server_transport = UnixTransport::new(server_stream);

        // First recv should succeed
        let _ = server_transport.recv().await.unwrap();

        // Second recv should fail because client closed
        let result = server_transport.recv().await;
        assert!(result.is_err());

        client_handle.await.unwrap();
    }
}
