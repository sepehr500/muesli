//! Post-call pipeline: Saved → Cleaned → Transcribed (+ titles) → Notes.
//! Resumable: each run starts at the first step that isn't Done.

use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow};
use muesli_audio::capture::RecordingInfo;
use muesli_audio::clean::{CleanOptions, clean};
use muesli_core::model::*;
use muesli_core::openrouter::Client;
use muesli_core::transcript::{self, TimeMap};
use muesli_core::{config, paths, prompts, store};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkRecord {
    me: bool,
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
        let info = tokio::task::spawn_blocking(move || RecordingInfo::load(&raw2).or_else(|_| RecordingInfo::scan(&raw2)))
            .await??;
        if m.ended_at.is_none() {
            m.ended_at = Some(m.started_at + (info.duration_s * 1000.0) as i64);
        }
        let detail = format!("{} of you and the call, kept on this computer", lower_duration(m.duration_ms().unwrap_or(0)));
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
            .map(|c| ChunkRecord { me: true, file: rel(&c.path), map: c.map.clone(), speech_s: c.speech_s })
            .chain(r.sys_chunks.iter().map(|c| ChunkRecord { me: false, file: rel(&c.path), map: c.map.clone(), speech_s: c.speech_s }))
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

    // 3. Transcribed (+ title suggestions)
    if m.pipeline.status_of(StepKind::Transcribed) != StepStatus::Done {
        set_step(ctx, &mut m, StepKind::Transcribed, StepStatus::Active, None).await?;
        let client = Client::from_config(&cfg).context("Add an OpenRouter key in Settings")?;
        let (segs, cost) = transcribe_all(&client, &cfg, &clean_dir, &manifest).await?;
        ctx.db(|c| store::replace_segments(c, id, &segs))?;
        m.cost_usd += cost;
        let others = transcript::other_speaker_count(&segs, &m.speaker_names);
        let detail = match segs.is_empty() {
            true => "No speech detected".to_string(),
            false => format!("{} · cost ${:.2}", others_phrase(others), m.cost_usd),
        };
        // Title suggestions (fast model). Failure here isn't fatal.
        if !segs.is_empty() && !m.title_confirmed {
            let notes = ctx.db(|c| store::get_notes(c, id))?.user_md;
            let msgs = prompts::title_messages(
                m.app.map(|a| a.label()).unwrap_or("Meeting"),
                m.hint.as_deref(),
                &transcript::render_for_prompt(&segs),
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
            let mut labels: Vec<String> = segs
                .iter()
                .filter_map(|s| match &s.speaker {
                    Speaker::Them(l) => Some(l.clone()),
                    Speaker::Me => None,
                })
                .collect();
            labels.sort();
            labels.dedup();
            let template = load_template();
            let msgs = prompts::enhance_messages(&transcript::render_numbered(&segs), &notes.user_md, &template, &labels);
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
            let enhanced = prompts::validate_sources(prompts::scrub_labels(enhanced, &speakers), segs.len());
            ctx.db(|c| store::set_enhanced(c, id, Some(&enhanced)))?;
            m.speaker_names = speakers;
            // Refresh the transcript detail with linked names.
            let others = transcript::other_speaker_count(&segs, &m.speaker_names);
            if !segs.is_empty() {
                m.pipeline = std::mem::take(&mut m.pipeline).with(
                    StepKind::Transcribed,
                    StepStatus::Done,
                    Some(format!("{} · cost ${:.2}", others_phrase(others), m.cost_usd)),
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
    if !enhance_only {
        ctx.notifier.info("Notes ready", &m.display_title(), &[(&format!("open:{id}"), "Open notes")]);
    }
    Ok(())
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
