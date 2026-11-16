//! Discord local IPC transport: socket discovery, frame codec and handshake.
//!
//! Wire format of every frame: `opcode: u32 LE`, `length: u32 LE`, then
//! `length` bytes of UTF-8 JSON.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// Upper bound for a single frame payload; protects against garbage lengths.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Sub-directories (relative to the runtime/temp dir) where Discord may put
/// its socket: native, Flatpak and Snap installs.
const SOCKET_SUBDIRS: &[&str] = &["", "app/com.discordapp.Discord", "snap.discord"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Handshake = 0,
    Frame = 1,
    Close = 2,
    Ping = 3,
    Pong = 4,
}

impl TryFrom<u32> for Opcode {
    type Error = anyhow::Error;

    fn try_from(value: u32) -> Result<Self> {
        Ok(match value {
            0 => Opcode::Handshake,
            1 => Opcode::Frame,
            2 => Opcode::Close,
            3 => Opcode::Ping,
            4 => Opcode::Pong,
            other => bail!("unknown IPC opcode {other}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub opcode: Opcode,
    pub payload: Value,
}

/// Encodes one frame into its wire representation.
pub fn encode_frame(opcode: Opcode, payload: &Value) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(payload).context("failed to serialize IPC payload")?;
    let len = u32::try_from(body.len()).context("IPC payload too large")?;
    if len > MAX_FRAME_LEN {
        bail!("IPC payload too large ({len} bytes)");
    }
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(opcode as u32).to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parses an 8-byte frame header into `(opcode, length)`.
pub fn decode_header(header: [u8; 8]) -> Result<(Opcode, u32)> {
    let opcode = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let len = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if len > MAX_FRAME_LEN {
        bail!("IPC frame too large ({len} bytes)");
    }
    Ok((Opcode::try_from(opcode)?, len))
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let mut header = [0u8; 8];
    reader
        .read_exact(&mut header)
        .await
        .context("failed to read IPC frame header (socket closed?)")?;
    let (opcode, len) = decode_header(header)?;
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .await
        .context("failed to read IPC frame body")?;
    let payload = serde_json::from_slice(&body).context("IPC frame body is not valid JSON")?;
    Ok(Frame { opcode, payload })
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    opcode: Opcode,
    payload: &Value,
) -> Result<()> {
    let bytes = encode_frame(opcode, payload)?;
    writer
        .write_all(&bytes)
        .await
        .context("failed to write IPC frame")?;
    writer.flush().await.context("failed to flush IPC frame")?;
    Ok(())
}

/// Lists candidate socket paths in priority order: for each index 0..=9,
/// the native, Flatpak and Snap locations under each base directory.
pub fn candidate_paths(base_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for i in 0..10 {
        for base in base_dirs {
            for sub in SOCKET_SUBDIRS {
                let dir: PathBuf = if sub.is_empty() {
                    base.clone()
                } else {
                    base.join(sub)
                };
                let path = dir.join(format!("discord-ipc-{i}"));
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Base directories to search: `$XDG_RUNTIME_DIR`, then the temp dir
/// variables, then `/tmp`.
pub fn default_base_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    if let Some(runtime) = dirs::runtime_dir() {
        dirs_out.push(runtime);
    }
    for var in ["TMPDIR", "TMP", "TEMP"] {
        if let Some(v) = std::env::var_os(var) {
            if !v.is_empty() {
                dirs_out.push(PathBuf::from(v));
            }
        }
    }
    dirs_out.push(PathBuf::from("/tmp"));
    dirs_out.dedup();
    dirs_out
}

/// An open, handshaken connection to the Discord client.
pub struct IpcConnection {
    stream: UnixStream,
}

impl IpcConnection {
    /// Connects to `socket_path` if given (the `socket_path` setting),
    /// otherwise to the first reachable socket among the default candidates.
    pub async fn connect(socket_path: Option<&Path>) -> Result<(Self, PathBuf)> {
        if let Some(path) = socket_path {
            let conn = Self::connect_path(path).await?;
            return Ok((conn, path.to_path_buf()));
        }
        let candidates = candidate_paths(&default_base_dirs());
        for path in &candidates {
            if !path.exists() {
                continue;
            }
            match Self::connect_path(path).await {
                Ok(conn) => return Ok((conn, path.clone())),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "socket not usable")
                }
            }
        }
        bail!(
            "no Discord IPC socket found (is the Discord desktop client running?); searched {} paths",
            candidates.len()
        )
    }

    pub async fn connect_path(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("failed to connect to {}", path.display()))?;
        Ok(Self { stream })
    }

    /// Sends the v1 handshake and waits for the READY dispatch.
    /// Returns the READY payload.
    pub async fn handshake(&mut self, client_id: &str) -> Result<Value> {
        handshake(&mut self.stream, client_id).await
    }

    pub async fn send(&mut self, payload: &Value) -> Result<()> {
        write_frame(&mut self.stream, Opcode::Frame, payload).await
    }

    /// Reads the next FRAME payload, answering PINGs transparently.
    pub async fn recv(&mut self) -> Result<Value> {
        recv_payload(&mut self.stream).await
    }
}

pub fn handshake_payload(client_id: &str) -> Value {
    json!({ "v": 1, "client_id": client_id })
}

async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    client_id: &str,
) -> Result<Value> {
    write_frame(stream, Opcode::Handshake, &handshake_payload(client_id)).await?;
    let ready = recv_payload(stream).await.context("handshake failed")?;
    if ready.get("evt").and_then(Value::as_str) != Some("READY") {
        bail!("unexpected handshake reply (evt = {:?})", ready.get("evt"));
    }
    Ok(ready)
}

async fn recv_payload<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Value> {
    loop {
        let frame = read_frame(stream).await?;
        match frame.opcode {
            Opcode::Frame => return Ok(frame.payload),
            Opcode::Ping => write_frame(stream, Opcode::Pong, &frame.payload).await?,
            Opcode::Pong => {}
            Opcode::Close => {
                let code = frame.payload.get("code").cloned().unwrap_or(Value::Null);
                let message = frame
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                bail!("Discord closed the IPC connection (code {code}): {message}");
            }
            Opcode::Handshake => bail!("unexpected HANDSHAKE frame from Discord"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pinned_handshake_frame() {
        let bytes = encode_frame(Opcode::Handshake, &handshake_payload("123")).unwrap();
        let body = br#"{"client_id":"123","v":1}"#;
        let mut expected = vec![0, 0, 0, 0];
        expected.extend_from_slice(&(body.len() as u32).to_le_bytes());
        expected.extend_from_slice(body);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn decodes_header_little_endian() {
        let (op, len) = decode_header([1, 0, 0, 0, 0x2a, 0x01, 0, 0]).unwrap();
        assert_eq!(op, Opcode::Frame);
        assert_eq!(len, 0x012a);
    }

    #[test]
    fn rejects_unknown_opcode_and_oversized_frame() {
        assert!(decode_header([9, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let too_big = (MAX_FRAME_LEN + 1).to_le_bytes();
        let mut h = [1, 0, 0, 0, 0, 0, 0, 0];
        h[4..].copy_from_slice(&too_big);
        assert!(decode_header(h).is_err());
    }

    #[tokio::test]
    async fn frame_roundtrip_through_stream() {
        let payload = json!({"cmd": "DISPATCH", "evt": "READY", "data": {"v": 1}});
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, Opcode::Frame, &payload).await.unwrap();
        let frame = read_frame(&mut b).await.unwrap();
        assert_eq!(
            frame,
            Frame {
                opcode: Opcode::Frame,
                payload
            }
        );
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error() {
        let mut bytes = encode_frame(Opcode::Frame, &json!({"a": 1})).unwrap();
        bytes.truncate(bytes.len() - 2);
        let mut reader = &bytes[..];
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn handshake_answers_ping_and_returns_ready() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let hs = read_frame(&mut server).await.unwrap();
            assert_eq!(hs.opcode, Opcode::Handshake);
            assert_eq!(hs.payload, json!({"v": 1, "client_id": "42"}));
            write_frame(&mut server, Opcode::Ping, &json!({"n": 1}))
                .await
                .unwrap();
            let pong = read_frame(&mut server).await.unwrap();
            assert_eq!(pong.opcode, Opcode::Pong);
            let ready = json!({"cmd": "DISPATCH", "evt": "READY", "data": {"v": 1}, "nonce": null});
            write_frame(&mut server, Opcode::Frame, &ready)
                .await
                .unwrap();
        });
        let ready = handshake(&mut client, "42").await.unwrap();
        assert_eq!(ready["evt"], "READY");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn close_frame_becomes_error() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_frame(
            &mut server,
            Opcode::Close,
            &json!({"code": 4000, "message": "Invalid Client ID"}),
        )
        .await
        .unwrap();
        let err = recv_payload(&mut client).await.unwrap_err().to_string();
        assert!(
            err.contains("4000") && err.contains("Invalid Client ID"),
            "{err}"
        );
    }

    #[test]
    fn candidate_paths_cover_native_flatpak_snap() {
        let paths = candidate_paths(&[PathBuf::from("/run/user/1000")]);
        assert_eq!(paths.len(), 30);
        assert_eq!(paths[0], PathBuf::from("/run/user/1000/discord-ipc-0"));
        assert_eq!(
            paths[1],
            PathBuf::from("/run/user/1000/app/com.discordapp.Discord/discord-ipc-0")
        );
        assert_eq!(
            paths[2],
            PathBuf::from("/run/user/1000/snap.discord/discord-ipc-0")
        );
        assert_eq!(
            paths[29],
            PathBuf::from("/run/user/1000/snap.discord/discord-ipc-9")
        );
    }

    #[tokio::test]
    async fn connect_uses_socket_path_override() {
        let dir = std::env::temp_dir().join(format!("discord-voice-ipc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom-ipc");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.is_ok() });
        let (_conn, used) = IpcConnection::connect(Some(&path)).await.unwrap();
        assert_eq!(used, path);
        assert!(accept.await.unwrap());
        // An override that does not work is an error, not a fallback.
        assert!(IpcConnection::connect(Some(&dir.join("missing")))
            .await
            .is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
