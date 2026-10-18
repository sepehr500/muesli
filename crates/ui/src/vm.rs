//! Pure view-model functions: everything the GTK shell displays is derived here
//! from plain core data, so it can be unit-tested without a display.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};
use muesli_core::detect::App;
use muesli_core::ipc::{SessionView, Status};
use muesli_core::model::{
    EnhancedNotes, Meeting, MeetingState, Pipeline, Segment, StepKind, StepStatus, fallback_title, human_duration,
};
use muesli_core::transcript::{display_speaker, other_speaker_count};
use std::collections::BTreeMap;

// ------------------------------------------------------------------ time

fn local<Tz: TimeZone>(tz: &Tz, ms: i64) -> Option<DateTime<Tz>> {
    tz.timestamp_millis_opt(ms).single()
}

/// "Today", "Yesterday", "Monday" (within a week), "Oct 3", "Oct 3, 2024".
pub fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%b %-d").to_string(),
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

/// A sidebar group: day label + meetings (input order preserved, newest first).
#[derive(Debug, Clone, PartialEq)]
pub struct DayGroup<'a> {
    pub label: String,
    pub meetings: Vec<&'a Meeting>,
}

pub fn group_by_day<'a, Tz: TimeZone>(meetings: &[&'a Meeting], tz: &Tz, now_ms: i64) -> Vec<DayGroup<'a>> {
    let day_of = |ms: i64| local(tz, ms).map(|t| t.date_naive()).unwrap_or_default();
    let today = day_of(now_ms);
    meetings.iter().fold(Vec::<(NaiveDate, DayGroup<'a>)>::new(), |mut acc, m| {
        let d = day_of(m.started_at);
        match acc.last_mut() {
            Some((last, g)) if *last == d => g.meetings.push(m),
            _ => acc.push((d, DayGroup { label: day_label(d, today), meetings: vec![m] })),
        }
        acc
    })
    .into_iter()
    .map(|(_, g)| g)
    .collect()
}

/// Sidebar second line: "10:30 · 42 min", "Just now · 37 min", "Recording · 12 min".
pub fn row_meta<Tz: TimeZone>(m: &Meeting, tz: &Tz, now_ms: i64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let clock = local(tz, m.started_at).map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    match (m.state, m.ended_at) {
        (MeetingState::Recording, _) => format!("Recording · {}", human_duration(now_ms - m.started_at)),
        (_, Some(end)) if (0..10 * 60_000).contains(&(now_ms - end)) => format!("Just now · {}", human_duration(end - m.started_at)),
        (_, Some(end)) => format!("{clock} · {}", human_duration(end - m.started_at)),
        (_, None) => clock,
    }
}

/// "Fri, Oct 17 · 10:30–11:12".
pub fn time_range<Tz: TimeZone>(m: &Meeting, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let start = local(tz, m.started_at);
    let day = start.as_ref().map(|t| t.format("%a, %b %-d").to_string()).unwrap_or_default();
    let from = start.as_ref().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    let to = m.ended_at.and_then(|e| local(tz, e)).map(|t| format!("–{}", t.format("%H:%M"))).unwrap_or_default();
    format!("{day} · {from}{to}")
}

/// "Lena, Marco, Priya, you" when every other speaker has a name, else "You + 3 others".
/// None when there's no transcript yet.
pub fn people_line(segs: &[Segment], names: &BTreeMap<String, String>) -> Option<String> {
    use muesli_core::model::Speaker;
    if segs.is_empty() {
        return None;
    }
    let others: Vec<&Segment> = segs.iter().filter(|s| s.speaker != Speaker::Me).collect();
    let all_named = others.iter().all(|s| matches!(&s.speaker, Speaker::Them(l) if names.contains_key(l)));
    let mut named: Vec<String> = others.iter().map(|s| display_speaker(&s.speaker, names)).collect();
    named.dedup();
    let named = named.into_iter().fold(Vec::<String>::new(), |mut v, n| {
        if !v.contains(&n) {
            v.push(n);
        }
        v
    });
    Some(match (others.is_empty(), all_named && named.len() <= 4) {
        (true, _) => "Just you".into(),
        (false, true) => format!("{}, you", named.join(", ")),
        (false, false) => match other_speaker_count(segs, names) {
            1 => "You + 1 other".into(),
            n => format!("You + {n} others"),
        },
    })
}

pub fn app_label(app: Option<App>) -> &'static str {
    app.map(App::label).unwrap_or("Manual")
}

// ------------------------------------------------------------------ naming

/// Suggestion chips: distinct LLM suggestions, then the offline fallback (dashed chip).
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub title: String,
    pub fallback: bool,
}

