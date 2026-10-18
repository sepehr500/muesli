//! Domain types shared by daemon, CLI, UI and (via JSON) the Omarchy plugin.

use crate::detect::App;
use serde::{Deserialize, Serialize};

pub type MeetingId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingState {
    Recording,
    Processing,
    Ready,
    Failed,
}

impl MeetingState {
    pub fn as_str(self) -> &'static str {
        match self {
            MeetingState::Recording => "recording",
            MeetingState::Processing => "processing",
            MeetingState::Ready => "ready",
            MeetingState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "recording" => MeetingState::Recording,
            "processing" => MeetingState::Processing,
            "ready" => MeetingState::Ready,
            _ => MeetingState::Failed,
        }
    }
}

/// The post-call pipeline steps, in order. Mirrors the "Wrapping up" list in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// "Recording saved"
    Saved,
    /// "Audio cleaned" (echo removed, silence trimmed)
    Cleaned,
    /// "Transcript ready"
    Transcribed,
    /// "Notes ready" / "Notes in progress…"
    Notes,
}

impl StepKind {
    pub const ALL: [StepKind; 4] =
        [StepKind::Saved, StepKind::Cleaned, StepKind::Transcribed, StepKind::Notes];

    /// Sentence-case label, done form.
    pub fn done_label(self) -> &'static str {
        match self {
            StepKind::Saved => "Recording saved",
            StepKind::Cleaned => "Audio cleaned",
            StepKind::Transcribed => "Transcript ready",
            StepKind::Notes => "Notes ready",
        }
    }
    /// Sentence-case label, in-progress form.
    pub fn active_label(self) -> &'static str {
        match self {
            StepKind::Saved => "Saving recording…",
            StepKind::Cleaned => "Cleaning audio…",
            StepKind::Transcribed => "Transcribing…",
            StepKind::Notes => "Notes in progress…",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub kind: StepKind,
    pub status: StepStatus,
    /// Human detail, e.g. "Removed speaker echo and 15 min of silence".
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub steps: Vec<Step>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Pipeline {
            steps: StepKind::ALL
                .iter()
                .map(|&kind| Step { kind, status: StepStatus::Pending, detail: None })
                .collect(),
        }
    }
}

impl Pipeline {
    pub fn with(mut self, kind: StepKind, status: StepStatus, detail: Option<String>) -> Self {
        if let Some(s) = self.steps.iter_mut().find(|s| s.kind == kind) {
            s.status = status;
            if detail.is_some() {
                s.detail = detail;
            }
        }
        self
    }
    pub fn status_of(&self, kind: StepKind) -> StepStatus {
        self.steps.iter().find(|s| s.kind == kind).map(|s| s.status).unwrap_or(StepStatus::Pending)
    }
    pub fn done_count(&self) -> usize {
        self.steps.iter().filter(|s| s.status == StepStatus::Done).count()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub id: MeetingId,
    pub title: Option<String>,
    /// False until the user saves/accepts a name.
    pub title_confirmed: bool,
    pub title_suggestions: Vec<String>,
    pub app: Option<App>,
    pub hint: Option<String>,
    /// Unix ms.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub state: MeetingState,
    pub pipeline: Pipeline,
    /// Chunk-local speaker label → display name (from LLM speaker linking).
    pub speaker_names: std::collections::BTreeMap<String, String>,
    pub cost_usd: f64,
    pub error: Option<String>,
}

impl Meeting {
    pub fn duration_ms(&self) -> Option<i64> {
        self.ended_at.map(|e| e - self.started_at)
    }
    /// Title for display, never empty.
    pub fn display_title(&self) -> String {
        self.title.clone().unwrap_or_else(|| "Untitled meeting".to_string())
    }
}

/// Who spoke. `Me` = mic channel; `Them(label)` = system channel with a chunk-local label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "label", rename_all = "snake_case")]
pub enum Speaker {
    Me,
    Them(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub speaker: Speaker,
    /// Seconds since meeting start.
    pub t_start: f64,
    pub t_end: f64,
    pub text: String,
}

/// LLM-enhanced notes, structured so the UI can style user vs AI text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnhancedNotes {
    #[serde(default)]
    pub sections: Vec<NoteSection>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteSection {
    pub title: String,
    #[serde(default)]
    pub items: Vec<NoteItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteItem {
    pub text: String,
    /// True if this point came from the user's own notes.
    #[serde(default)]
    pub from_user: bool,
    /// AI elaboration shown on its own line beneath `text`.
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub from_user: bool,
    #[serde(default)]
    pub done: bool,
}

impl EnhancedNotes {
    pub fn to_markdown(&self) -> String {
        let sections = self.sections.iter().map(|s| {
            let items = s.items.iter().map(|i| match &i.detail {
                Some(d) => format!("- {}\n  {}", i.text, d),
                None => format!("- {}", i.text),
            });
            format!("## {}\n\n{}", s.title, items.collect::<Vec<_>>().join("\n"))
        });
        let actions = (!self.action_items.is_empty()).then(|| {
            let rows = self.action_items.iter().map(|a| {
                let owner = a.owner.as_deref().map(|o| format!(" — {o}")).unwrap_or_default();
                format!("- [{}] {}{}", if a.done { "x" } else { " " }, a.text, owner)
            });
            format!("## Action items\n\n{}", rows.collect::<Vec<_>>().join("\n"))
        });
        sections.chain(actions).collect::<Vec<_>>().join("\n\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notes {
    pub meeting_id: MeetingId,
    /// The user's own markdown notes.
    pub user_md: String,
    pub enhanced: Option<EnhancedNotes>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

/// Fallback title when there's no LLM suggestion: "Zoom call · Oct 17, 13:25".
pub fn fallback_title(app: Option<App>, started_at_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let when = Local
        .timestamp_millis_opt(started_at_ms)
        .single()
        .map(|t| t.format("%b %-d, %H:%M").to_string())
        .unwrap_or_default();
    match app {
        Some(a) => format!("{} call · {when}", a.label()),
        None => format!("Meeting · {when}"),
    }
}

/// "37 min", "1 h 12 min".
pub fn human_duration(ms: i64) -> String {
    let mins = (ms as f64 / 60_000.0).round() as i64;
    match mins {
        m if m < 1 => "<1 min".to_string(),
        m if m < 60 => format!("{m} min"),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(37 * 60_000 + 12_000), "37 min");
        assert_eq!(human_duration(72 * 60_000), "1 h 12 min");
        assert_eq!(human_duration(20_000), "<1 min");
    }

    #[test]
    fn markdown_render() {
        let n = EnhancedNotes {
            sections: vec![NoteSection {
                title: "Decisions".into(),
                items: vec![NoteItem {
                    text: "Ship offline mode".into(),
                    from_user: true,
                    detail: Some("Dec 12 release.".into()),
                }],
            }],
            action_items: vec![ActionItem {
                text: "Write spec".into(),
                owner: Some("You".into()),
                from_user: true,
                done: false,
            }],
        };
        assert_eq!(
            n.to_markdown(),
            "## Decisions\n\n- Ship offline mode\n  Dec 12 release.\n\n## Action items\n\n- [ ] Write spec — You"
        );
    }

    #[test]
    fn pipeline_updates() {
        let p = Pipeline::default().with(StepKind::Saved, StepStatus::Done, Some("x".into()));
        assert_eq!(p.status_of(StepKind::Saved), StepStatus::Done);
        assert_eq!(p.done_count(), 1);
    }
}
