//! SQLite storage at `paths::db_file()`. WAL mode so the daemon (writer) and UI
//! (mostly reader, plus note edits) can share it.

use crate::detect::App;
use crate::model::*;
use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};
use std::path::Path;

pub type Connection = rusqlite::Connection;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meetings (
    id                TEXT PRIMARY KEY,
    title             TEXT,
    title_confirmed   INTEGER NOT NULL DEFAULT 0,
    title_suggestions TEXT NOT NULL DEFAULT '[]',
    app               TEXT,
    hint              TEXT,
    started_at        INTEGER NOT NULL,
    ended_at          INTEGER,
    state             TEXT NOT NULL,
    pipeline          TEXT NOT NULL,
    speaker_names     TEXT NOT NULL DEFAULT '{}',
    cost_usd          REAL NOT NULL DEFAULT 0,
    error             TEXT
);
CREATE INDEX IF NOT EXISTS meetings_started ON meetings(started_at DESC);

CREATE TABLE IF NOT EXISTS segments (
    id         INTEGER PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    speaker    TEXT NOT NULL,
    t_start    REAL NOT NULL,
    t_end      REAL NOT NULL,
    text       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS segments_meeting ON segments(meeting_id, t_start);

CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(text, content='segments', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS segments_ai AFTER INSERT ON segments BEGIN
  INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS segments_ad AFTER DELETE ON segments BEGIN
  INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TABLE IF NOT EXISTS notes (
    meeting_id    TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
    user_md       TEXT NOT NULL DEFAULT '',
    enhanced_json TEXT,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chats (
    id         INTEGER PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Hook outbox: every event is stored before its scripts run, so a crash or reboot can't lose
-- it. Not tied to meetings by foreign key: meeting-deleted events outlive their meeting.
CREATE TABLE IF NOT EXISTS hook_events (
    id         TEXT PRIMARY KEY,
    event      TEXT NOT NULL,
    meeting_id TEXT NOT NULL,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hook_runs (
    event_id   TEXT NOT NULL REFERENCES hook_events(id) ON DELETE CASCADE,
    script     TEXT NOT NULL,
    status     TEXT NOT NULL,          -- pending | ok | failed
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    INTEGER NOT NULL,
    exit_code  INTEGER,
    log        TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (event_id, script)
);
CREATE INDEX IF NOT EXISTS hook_runs_due ON hook_runs(status, next_at);

-- Recordings imported from outside muesli (e.g. a Discord bot). `source_id` makes imports
-- idempotent; `speakers` keeps the known per-track names for reprocessing and renames.
CREATE TABLE IF NOT EXISTS imports (
    source_id  TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL UNIQUE REFERENCES meetings(id) ON DELETE CASCADE,
    speakers   TEXT NOT NULL,
    manifest   TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn open_default() -> Result<Connection> {
    open(&crate::paths::db_file())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn app_to_str(a: Option<App>) -> Option<&'static str> {
    a.map(App::key)
}

fn app_from_str(s: Option<String>) -> Option<App> {
    match s.as_deref() {
        Some("zoom") => Some(App::Zoom),
        Some("discord") => Some(App::Discord),
        _ => None,
    }
}

fn speaker_to_str(s: &Speaker) -> String {
    match s {
        Speaker::Me => "me".into(),
        Speaker::Them(l) => format!("them:{l}"),
    }
}

fn speaker_from_str(s: &str) -> Speaker {
    match s.strip_prefix("them:") {
        Some(l) => Speaker::Them(l.to_string()),
        None => Speaker::Me,
    }
}

const MEETING_COLS: &str = "id, title, title_confirmed, title_suggestions, app, hint, started_at, \
     ended_at, state, pipeline, speaker_names, cost_usd, error";

fn meeting_from_row(r: &Row) -> rusqlite::Result<Meeting> {
    let json = |i: usize| -> rusqlite::Result<String> { r.get(i) };
    Ok(Meeting {
        id: r.get(0)?,
        title: r.get(1)?,
        title_confirmed: r.get::<_, i64>(2)? != 0,
        title_suggestions: serde_json::from_str(&json(3)?).unwrap_or_default(),
        app: app_from_str(r.get(4)?),
        hint: r.get(5)?,
        started_at: r.get(6)?,
        ended_at: r.get(7)?,
        state: MeetingState::parse(&r.get::<_, String>(8)?),
        pipeline: serde_json::from_str(&json(9)?).unwrap_or_default(),
        speaker_names: serde_json::from_str(&json(10)?).unwrap_or_default(),
        cost_usd: r.get(11)?,
        error: r.get(12)?,
    })
}

pub fn upsert_meeting(c: &Connection, m: &Meeting) -> Result<()> {
    c.execute(
        &format!(
            "INSERT INTO meetings ({MEETING_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(id) DO UPDATE SET title=?2, title_confirmed=?3, title_suggestions=?4, app=?5,
               hint=?6, started_at=?7, ended_at=?8, state=?9, pipeline=?10, speaker_names=?11,
               cost_usd=?12, error=?13"
        ),
        params![
            m.id,
            m.title,
            m.title_confirmed as i64,
            serde_json::to_string(&m.title_suggestions)?,
            app_to_str(m.app),
            m.hint,
            m.started_at,
            m.ended_at,
            m.state.as_str(),
            serde_json::to_string(&m.pipeline)?,
            serde_json::to_string(&m.speaker_names)?,
            m.cost_usd,
            m.error,
        ],
    )?;
    Ok(())
}

pub fn get_meeting(c: &Connection, id: &str) -> Result<Option<Meeting>> {
    Ok(c.query_row(&format!("SELECT {MEETING_COLS} FROM meetings WHERE id=?1"), [id], meeting_from_row)
        .optional()?)
}

pub fn list_meetings(c: &Connection, limit: usize) -> Result<Vec<Meeting>> {
    let mut st =
        c.prepare(&format!("SELECT {MEETING_COLS} FROM meetings ORDER BY started_at DESC LIMIT ?1"))?;
    let rows = st.query_map([limit as i64], meeting_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn meetings_in_state(c: &Connection, state: MeetingState) -> Result<Vec<Meeting>> {
    let mut st = c.prepare(&format!(
        "SELECT {MEETING_COLS} FROM meetings WHERE state=?1 ORDER BY started_at DESC"
    ))?;
    let rows = st.query_map([state.as_str()], meeting_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Meetings are named automatically once transcribed. Meetings from before that (left
/// waiting for a name) get their current title confirmed. Returns how many were updated.
pub fn confirm_pending_titles(c: &Connection) -> Result<usize> {
    let mut st = c.prepare(&format!("SELECT {MEETING_COLS} FROM meetings WHERE title_confirmed=0"))?;
    let pending: Vec<Meeting> = st.query_map([], meeting_from_row)?.collect::<rusqlite::Result<_>>()?;
    pending
        .iter()
        .filter(|m| m.state != MeetingState::Recording && m.pipeline.status_of(StepKind::Transcribed) == StepStatus::Done)
        .try_fold(0, |n, m| {
            let title = m.title.clone().unwrap_or_else(|| fallback_title(m.app, m.started_at));
            set_title(c, &m.id, &title, true).map(|_| n + 1)
        })
}

// ------------------------------------------------------------------ hooks

#[derive(Debug, Clone, PartialEq)]
pub struct HookRun {
    pub event_id: String,
    pub event: String,
    pub meeting_id: String,
    pub script: String,
    pub status: String,
    pub attempts: u32,
    pub exit_code: Option<i32>,
    pub log: String,
    pub updated_at: i64,
}

/// Store an event and one pending run per script, atomically.
pub fn insert_hook_event(
    c: &mut Connection,
    id: &str,
    event: &str,
    meeting_id: &str,
    payload: &str,
    scripts: &[String],
) -> Result<()> {
    let now = now_ms();
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO hook_events (id, event, meeting_id, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, event, meeting_id, payload, now],
    )?;
    for s in scripts {
        tx.execute(
            "INSERT INTO hook_runs (event_id, script, status, next_at, updated_at) VALUES (?1, ?2, 'pending', ?3, ?3)",
            params![id, s, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

const RUN_COLS: &str = "r.event_id, e.event, e.meeting_id, r.script, r.status, r.attempts, r.exit_code, r.log, r.updated_at";

fn run_from_row(r: &Row) -> rusqlite::Result<HookRun> {
    Ok(HookRun {
        event_id: r.get(0)?,
        event: r.get(1)?,
        meeting_id: r.get(2)?,
        script: r.get(3)?,
        status: r.get(4)?,
        attempts: r.get(5)?,
        exit_code: r.get(6)?,
        log: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

/// Pending runs whose time has come, oldest event first, with their payloads.
pub fn due_hook_runs(c: &Connection, now: i64) -> Result<Vec<(HookRun, String)>> {
    let mut st = c.prepare(&format!(
        "SELECT {RUN_COLS}, e.payload FROM hook_runs r JOIN hook_events e ON e.id = r.event_id
         WHERE r.status = 'pending' AND r.next_at <= ?1 ORDER BY e.created_at, r.script"
    ))?;
    let rows = st.query_map([now], |r| Ok((run_from_row(r)?, r.get::<_, String>(9)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Earliest time a pending run becomes due.
pub fn next_hook_due(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT MIN(next_at) FROM hook_runs WHERE status = 'pending'", [], |r| r.get(0))?)
}

/// Record an attempt. `retry_at` keeps the run pending until then; otherwise it's final.
pub fn finish_hook_run(
    c: &Connection,
    event_id: &str,
    script: &str,
    ok: bool,
    exit_code: Option<i32>,
    log: &str,
    retry_at: Option<i64>,
) -> Result<()> {
    let status = match (ok, retry_at) {
        (true, _) => "ok",
        (false, Some(_)) => "pending",
        (false, None) => "failed",
    };
    c.execute(
        "UPDATE hook_runs SET status=?3, attempts=attempts+1, exit_code=?4, log=?5, next_at=?6, updated_at=?7
         WHERE event_id=?1 AND script=?2",
        params![event_id, script, status, exit_code, log, retry_at.unwrap_or(0), now_ms()],
    )?;
    Ok(())
}

/// Queue an event's failed runs again. Returns how many.
pub fn retry_hook_event(c: &Connection, event_id: &str) -> Result<usize> {
    Ok(c.execute(
        "UPDATE hook_runs SET status='pending', attempts=0, next_at=?2 WHERE event_id=?1 AND status='failed'",
        params![event_id, now_ms()],
    )?)
}

pub fn hook_runs_for_event(c: &Connection, event_id: &str) -> Result<Vec<HookRun>> {
    let mut st = c.prepare(&format!(
        "SELECT {RUN_COLS} FROM hook_runs r JOIN hook_events e ON e.id = r.event_id WHERE r.event_id = ?1 ORDER BY r.script"
    ))?;
    let rows = st.query_map([event_id], run_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Most recent runs, optionally for one meeting.
pub fn recent_hook_runs(c: &Connection, meeting_id: Option<&str>, limit: usize) -> Result<Vec<HookRun>> {
    let mut st = c.prepare(&format!(
        "SELECT {RUN_COLS} FROM hook_runs r JOIN hook_events e ON e.id = r.event_id
         WHERE ?1 IS NULL OR e.meeting_id = ?1 ORDER BY r.updated_at DESC LIMIT ?2"
    ))?;
    let rows = st.query_map(params![meeting_id, limit as i64], run_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Drop finished events older than `cutoff` (keeps the outbox small).
pub fn prune_hook_events(c: &Connection, cutoff: i64) -> Result<usize> {
    Ok(c.execute(
        "DELETE FROM hook_events WHERE created_at < ?1
         AND NOT EXISTS (SELECT 1 FROM hook_runs r WHERE r.event_id = hook_events.id AND r.status = 'pending')",
        [cutoff],
    )?)
}

pub fn delete_meeting(c: &Connection, id: &str) -> Result<()> {
    c.execute("DELETE FROM meetings WHERE id=?1", [id])?;
    Ok(())
}

pub fn set_title(c: &Connection, id: &str, title: &str, confirmed: bool) -> Result<()> {
    c.execute(
        "UPDATE meetings SET title=?2, title_confirmed=?3 WHERE id=?1",
        params![id, title, confirmed as i64],
    )?;
    Ok(())
}

pub fn replace_segments(c: &mut Connection, id: &str, segs: &[Segment]) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute("DELETE FROM segments WHERE meeting_id=?1", [id])?;
    {
        let mut st = tx.prepare(
            "INSERT INTO segments (meeting_id, speaker, t_start, t_end, text) VALUES (?1,?2,?3,?4,?5)",
        )?;
        for s in segs {
            st.execute(params![id, speaker_to_str(&s.speaker), s.t_start, s.t_end, s.text])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn segments(c: &Connection, id: &str) -> Result<Vec<Segment>> {
    let mut st = c.prepare(
        "SELECT speaker, t_start, t_end, text FROM segments WHERE meeting_id=?1 ORDER BY t_start, rowid",
    )?;
    let rows = st.query_map([id], |r| {
        Ok(Segment {
            speaker: speaker_from_str(&r.get::<_, String>(0)?),
            t_start: r.get(1)?,
            t_end: r.get(2)?,
            text: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_notes(c: &Connection, id: &str) -> Result<Notes> {
    let row = c
        .query_row(
            "SELECT user_md, enhanced_json, updated_at FROM notes WHERE meeting_id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((user_md, enhanced, updated_at)) => Notes {
            meeting_id: id.to_string(),
            user_md,
            enhanced: enhanced.and_then(|j| serde_json::from_str(&j).ok()),
            updated_at,
        },
        None => Notes { meeting_id: id.to_string(), user_md: String::new(), enhanced: None, updated_at: 0 },
    })
}

pub fn set_user_notes(c: &Connection, id: &str, md: &str) -> Result<()> {
    c.execute(
        "INSERT INTO notes (meeting_id, user_md, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(meeting_id) DO UPDATE SET user_md=?2, updated_at=?3",
        params![id, md, now_ms()],
    )?;
    Ok(())
}

pub fn set_enhanced(c: &Connection, id: &str, notes: Option<&EnhancedNotes>) -> Result<()> {
    let json = notes.map(serde_json::to_string).transpose()?;
    c.execute(
        "INSERT INTO notes (meeting_id, enhanced_json, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(meeting_id) DO UPDATE SET enhanced_json=?2, updated_at=?3",
        params![id, json, now_ms()],
    )?;
    Ok(())
}

pub fn add_chat(c: &Connection, id: &str, role: &str, content: &str) -> Result<()> {
    c.execute(
        "INSERT INTO chats (meeting_id, role, content, created_at) VALUES (?1,?2,?3,?4)",
        params![id, role, content, now_ms()],
    )?;
    Ok(())
}

pub fn chats(c: &Connection, id: &str) -> Result<Vec<ChatMessage>> {
    let mut st =
        c.prepare("SELECT role, content, created_at FROM chats WHERE meeting_id=?1 ORDER BY id")?;
    let rows = st.query_map([id], |r| {
        Ok(ChatMessage { role: r.get(0)?, content: r.get(1)?, created_at: r.get(2)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Full-text search across titles and transcripts. Returns meeting ids, newest first.
pub fn search(c: &Connection, query: &str, limit: usize) -> Result<Vec<MeetingId>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }
    // Quote each term so user input can't break FTS syntax; prefix-match the last one.
    let fts: String = q
        .split_whitespace()
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    let like = format!("%{q}%");
    let mut st = c.prepare(
        "SELECT id FROM meetings WHERE title LIKE ?2 OR hint LIKE ?2
            OR id IN (SELECT s.meeting_id FROM segments_fts f JOIN segments s ON s.id = f.rowid
                      WHERE segments_fts MATCH ?1)
         ORDER BY started_at DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![fts, like, limit as i64], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The meeting a `source_id` was imported as, if any.
pub fn import_meeting(c: &Connection, source_id: &str) -> Result<Option<MeetingId>> {
    Ok(c.query_row("SELECT meeting_id FROM imports WHERE source_id=?1", [source_id], |r| r.get(0)).optional()?)
}

/// Speakers of an imported meeting (empty for normal recordings).
pub fn import_speakers(c: &Connection, meeting_id: &str) -> Result<Vec<crate::import::ImportSpeaker>> {
    let json: Option<String> =
        c.query_row("SELECT speakers FROM imports WHERE meeting_id=?1", [meeting_id], |r| r.get(0)).optional()?;
    Ok(json.map(|j| serde_json::from_str(&j)).transpose()?.unwrap_or_default())
}

/// Create an imported meeting and claim its `source_id`, atomically.
pub fn insert_import(
    c: &mut Connection,
    m: &Meeting,
    source_id: &str,
    speakers: &[crate::import::ImportSpeaker],
    manifest: &str,
) -> Result<()> {
    let tx = c.transaction()?;
    upsert_meeting(&tx, m)?;
    tx.execute(
        "INSERT INTO imports (source_id, meeting_id, speakers, manifest, created_at) VALUES (?1,?2,?3,?4,?5)",
        params![source_id, m.id, serde_json::to_string(speakers)?, manifest, now_ms()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn new_meeting(app: Option<App>, hint: Option<String>, started_at: i64) -> Meeting {
    Meeting {
        id: ulid::Ulid::generate().to_string().to_lowercase(),
        title: None,
        title_confirmed: false,
        title_suggestions: vec![],
        app,
        hint,
        started_at,
        ended_at: None,
        state: MeetingState::Recording,
        pipeline: Pipeline::default(),
        speaker_names: Default::default(),
        cost_usd: 0.0,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    #[test]
    fn hook_outbox() {
        let mut c = mem();
        insert_hook_event(&mut c, "ev1", "meeting-ready", "m1", "{}", &["a.sh".into(), "b.sh".into()]).unwrap();
        let due = due_hook_runs(&c, now_ms()).unwrap();
        assert_eq!(due.iter().map(|(r, _)| r.script.as_str()).collect::<Vec<_>>(), vec!["a.sh", "b.sh"]);
        finish_hook_run(&c, "ev1", "a.sh", true, Some(0), "", None).unwrap();
        finish_hook_run(&c, "ev1", "b.sh", false, Some(1), "boom", Some(now_ms() + 60_000)).unwrap();
        assert!(due_hook_runs(&c, now_ms()).unwrap().is_empty(), "retry is in the future");
        assert_eq!(due_hook_runs(&c, now_ms() + 61_000).unwrap().len(), 1);
        finish_hook_run(&c, "ev1", "b.sh", false, Some(1), "boom", None).unwrap();
        let runs = hook_runs_for_event(&c, "ev1").unwrap();
        assert_eq!((runs[1].status.as_str(), runs[1].attempts, runs[1].log.as_str()), ("failed", 2, "boom"));
        assert_eq!(retry_hook_event(&c, "ev1").unwrap(), 1);
        assert_eq!(due_hook_runs(&c, now_ms()).unwrap().len(), 1);
        assert_eq!(recent_hook_runs(&c, Some("m1"), 10).unwrap().len(), 2);
        assert_eq!(prune_hook_events(&c, now_ms() + 1).unwrap(), 0, "pending runs are kept");
    }

    #[test]
    fn meeting_roundtrip_and_search() {
        let mut c = mem();
        let mut m = new_meeting(Some(App::Discord), Some("Hiking crew · #planning".into()), 1000);
        m.title_suggestions = vec!["A".into(), "B".into()];
        m.pipeline = Pipeline::default().with(StepKind::Saved, StepStatus::Done, None);
        upsert_meeting(&c, &m).unwrap();
        assert_eq!(get_meeting(&c, &m.id).unwrap().unwrap(), m);

        replace_segments(
            &mut c,
            &m.id,
            &[
                Segment { speaker: Speaker::Me, t_start: 0.0, t_end: 1.0, text: "offline sync".into() },
                Segment { speaker: Speaker::Them("c0s1".into()), t_start: 1.0, t_end: 2.0, text: "pricing".into() },
            ],
        )
        .unwrap();
        assert_eq!(segments(&c, &m.id).unwrap()[1].speaker, Speaker::Them("c0s1".into()));
        assert_eq!(search(&c, "pric", 10).unwrap(), vec![m.id.clone()]);
        assert_eq!(search(&c, "hiking", 10).unwrap(), vec![m.id.clone()]);
        assert!(search(&c, "zebra", 10).unwrap().is_empty());

        set_user_notes(&c, &m.id, "- ship it").unwrap();
        set_enhanced(&c, &m.id, Some(&EnhancedNotes::default())).unwrap();
        let n = get_notes(&c, &m.id).unwrap();
        assert_eq!((n.user_md.as_str(), n.enhanced.is_some()), ("- ship it", true));

        assert_eq!(confirm_pending_titles(&c).unwrap(), 0); // still recording
        m.state = MeetingState::Ready;
        m.pipeline = m.pipeline.with(StepKind::Transcribed, StepStatus::Done, None);
        upsert_meeting(&c, &m).unwrap();
        assert_eq!(confirm_pending_titles(&c).unwrap(), 1);
        let got = get_meeting(&c, &m.id).unwrap().unwrap();
        assert!(got.title_confirmed);
        assert_eq!(got.title, Some(fallback_title(m.app, m.started_at)));
        assert_eq!(confirm_pending_titles(&c).unwrap(), 0);
    }

    #[test]
    fn imports_are_unique_and_follow_deletes() {
        use crate::import::ImportSpeaker;
        let mut c = mem();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        let m = new_meeting(Some(App::Discord), None, 1000);
        let sp = vec![ImportSpeaker { label: "t0".into(), name: Some("Matt Rosenthal".into()) }];
        insert_import(&mut c, &m, "discord:1:a", &sp, "{}").unwrap();
        assert_eq!(import_meeting(&c, "discord:1:a").unwrap(), Some(m.id.clone()));
        assert_eq!(import_speakers(&c, &m.id).unwrap(), sp);
        // Same source again: rejected, and the meeting row is rolled back with it.
        let other = new_meeting(Some(App::Discord), None, 2000);
        assert!(insert_import(&mut c, &other, "discord:1:a", &sp, "{}").is_err());
        assert!(get_meeting(&c, &other.id).unwrap().is_none());
        assert!(import_speakers(&c, &other.id).unwrap().is_empty());
        delete_meeting(&c, &m.id).unwrap();
        assert_eq!(import_meeting(&c, "discord:1:a").unwrap(), None);
    }
}
