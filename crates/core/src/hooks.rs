//! Hooks: executable scripts in `hooks/<event>.d/` run when something happens to a meeting.
//! This module is pure: event names, the JSON payload (schema version 1), the exported
//! markdown, and which files count as scripts. The daemon owns running them.

use crate::model::{EnhancedNotes, Meeting, Notes, Segment, Speaker};
use crate::transcript::display_speaker;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PAYLOAD_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    RecordingStarted,
    RecordingStopped,
    /// Transcript and notes are done (also after re-enhancing).
    MeetingReady,
    /// Title or speaker names changed.
    MeetingUpdated,
    MeetingFailed,
    MeetingDeleted,
}

impl HookEvent {
    pub const ALL: [HookEvent; 6] = [
        HookEvent::RecordingStarted,
        HookEvent::RecordingStopped,
        HookEvent::MeetingReady,
        HookEvent::MeetingUpdated,
        HookEvent::MeetingFailed,
        HookEvent::MeetingDeleted,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HookEvent::RecordingStarted => "recording-started",
            HookEvent::RecordingStopped => "recording-stopped",
            HookEvent::MeetingReady => "meeting-ready",
            HookEvent::MeetingUpdated => "meeting-updated",
            HookEvent::MeetingFailed => "meeting-failed",
            HookEvent::MeetingDeleted => "meeting-deleted",
        }
    }

    pub fn parse(s: &str) -> Option<HookEvent> {
        Self::ALL.into_iter().find(|e| e.name() == s)
    }

    /// Events whose meeting still exists and has something worth exporting.
    pub fn exports(self) -> bool {
        matches!(self, HookEvent::RecordingStopped | HookEvent::MeetingReady | HookEvent::MeetingUpdated | HookEvent::MeetingFailed)
    }
}

/// Editor leftovers and switched-off scripts don't run.
pub fn is_hook_script(file_name: &str) -> bool {
    !file_name.is_empty()
        && !file_name.starts_with('.')
        && !file_name.ends_with('~')
        && ![".sample", ".disabled", ".bak", ".swp", ".orig"].iter().any(|s| file_name.ends_with(s))
}

/// Executable scripts for an event, sorted by name (`hooks/<event>.d/*`).
pub fn scripts(event: HookEvent) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = script_dir(event);
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_hook_script(n))
        .collect();
    names.sort();
    names
}

pub fn script_dir(event: HookEvent) -> PathBuf {
    crate::paths::hooks_dir().join(format!("{}.d", event.name()))
}

/// Files written to `<meeting>/export/` for scripts to read.
#[derive(Debug, Clone, Serialize)]
pub struct ExportFiles {
    pub dir: PathBuf,
    pub notes_md: PathBuf,
    pub transcript_md: PathBuf,
    pub transcript_json: PathBuf,
    pub meeting_json: PathBuf,
}

impl ExportFiles {
    pub fn in_dir(dir: &Path) -> ExportFiles {
        ExportFiles {
            dir: dir.to_path_buf(),
            notes_md: dir.join("notes.md"),
            transcript_md: dir.join("transcript.md"),
            transcript_json: dir.join("transcript.json"),
            meeting_json: dir.join("meeting.json"),
        }
    }
}

pub fn clock(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    match t >= 3600 {
        true => format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60),
        false => format!("{:02}:{:02}", t / 60, t % 60),
    }
}

/// Start time of the first cited transcript line.
fn source_start(sources: &[usize], segs: &[Segment]) -> Option<f64> {
    sources.iter().filter_map(|&i| segs.get(i)).map(|s| s.t_start).reduce(f64::min)
}

