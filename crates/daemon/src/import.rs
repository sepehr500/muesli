//! `Request::Import`: ingest an external recording (see `muesli_core::import`).
//!
//! Reply only after the audio is copied into muesli's storage and the meeting row (with its
//! `source_id` claim) is committed. Conversion, cleaning, transcription and notes run in the
//! background pipeline. Never touches the live capture session.

use crate::ctx::Ctx;
use anyhow::{Context, Result};
use muesli_core::import::{self, ImportPlan, ImportReply, invalid};
use muesli_core::model::{Meeting, MeetingState, Pipeline};
use muesli_core::{paths, store};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Read and validate the manifest. Returns (canonical manifest directory, raw JSON, plan).
fn read_manifest(path: &Path) -> Result<(PathBuf, String, ImportPlan)> {
    anyhow::ensure!(path.is_absolute(), invalid(format!("manifest path {} is not absolute", path.display())));
    let real = path.canonicalize().map_err(|e| invalid(format!("manifest {}: {e}", path.display())))?;
    let meta = std::fs::metadata(&real)?;
    anyhow::ensure!(meta.is_file(), invalid("manifest is not a regular file"));
    anyhow::ensure!(meta.len() <= import::MAX_MANIFEST_BYTES, invalid("manifest is larger than 1 MB"));
    let json = std::fs::read_to_string(&real).map_err(|e| invalid(format!("manifest: {e}")))?;
    let plan = import::parse(&json)?.validate(store::now_ms())?;
    let root = real.parent().context("manifest has no folder")?.to_path_buf();
    Ok((root, json, plan))
}

pub async fn import(ctx: &Arc<Ctx>, manifest: PathBuf) -> Result<ImportReply> {
    // One import at a time: the source_id check and the claim below must not interleave.
    let _one = ctx.imports.lock().await;
    let (root, json, plan) = tokio::task::spawn_blocking(move || read_manifest(&manifest)).await??;
    if let Some(id) = ctx.db(|c| store::import_meeting(c, &plan.source_id))? {
        let state = ctx.meeting(&id)?.state;
        tracing::info!("import {} already stored as {id}", plan.source_id);
        return Ok(ImportReply { id, state, duplicate: true });
    }
    let id = import::meeting_id(&plan.source_id, plan.started_at);
    anyhow::ensure!(
        ctx.db(|c| store::get_meeting(c, &id))?.is_none(),
        "meeting {id} already exists but isn't linked to {}",
        plan.source_id
    );
    let dir = paths::meeting_dir(&id);
    let staged = {
        let (dir, plan) = (dir.clone(), plan.clone());
        tokio::task::spawn_blocking(move || {
            // Leftovers of an attempt that crashed before its database commit.
            let _ = std::fs::remove_dir_all(&dir);
            paths::ensure_private_dir(&dir)?;
            muesli_audio::import::stage(&dir.join("raw"), &root, &plan)
        })
        .await?
    };
    let bytes = match staged {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let m = Meeting {
        id: id.clone(),
        title: None,
        title_confirmed: false,
        title_suggestions: vec![],
        app: plan.app,
        hint: plan.hint.clone(),
        started_at: plan.started_at,
        ended_at: Some(plan.ended_at),
        state: MeetingState::Processing,
        pipeline: Pipeline::default(),
        speaker_names: plan.known_names(),
        cost_usd: 0.0,
        error: None,
    };
    if let Err(e) = ctx.db(|c| store::insert_import(c, &m, &plan.source_id, &plan.speakers, &json)) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    tracing::info!(
        "imported {} → {id} ({} tracks, {} speakers, {} MB)",
        plan.source_id,
        plan.tracks.len(),
        plan.speakers.len(),
        bytes / 1_000_000
    );
    ctx.meeting_updated(&id);
    tokio::spawn(crate::pipeline::run(ctx.clone(), id.clone(), false));
    Ok(ImportReply { id, state: MeetingState::Processing, duplicate: false })
}
