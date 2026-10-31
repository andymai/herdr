//! A pane backed by a herdr-core terminal instead of a local PTY.
//!
//! The pane speaks herdr-core's framed attach protocol over its Unix socket:
//! the core sends the first paint and then the program's output on a stream
//! channel, and the pane sends keys, terminal replies and size changes back.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::pty::actor::PtyReadResult;

const CONTROL_CHANNEL: u32 = 0;
const KIND_DATA: u8 = 0;
const KIND_CREDIT: u8 = 1;
const KIND_CLOSE: u8 = 2;
const KIND_RESIZE: u8 = 3;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_REQUEST_ID: u64 = 2;

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type EndCallback = Box<dyn FnOnce(CoreAttachEnd) + Send + 'static>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreAttachEnd {
    Exited,
    Closed(String),
    Disconnected,
}

pub(crate) struct CoreAttachConfig {
    pub pane_id: u32,
    pub terminal: String,
    pub rows: u16,
    pub cols: u16,
    pub on_read: ReadCallback,
    pub on_end: EndCallback,
}

pub(crate) struct CoreAttachStart {
    pub handle: CoreAttachHandle,
    pub cwd: Option<PathBuf>,
}

struct Frame {
    kind: u8,
    payload: Bytes,
}

#[derive(Clone)]
pub(crate) struct CoreAttachHandle {
    terminal: Arc<str>,
    frames: mpsc::UnboundedSender<Frame>,
    detached: Arc<AtomicBool>,
    size: Arc<Mutex<ViewSize>>,
}

/// The core gives a terminal the size of whichever viewer was active last, so
/// a pane nobody is looking at keeps its size to itself: otherwise every
/// mirrored terminal would shrink to the phone the moment it attached.
#[derive(Debug, Default)]
struct ViewSize {
    latest: Option<(u16, u16)>,
    sent: Option<(u16, u16)>,
    visible: bool,
}

impl CoreAttachHandle {
    pub(crate) fn spawn(config: CoreAttachConfig) -> io::Result<CoreAttachStart> {
        let CoreAttachConfig {
            pane_id,
            terminal,
            rows,
            cols,
            on_read,
            on_end,
        } = config;
        let address: Arc<str> = Arc::from(terminal.as_str());
        let path = socket_path()?;
        let mut stream = UnixStream::connect(&path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot reach herdr-core at {}: {err}", path.display()),
            )
        })?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        write_line(
            &mut stream,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"framed": true}}),
        )?;
        rpc_result(read_line(&mut stream)?)?;

        let attach = json!({
            "jsonrpc": "2.0",
            "id": ATTACH_REQUEST_ID,
            "method": "terminal.attach",
            "params": {"terminal": terminal, "cols": cols, "rows": rows, "resumed": true},
        });
        write_frame(
            &mut stream,
            CONTROL_CHANNEL,
            KIND_DATA,
            &serde_json::to_vec(&attach).map_err(io::Error::other)?,
        )?;

        let mut early = Vec::new();
        let result = loop {
            let (channel, kind, payload) = read_frame(&mut stream)?;
            if channel != CONTROL_CHANNEL {
                early.push((channel, kind, payload));
                continue;
            }
            if kind != KIND_DATA {
                continue;
            }
            let message: Value = serde_json::from_slice(&payload).map_err(io::Error::other)?;
            if message.get("id").and_then(Value::as_u64) == Some(ATTACH_REQUEST_ID) {
                break rpc_result(message)?;
            }
        };
        let channel = result
            .get("channel")
            .and_then(Value::as_u64)
            .and_then(|channel| u32::try_from(channel).ok())
            .ok_or_else(|| io::Error::other("terminal.attach reply has no channel"))?;
        let cwd = result
            .pointer("/terminal/cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let early = early
            .into_iter()
            .filter(|(early_channel, _, _)| *early_channel == channel)
            .map(|(_, kind, payload)| (kind, payload))
            .collect();
        stream.set_read_timeout(None)?;

        let reader = stream.try_clone()?;
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let detached = Arc::new(AtomicBool::new(false));

        crate::thread_spawn::spawn_named(&format!("core-attach-write-{pane_id}"), move || {
            write_loop(stream, channel, frames_rx)
        })?;
        {
            let frames = frames_tx.clone();
            let detached = detached.clone();
            crate::thread_spawn::spawn_named(&format!("core-attach-read-{pane_id}"), move || {
                let end = read_loop(&reader, channel, early, on_read, &frames);
                let _ = reader.shutdown(Shutdown::Both);
                if !detached.load(Ordering::Acquire) {
                    debug!(pane = pane_id, ?end, "core attach ended");
                    on_end(end);
                }
            })?;
        }

        Ok(CoreAttachStart {
            handle: Self {
                terminal: address,
                frames: frames_tx,
                detached,
                size: Arc::new(Mutex::new(ViewSize {
                    latest: Some((rows, cols)),
                    ..ViewSize::default()
                })),
            },
            cwd,
        })
    }

    pub(crate) fn terminal(&self) -> &str {
        &self.terminal
    }

    /// Typing means someone is looking, so the pane's size goes first.
    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.send_pending_size(true);
        self.write_data(bytes)
    }

    pub(crate) fn write_terminal_response(&self, bytes: Bytes) {
        let _ = self.write_data(bytes);
    }

    pub(crate) fn resize(&self, rows: u16, cols: u16) {
        let visible = {
            let mut size = self.lock_size();
            size.latest = Some((rows, cols));
            size.visible
        };
        self.send_pending_size(visible);
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        let became_visible = {
            let mut size = self.lock_size();
            let became_visible = visible && !size.visible;
            size.visible = visible;
            became_visible
        };
        if became_visible {
            self.send_pending_size(true);
        }
    }

    fn send_pending_size(&self, allowed: bool) {
        if !allowed {
            return;
        }
        let pending = {
            let mut size = self.lock_size();
            match size.latest {
                Some(latest) if size.sent != Some(latest) => {
                    size.sent = Some(latest);
                    Some(latest)
                }
                _ => None,
            }
        };
        if let Some((rows, cols)) = pending {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&rows.to_le_bytes());
            self.send(KIND_RESIZE, payload.into());
        }
    }

    fn lock_size(&self) -> std::sync::MutexGuard<'_, ViewSize> {
        self.size
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_data(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        if self.detached.load(Ordering::Acquire) {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        self.frames
            .send(Frame {
                kind: KIND_DATA,
                payload: bytes,
            })
            .map_err(|err| mpsc::error::TrySendError::Closed(err.0.payload))
    }

    pub(crate) fn shutdown(&self) {
        if !self.detached.swap(true, Ordering::AcqRel) {
            self.send(KIND_CLOSE, Bytes::new());
        }
    }

    fn send(&self, kind: u8, payload: Bytes) {
        let _ = self.frames.send(Frame { kind, payload });
    }
}

