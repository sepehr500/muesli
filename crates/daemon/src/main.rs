//! muesli-daemon: detects calls, records them, and turns them into notes.

mod ctx;
mod detector;
mod notify;
mod pipeline;
mod server;

use anyhow::Result;
use ctx::{Ctx, SessionCmd};
use muesli_audio::capture::Recorder;
use muesli_core::detect::{self, PwNode};
use muesli_core::ipc::{Event, SessionView};
use muesli_core::model::*;
use muesli_core::session::{self, Effect, Notice, State};
use muesli_core::{config::Config, paths, store};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

struct Active {
    meeting: Meeting,
    recorder: Recorder,
}

fn view(state: &State, active: &Option<Active>) -> SessionView {
    match (state, active) {
        (State::Prompting { candidate, since }, _) => {
            SessionView::Prompting { app: candidate.app, hint: candidate.hint.clone(), since: *since }
        }
        (State::Recording { app, hint, manual, .. }, Some(a)) => SessionView::Recording {
            meeting_id: a.meeting.id.clone(),
            app: *app,
            hint: hint.clone(),
            started_at: a.meeting.started_at,
            manual: *manual,
        },
        _ => SessionView::Idle,
    }
}

/// Does any live, non-muesli capture stream exist? (Only then is `hyprctl` worth calling.)
fn any_live(nodes: &[PwNode]) -> bool {
    nodes.iter().any(|n| !n.corked)
}

async fn observe(nodes: &[PwNode]) -> Option<detect::Candidate> {
    if !any_live(nodes) {
        return None;
    }
    detect::classify(nodes, &detector::windows().await)
}

fn label(app: Option<detect::App>) -> &'static str {
    app.map(|a| a.label()).unwrap_or("Manual")
}

async fn apply(ctx: &Arc<Ctx>, fx: Vec<Effect>, active: &mut Option<Active>, state: &mut State) {
    for e in fx {
        match e {
            Effect::StartRecording { app, hint, .. } => {
                let mut m = store::new_meeting(app, hint, store::now_ms());
                let dir = paths::meeting_dir(&m.id);
                let started = tokio::task::block_in_place(|| Recorder::start(&dir));
                match started {
                    Ok(recorder) => {
                        if let Some(a) = app {
                            m.title = None;
                            tracing::info!("recording {} call → {}", a.label(), m.id);
                        }
                        if let Err(e) = ctx.save(&m) {
                            tracing::error!("save meeting: {e}");
                        }
                        *active = Some(Active { meeting: m, recorder });
                    }
                    Err(e) => {
                        tracing::error!("could not start recording: {e:#}");
                        ctx.notifier.info("Couldn't start recording", &format!("{e}"), &[]);
                        *state = State::Idle;
                    }
                }
            }
            Effect::StopRecording | Effect::DiscardRecording => {
                ctx.notifier.dismiss();
                let Some(a) = active.take() else { continue };
                let discard = matches!(e, Effect::DiscardRecording);
                let mut m = a.meeting;
                let info = tokio::task::spawn_blocking(move || a.recorder.stop()).await;
                if discard {
                    let _ = ctx.db(|c| store::delete_meeting(c, &m.id));
                    let _ = std::fs::remove_dir_all(paths::meeting_dir(&m.id));
                    ctx.meeting_updated(&m.id);
                    tracing::info!("discarded {}", m.id);
                    continue;
                }
                match info {
                    Ok(Ok(info)) => {
                        m.ended_at = Some(m.started_at + (info.duration_s * 1000.0) as i64);
                        m.state = MeetingState::Processing;
                        let _ = ctx.save(&m);
                        tracing::info!("stopped {} ({:.0}s, {} KB)", m.id, info.duration_s, info.bytes / 1024);
                        tokio::spawn(pipeline::run(ctx.clone(), m.id.clone(), false));
                    }
                    other => {
                        tracing::error!("stopping recording failed: {other:?}");
                        m.ended_at = Some(store::now_ms());
                        m.state = MeetingState::Processing;
                        let _ = ctx.save(&m);
                        tokio::spawn(pipeline::run(ctx.clone(), m.id.clone(), false));
                    }
                }
            }
            Effect::Notify(Notice::RecordingStarted { app, hint }) => ctx.notifier.session(
                &format!("Recording {} call", label(app)),
                hint.as_deref().unwrap_or("muesli is taking notes"),
                &[("stop", "Stop"), ("discard", "Discard")],
                false,
            ),
            Effect::Notify(Notice::AskToRecord { app, hint }) => ctx.notifier.session(
                &format!("Call detected in {}", app.label()),
                &format!("{}Record this call?", hint.map(|h| format!("{h} · ")).unwrap_or_default()),
                &[("record", "Record"), ("ignore", "Ignore")],
                true,
            ),
            Effect::Notify(Notice::Dismiss) => ctx.notifier.dismiss(),
        }
    }
}