pub fn chips(m: &Meeting) -> Vec<Chip> {
    let fb = fallback_title(m.app, m.started_at);
    let suggestions = m
        .title_suggestions
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && *s != fb)
        .fold(Vec::<String>::new(), |mut v, s| {
            if !v.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
                v.push(s);
            }
            v
        });
    suggestions
        .into_iter()
        .map(|title| Chip { title, fallback: false })
        .chain(std::iter::once(Chip { title: fb, fallback: true }))
        .collect()
}

/// Text to prefill the title field with: current title, best suggestion, or fallback.
pub fn initial_title(m: &Meeting) -> String {
    m.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| m.title_suggestions.first().cloned())
        .unwrap_or_else(|| fallback_title(m.app, m.started_at))
}

/// What "Save" should send for the typed title.
#[derive(Debug, Clone, PartialEq)]
pub enum TitleAction {
    Confirm,
    Rename(String),
    Nothing,
}

pub fn title_action(m: &Meeting, typed: &str) -> TitleAction {
    let typed = typed.trim();
    match (typed.is_empty(), m.title.as_deref() == Some(typed), m.title_confirmed) {
        (true, _, _) => TitleAction::Nothing,
        (false, true, true) => TitleAction::Nothing,
        (false, true, false) => TitleAction::Confirm,
        (false, false, _) => TitleAction::Rename(typed.to_string()),
    }
}

/// Whether the post-call naming chrome is shown.
pub fn shows_naming(m: &Meeting, skipped: bool) -> bool {
    !m.title_confirmed && !skipped && matches!(m.state, MeetingState::Processing | MeetingState::Ready)
}

pub fn notes_done(p: &Pipeline) -> bool {
    p.status_of(StepKind::Notes) == StepStatus::Done
}

// ------------------------------------------------------------------ wrapping up

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    None,
    Check,
    Spinner,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WrapRow {
    pub kind: StepKind,
    pub label: String,
    pub detail: String,
    pub indicator: Indicator,
    /// The active step: emphasised label + accent tile.
    pub active: bool,
    pub pending: bool,
}

fn default_detail(kind: StepKind, status: StepStatus, duration_ms: Option<i64>) -> String {
    match (kind, status) {
        (_, StepStatus::Failed) => "Something went wrong".into(),
        (StepKind::Saved, _) => match duration_ms {
            Some(d) => format!("{} of you and the call, kept on this computer", human_duration(d)),
            None => "Kept on this computer".into(),
        },
        (StepKind::Cleaned, _) => "Removing speaker echo and silence".into(),
        (StepKind::Transcribed, _) => "Turning both sides of the call into text".into(),
        (StepKind::Notes, _) => "Filling in your bullets with decisions and to-dos".into(),
    }
}

pub fn wrapping_rows(p: &Pipeline, duration_ms: Option<i64>) -> Vec<WrapRow> {
    p.steps
        .iter()
        .map(|s| WrapRow {
            kind: s.kind,
            label: match s.status {
                StepStatus::Active => s.kind.active_label().to_string(),
                StepStatus::Failed => format!("{} failed", s.kind.done_label().split(' ').next().unwrap_or("Step")),
                _ => s.kind.done_label().to_string(),
            },
            detail: s.detail.clone().unwrap_or_else(|| default_detail(s.kind, s.status, duration_ms)),
            indicator: match s.status {
                StepStatus::Done => Indicator::Check,
                StepStatus::Active => Indicator::Spinner,
                StepStatus::Failed => Indicator::Failed,
                StepStatus::Pending => Indicator::None,
            },
            active: s.status == StepStatus::Active,
            pending: s.status == StepStatus::Pending,
        })
        .collect()
}

/// Right-hand note in the "Wrapping up" header.
pub fn wrapping_summary(p: &Pipeline) -> String {
    let st = |k| p.status_of(k);
    match () {
        _ if p.steps.iter().any(|s| s.status == StepStatus::Failed) => "Stopped — try Reprocess".into(),
        _ if st(StepKind::Notes) == StepStatus::Done => "All done".into(),
        _ if st(StepKind::Notes) == StepStatus::Active => "Notes ready in about a minute".into(),
        _ if st(StepKind::Transcribed) == StepStatus::Done => "Notes ready in about a minute".into(),
        _ => "Notes ready in a few minutes".into(),
    }
}

// ------------------------------------------------------------------ status footer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterKind {
    Listening,
    Prompting,
    Recording,
    Offline,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Footer {
    pub kind: FooterKind,
    pub title: String,
    pub subtitle: String,
}

