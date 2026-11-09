//! Shared daemon context.

use anyhow::Result;
use muesli_core::config::{self, Config};
use muesli_core::ipc::{Event, ProcessingView, SessionView, Status};
use muesli_core::model::{Meeting, MeetingState, fallback_title};
use muesli_core::session;
use muesli_core::store;
use muesli_core::store::Connection;
use std::collections::HashSet;
use std::sync::{Mutex, RwLock};
use tokio::sync::{broadcast, mpsc};

/// Commands into the session loop.
#[derive(Debug)]
pub enum SessionCmd {
    Event(session::Event),
}

pub struct Ctx {
    pub cfg: RwLock<Config>,
    pub db: Mutex<Connection>,
    pub events: broadcast::Sender<Event>,
    pub session_view: RwLock<SessionView>,
    pub cmds: mpsc::UnboundedSender<SessionCmd>,
    pub jobs: Mutex<HashSet<String>>,
    /// Serializes imports (source_id check + claim).
    pub imports: tokio::sync::Mutex<()>,
    pub notifier: crate::notify::Notifier,
    /// Wakes the hook runner when an event is queued.
    pub hooks_wake: tokio::sync::Notify,
}

impl Ctx {
    pub fn cfg(&self) -> Config {
        self.cfg.read().expect("cfg lock").clone()
    }

    pub fn db<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut c = self.db.lock().map_err(|_| anyhow::anyhow!("db lock poisoned"))?;
        f(&mut c)
    }

    pub fn meeting(&self, id: &str) -> Result<Meeting> {
        self.db(|c| store::get_meeting(c, id))?.ok_or_else(|| anyhow::anyhow!("no meeting {id}"))
    }

    pub fn save(&self, m: &Meeting) -> Result<()> {
        self.db(|c| store::upsert_meeting(c, m))?;
        self.meeting_updated(&m.id);
        Ok(())
    }

    pub fn meeting_updated(&self, id: &str) {
        let _ = self.events.send(Event::MeetingUpdated { id: id.to_string() });
        self.broadcast_status();
    }

    pub fn broadcast_status(&self) {
        match self.status() {
            Ok(s) => drop(self.events.send(Event::Status(s))),
            Err(e) => tracing::warn!("status: {e}"),
        }
    }

    pub fn status(&self) -> Result<Status> {
        let session = self.session_view.read().expect("view lock").clone();
        let processing = self.db(|c| store::meetings_in_state(c, MeetingState::Processing))?;
        let title = |m: &Meeting| m.title.clone().unwrap_or_else(|| fallback_title(m.app, m.started_at));
        Ok(Status {
            session,
            auto_record: self.cfg().recording.auto_record,
            processing: processing
                .iter()
                .map(|m| ProcessingView { meeting_id: m.id.clone(), title: title(m), app: m.app, pipeline: m.pipeline.clone() })
                .collect(),
            api_key: config::api_key().is_some(),
        })
    }
}