fn write_loop(mut stream: UnixStream, channel: u32, mut frames: mpsc::UnboundedReceiver<Frame>) {
    while let Some(frame) = frames.blocking_recv() {
        if let Err(err) = write_frame(&mut stream, channel, frame.kind, &frame.payload) {
            debug!(err = %err, "core attach write failed");
            break;
        }
        if frame.kind == KIND_CLOSE {
            // The reader would otherwise block until the core acknowledges the
            // close, which leaks its thread if the reply never comes.
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
    }
}

fn read_loop(
    mut stream: &UnixStream,
    channel: u32,
    early: Vec<(u8, Vec<u8>)>,
    mut on_read: ReadCallback,
    frames: &mpsc::UnboundedSender<Frame>,
) -> CoreAttachEnd {
    for (kind, payload) in early {
        if let Some(end) = apply_frame(kind, &payload, &mut on_read, frames) {
            return end;
        }
    }
    loop {
        match read_frame(&mut stream) {
            Ok((frame_channel, kind, payload)) if frame_channel == channel => {
                if let Some(end) = apply_frame(kind, &payload, &mut on_read, frames) {
                    return end;
                }
            }
            Ok(_) => {}
            Err(err) => {
                debug!(err = %err, "core attach read ended");
                return CoreAttachEnd::Disconnected;
            }
        }
    }
}

fn apply_frame(
    kind: u8,
    payload: &[u8],
    on_read: &mut ReadCallback,
    frames: &mpsc::UnboundedSender<Frame>,
) -> Option<CoreAttachEnd> {
    match kind {
        KIND_DATA => {
            let result = on_read(payload);
            for response in result.terminal_responses {
                let _ = frames.send(Frame {
                    kind: KIND_DATA,
                    payload: response,
                });
            }
            let consumed = u32::try_from(payload.len()).unwrap_or(u32::MAX);
            let _ = frames.send(Frame {
                kind: KIND_CREDIT,
                payload: Bytes::copy_from_slice(&consumed.to_le_bytes()),
            });
            None
        }
        KIND_CLOSE => Some(close_reason(payload)),
        _ => None,
    }
}

fn close_reason(payload: &[u8]) -> CoreAttachEnd {
    let reason = serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match reason.as_deref() {
        Some("exited") => CoreAttachEnd::Exited,
        Some(reason) => CoreAttachEnd::Closed(reason.to_owned()),
        None => CoreAttachEnd::Closed("detached".to_owned()),
    }
}

// herdr-core can also take its socket from its own config file; HC_SOCKET
// covers that case here.
pub(crate) fn socket_path() -> io::Result<PathBuf> {
    if let Some(socket) = non_empty_env("HC_SOCKET") {
        return Ok(PathBuf::from(socket));
    }
    let session = non_empty_env("HC_SESSION").unwrap_or_else(|| "default".to_owned());
    let runtime_dir = non_empty_env("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
            dir.is_dir().then_some(dir)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no runtime directory for the herdr-core socket; set HC_SOCKET",
            )
        })?;
    Ok(runtime_dir
        .join("herdr-core")
        .join(format!("{session}.sock")))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn rpc_result(message: Value) -> io::Result<Value> {
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("herdr-core returned an error");
        warn!(error = %error, "herdr-core request failed");
        return Err(io::Error::other(text.to_owned()));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| io::Error::other("herdr-core reply has no result"))
}

