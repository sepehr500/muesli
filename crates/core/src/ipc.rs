//! Daemon IPC: newline-delimited JSON over a Unix socket at `paths::socket_path()`.
//!
//! Client → daemon: one `Request` per line. Daemon replies with one `Response` line.
//! `{"cmd":"watch"}` replies once, then streams `Event` lines until the client disconnects.
//!
//! Example session:
//!   → {"cmd":"status"}
//!   ← {"ok":true,"data":{"session":{"kind":"idle"},"auto_record":true,...}}

use crate::detect::App;
use crate::model::{MeetingId, Pipeline};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Watch,
    /// Manual start, or accept a prompt.
    Start,
    /// Accept the "Record?" prompt.
    Record,
    /// Decline the prompt for this call.
    Ignore,
    Stop,
    Discard,
    SetAutoRecord { value: bool },
    /// Set the title; `confirm` marks the naming step done.
    Rename { id: MeetingId, title: String, #[serde(default = "yes")] confirm: bool },
    /// Mark naming done without changing the title ("Skip for now" keeps it unconfirmed;
    /// this is for accepting the current suggestion).
    ConfirmTitle { id: MeetingId },
    /// Re-run the post-call pipeline from the first unfinished step.
    Reprocess { id: MeetingId },
    /// Re-run only note enhancement (e.g. after editing your own notes).
    Enhance { id: MeetingId },
    Delete { id: MeetingId },
    /// Rename the person shown as `from` (e.g. "Speaker 2") everywhere in this meeting.
    /// Giving two speakers the same name merges them; an empty `to` resets the name.
    RenameSpeaker { id: MeetingId, from: String, to: String },
    /// Make sure the meeting has a playback copy (built from the raw audio on first use) and
    /// return its directory as `{"dir": "..."}`.
    PreparePlayback { id: MeetingId },
    /// The daemon reloads config.toml.
    ReloadConfig,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Response { ok: true, data: serde_json::to_value(data).ok(), error: None }
    }
    pub fn empty() -> Self {
        Response { ok: true, data: None, error: None }
    }
    pub fn err(e: impl std::fmt::Display) -> Self {
        Response { ok: false, data: None, error: Some(e.to_string()) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Full snapshot; sent on every change. Clients can be stateless.
    Status(Status),
    /// Input levels 0..1 while recording, ~8 Hz.
    Levels { me: f32, them: f32 },
    /// A meeting row changed (title, notes, pipeline...). UIs should reload it.
    MeetingUpdated { id: MeetingId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionView {
    Idle,
    Prompting { app: App, hint: Option<String>, since: i64 },
    Recording {
        meeting_id: MeetingId,
        app: Option<App>,
        hint: Option<String>,
        /// Unix ms.
        started_at: i64,
        manual: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessingView {
    pub meeting_id: MeetingId,
    pub title: String,
    pub app: Option<App>,
    pub pipeline: Pipeline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeedsNameView {
    pub meeting_id: MeetingId,
    /// Current (suggested or fallback) title.
    pub title: String,
    pub suggestions: Vec<String>,
    pub app: Option<App>,
    pub duration_ms: i64,
    /// Notes finished too (false while enhancement is still running).
    pub notes_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub session: SessionView,
    pub auto_record: bool,
    pub processing: Vec<ProcessingView>,
    /// Most recent first.
    pub needs_name: Vec<NeedsNameView>,
    /// False when no OpenRouter key is configured.
    pub api_key: bool,
}

// ---------------------------------------------------------------- blocking client

pub fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(crate::paths::socket_path())
}

/// One request, one response.
pub fn request(req: &Request) -> anyhow::Result<Response> {
    let mut stream = connect().map_err(|e| anyhow::anyhow!("muesli-daemon not running ({e})"))?;
    writeln!(stream, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Subscribe; calls `on_event` for every event until it returns false or the socket closes.
pub fn watch(mut on_event: impl FnMut(Event) -> bool) -> anyhow::Result<()> {
    let mut stream = connect().map_err(|e| anyhow::anyhow!("muesli-daemon not running ({e})"))?;
    writeln!(stream, "{}", serde_json::to_string(&Request::Watch)?)?;
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first)?; // ack
    for line in reader.lines() {
        let Ok(event) = serde_json::from_str::<Event>(&line?) else { continue };
        if !on_event(event) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        let r: Request = serde_json::from_str(r#"{"cmd":"rename","id":"x","title":"T"}"#).unwrap();
        assert_eq!(r, Request::Rename { id: "x".into(), title: "T".into(), confirm: true });
        let e = Event::Levels { me: 0.5, them: 0.25 };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"event":"levels","data":{"me":0.5,"them":0.25}}"#);
        let s = SessionView::Prompting { app: App::Discord, hint: None, since: 1 };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"kind":"prompting","app":"discord","hint":null,"since":1}"#
        );
    }
}
