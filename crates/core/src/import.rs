//! External recording imports: the manifest wire format and its (pure) validation.
//!
//! A recorder outside muesli (e.g. a Discord bot that records every speaker separately)
//! writes WAV files plus an `import.json` manifest, then runs
//! `muesli-cli import --manifest /abs/import.json --json`. The daemon copies the audio into
//! its own storage, creates the meeting and replies `{id, state, duplicate}`; cleaning,
//! transcription and notes run in the background like any other recording.
//!
//! See `docs/import.md` for the full contract.

use crate::detect::App;
use crate::model::{MeetingId, MeetingState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub const MANIFEST_VERSION: u32 = 1;
/// Every track must be 16-bit PCM, mono, at this rate.
pub const RATE: u32 = 16_000;
pub const MAX_TRACKS: usize = 64;
pub const MAX_MANIFEST_BYTES: u64 = 1 << 20;
pub const MAX_DURATION_MS: i64 = 12 * 3_600_000;
/// A track may run this far past `ended_at - started_at` (clock skew between recorder parts).
pub const TRACK_SLACK_S: f64 = 30.0;
const MAX_NAME: usize = 100;
const MAX_HINT: usize = 200;
const MAX_SOURCE_ID: usize = 200;
/// 2000-01-01: anything earlier is a units bug (seconds instead of milliseconds).
const MIN_STARTED_AT: i64 = 946_684_800_000;

/// `import.json`, version 1. Unknown fields are rejected so typos fail loudly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    /// Stable caller identity of the recording, e.g. `discord:<guild>:<session uuid>`.
    /// Importing the same `source_id` again returns the existing meeting.
    pub source_id: String,
    #[serde(default)]
    pub app: Option<String>,
    /// Channel or room name; helps naming the meeting.
    #[serde(default)]
    pub hint: Option<String>,
    /// Unix ms of meeting time zero (every track starts here).
    pub started_at: i64,
    pub ended_at: i64,
    pub tracks: Vec<ManifestTrack>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTrack {
    /// The speaker's display name. Tracks with the same name are the same person (e.g. one
    /// file per resumed recording segment). Omit for a mixed track with several voices,
    /// which is diarized and named like a normal call.
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute path to a 16 kHz mono 16-bit PCM WAV, aligned to meeting time zero.
    pub path: PathBuf,
}

/// One speaker in an import. Named speakers get labels `t0, t1, …` (first appearance order);
/// each mixed track gets its own `m0, m1, …`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportSpeaker {
    pub label: String,
    pub name: Option<String>,
}

/// A validated manifest, ready to ingest.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPlan {
    pub source_id: String,
    pub app: Option<App>,
    pub hint: Option<String>,
    pub started_at: i64,
    pub ended_at: i64,
    pub speakers: Vec<ImportSpeaker>,
    /// (speaker label, source path), in manifest order.
    pub tracks: Vec<(String, PathBuf)>,
}

impl ImportPlan {
    pub fn duration_s(&self) -> f64 {
        (self.ended_at - self.started_at) as f64 / 1000.0
    }
    /// Label → display name for named speakers (the deterministic, known names).
    pub fn known_names(&self) -> BTreeMap<String, String> {
        known_names(&self.speakers)
    }
}

pub fn known_names(speakers: &[ImportSpeaker]) -> BTreeMap<String, String> {
    speakers.iter().filter_map(|s| Some((s.label.clone(), s.name.clone()?))).collect()
}

/// Reply to an import request (`--json` output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportReply {
    pub id: MeetingId,
    pub state: MeetingState,
    /// True when this `source_id` was already imported; nothing new was stored.
    pub duplicate: bool,
}

/// Errors that retrying cannot fix start with this prefix (the CLI exits 2 for them).
pub const INVALID: &str = "invalid import: ";

pub fn invalid(msg: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{INVALID}{msg}")
}

pub fn is_invalid(e: &str) -> bool {
    e.starts_with(INVALID)
}

pub fn parse(json: &str) -> anyhow::Result<Manifest> {
    serde_json::from_str(json).map_err(|e| invalid(format!("manifest: {e}")))
}

fn clean_text(what: &str, s: &str, max: usize) -> anyhow::Result<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match () {
        _ if s.is_empty() => Err(invalid(format!("{what} is empty"))),
        _ if s.chars().count() > max => Err(invalid(format!("{what} is longer than {max} characters"))),
        _ if s.chars().any(char::is_control) => Err(invalid(format!("{what} has control characters"))),
        _ => Ok(s),
    }
}