/// Structured notes with source times resolved (scripts shouldn't need line numbers).
fn notes_json(n: &EnhancedNotes, segs: &[Segment]) -> Value {
    json!({
        "sections": n.sections.iter().map(|s| json!({
            "title": s.title,
            "items": s.items.iter().map(|i| json!({
                "text": i.text,
                "detail": i.detail,
                "from_user": i.from_user,
                "source_start_s": source_start(&i.sources, segs),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "action_items": n.action_items.iter().map(|a| json!({
            "text": a.text,
            "owner": a.owner,
            "done": a.done,
            "from_user": a.from_user,
            "source_start_s": source_start(&a.sources, segs),
        })).collect::<Vec<_>>(),
    })
}

/// Portable notes markdown (no title heading; the title is in the payload).
pub fn notes_markdown(notes: &Notes, segs: &[Segment]) -> String {
    let at = |sources: &[usize]| source_start(sources, segs).map(|t| format!(" `{}`", clock(t))).unwrap_or_default();
    match &notes.enhanced {
        Some(n) if !n.sections.is_empty() || !n.action_items.is_empty() => {
            let sections = n.sections.iter().map(|s| {
                let items = s.items.iter().map(|i| {
                    let detail = i.detail.as_deref().map(|d| format!("\n  - {d}")).unwrap_or_default();
                    format!("- {}{}{detail}", i.text, at(&i.sources))
                });
                format!("## {}\n\n{}", s.title, items.collect::<Vec<_>>().join("\n"))
            });
            let actions = (!n.action_items.is_empty()).then(|| {
                let rows = n.action_items.iter().map(|a| {
                    let owner = a.owner.as_deref().map(|o| format!(" — {o}")).unwrap_or_default();
                    format!("- [{}] {}{owner}{}", if a.done { "x" } else { " " }, a.text, at(&a.sources))
                });
                format!("## Action items\n\n{}", rows.collect::<Vec<_>>().join("\n"))
            });
            sections.chain(actions).collect::<Vec<_>>().join("\n\n") + "\n"
        }
        _ if !notes.user_md.trim().is_empty() => format!("## Your notes\n\n{}\n", notes.user_md.trim()),
        _ => String::new(),
    }
}

pub fn transcript_markdown(segs: &[Segment], names: &BTreeMap<String, String>) -> String {
    segs.iter()
        .map(|s| format!("[{}] {}: {}\n", clock(s.t_start), display_speaker(&s.speaker, names), s.text))
        .collect()
}

pub fn transcript_json(segs: &[Segment], names: &BTreeMap<String, String>) -> Value {
    Value::Array(
        segs.iter()
            .map(|s| json!({
                "start_s": s.t_start,
                "end_s": s.t_end,
                "speaker": display_speaker(&s.speaker, names),
                "me": s.speaker == Speaker::Me,
                "text": s.text,
            }))
            .collect(),
    )
}

/// People in the meeting, by the names shown in the app ("You" first, unnamed speakers last).
pub fn people(segs: &[Segment], names: &BTreeMap<String, String>) -> Vec<String> {
    let mut named: Vec<String> = segs.iter().map(|s| display_speaker(&s.speaker, names)).collect();
    named.sort_by_key(|n| (n != "You", n.starts_with("Speaker "), n.clone()));
    named.dedup();
    named
}

fn local_time(ms: i64) -> Option<String> {
    use chrono::{Local, TimeZone};
    Local.timestamp_millis_opt(ms).single().map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

/// The meeting part of the payload (also written as `meeting.json`).
pub fn meeting_json(m: &Meeting, notes: &Notes, segs: &[Segment]) -> Value {
    let todos = notes.enhanced.as_ref().map(|n| n.action_items.as_slice()).unwrap_or_default();
    json!({
        "id": m.id,
        "title": m.display_title(),
        "app": m.app.map(|a| a.key()),
        "hint": m.hint,
        "state": m.state,
        "started_at": local_time(m.started_at),
        "ended_at": m.ended_at.and_then(local_time),
        "duration_s": m.duration_ms().map(|d| d / 1000),
        "people": people(segs, &m.speaker_names),
        "todos": { "total": todos.len(), "open": todos.iter().filter(|a| !a.done).count() },
        "cost_usd": m.cost_usd,
        "error": m.error,
        "notes": notes.enhanced.as_ref().map(|n| notes_json(n, segs)),
        "user_notes": notes.user_md,
    })
}

pub fn payload(event: HookEvent, event_id: &str, meeting: Value, files: Option<&ExportFiles>) -> Value {
    json!({
        "version": PAYLOAD_VERSION,
        "event": event.name(),
        "event_id": event_id,
        "meeting": meeting,
        "files": files,
    })
}

/// Shortcuts for shell scripts; the full payload is on stdin.
pub fn env_vars(payload: &Value) -> Vec<(String, String)> {
    let s = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    let m = &payload["meeting"];
    let f = &payload["files"];
    [
        ("MUESLI_EVENT", &payload["event"]),
        ("MUESLI_EVENT_ID", &payload["event_id"]),
        ("MUESLI_MEETING_ID", &m["id"]),
        ("MUESLI_TITLE", &m["title"]),
        ("MUESLI_APP", &m["app"]),
        ("MUESLI_STARTED_AT", &m["started_at"]),
        ("MUESLI_DURATION_S", &m["duration_s"]),
        ("MUESLI_EXPORT_DIR", &f["dir"]),
        ("MUESLI_NOTES_MD", &f["notes_md"]),
        ("MUESLI_TRANSCRIPT_MD", &f["transcript_md"]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), s(v)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn seg(speaker: Speaker, t: f64, text: &str) -> Segment {
        Segment { speaker, t_start: t, t_end: t + 2.0, text: text.into() }
    }

    #[test]
    fn event_names_roundtrip() {
        for e in HookEvent::ALL {
            assert_eq!(HookEvent::parse(e.name()), Some(e));
        }
        assert_eq!(HookEvent::parse("nope"), None);
    }

    #[test]
    fn script_filter() {
        assert!(is_hook_script("10-sync.py"));
        assert!(!is_hook_script(".hidden"));
        assert!(!is_hook_script("sync.py~"));
        assert!(!is_hook_script("sync.sample"));
        assert!(!is_hook_script("sync.py.disabled"));
    }

    #[test]
    fn markdown_and_payload() {
        let segs = vec![seg(Speaker::Me, 3.0, "Hi."), seg(Speaker::Them("c0s0".into()), 65.0, "Ship it Friday.")];
        let names = BTreeMap::from([("c0s0".to_string(), "Lena".to_string())]);
        let notes = Notes {
            meeting_id: "m".into(),
            user_md: "- ship".into(),
            enhanced: Some(EnhancedNotes {
                sections: vec![NoteSection {
                    title: "Decisions".into(),
                    items: vec![NoteItem { text: "Ship Friday".into(), from_user: true, detail: Some("Lena agreed.".into()), sources: vec![1] }],
                }],
                action_items: vec![ActionItem { text: "Tag the release".into(), owner: Some("You".into()), from_user: false, done: false, sources: vec![] }],
            }),
            updated_at: 0,
        };
        assert_eq!(
            notes_markdown(&notes, &segs),
            "## Decisions\n\n- Ship Friday `01:05`\n  - Lena agreed.\n\n## Action items\n\n- [ ] Tag the release — You\n"
        );
        assert_eq!(transcript_markdown(&segs, &names), "[00:03] You: Hi.\n[01:05] Lena: Ship it Friday.\n");
        assert_eq!(people(&segs, &names), vec!["You", "Lena"]);

        let mut m = crate::store::new_meeting(Some(crate::detect::App::Zoom), None, 1_700_000_000_000);
        m.ended_at = Some(m.started_at + 90_000);
        m.speaker_names = names;
        let files = ExportFiles::in_dir(Path::new("/x/export"));
        let p = payload(HookEvent::MeetingReady, "ev1", meeting_json(&m, &notes, &segs), Some(&files));
        assert_eq!(p["event"], "meeting-ready");
        assert_eq!(p["meeting"]["duration_s"], 90);
        assert_eq!(p["meeting"]["todos"]["open"], 1);
        assert_eq!(p["meeting"]["notes"]["sections"][0]["items"][0]["source_start_s"], 65.0);
        let env = env_vars(&p);
        assert!(env.contains(&("MUESLI_APP".into(), "zoom".into())));
        assert!(env.contains(&("MUESLI_NOTES_MD".into(), "/x/export/notes.md".into())));
    }

    #[test]
    fn user_notes_only() {
        let notes = Notes { meeting_id: "m".into(), user_md: "- a\n".into(), enhanced: None, updated_at: 0 };
        assert_eq!(notes_markdown(&notes, &[]), "## Your notes\n\n- a\n");
    }
}