async fn session_loop(
    ctx: Arc<Ctx>,
    mut nodes_rx: mpsc::UnboundedReceiver<Vec<PwNode>>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    mut actions_rx: mpsc::UnboundedReceiver<String>,
) {
    let mut state = State::Idle;
    let mut active: Option<Active> = None;
    let mut nodes: Vec<PwNode> = vec![];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut levels = tokio::time::interval(Duration::from_millis(125));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    levels.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let before = view(&state, &active);
        let event = tokio::select! {
            Some(snap) = nodes_rx.recv() => {
                nodes = snap;
                Some(session::Event::Observed { candidate: observe(&nodes).await, now: store::now_ms() })
            }
            Some(SessionCmd::Event(e)) = cmd_rx.recv() => Some(e),
            Some(action) = actions_rx.recv() => match action.as_str() {
                "stop" => Some(session::Event::Stop),
                "discard" => Some(session::Event::Discard),
                "record" => Some(session::Event::Record { now: store::now_ms() }),
                "ignore" => Some(session::Event::Ignore),
                a => {
                    if let Some(id) = a.strip_prefix("open:") {
                        let _ = std::process::Command::new("muesli").args(["--meeting", id]).spawn();
                    } else if let Some(id) = a.strip_prefix("reprocess:") {
                        tokio::spawn(pipeline::run(ctx.clone(), id.to_string(), false));
                    }
                    None
                }
            },
            _ = tick.tick(), if session::needs_ticks(&state) => {
                // Re-observe too: window titles (hints) may have appeared.
                let now = store::now_ms();
                let obs = session::Event::Observed { candidate: observe(&nodes).await, now };
                let (s, fx) = session::step(state, obs, &ctx.cfg().policy());
                state = s;
                apply(&ctx, fx, &mut active, &mut state).await;
                Some(session::Event::Tick { now })
            }
            _ = levels.tick(), if active.is_some() => {
                if let (Some(a), true) = (&active, ctx.events.receiver_count() > 0) {
                    let (me, them) = a.recorder.levels.get();
                    let _ = ctx.events.send(Event::Levels { me, them });
                }
                None
            }
        };
        if let Some(event) = event {
            let (s, fx) = session::step(state, event, &ctx.cfg().policy());
            state = s;
            apply(&ctx, fx, &mut active, &mut state).await;
        }
        // Keep the meeting's hint fresh if a window title showed up later.
        if let (State::Recording { hint: Some(h), .. }, Some(a)) = (&state, active.as_mut())
            && a.meeting.hint.as_deref() != Some(h.as_str()) {
                a.meeting.hint = Some(h.clone());
                let _ = ctx.save(&a.meeting);
            }
        let after = view(&state, &active);
        if after != before {
            *ctx.session_view.write().expect("view") = after;
            ctx.broadcast_status();
        }
    }
}

/// Crash recovery: finish meetings left mid-recording/processing.
fn recover(ctx: &Arc<Ctx>) -> Result<Vec<String>> {
    let (recording, processing) = ctx.db(|c| {
        Ok((store::meetings_in_state(c, MeetingState::Recording)?, store::meetings_in_state(c, MeetingState::Processing)?))
    })?;
    for mut m in recording.iter().cloned() {
        tracing::warn!("recovering interrupted recording {}", m.id);
        m.state = MeetingState::Processing;
        ctx.save(&m)?;
    }
    Ok(recording.into_iter().chain(processing).map(|m| m.id).collect())
}

/// Delete raw audio older than the retention window.
fn prune_audio(ctx: &Ctx) {
    let days = ctx.cfg().recording.audio_retention_days;
    if days == 0 {
        return;
    }
    let cutoff = store::now_ms() - days as i64 * 86_400_000;
    let old = ctx.db(|c| store::list_meetings(c, 100_000)).unwrap_or_default();
    old.iter()
        .filter(|m| m.state == MeetingState::Ready && m.started_at < cutoff)
        .for_each(|m| {
            let dir = paths::meeting_dir(&m.id);
            let _ = std::fs::remove_dir_all(dir.join("raw"));
            let _ = std::fs::remove_dir_all(dir.join("clean"));
        });
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "muesli_daemon=info,muesli_core=info,muesli_audio=info".into()),
        )
        .with_target(false)
        .init();

    let (events, _) = broadcast::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (actions_tx, actions_rx) = mpsc::unbounded_channel();
    let (nodes_tx, nodes_rx) = mpsc::unbounded_channel();

    let ctx = Arc::new(Ctx {
        cfg: RwLock::new(Config::load()),
        db: std::sync::Mutex::new(store::open_default()?),
        events,
        session_view: RwLock::new(SessionView::Idle),
        cmds: cmd_tx,
        jobs: Default::default(),
        notifier: notify::Notifier::new(actions_tx),
    });

    std::fs::create_dir_all(paths::meetings_dir())?;
    let server = tokio::spawn(server::serve(ctx.clone()));
    // Fail fast if another daemon owns the socket.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if server.is_finished() {
        return server.await?;
    }

    for id in recover(&ctx)? {
        tokio::spawn(pipeline::run(ctx.clone(), id, false));
    }
    prune_audio(&ctx);

    let _detector = detector::spawn(nodes_tx);
    let session = tokio::spawn(session_loop(ctx.clone(), nodes_rx, cmd_rx, actions_rx));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = server => { r??; }
        _ = session => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    // Stop an in-flight recording cleanly so audio is finalized.
    let _ = ctx.cmds.send(SessionCmd::Event(session::Event::Stop));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = std::fs::remove_file(paths::socket_path());
    Ok(())
}
