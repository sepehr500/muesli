//! Post-call pipeline: Saved → Cleaned → Transcribed (+ titles) → Notes.
//! Resumable: each run starts at the first step that isn't Done.

use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow};
use muesli_audio::clean::{CleanOptions, clean};
use muesli_core::model::*;
use muesli_core::openrouter::Client;
use muesli_core::transcript::{self, TimeMap};
use muesli_core::{config, import, paths, prompts, store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkRecord {
    me: bool,
    /// Imported named track: every line in this chunk is this speaker label (no diarization).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<String>,
    file: String,
    map: TimeMap,
    speech_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CleanManifest {
    chunks: Vec<ChunkRecord>,
    silence_s: f64,
    duration_s: f64,
    echo_cancelled: bool,
}

pub fn others_phrase(n: usize) -> String {
    match n {
        0 => "Just you".into(),
        1 => "You and 1 other".into(),
        n => format!("You and {n} others"),
    }
}

/// "You and 2 others", or for imported recordings (no "You" channel) "3 speakers".
pub fn people_phrase(segs: &[Segment], names: &BTreeMap<String, String>, imported: bool) -> String {
    let n = transcript::other_speaker_count(segs, names);
    match (imported, n) {
        (false, n) => others_phrase(n),
        (true, 1) => "1 speaker".into(),
        (true, n) => format!("{n} speakers"),
    }
}

/// Known names of an imported meeting's tracks (label → name). A name the user gave a
/// track (`current`) wins over the manifest name, so renames survive reprocessing; a reset
/// rename falls back to the manifest name. Empty for normal recordings.
pub fn known_names(speakers: &[import::ImportSpeaker], current: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    import::known_names(speakers)
        .into_iter()
        .map(|(label, name)| {
            let name = current.get(&label).cloned().unwrap_or(name);
            (label, name)
        })
        .collect()
}

/// Like `human_duration`, but seconds under a minute ("45 s").
fn lower_duration(ms: i64) -> String {
    match ms < 60_000 {
        true => format!("{} s", (ms as f64 / 1000.0).round() as i64),
        false => human_duration(ms),
    }
}

/// Run (or resume) the pipeline. `enhance_only` re-runs just the Notes step.
pub async fn run(ctx: Arc<Ctx>, id: String, enhance_only: bool) {
    {
        let mut jobs = ctx.jobs.lock().expect("jobs");
        if !jobs.insert(id.clone()) {
            tracing::info!("pipeline for {id} already running");
            return;
        }
    }
    let res = steps(&ctx, &id, enhance_only).await;
    ctx.jobs.lock().expect("jobs").remove(&id);
    if let Err(e) = res {
        tracing::error!("pipeline {id} failed: {e:#}");
        if let Ok(mut m) = ctx.meeting(&id) {
            let active = m.pipeline.steps.iter().find(|s| s.status == StepStatus::Active).map(|s| s.kind);
            if let Some(k) = active {
                m.pipeline = m.pipeline.with(k, StepStatus::Failed, Some(short_error(&e)));
            }
            m.state = MeetingState::Failed;
            m.error = Some(format!("{e:#}"));
            let _ = ctx.save(&m);
            ctx.notifier.info(
                "Couldn't finish your notes",
                &format!("{} — {}", m.display_title(), short_error(&e)),
                &[(&format!("reprocess:{id}"), "Retry"), (&format!("open:{id}"), "Open")],
            );
            crate::hooks::emit_logged(&ctx, muesli_core::hooks::HookEvent::MeetingFailed, &id);
        }
    }
}

fn short_error(e: &anyhow::Error) -> String {
    let s = format!("{e}");
    match s.len() > 140 {
        true => format!("{}…", s.chars().take(140).collect::<String>()),
        false => s,
    }
}

async fn set_step(ctx: &Ctx, m: &mut Meeting, kind: StepKind, status: StepStatus, detail: Option<String>) -> Result<()> {
    m.pipeline = std::mem::take(&mut m.pipeline).with(kind, status, detail);
    save(ctx, m)
}

/// Save the pipeline's copy of the meeting without clobbering a title the user set while
/// it was running (the pipeline holds `m` across slow network calls).
fn save(ctx: &Ctx, m: &mut Meeting) -> Result<()> {
    if let Ok(cur) = ctx.meeting(&m.id)
        && cur.title_confirmed
    {
        m.title = cur.title;
        m.title_confirmed = true;
    }
    ctx.save(m)
}

async fn steps(ctx: &Arc<Ctx>, id: &str, enhance_only: bool) -> Result<()> {
    let mut m = ctx.meeting(id)?;
    let dir = paths::meeting_dir(id);
    let raw = dir.join("raw");
    let clean_dir = dir.join("clean");
    let cfg = ctx.cfg();
    m.state = MeetingState::Processing;
    m.error = None;
    // Reset failed steps to pending so they re-run.
    m.pipeline.steps.iter_mut().filter(|s| s.status == StepStatus::Failed || s.status == StepStatus::Active).for_each(|s| s.status = StepStatus::Pending);
    if enhance_only {
        m.pipeline = std::mem::take(&mut m.pipeline).with(StepKind::Notes, StepStatus::Pending, None);
    }
    ctx.save(&m)?;

    // 1. Saved
    if m.pipeline.status_of(StepKind::Saved) != StepStatus::Done {
        let raw2 = raw.clone();
        // Imports: converts the staged copies into raw tracks first (restartable).
        let info = tokio::task::spawn_blocking(move || muesli_audio::import::recording_info(&raw2)).await??;
        if m.ended_at.is_none() {
            m.ended_at = Some(m.started_at + (info.duration_s * 1000.0) as i64);
        }
        let dur = lower_duration(m.duration_ms().unwrap_or(0));
        let detail = match info.tracks.len() {
            0 => format!("{dur} of you and the call, kept on this computer"),
            1 => format!("{dur} imported, kept on this computer"),
            n => format!("{dur} imported in {n} tracks, kept on this computer"),
        };
        set_step(ctx, &mut m, StepKind::Saved, StepStatus::Done, Some(detail)).await?;
    }

    // 2. Cleaned
    let manifest_path = clean_dir.join("chunks.json");
    if m.pipeline.status_of(StepKind::Cleaned) != StepStatus::Done || !manifest_path.exists() {
        set_step(ctx, &mut m, StepKind::Cleaned, StepStatus::Active, None).await?;
        let (raw2, clean2) = (raw.clone(), clean_dir.clone());
        let opts = CleanOptions {
            echo_cancellation: cfg.recording.echo_cancellation,
            playback_dir: Some(paths::playback_dir(id)),
            ..Default::default()
        };
        let r = tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(&clean2);
            clean(&raw2, &clean2, &opts)
        })
        .await??;
        let rel = |p: &PathBuf| p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let chunks = r
            .mic_chunks
            .iter()
            .map(|c| ChunkRecord { me: true, track: None, file: rel(&c.path), map: c.map.clone(), speech_s: c.speech_s })
            .chain(r.sys_chunks.iter().map(|c| ChunkRecord { me: false, track: None, file: rel(&c.path), map: c.map.clone(), speech_s: c.speech_s }))
            .chain(r.track_chunks.iter().flat_map(|t| {
                // Named tracks keep their label; mixed tracks are diarized like the call channel.
                let track = t.name.as_ref().map(|_| t.label.clone());
                t.chunks.iter().map(move |c| ChunkRecord {
                    me: false,
                    track: track.clone(),
                    file: rel(&c.path),
                    map: c.map.clone(),
                    speech_s: c.speech_s,
                })
            }))
            .collect();
        let manifest = CleanManifest { chunks, silence_s: r.silence_s, duration_s: r.duration_s, echo_cancelled: r.echo_cancelled };
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        let silence = lower_duration((r.silence_s * 1000.0) as i64);
        let detail = match (r.echo_cancelled, r.silence_s >= 5.0) {
            (true, true) => format!("Removed speaker echo and {silence} of silence"),
            (true, false) => "Removed speaker echo".to_string(),
            (false, true) => format!("Trimmed {silence} of silence"),
            (false, false) => "Ready for transcription".to_string(),
        };
        set_step(ctx, &mut m, StepKind::Cleaned, StepStatus::Done, Some(detail)).await?;
    }
    let manifest: CleanManifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let import_speakers = ctx.db(|c| store::import_speakers(c, id))?;
    let imported = !import_speakers.is_empty();

    // 3. Transcribed (+ title suggestions)
    if m.pipeline.status_of(StepKind::Transcribed) != StepStatus::Done {
        set_step(ctx, &mut m, StepKind::Transcribed, StepStatus::Active, None).await?;
        let client = Client::from_config(&cfg).context("Add an OpenRouter key in Settings")?;
        let (segs, cost) = transcribe_all(&client, &cfg, &clean_dir, &manifest).await?;
        ctx.db(|c| store::replace_segments(c, id, &segs))?;
        m.cost_usd += cost;
        let known = known_names(&import_speakers, &ctx.meeting(id)?.speaker_names);
        m.speaker_names.extend(known.clone());
        let detail = match segs.is_empty() {
            true => "No speech detected".to_string(),
            false => format!("{} · cost ${:.2}", people_phrase(&segs, &m.speaker_names, imported), m.cost_usd),
        };
        // Title suggestions (fast model). Failure here isn't fatal.
        if !segs.is_empty() && !m.title_confirmed {
            let notes = ctx.db(|c| store::get_notes(c, id))?.user_md;
            let msgs = prompts::title_messages(
                m.app.map(|a| a.label()).unwrap_or("Meeting"),
                m.hint.as_deref(),
                &transcript::render_for_prompt(&segs, &known),
                &notes,
            );
            match client.chat(&cfg.openrouter.chat_model, &msgs, true).await {
                Ok(r) => {
                    m.cost_usd += r.cost;
                    let titles = prompts::parse_titles(&r.content);
                    if let Some(first) = titles.first() {
                        m.title = Some(first.clone());
                    }
                    m.title_suggestions = titles;
                }
                Err(e) => tracing::warn!("title suggestions failed: {e:#}"),
            }
        }
        if m.title.is_none() {
            m.title = Some(m.hint.clone().unwrap_or_else(|| fallback_title(m.app, m.started_at)));
        }
        // Named for you: the best suggestion is the title (rename any time in the app).
        m.title_confirmed = true;
        set_step(ctx, &mut m, StepKind::Transcribed, StepStatus::Done, Some(detail)).await?;
    }

    // 4. Notes
    if m.pipeline.status_of(StepKind::Notes) != StepStatus::Done {
        set_step(ctx, &mut m, StepKind::Notes, StepStatus::Active, Some("Filling in your bullets with decisions and to-dos".into())).await?;
        let segs = ctx.db(|c| store::segments(c, id))?;
        let notes = ctx.db(|c| store::get_notes(c, id))?;
        if segs.is_empty() && notes.user_md.trim().is_empty() {
            set_step(ctx, &mut m, StepKind::Notes, StepStatus::Done, Some("Nothing to write up".into())).await?;
        } else {
            let client = Client::from_config(&cfg).context("Add an OpenRouter key in Settings")?;
            // Imported tracks are named deterministically; the model only names the rest.
            let known = known_names(&import_speakers, &ctx.meeting(id)?.speaker_names);
            let mut labels: Vec<String> = segs
                .iter()
                .filter_map(|s| match &s.speaker {
                    Speaker::Them(l) if !known.contains_key(l) => Some(l.clone()),
                    _ => None,
                })
                .collect();
            labels.sort();
            labels.dedup();
            let mut known_list: Vec<String> = known.values().cloned().collect();
            known_list.dedup();
            let who = prompts::KnownSpeakers {
                names: known_list,
                no_you: imported && !segs.iter().any(|s| s.speaker == Speaker::Me),
            };
            let template = load_template();
            let msgs = prompts::enhance_messages(&transcript::render_numbered(&segs, &known), &notes.user_md, &template, &labels, &who);
            // Models occasionally return malformed JSON; one fresh attempt usually fixes it.
            let mut parsed = None;
            for attempt in 1..=2 {
                let r = client.chat(&cfg.openrouter.enhance_model, &msgs, true).await?;
                m.cost_usd += r.cost;
                parsed = prompts::parse_enhanced(&r.content);
                if parsed.is_some() {
                    break;
                }
                let head: String = r.content.chars().take(160).collect();
                tracing::warn!("notes reply {attempt}/2 unreadable: {head:?}");
            }
            let (enhanced, speakers) = parsed.ok_or_else(|| anyhow!("the notes model returned something unreadable"))?;
            let speakers = merge_names(speakers, &known);
            let enhanced = prompts::validate_sources(prompts::scrub_labels(enhanced, &speakers), segs.len());
            ctx.db(|c| store::set_enhanced(c, id, Some(&enhanced)))?;
            m.speaker_names = speakers;
            // Refresh the transcript detail with linked names.
            if !segs.is_empty() {
                m.pipeline = std::mem::take(&mut m.pipeline).with(
                    StepKind::Transcribed,
                    StepStatus::Done,
                    Some(format!("{} · cost ${:.2}", people_phrase(&segs, &m.speaker_names, imported), m.cost_usd)),
                );
            }
            let points: usize = enhanced.sections.iter().map(|s| s.items.len()).sum();
            let todos = enhanced.action_items.len();
            let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
            let detail = format!("{} · {}", plural(points, "point"), plural(todos, "to-do"));
            set_step(ctx, &mut m, StepKind::Notes, StepStatus::Done, Some(detail)).await?;
        }
    }

    m.state = MeetingState::Ready;
    save(ctx, &mut m)?;
    crate::hooks::emit_logged(ctx, muesli_core::hooks::HookEvent::MeetingReady, id);
    if !enhance_only {
        ctx.notifier.info("Notes ready", &m.display_title(), &[(&format!("open:{id}"), "Open notes")]);
    }
    Ok(())
}