pub(crate) fn write_line(stream: &mut UnixStream, message: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

// Byte-at-a-time so nothing past the newline is consumed: the core switches to
// framed mode right after this reply.
pub(crate) fn read_line(stream: &mut UnixStream) -> io::Result<Value> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    serde_json::from_slice(&line).map_err(io::Error::other)
}

fn write_frame(stream: &mut UnixStream, channel: u32, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(9 + payload.len());
    frame.extend_from_slice(&channel.to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn read_frame(stream: &mut impl Read) -> io::Result<(u32, u8, Vec<u8>)> {
    let mut header = [0u8; 9];
    stream.read_exact(&mut header)?;
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = header[4];
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("herdr-core frame of {len} bytes is over the 16 MiB limit"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((channel, kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_frame(&mut a, 7, KIND_RESIZE, &[80, 0, 24, 0]).unwrap();
        let (channel, kind, payload) = read_frame(&mut b).unwrap();
        assert_eq!(
            (channel, kind, payload),
            (7, KIND_RESIZE, vec![80, 0, 24, 0])
        );
    }

    #[test]
    fn close_payload_maps_to_end() {
        assert_eq!(
            close_reason(br#"{"reason":"exited","exit_code":0,"signal":null}"#),
            CoreAttachEnd::Exited
        );
        assert_eq!(
            close_reason(br#"{"reason":"server_stop"}"#),
            CoreAttachEnd::Closed("server_stop".to_owned())
        );
        assert_eq!(
            close_reason(b""),
            CoreAttachEnd::Closed("detached".to_owned())
        );
    }

    fn handle() -> (CoreAttachHandle, mpsc::UnboundedReceiver<Frame>) {
        let (frames, received) = mpsc::unbounded_channel();
        let handle = CoreAttachHandle {
            terminal: Arc::from("t1"),
            frames,
            detached: Arc::new(AtomicBool::new(false)),
            size: Arc::new(Mutex::new(ViewSize {
                latest: Some((40, 120)),
                ..ViewSize::default()
            })),
        };
        (handle, received)
    }

    fn resizes(received: &mut mpsc::UnboundedReceiver<Frame>) -> Vec<(u16, u16)> {
        let mut sizes = Vec::new();
        while let Ok(frame) = received.try_recv() {
            if frame.kind == KIND_RESIZE {
                let cols = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
                let rows = u16::from_le_bytes([frame.payload[2], frame.payload[3]]);
                sizes.push((rows, cols));
            }
        }
        sizes
    }

    #[test]
    fn hidden_panes_keep_their_size_until_seen() {
        let (handle, mut received) = handle();
        handle.resize(30, 90);
        assert!(resizes(&mut received).is_empty());
        handle.write_terminal_response(Bytes::from_static(b"\x1b[0n"));
        assert!(resizes(&mut received).is_empty());
        handle.set_visible(true);
        assert_eq!(resizes(&mut received), vec![(30, 90)]);
        handle.resize(30, 90);
        handle.set_visible(true);
        assert!(resizes(&mut received).is_empty());
        handle.resize(20, 60);
        assert_eq!(resizes(&mut received), vec![(20, 60)]);
    }

    #[test]
    fn typing_into_a_hidden_pane_sends_its_size_first() {
        let (handle, mut received) = handle();
        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .unwrap();
        assert_eq!(resizes(&mut received), vec![(40, 120)]);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_le_bytes());
        header.push(KIND_DATA);
        header.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        let err = read_frame(&mut header.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