/// Absolute, no `.`/`..` components, `.wav`. (Containment and file checks happen on disk.)
pub fn check_track_path(p: &Path) -> anyhow::Result<()> {
    let shown = p.display();
    anyhow::ensure!(p.is_absolute(), invalid(format!("track path {shown} is not absolute")));
    anyhow::ensure!(
        p.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        invalid(format!("track path {shown} has . or .. components"))
    );
    let wav = p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    anyhow::ensure!(wav, invalid(format!("track path {shown} is not a .wav file")));
    Ok(())
}

impl Manifest {
    /// Check everything that doesn't need the filesystem. `now_ms` bounds `started_at`.
    pub fn validate(&self, now_ms: i64) -> anyhow::Result<ImportPlan> {
        use anyhow::ensure;
        ensure!(self.version == MANIFEST_VERSION, invalid(format!("unsupported version {} (expected 1)", self.version)));
        let id = &self.source_id;
        ensure!(
            !id.is_empty()
                && id.len() <= MAX_SOURCE_ID
                && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)),
            invalid(format!("source_id must be 1-{MAX_SOURCE_ID} characters of A-Z a-z 0-9 - _ . :"))
        );
        let app = match self.app.as_deref() {
            None => None,
            Some("discord") => Some(App::Discord),
            Some("zoom") => Some(App::Zoom),
            Some(a) => return Err(invalid(format!("unknown app {a:?} (expected \"discord\" or \"zoom\")"))),
        };
        if app == Some(App::Discord) {
            ensure!(id.starts_with("discord:"), invalid("discord source_id must look like discord:<guild>:<session>"));
        }
        let hint = self.hint.as_deref().filter(|h| !h.trim().is_empty()).map(|h| clean_text("hint", h, MAX_HINT)).transpose()?;
        ensure!(
            (MIN_STARTED_AT..=now_ms + 86_400_000).contains(&self.started_at),
            invalid("started_at must be unix milliseconds, not in the future")
        );
        let dur = self.ended_at - self.started_at;
        ensure!(dur > 0, invalid("ended_at must be after started_at"));
        ensure!(dur <= MAX_DURATION_MS, invalid("meetings longer than 12 hours can't be imported"));
        ensure!(!self.tracks.is_empty(), invalid("no tracks"));
        ensure!(self.tracks.len() <= MAX_TRACKS, invalid(format!("more than {MAX_TRACKS} tracks")));
        let mut seen = std::collections::BTreeSet::new();
        for t in &self.tracks {
            check_track_path(&t.path)?;
            ensure!(seen.insert(&t.path), invalid(format!("track {} is listed twice", t.path.display())));
        }
        let names = self
            .tracks
            .iter()
            .map(|t| {
                let name = t.name.as_deref().map(|n| clean_text("track name", n, MAX_NAME)).transpose()?;
                ensure!(
                    !name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case("you")),
                    invalid("\"You\" is reserved for your own microphone")
                );
                Ok(name)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Same name → same speaker label; every mixed track is its own label.
        let (speakers, labels) = names.iter().fold((Vec::<ImportSpeaker>::new(), vec![]), |(mut sp, mut labels), name| {
            let label = match name {
                Some(n) => match sp.iter().find(|s| s.name.as_ref() == Some(n)) {
                    Some(s) => s.label.clone(),
                    None => {
                        let l = format!("t{}", sp.iter().filter(|s| s.name.is_some()).count());
                        sp.push(ImportSpeaker { label: l.clone(), name: Some(n.clone()) });
                        l
                    }
                },
                None => {
                    let l = format!("m{}", sp.iter().filter(|s| s.name.is_none()).count());
                    sp.push(ImportSpeaker { label: l.clone(), name: None });
                    l
                }
            };
            labels.push(label);
            (sp, labels)
        });
        Ok(ImportPlan {
            source_id: id.clone(),
            app,
            hint,
            started_at: self.started_at,
            ended_at: self.ended_at,
            speakers,
            tracks: labels.into_iter().zip(self.tracks.iter().map(|t| t.path.clone())).collect(),
        })
    }
}