/// The model's speaker linking plus the known names, which it can never override.
fn merge_names(linked: BTreeMap<String, String>, known: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    linked.into_iter().filter(|(l, _)| !known.contains_key(l)).chain(known.clone()).collect()
}

fn load_template() -> String {
    std::fs::read_to_string(paths::templates_dir().join("default.md"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| prompts::DEFAULT_TEMPLATE.to_string())
}

async fn transcribe_all(
    client: &Client,
    cfg: &config::Config,
    clean_dir: &std::path::Path,
    manifest: &CleanManifest,
) -> Result<(Vec<Segment>, f64)> {
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let jobs = manifest.chunks.iter().enumerate().map(|(i, c)| {
        let (client, sem, c) = (client.clone(), sem.clone(), c.clone());
        let models = cfg.openrouter.stt_models.clone();
        let lang = cfg.openrouter.language.clone();
        let path = clean_dir.join(&c.file);
        tokio::spawn(async move {
            let _permit = sem.acquire().await?;
            let bytes = tokio::fs::read(&path).await?;
            let reply = match client.transcribe(&models, &bytes, "ogg", lang.as_deref()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("ogg upload failed ({e:#}); retrying as wav");
                    let pcm = muesli_audio::oggopus::decode_bytes(&bytes, 16_000)?;
                    let wav = muesli_audio::oggopus::wav_bytes(&pcm, 16_000);
                    client.transcribe(&models, &wav, "wav", lang.as_deref()).await?
                }
            };
            tracing::info!("chunk {} ({:.0}s speech) → {} via {}", c.file, c.speech_s, reply.text.len(), reply.model);
            let segs = transcript::segments_from_stt(&reply.text, &reply.segments, &reply.words, &c.map, c.me, i);
            // One person per imported track: ignore any diarization inside it.
            let segs: Vec<Segment> = match &c.track {
                Some(label) => segs.into_iter().map(|s| Segment { speaker: Speaker::Them(label.clone()), ..s }).collect(),
                None => segs,
            };
            anyhow::Ok((c.me, segs, reply.cost))
        })
    });
    let results = futures_join(jobs.collect()).await;
    let (mut me, mut them, mut cost) = (vec![], vec![], 0.0);
    for r in results {
        let (is_me, segs, c) = r??;
        cost += c;
        match is_me {
            true => me.extend(segs),
            false => them.extend(segs),
        }
    }
    let me = transcript::drop_echo(me, &them);
    Ok((transcript::merge(me, them), cost))
}

