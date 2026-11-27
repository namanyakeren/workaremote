//! Wire protocol shared by the agent, the relay server and the browser viewer.
//!
//! Transport is a WebSocket. Control messages are JSON text frames, media is
//! binary frames. Everything is little endian.

use serde::{Deserialize, Serialize};

/// Binary frame markers.
pub const MSG_TILES: u8 = 1;
pub const MSG_FILE: u8 = 2;
pub const MSG_AUDIO: u8 = 3;
pub const MSG_VIDEO: u8 = 4;

/// Size of one tile in pixels. Multiple of 16 so JPEG blocks line up.
pub const TILE: u16 = 256;

/// Pseudo monitor index meaning "the whole virtual desktop".
pub const MON_ALL: u8 = 255;

/// Audio is fixed format: signed 16 bit little endian, mono, 48 kHz.
pub const AUDIO_RATE: u32 = 48_000;
/// 20 ms per packet.
pub const AUDIO_FRAME: usize = 960 * 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub i: u8,
    pub x: i32,
    pub y: i32,
    pub w: u16,
    pub h: u16,
    pub primary: bool,
    #[serde(default)]
    pub name: String,
}

/// Agent -> server -> viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    Register {
        agent_id: String,
        secret: String,
        #[serde(default)]
        enroll: String,
        hostname: String,
        os: String,
        version: String,
        /// True when running as the SYSTEM helper: unlocks secure desktop,
        /// SAS and service features.
        #[serde(default)]
        elevated: bool,
    },
    /// Reply to a `SessionRequest`. `reason == Some("password")` means the
    /// viewer must supply (or retry) a password.
    Answer {
        sid: String,
        accept: bool,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Monitor layout of the machine being shared.
    Screen {
        monitors: Vec<Monitor>,
    },
    /// Clipboard text changed on the target machine.
    Clipboard {
        text: String,
    },
    /// Offer a file to the viewer (download).
    FileOffer {
        id: String,
        name: String,
        size: u64,
    },
    FileAccept {
        id: String,
    },
    FileReject {
        id: String,
        reason: String,
    },
    FileDone {
        id: String,
        ok: bool,
        #[serde(default)]
        error: String,
    },
    FileData {
        id: String,
    },
    /// Answer to `Sas`/`Audio`/`Video` when the platform cannot comply.
    Unsupported {
        what: String,
    },
}

/// Server -> agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerToAgent {
    Registered {
        agent_id: String,
    },
    Reject {
        code: String,
        msg: String,
    },
    SessionRequest {
        sid: String,
        from: String,
        #[serde(default)]
        password: Option<String>,
    },
    SessionEnd {
        sid: String,
    },
}

/// Viewer -> server -> agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ViewerMsg {
    Move {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: u8,
        down: bool,
    },
    Wheel {
        x: f32,
        y: f32,
        dx: i32,
        dy: i32,
    },
    Key {
        code: String,
        key: String,
        down: bool,
        /// bit 0 shift, 1 ctrl, 2 alt, 3 meta. Windows needs it to decide
        /// between unicode injection and virtual-key injection.
        #[serde(default)]
        mods: u8,
    },
    Monitor {
        i: u8,
    },
    Clipboard {
        text: String,
    },
    /// Ctrl+Alt+Del on the target.
    Sas,
    Audio {
        on: bool,
    },
    /// Ask for H.264 instead of JPEG tiles.
    Video {
        codec: String,
    },
    /// Upload: viewer offers a file, agent answers with `file_accept`.
    FileOffer {
        id: String,
        name: String,
        size: u64,
    },
    /// Download: viewer asks for a path on the target machine.
    FileGet {
        id: String,
        path: String,
    },
}

/// Server -> viewer, control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ViewerEvent {
    Waiting,
    Connected,
    NeedPassword,
    Denied {
        reason: String,
    },
    End {
        reason: String,
    },
    /// Agent accepted an upload offer.
    FileAccept {
        id: String,
    },
    FileReject {
        id: String,
        reason: String,
    },
}

/// Encodes one binary tile batch. Tiles are `(x, y, w, h, jpeg)`.
pub fn encode_tiles(tiles: &[(u16, u16, u16, u16, &[u8])]) -> Vec<u8> {
    let size = 3 + tiles.iter().map(|t| 8 + 4 + t.4.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.push(MSG_TILES);
    out.extend_from_slice(&(tiles.len() as u16).to_le_bytes());
    for (x, y, w, h, jpeg) in tiles {
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        out.extend_from_slice(jpeg);
    }
    out
}

/// `[MSG_FILE][id_len][id][bytes]`
pub fn encode_file_chunk(id: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + id.len() + data.len());
    out.push(MSG_FILE);
    out.push(id.len() as u8);
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(data);
    out
}

pub fn decode_file_chunk(buf: &[u8]) -> Option<(String, &[u8])> {
    if buf.len() < 3 || buf[0] != MSG_FILE {
        return None;
    }
    let n = buf[1] as usize;
    if buf.len() < 2 + n {
        return None;
    }
    let id = std::str::from_utf8(&buf[2..2 + n]).ok()?.to_string();
    Some((id, &buf[2 + n..]))
}

/// `[MSG_AUDIO][u16 seq][pcm]`
pub fn encode_audio(seq: u16, pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + pcm.len());
    out.push(MSG_AUDIO);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// `[MSG_VIDEO][u8 flags][u32 ts][annex-b]`
pub fn encode_video(keyframe: bool, ts_ms: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + data.len());
    out.push(MSG_VIDEO);
    out.push(keyframe as u8);
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Rejects names that would escape the download directory.
pub fn safe_file_name(name: &str) -> String {
    let base = name
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or("file");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .map(|c| {
            if c == ':' || c == '*' || c == '?' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_chunk_roundtrip() {
        let buf = encode_file_chunk("abc123", b"hello");
        let (id, data) = decode_file_chunk(&buf).unwrap();
        assert_eq!(id, "abc123");
        assert_eq!(data, b"hello");
    }

    #[test]
    fn file_names_cannot_escape() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Windows\\win.ini"), "win.ini");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name("a\0b"), "ab");
        assert_eq!(safe_file_name("normal.txt"), "normal.txt");
    }

    #[test]
    fn media_headers_are_little_endian() {
        let audio = encode_audio(0x1234, &[1, 2]);
        assert_eq!(&audio[..4], &[MSG_AUDIO, 0x34, 0x12, 1]);
        let video = encode_video(true, 0x12345678, &[9]);
        assert_eq!(&video[..6], &[MSG_VIDEO, 1, 0x78, 0x56, 0x34, 0x12]);
    }
}
