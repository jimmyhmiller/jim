//! Wire protocol for the jim message bus.
//!
//! Length-prefixed bincode frames: `[u32 BE length][bincode payload]` —
//! the same shape as `jim_daemon::proto` and `claude_bus::proto`, so the
//! whole codebase sees one consistent IPC style.
//!
//! Two roles share the socket (chosen by the first `Hello` frame):
//!
//! * **Publisher** — sends `Hello { role: Publisher }` then one-or-more
//!   `Publish` frames, then closes. Fire-and-forget; the daemon never
//!   replies on the publisher channel.
//! * **Subscriber** — sends `Hello { role: Subscriber { since_seq } }`
//!   and stays connected. The daemon first replays the current retained
//!   store (bracketed by `ReplayStart` / `ReplayEnd`) so a late joiner
//!   learns current state (the agent roster, every retained topic), then
//!   streams live `Message` frames. `since_seq = Some(n)` instead resumes
//!   live delivery after seq `n` (reconnect), falling back to a full
//!   retained resync + `Lagged` if `n` aged out of the ring.
//!
//! Payloads ride as a JSON **string** (`payload_json`), never a
//! `serde_json::Value`: bincode is not self-describing, so it cannot
//! deserialize `Value` (which needs `deserialize_any`). Callers parse the
//! string on demand. This mirrors `claude_bus`.

use serde::{Deserialize, Serialize};

const LEN_PREFIX: usize = 4;

/// One bus message as it travels publisher → daemon → subscribers. Mirrors
/// the GUI's `PendingMsg` / `BusMessageObserved` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    /// Project channel. `Some(p)` is delivered only to widgets in project
    /// `p`; `None` is the GLOBAL channel (every widget) — the channel the
    /// cross-project `agent.*` bus rides on.
    pub project: Option<u64>,
    pub topic: String,
    /// Payload as a JSON string (opaque to the wire; parsed on demand).
    pub payload_json: String,
    /// Publishing participant's id.
    pub sender: String,
    /// Keep as the topic's retained last value for late joiners. A retained
    /// message whose payload is JSON `null` is a tombstone: it clears the
    /// topic from the retained store.
    pub retain: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Role {
    /// Will only send `Publish` frames, never reads from the socket.
    Publisher,
    /// Wants to receive messages. `since_seq = None` → full retained replay
    /// then live; `Some(n)` → resume live after seq `n` (reconnect).
    Subscriber { since_seq: Option<u64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientFrame {
    /// MUST be the first frame on every connection.
    Hello { role: Role },
    /// Publisher → daemon. One message. The daemon assigns a seq, updates
    /// the retained store (if `retain`), persists, and broadcasts.
    Publish(BusMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusFrame {
    /// Daemon → subscriber. Start of the retained-store replay segment.
    ReplayStart,
    /// Daemon → subscriber. One delivered message (live or replayed).
    Message { seq: u64, msg: BusMessage },
    /// Daemon → subscriber. Retained replay finished; everything after is
    /// live.
    ReplayEnd,
    /// Daemon → subscriber. A resume `since_seq` aged out of the ring; the
    /// daemon is sending a full retained resync instead. Informational.
    Lagged { requested: u64, replay_from: u64 },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard())?;
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(LEN_PREFIX + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode one frame from the head of `buf`. Returns `Ok(None)` if more
/// bytes are needed; `Err` only on a malformed payload (caller should drop
/// the connection).
pub fn decode<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, bincode::error::DecodeError> {
    if buf.len() < LEN_PREFIX {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let total = LEN_PREFIX + len;
    if buf.len() < total {
        return Ok(None);
    }
    let (msg, _) =
        bincode::serde::decode_from_slice(&buf[LEN_PREFIX..total], bincode::config::standard())?;
    Ok(Some((msg, total)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_publish() {
        let m = ClientFrame::Publish(BusMessage {
            project: None,
            topic: "agent.all".into(),
            payload_json: r#"{"from":"a","text":"hi"}"#.into(),
            sender: "a".into(),
            retain: false,
        });
        let bytes = encode(&m).unwrap();
        let (decoded, n): (ClientFrame, _) = decode(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        match decoded {
            ClientFrame::Publish(msg) => {
                assert_eq!(msg.topic, "agent.all");
                assert!(!msg.retain);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_partial_is_none() {
        let bytes = encode(&ClientFrame::Hello {
            role: Role::Subscriber { since_seq: None },
        })
        .unwrap();
        let res: Result<Option<(ClientFrame, _)>, _> = decode(&bytes[..bytes.len() - 1]);
        assert!(matches!(res, Ok(None)));
    }
}
