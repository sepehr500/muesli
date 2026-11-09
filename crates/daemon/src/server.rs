//! Unix-socket JSON-lines server.

use crate::ctx::{Ctx, SessionCmd};
use anyhow::Result;
use muesli_core::ipc::{Event, Request, Response, SessionView};
use muesli_core::model::{StepKind, StepStatus, fallback_title};
use muesli_core::hooks::{self, HookEvent};
use muesli_core::{config, paths, prompts, session, store, transcript};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub async fn serve(ctx: Arc<Ctx>) -> Result<()> {
    let path = paths::socket_path();
    if path.exists() {
        // Refuse to steal a live daemon's socket.
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another muesli-daemon is running ({})", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    tracing::info!("listening on {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = client(ctx, stream).await {
                tracing::debug!("client: {e}");
            }
        });
    }
}

async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: &impl serde::Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn client(ctx: Arc<Ctx>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut w, &Response::err(format!("bad request: {e}"))).await?;
                continue;
            }
        };
        if req == Request::Watch {
            let mut rx = ctx.events.subscribe();
            send(&mut w, &Response::empty()).await?;
            send(&mut w, &Event::Status(ctx.status()?)).await?;
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Event::Status(ctx.status()?),
                    Err(_) => return Ok(()),
                };
                send(&mut w, &ev).await?;
            }
        }
        let resp = handle(&ctx, req).await.unwrap_or_else(Response::err);
        send(&mut w, &resp).await?;
    }
    Ok(())
}

fn session(ctx: &Ctx, e: session::Event) -> Result<Response> {
    ctx.cmds.send(SessionCmd::Event(e)).map_err(|_| anyhow::anyhow!("session loop stopped"))?;
    Ok(Response::empty())
}