pub fn mmss(ms: i64) -> String {
    let s = (ms.max(0) / 1000) as u64;
    match s >= 3600 {
        true => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        false => format!("{:02}:{:02}", s / 60, s % 60),
    }
}

pub fn footer(status: Option<&Status>, now_ms: i64) -> Footer {
    let Some(s) = status else {
        return Footer {
            kind: FooterKind::Offline,
            title: "Daemon not running".into(),
            subtitle: "Recording unavailable".into(),
        };
    };
    let auto = if s.auto_record { "Auto-record on" } else { "Auto-record off" };
    match &s.session {
        SessionView::Idle => match s.processing.first() {
            Some(p) => Footer {
                kind: FooterKind::Listening,
                title: "Wrapping up a call".into(),
                subtitle: p
                    .pipeline
                    .steps
                    .iter()
                    .find(|st| st.status == StepStatus::Active)
                    .map(|st| st.kind.active_label().to_string())
                    .unwrap_or_else(|| auto.to_string()),
            },
            None => Footer { kind: FooterKind::Listening, title: "Listening for calls".into(), subtitle: auto.into() },
        },
        SessionView::Prompting { app, hint, .. } => Footer {
            kind: FooterKind::Prompting,
            title: format!("{} call detected", app.label()),
            subtitle: hint.clone().unwrap_or_else(|| "Record this call?".into()),
        },
        SessionView::Recording { app, hint, started_at, .. } => Footer {
            kind: FooterKind::Recording,
            title: format!("Recording · {}", mmss(now_ms - started_at)),
            subtitle: hint.clone().unwrap_or_else(|| app_label(*app).to_string()),
        },
    }
}

// ------------------------------------------------------------------ search, copy, transcript

/// Sidebar filter: FTS hits (from `store::search`) or a case-insensitive title/hint match.
pub fn filter_meetings<'a>(meetings: &'a [Meeting], query: &str, fts_ids: &[String]) -> Vec<&'a Meeting> {
    let q = query.trim().to_lowercase();
    meetings
        .iter()
        .filter(|m| {
            q.is_empty()
                || fts_ids.contains(&m.id)
                || m.display_title().to_lowercase().contains(&q)
                || m.hint.as_deref().is_some_and(|h| h.to_lowercase().contains(&q))
        })
        .collect()
}

/// Clipboard text: `# Title` + enhanced notes markdown (or the user's own notes).
pub fn copy_text(title: &str, enhanced: Option<&EnhancedNotes>, user_md: &str) -> String {
    let body = enhanced.map(EnhancedNotes::to_markdown).filter(|s| !s.trim().is_empty());
    let body = body.unwrap_or_else(|| user_md.trim().to_string());
    match body.is_empty() {
        true => format!("# {title}\n"),
        false => format!("# {title}\n\n{body}\n"),
    }
}

