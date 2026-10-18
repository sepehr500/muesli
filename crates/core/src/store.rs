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
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
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

/// Meetings whose name hasn't been confirmed yet (newest first).
pub fn needing_name(c: &Connection, limit: usize) -> Result<Vec<Meeting>> {
    let mut st = c.prepare(&format!(
        "SELECT {MEETING_COLS} FROM meetings WHERE title_confirmed=0 AND state IN ('processing','ready')
         ORDER BY started_at DESC LIMIT ?1"
    ))?;
    let rows = st.query_map([limit as i64], meeting_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        "SELECT speaker, t_start, t_end, text FROM segments WHERE meeting_id=?1 ORDER BY t_start",
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

        assert_eq!(needing_name(&c, 5).unwrap().len(), 0); // still recording
        m.state = MeetingState::Ready;
        upsert_meeting(&c, &m).unwrap();
        assert_eq!(needing_name(&c, 5).unwrap().len(), 1);
        set_title(&c, &m.id, "Trip", true).unwrap();
        assert_eq!(needing_name(&c, 5).unwrap().len(), 0);
    }
}