async fn handle(ctx: &Arc<Ctx>, req: Request) -> Result<Response> {
    let now = store::now_ms();
    match req {
        Request::Ping => Ok(Response::ok("pong")),
        Request::Status => Ok(Response::ok(ctx.status()?)),
        Request::Watch => unreachable!("handled by caller"),
        Request::Start => session(ctx, session::Event::Start { now }),
        Request::Record => session(ctx, session::Event::Record { now }),
        Request::Ignore => session(ctx, session::Event::Ignore),
        Request::Stop => session(ctx, session::Event::Stop),
        Request::Discard => session(ctx, session::Event::Discard),
        // The session loop fills in the detector's current verdict.
        Request::Next => session(ctx, session::Event::Next { now, candidate: None }),
        Request::SetAutoRecord { value } => {
            let cfg = {
                let mut c = ctx.cfg.write().expect("cfg");
                c.recording.auto_record = value;
                c.clone()
            };
            cfg.save()?;
            ctx.broadcast_status();
            Ok(Response::empty())
        }
        Request::Rename { id, title, confirm } => {
            let title = title.trim();
            anyhow::ensure!(!title.is_empty(), "title can't be empty");
            ctx.db(|c| store::set_title(c, &id, title, confirm))?;
            ctx.meeting_updated(&id);
            crate::hooks::emit_logged(ctx, HookEvent::MeetingUpdated, &id);
            Ok(Response::empty())
        }
        Request::ConfirmTitle { id } => {
            let m = ctx.meeting(&id)?;
            let title = m.title.clone().unwrap_or_else(|| fallback_title(m.app, m.started_at));
            ctx.db(|c| store::set_title(c, &id, &title, true))?;
            ctx.meeting_updated(&id);
            Ok(Response::empty())
        }
        Request::Reprocess { id } => {
            // Keep the recording and cleaned audio; redo transcription and notes.
            let mut m = ctx.meeting(&id)?;
            m.pipeline = [StepKind::Transcribed, StepKind::Notes]
                .into_iter()
                .fold(std::mem::take(&mut m.pipeline), |p, k| p.with(k, StepStatus::Pending, None));
            ctx.save(&m)?;
            tokio::spawn(crate::pipeline::run(ctx.clone(), id, false));
            Ok(Response::empty())
        }
        Request::Enhance { id } => {
            ctx.meeting(&id)?;
            tokio::spawn(crate::pipeline::run(ctx.clone(), id, true));
            Ok(Response::empty())
        }
        Request::RenameSpeaker { id, from, to } => {
            let to = to.trim();
            anyhow::ensure!(!to.eq_ignore_ascii_case("you"), "\"You\" is reserved for your own microphone");
            let mut m = ctx.meeting(&id)?;
            let segs = ctx.db(|c| store::segments(c, &id))?;
            let names = transcript::rename_speaker(&segs, &m.speaker_names, &from, to)
                .ok_or_else(|| anyhow::anyhow!("nobody called \"{from}\" in this transcript"))?;
            // Imported tracks: resetting a name goes back to the manifest's name.
            let import_speakers = ctx.db(|c| store::import_speakers(c, &id))?;
            let known = crate::pipeline::known_names(&import_speakers, &names);
            let names: std::collections::BTreeMap<String, String> = names.into_iter().chain(known).collect();
            // Notes mention people by the name shown at the time; follow the rename there too.
            let shown = |names: &std::collections::BTreeMap<String, String>| {
                segs.iter()
                    .find(|s| transcript::display_speaker(&s.speaker, &m.speaker_names) == from)
                    .map(|s| transcript::display_speaker(&s.speaker, names))
                    .unwrap_or_default()
            };
            let new_name = shown(&names);
            let notes = ctx.db(|c| store::get_notes(c, &id))?;
            if let Some(enh) = notes.enhanced {
                let enh = prompts::rename_in_notes(enh, &from, &new_name);
                ctx.db(|c| store::set_enhanced(c, &id, Some(&enh)))?;
            }
            m.speaker_names = names;
            if m.pipeline.status_of(StepKind::Transcribed) == StepStatus::Done {
                let people = crate::pipeline::people_phrase(&segs, &m.speaker_names, !import_speakers.is_empty());
                let detail = format!("{people} · cost ${:.2}", m.cost_usd);
                m.pipeline = std::mem::take(&mut m.pipeline).with(StepKind::Transcribed, StepStatus::Done, Some(detail));
            }
            ctx.save(&m)?;
            ctx.meeting_updated(&id);
            crate::hooks::emit_logged(ctx, HookEvent::MeetingUpdated, &id);
            Ok(Response::empty())
        }
        Request::PreparePlayback { id } => {
            ctx.meeting(&id)?;
            let dir = paths::playback_dir(&id);
            if muesli_audio::playback::load_index(&dir).is_err() {
                let raw = paths::meeting_dir(&id).join("raw");
                anyhow::ensure!(raw.is_dir(), "the audio for this meeting was deleted");
                let aec = ctx.cfg().recording.echo_cancellation;
                let d = dir.clone();
                tokio::task::spawn_blocking(move || muesli_audio::clean::render_playback(&raw, &d, aec)).await??;
                tracing::info!("built playback for {id}");
            }
            Ok(Response::ok(serde_json::json!({ "dir": dir })))
        }
        Request::Delete { id } => {
            let recording = matches!(&*ctx.session_view.read().expect("view"),
                SessionView::Recording { meeting_id, .. } if *meeting_id == id);
            anyhow::ensure!(!recording, "stop the recording first");
            // Snapshot before deleting: scripts still need to know which meeting it was.
            let snap = (!hooks::scripts(HookEvent::MeetingDeleted).is_empty())
                .then(|| crate::hooks::snapshot(ctx, HookEvent::MeetingDeleted, &id).ok())
                .flatten();
            ctx.db(|c| store::delete_meeting(c, &id))?;
            let _ = std::fs::remove_dir_all(paths::meeting_dir(&id));
            ctx.meeting_updated(&id);
            if let Some((meeting, _)) = snap
                && let Err(e) = crate::hooks::emit_with(ctx, HookEvent::MeetingDeleted, &id, meeting, None)
            {
                tracing::warn!("hook meeting-deleted for {id} not queued: {e:#}");
            }
            Ok(Response::empty())
        }
        Request::Import { manifest } => Ok(Response::ok(crate::import::import(ctx, manifest).await?)),
        Request::RunHook { event, id } => {
            let ev = HookEvent::parse(&event).ok_or_else(|| {
                let names: Vec<_> = HookEvent::ALL.iter().map(|e| e.name()).collect();
                anyhow::anyhow!("unknown event {event:?} (one of: {})", names.join(", "))
            })?;
            let event_id = crate::hooks::emit(ctx, ev, &id)?;
            Ok(Response::ok(serde_json::json!({ "event_id": event_id })))
        }
        Request::RetryHook { event_id } => {
            let n = ctx.db(|c| store::retry_hook_event(c, &event_id))?;
            ctx.hooks_wake.notify_one();
            Ok(Response::ok(serde_json::json!({ "requeued": n })))
        }
        Request::ReloadConfig => {
            *ctx.cfg.write().expect("cfg") = config::Config::load();
            ctx.broadcast_status();
            Ok(Response::empty())
        }
    }
}