/// "03:12" / "1:02:03".
pub fn timestamp(secs: f64) -> String {
    mmss((secs.max(0.0) * 1000.0) as i64)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRow {
    pub time: String,
    pub speaker: String,
    pub me: bool,
    pub text: String,
}

/// Consecutive segments from the same speaker are merged into one row.
pub fn transcript_rows(segs: &[Segment], names: &BTreeMap<String, String>) -> Vec<TranscriptRow> {
    use muesli_core::model::Speaker;
    segs.iter().fold(Vec::<TranscriptRow>::new(), |mut rows, s| {
        let speaker = display_speaker(&s.speaker, names);
        match rows.last_mut() {
            Some(r) if r.speaker == speaker => {
                r.text.push(' ');
                r.text.push_str(s.text.trim());
            }
            _ => rows.push(TranscriptRow {
                time: timestamp(s.t_start),
                speaker,
                me: s.speaker == Speaker::Me,
                text: s.text.trim().to_string(),
            }),
        }
        rows
    })
}

/// "Point" + "detail" as one paragraph, adding a full stop when the point has none.
pub fn join_sentences(a: &str, b: &str) -> String {
    let a = a.trim_end();
    match a.ends_with(['.', '?', '!', ':', ';']) {
        true => format!("{a} {}", b.trim()),
        false => format!("{a}. {}", b.trim()),
    }
}

/// Plain-text transcript for the clipboard: title, then "[00:05] Speaker 1: …" lines.
pub fn transcript_text(title: &str, rows: &[TranscriptRow]) -> String {
    let lines: Vec<String> = rows.iter().map(|r| format!("[{}] {}: {}", r.time, r.speaker, r.text)).collect();
    format!("{title}\n\n{}\n", lines.join("\n"))
}

/// "elevenlabs/scribe-v2, assemblyai/universal-3-6-pro" ⇄ list.
pub fn parse_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// "google/gemini-2.5-flash" → "Gemini 2.5 Flash"; short acronyms ("gpt") upper-cased.
pub fn model_label(model: &str) -> String {
    let word = |w: &str| -> String {
        let acronym = w.len() <= 3 && w.chars().all(|c| c.is_ascii_alphabetic()) && !matches!(w, "pro" | "max");
        let mut c = w.chars();
        match (acronym, c.next()) {
            (true, _) => w.to_uppercase(),
            (false, Some(f)) => f.to_uppercase().chain(c).collect(),
            (false, None) => String::new(),
        }
    };
    model.rsplit('/').next().unwrap_or(model).split('-').map(word).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use muesli_core::model::{ActionItem, NoteItem, NoteSection, Speaker};
    use muesli_core::store::new_meeting;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        tz().with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }
    fn meeting(start: i64, mins: i64) -> Meeting {
        let mut m = new_meeting(Some(App::Zoom), None, start);
        m.ended_at = Some(start + mins * 60_000);
        m.state = MeetingState::Ready;
        m
    }

    #[test]
    fn groups_by_day() {
        let now = at(2025, 10, 17, 15, 0);
        let ms = [meeting(at(2025, 10, 17, 10, 30), 42), meeting(at(2025, 10, 17, 8, 15), 18),
            meeting(at(2025, 10, 16, 16, 0), 55), meeting(at(2025, 10, 13, 9, 0), 5), meeting(at(2025, 9, 1, 9, 0), 5),
            meeting(at(2024, 9, 1, 9, 0), 5)];
        let refs: Vec<&Meeting> = ms.iter().collect();
        let g = group_by_day(&refs, &tz(), now);
        let labels: Vec<&str> = g.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["Today", "Yesterday", "Monday", "Sep 1", "Sep 1, 2024"]);
        assert_eq!(g[0].meetings.len(), 2);
    }

    #[test]
    fn row_and_range_meta() {
        let now = at(2025, 10, 17, 15, 0);
        let m = meeting(at(2025, 10, 17, 10, 30), 42);
        assert_eq!(row_meta(&m, &tz(), now), "10:30 · 42 min");
        assert_eq!(time_range(&m, &tz()), "Fri, Oct 17 · 10:30–11:12");
        let fresh = meeting(now - 40 * 60_000, 37);
        assert_eq!(row_meta(&fresh, &tz(), now), "Just now · 37 min");
        let mut rec = new_meeting(None, None, now - 12 * 60_000);
        rec.state = MeetingState::Recording;
        assert_eq!(row_meta(&rec, &tz(), now), "Recording · 12 min");
    }

    fn seg(sp: Speaker) -> Segment {
        Segment { speaker: sp, t_start: 0.0, t_end: 1.0, text: "x".into() }
    }

    #[test]
    fn people() {
        let them = |l: &str| seg(Speaker::Them(l.into()));
        let segs = vec![seg(Speaker::Me), them("c0s0"), them("c0s1"), them("c1s0")];
        let mut names = BTreeMap::new();
        assert_eq!(people_line(&segs, &names).unwrap(), "You + 2 others");
        names.insert("c0s0".into(), "Lena".into());
        names.insert("c1s0".into(), "Lena".into());
        names.insert("c0s1".into(), "Marco".into());
        assert_eq!(people_line(&segs, &names).unwrap(), "Lena, Marco, you");
        assert_eq!(people_line(&[seg(Speaker::Me)], &names).unwrap(), "Just you");
        assert_eq!(people_line(&[], &names), None);
    }

    #[test]
    fn naming() {
        let mut m = meeting(at(2025, 10, 17, 13, 25), 37);
        m.state = MeetingState::Processing;
        m.title_suggestions = vec!["Offline sync spec review".into(), "offline sync spec review".into(), "B".into()];
        let c = chips(&m);
        assert_eq!(c.len(), 3);
        assert!(c[2].fallback && c[2].title.starts_with("Zoom call · Oct 17"));
        assert_eq!(initial_title(&m), "Offline sync spec review");
        assert!(shows_naming(&m, false));
        assert!(!shows_naming(&m, true));
        m.title = Some("Offline sync spec review".into());
        assert_eq!(title_action(&m, " Offline sync spec review "), TitleAction::Confirm);
        assert_eq!(title_action(&m, "New"), TitleAction::Rename("New".into()));
        assert_eq!(title_action(&m, "  "), TitleAction::Nothing);
        m.title_confirmed = true;
        assert_eq!(title_action(&m, "Offline sync spec review"), TitleAction::Nothing);
    }

    #[test]
    fn wrapping() {
        let p = Pipeline::default()
            .with(StepKind::Saved, StepStatus::Done, None)
            .with(StepKind::Cleaned, StepStatus::Done, Some("Removed speaker echo and 15 min of silence".into()))
            .with(StepKind::Transcribed, StepStatus::Done, None)
            .with(StepKind::Notes, StepStatus::Active, None);
        let rows = wrapping_rows(&p, Some(37 * 60_000));
        assert_eq!(rows[0].detail, "37 min of you and the call, kept on this computer");
        assert_eq!(rows[1].detail, "Removed speaker echo and 15 min of silence");
        assert_eq!((rows[3].label.as_str(), rows[3].indicator, rows[3].active), ("Notes in progress…", Indicator::Spinner, true));
        assert_eq!(rows[0].indicator, Indicator::Check);
        assert_eq!(wrapping_summary(&p), "Notes ready in about a minute");
        let f = p.clone().with(StepKind::Notes, StepStatus::Failed, None);
        assert_eq!(wrapping_rows(&f, None)[3].label, "Notes failed");
        assert!(!notes_done(&p));
    }

    #[test]
    fn footer_states() {
        assert_eq!(footer(None, 0).kind, FooterKind::Offline);
        let s = Status { session: SessionView::Idle, auto_record: true, processing: vec![], needs_name: vec![], api_key: true };
        assert_eq!(footer(Some(&s), 0).title, "Listening for calls");
        assert_eq!(footer(Some(&s), 0).subtitle, "Auto-record on");
        let r = Status {
            session: SessionView::Recording { meeting_id: "x".into(), app: Some(App::Discord), hint: None, started_at: 0, manual: false },
            ..s
        };
        assert_eq!(footer(Some(&r), 75_000).title, "Recording · 01:15");
    }

    #[test]
    fn search_filter_and_copy() {
        let mut a = meeting(0, 1);
        a.title = Some("Q4 roadmap with Lena".into());
        let mut b = meeting(0, 1);
        b.hint = Some("Hiking crew · #planning".into());
        let ms = vec![a.clone(), b.clone()];
        assert_eq!(filter_meetings(&ms, "", &[]).len(), 2);
        assert_eq!(filter_meetings(&ms, "roadmap", &[]).len(), 1);
        assert_eq!(filter_meetings(&ms, "HIKING", &[]).len(), 1);
        assert_eq!(filter_meetings(&ms, "pricing", &[b.id.clone()])[0].id, b.id);

        let n = EnhancedNotes {
            sections: vec![NoteSection { title: "Decisions".into(), items: vec![NoteItem { text: "Ship".into(), from_user: true, detail: None }] }],
            action_items: vec![ActionItem { text: "Spec".into(), owner: None, from_user: false, done: false }],
        };
        assert_eq!(copy_text("T", Some(&n), ""), "# T\n\n## Decisions\n\n- Ship\n\n## Action items\n\n- [ ] Spec\n");
        assert_eq!(copy_text("T", None, " - mine "), "# T\n\n- mine\n");
    }

    #[test]
    fn transcript() {
        let s = |sp: Speaker, t: f64, x: &str| Segment { speaker: sp, t_start: t, t_end: t + 1.0, text: x.into() };
        let segs = vec![s(Speaker::Me, 0.0, "Hi."), s(Speaker::Me, 2.0, "Ready?"), s(Speaker::Them("c0s0".into()), 192.0, "Yes")];
        let rows = transcript_rows(&segs, &BTreeMap::new());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "Hi. Ready?");
        assert_eq!((rows[1].time.as_str(), rows[1].speaker.as_str()), ("03:12", "Speaker 1"));
        assert_eq!(timestamp(3723.0), "1:02:03");
        assert_eq!(transcript_text("Sync", &rows), "Sync\n\n[00:00] You: Hi. Ready?\n[03:12] Speaker 1: Yes\n");
    }

    #[test]
    fn misc() {
        assert_eq!(join_sentences("Free Granola alternative", "Testing live."), "Free Granola alternative. Testing live.");
        assert_eq!(join_sentences("Ship it?", "Yes."), "Ship it? Yes.");
        assert_eq!(parse_list(" a/b , ,c "), vec!["a/b", "c"]);
        assert_eq!(model_label("google/gemini-2.5-flash"), "Gemini 2.5 Flash");
        assert_eq!(model_label("anthropic/claude-sonnet-4.5"), "Claude Sonnet 4.5");
        assert_eq!(model_label("openai/gpt-4o"), "GPT 4o");
    }
}
