//! Runs hook scripts. `emit` stores the event in the outbox (so a crash can't lose it) and
//! wakes the runner, which runs due scripts one at a time with a timeout and retries.

use crate::ctx::Ctx;
use anyhow::{Context, Result};
use muesli_core::hooks::{self, ExportFiles, HookEvent};
use muesli_core::store::{self, HookRun};
use muesli_core::paths;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const TIMEOUT: Duration = Duration::from_secs(300);
/// Delay before each retry; after the last one a failure is final (and notified).
const RETRY_DELAYS_MS: [i64; 2] = [60_000, 600_000];
const LOG_TAIL: usize = 4000;
const KEEP_EVENTS_MS: i64 = 30 * 86_400_000;

/// The meeting's current state for a payload, writing `export/` when the event wants files.
pub fn snapshot(ctx: &Ctx, event: HookEvent, id: &str) -> Result<(Value, Option<ExportFiles>)> {
    let m = ctx.meeting(id)?;
    let (notes, segs) = ctx.db(|c| Ok((store::get_notes(c, id)?, store::segments(c, id)?)))?;
    let meeting = hooks::meeting_json(&m, &notes, &segs);
    let files = match event.exports() {
        true => Some(write_export(id, &meeting, &hooks::notes_markdown(&notes, &segs), &segs, &m.speaker_names)?),
        false => None,
    };
    Ok((meeting, files))
}

fn write_export(
    id: &str,
    meeting: &Value,
    notes_md: &str,
    segs: &[muesli_core::model::Segment],
    names: &std::collections::BTreeMap<String, String>,
) -> Result<ExportFiles> {
    let f = ExportFiles::in_dir(&paths::export_dir(id));
    std::fs::create_dir_all(&f.dir).with_context(|| format!("create {}", f.dir.display()))?;
    std::fs::write(&f.notes_md, notes_md)?;
    std::fs::write(&f.transcript_md, hooks::transcript_markdown(segs, names))?;
    std::fs::write(&f.transcript_json, serde_json::to_vec_pretty(&hooks::transcript_json(segs, names))?)?;
    std::fs::write(&f.meeting_json, serde_json::to_vec_pretty(meeting)?)?;
    Ok(f)
}

/// Queue an event for every script registered for it. Returns the event id, or None when no
/// script is registered (nothing is stored then).
pub fn emit_with(ctx: &Ctx, event: HookEvent, id: &str, meeting: Value, files: Option<ExportFiles>) -> Result<Option<String>> {
    let scripts = hooks::scripts(event);
    if scripts.is_empty() {
        return Ok(None);
    }
    let event_id = format!("ev_{}", ulid::Ulid::generate().to_string().to_lowercase());
    let payload = hooks::payload(event, &event_id, meeting, files.as_ref());
    ctx.db(|c| store::insert_hook_event(c, &event_id, event.name(), id, &payload.to_string(), &scripts))?;
    tracing::info!("hook {} for {id}: {} script(s)", event.name(), scripts.len());
    ctx.hooks_wake.notify_one();
    Ok(Some(event_id))
}

pub fn emit(ctx: &Ctx, event: HookEvent, id: &str) -> Result<Option<String>> {
    if hooks::scripts(event).is_empty() {
        return Ok(None);
    }
    let (meeting, files) = snapshot(ctx, event, id)?;
    emit_with(ctx, event, id, meeting, files)
}

/// Fire-and-forget form for the pipeline and session loop: hooks must never break them.
pub fn emit_logged(ctx: &Ctx, event: HookEvent, id: &str) {
    if let Err(e) = emit(ctx, event, id) {
        tracing::warn!("hook {} for {id} not queued: {e:#}", event.name());
    }
}

pub async fn runner(ctx: Arc<Ctx>) {
    if let Err(e) = ctx.db(|c| store::prune_hook_events(c, store::now_ms() - KEEP_EVENTS_MS)) {
        tracing::warn!("prune hook events: {e:#}");
    }
    loop {
        let due = ctx.db(|c| store::due_hook_runs(c, store::now_ms())).unwrap_or_else(|e| {
            tracing::error!("read hook outbox: {e:#}");
            vec![]
        });
        for (run, payload) in due {
            run_one(&ctx, run, &payload).await;
        }
        let next = ctx.db(store_next).ok().flatten();
        let wait_ms = next.map(|t| (t - store::now_ms()).clamp(0, 3_600_000)).unwrap_or(3_600_000);
        tokio::select! {
            _ = ctx.hooks_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(wait_ms as u64)) => {}
        }
    }
}

fn store_next(c: &mut store::Connection) -> Result<Option<i64>> {
    store::next_hook_due(c)
}

fn tail(s: &str, n: usize) -> String {
    let start = s.char_indices().rev().nth(n.saturating_sub(1)).map(|(i, _)| i).unwrap_or(0);
    s[start..].to_string()
}

/// Run one script with the payload on stdin. Returns (ok, exit code, combined output tail).
async fn exec(path: &PathBuf, payload: &str, value: &Value) -> (bool, Option<i32>, String) {
    let cwd = value["files"]["dir"].as_str().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(paths::hooks_dir);
    let mut cmd = tokio::process::Command::new(path);
    cmd.envs(hooks::env_vars(value))
        .env_remove("OPENROUTER_API_KEY")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let run = async {
        let mut child = cmd.spawn().with_context(|| format!("start {}", path.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let body = payload.as_bytes().to_vec();
            // Write concurrently with reading output, so a chatty script can't deadlock us.
            tokio::spawn(async move {
                let _ = stdin.write_all(&body).await;
            });
        }
        anyhow::Ok(child.wait_with_output().await?)
    };
    match tokio::time::timeout(TIMEOUT, run).await {
        Ok(Ok(out)) => {
            let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            (out.status.success(), out.status.code(), tail(log.trim(), LOG_TAIL))
        }
        Ok(Err(e)) => (false, None, format!("{e:#}")),
        Err(_) => (false, None, format!("timed out after {} s", TIMEOUT.as_secs())),
    }
}

async fn run_one(ctx: &Arc<Ctx>, run: HookRun, payload: &str) {
    let Some(event) = HookEvent::parse(&run.event) else { return };
    let path = hooks::script_dir(event).join(&run.script);
    let value: Value = serde_json::from_str(payload).unwrap_or_default();
    let (ok, code, log) = exec(&path, payload, &value).await;
    let retry_at = (!ok)
        .then(|| RETRY_DELAYS_MS.get(run.attempts as usize).map(|d| store::now_ms() + d))
        .flatten();
    if let Err(e) = ctx.db(|c| store::finish_hook_run(c, &run.event_id, &run.script, ok, code, &log, retry_at)) {
        tracing::error!("record hook run: {e:#}");
    }
    let last = log.lines().last().unwrap_or("").to_string();
    match (ok, retry_at) {
        (true, _) => tracing::info!("hook {}/{} for {}: ok", run.event, run.script, run.meeting_id),
        (false, Some(_)) => tracing::warn!("hook {}/{} failed (attempt {}), retrying: {last}", run.event, run.script, run.attempts + 1),
        (false, None) => {
            tracing::error!("hook {}/{} gave up: {last}", run.event, run.script);
            let title = value["meeting"]["title"].as_str().unwrap_or("a meeting");
            ctx.notifier.info(
                &format!("Hook {} failed", run.script),
                &format!("{title} — {last}"),
                &[(&format!("hooks-retry:{}", run.event_id), "Retry")],
            );
        }
    }
}
