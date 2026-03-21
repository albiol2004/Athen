//! JSON-RPC 2.0 codec for IPC messages.
//!
//! Provides length-prefixed framing: each message is preceded by a 4-byte
//! big-endian u32 indicating the length of the JSON payload that follows.

use athen_core::error::{AthenError, Result};
use athen_core::ipc::IpcMessage;

/// Encode an `IpcMessage` into a length-prefixed byte buffer.
///
/// Format: [4 bytes big-endian length][JSON payload bytes]
pub fn encode(message: &IpcMessage) -> Result<Vec<u8>> {
    let json_bytes = serde_json::to_vec(message)?;
    let len = json_bytes.len() as u32;
    let mut buf = Vec::with_capacity(4 + json_bytes.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json_bytes);
    Ok(buf)
}

/// Decode an `IpcMessage` from a JSON byte slice (without the length prefix).
///
/// The caller is responsible for reading the 4-byte length prefix and then
/// reading exactly that many bytes before calling this function.
pub fn decode(data: &[u8]) -> Result<IpcMessage> {
    serde_json::from_slice(data).map_err(AthenError::from)
}

/// Read the 4-byte big-endian length prefix from a byte slice.
///
/// Returns `None` if the slice has fewer than 4 bytes.
pub fn read_length_prefix(data: &[u8]) -> Option<u32> {
    if data.len() < 4 {
        return None;
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&data[..4]);
    Some(u32::from_be_bytes(len_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::ipc::{ProcessId, ProcessTarget, ProcessType, IpcPayload};
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

    #[test]
    fn test_encode_decode_roundtrip() {
        let msg = make_test_message();
        let encoded = encode(&msg).unwrap();

        // First 4 bytes are the length prefix
        let len = read_length_prefix(&encoded).unwrap();
        assert_eq!(len as usize, encoded.len() - 4);

        // Decode the payload portion
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(decoded.id, msg.id);
        assert_eq!(decoded.source, msg.source);
    }

    #[test]
    fn test_length_prefix_encoding() {
        let msg = make_test_message();
        let encoded = encode(&msg).unwrap();
        let json_bytes = serde_json::to_vec(&msg).unwrap();

        // Verify length prefix matches actual JSON length
        let prefix_len = read_length_prefix(&encoded).unwrap();
        assert_eq!(prefix_len as usize, json_bytes.len());
    }

    #[test]
    fn test_read_length_prefix_too_short() {
        assert!(read_length_prefix(&[]).is_none());
        assert!(read_length_prefix(&[0, 1]).is_none());
        assert!(read_length_prefix(&[0, 1, 2]).is_none());
    }

    #[test]
    fn test_read_length_prefix_exact() {
        assert_eq!(read_length_prefix(&[0, 0, 0, 42]), Some(42));
        assert_eq!(read_length_prefix(&[0, 0, 1, 0]), Some(256));
    }

    #[test]
    fn test_decode_invalid_json() {
        let result = decode(b"not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_decode_health_pong() {
        use athen_core::ipc::ProcessHealthStatus;

        let msg = IpcMessage {
            id: Uuid::new_v4(),
            source: ProcessId {
                process_type: ProcessType::Coordinator,
                instance_id: Uuid::new_v4(),
            },
            target: ProcessTarget::Broadcast(ProcessType::Agent),
            payload: IpcPayload::HealthPong {
                status: ProcessHealthStatus::Healthy,
            },
        };

        let encoded = encode(&msg).unwrap();
        let len = read_length_prefix(&encoded).unwrap();
        let decoded = decode(&encoded[4..(4 + len as usize)]).unwrap();
        assert_eq!(decoded.id, msg.id);
    }
}