async fn futures_join<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<Result<T, tokio::task::JoinError>> {
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use muesli_core::import::ImportSpeaker;

    fn names(v: &[(&str, &str)]) -> BTreeMap<String, String> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn known_track_names_beat_the_model_and_keep_user_renames() {
        let sp = |l: &str, n: Option<&str>| ImportSpeaker { label: l.into(), name: n.map(Into::into) };
        let speakers = [sp("t0", Some("Matt Rosenthal")), sp("t1", Some("Lena")), sp("m0", None)];
        // The user renamed Lena; t0 has no current entry (e.g. reset) → manifest name.
        let known = known_names(&speakers, &names(&[("t1", "Lena K"), ("c3s0", "Bob")]));
        assert_eq!(known, names(&[("t0", "Matt Rosenthal"), ("t1", "Lena K")]));
        // The model tries to rename a known track and links a diarized label.
        let merged = merge_names(names(&[("t0", "Matthew"), ("c3s1", "Priya")]), &known);
        assert_eq!(merged, names(&[("c3s1", "Priya"), ("t0", "Matt Rosenthal"), ("t1", "Lena K")]));
        assert!(known_names(&[], &names(&[("c0s0", "X")])).is_empty(), "normal recordings: nothing known");
    }

    #[test]
    fn people_phrase_for_imports() {
        let seg = |l: &str| Segment { speaker: Speaker::Them(l.into()), t_start: 0.0, t_end: 1.0, text: "hi".into() };
        let segs = [seg("t0"), seg("t1"), seg("t0")];
        assert_eq!(people_phrase(&segs, &names(&[("t0", "A"), ("t1", "B")]), true), "2 speakers");
        assert_eq!(people_phrase(&segs[..1], &BTreeMap::new(), true), "1 speaker");
        assert_eq!(people_phrase(&segs, &BTreeMap::new(), false), "You and 2 others");
    }

    #[test]
    fn chunk_records_stay_readable() {
        // Old manifests (no `track`) still parse; named tracks round-trip.
        let old: ChunkRecord = serde_json::from_str(r#"{"me":false,"file":"sys-000.opus","map":{"spans":[]},"speech_s":1.0}"#).unwrap();
        assert_eq!(old.track, None);
        let new = ChunkRecord { track: Some("t0".into()), ..old };
        let back: ChunkRecord = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back.track.as_deref(), Some("t0"));
    }
}