/// FNV-1a, 64-bit: stable across builds (unlike `DefaultHasher`).
fn fnv64(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// Deterministic meeting id for an import: a ULID whose time is the meeting start and whose
/// random part is a hash of `source_id`. A retry after a crash lands in the same directory.
pub fn meeting_id(source_id: &str, started_at: i64) -> MeetingId {
    let a = fnv64(0xcbf2_9ce4_8422_2325, source_id.as_bytes());
    let b = fnv64(0x8422_2325_cbf2_9ce4, source_id.as_bytes());
    let random = (((a as u128) << 64) | b as u128) & ((1u128 << 80) - 1);
    ulid::Ulid::from_parts(started_at.max(0) as u64, random).to_string().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000_000;

    fn manifest(tracks: serde_json::Value) -> String {
        serde_json::json!({
            "version": 1,
            "source_id": "discord:123456789:1b4e28ba-2fa1-11d2-883f-0016d3cca427",
            "app": "discord",
            "hint": "  Hiking crew  #planning ",
            "started_at": NOW - 3_600_000,
            "ended_at": NOW,
            "tracks": tracks,
        })
        .to_string()
    }

    fn err(json: &str) -> String {
        parse(json).and_then(|m| m.validate(NOW)).unwrap_err().to_string()
    }

    #[test]
    fn wire_format_and_speaker_grouping() {
        let json = manifest(serde_json::json!([
            {"name": "Matt Rosenthal", "path": "/rec/s/matt-1.wav"},
            {"name": "Lena", "path": "/rec/s/lena.wav"},
            {"name": "Matt Rosenthal", "path": "/rec/s/matt-2.wav"},
            {"path": "/rec/s/room.wav"},
        ]));
        let plan = parse(&json).unwrap().validate(NOW).unwrap();
        assert_eq!(plan.app, Some(App::Discord));
        assert_eq!(plan.hint.as_deref(), Some("Hiking crew #planning"));
        assert_eq!(plan.duration_s(), 3600.0);
        let labels: Vec<_> = plan.tracks.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["t0", "t1", "t0", "m0"], "a resumed speaker keeps one label");
        assert_eq!(
            plan.known_names().into_iter().collect::<Vec<_>>(),
            [("t0".to_string(), "Matt Rosenthal".to_string()), ("t1".to_string(), "Lena".to_string())]
        );
        let reply = ImportReply { id: "01x".into(), state: MeetingState::Processing, duplicate: false };
        assert_eq!(serde_json::to_string(&reply).unwrap(), r#"{"id":"01x","state":"processing","duplicate":false}"#);
    }

    #[test]
    fn rejects_bad_manifests() {
        let track = |p: &str| serde_json::json!([{"name": "A", "path": p}]);
        assert!(err(&manifest(track("rel/a.wav"))).contains("not absolute"));
        assert!(err(&manifest(track("/rec/../etc/a.wav"))).contains(".."));
        assert!(err(&manifest(track("/rec/a.mp3"))).contains(".wav"));
        assert!(err(&manifest(serde_json::json!([]))).contains("no tracks"));
        assert!(err(&manifest(serde_json::json!([{"name": "You", "path": "/a.wav"}]))).contains("reserved"));
        assert!(err(&manifest(serde_json::json!([{"name": " ", "path": "/a.wav"}]))).contains("empty"));
        assert!(err(&manifest(serde_json::json!([{"name": "A", "path": "/a.wav"}, {"name": "B", "path": "/a.wav"}]))).contains("twice"));
        let many: Vec<_> = (0..65).map(|i| serde_json::json!({"name": "A", "path": format!("/t{i}.wav")})).collect();
        assert!(err(&manifest(serde_json::json!(many))).contains("more than 64"));
        let base: serde_json::Value = serde_json::from_str(&manifest(track("/a.wav"))).unwrap();
        let with = |k: &str, v: serde_json::Value| {
            let mut m = base.clone();
            m[k] = v;
            m.to_string()
        };
        assert!(err(&with("version", 2.into())).contains("version"));
        assert!(err(&with("source_id", "discord:a b".into())).contains("source_id"));
        assert!(err(&with("source_id", "zoom:1".into())).contains("discord:"));
        assert!(err(&with("app", "teams".into())).contains("unknown app"));
        assert!(err(&with("started_at", 1_700_000_000.into())).contains("milliseconds"), "seconds, not ms");
        assert!(err(&with("ended_at", (NOW - 3_600_000).into())).contains("after"));
        assert!(err(&with("started_at", (NOW - 13 * 3_600_000).into())).contains("12 hours"));
        assert!(err(&with("extra", 1.into())).contains("unknown field"));
        assert!(err(r#"{"version":1}"#).starts_with(INVALID));
        assert!(is_invalid(&err(&with("version", 2.into()))));
    }

    #[test]
    fn meeting_id_is_deterministic_and_sorts_by_start() {
        let a = meeting_id("discord:1:x", 1_000);
        assert_eq!(a, meeting_id("discord:1:x", 1_000));
        assert_ne!(a, meeting_id("discord:1:y", 1_000));
        assert!(meeting_id("discord:1:x", 2_000) > a);
        assert_eq!(a.len(), 26);
        assert!(a.parse::<ulid::Ulid>().is_ok() || ulid::Ulid::from_string(&a.to_uppercase()).is_ok());
    }
}
