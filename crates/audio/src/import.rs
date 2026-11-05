//! Import recordings made outside muesli (e.g. a Discord bot that records each speaker
//! separately) into a meeting's `raw/` directory.
//!
//! Two phases, so the caller gets a fast, durable acknowledgement:
//! 1. [`stage`] (before the daemon replies): copy every source WAV into `raw/import/`, fsync,
//!    and validate the copies (so the caller can delete its files right after the reply).
//! 2. [`finalize`] (the pipeline's "Saved" step): convert the copies into aligned 16 kHz
//!    Ogg Opus segments, one track per speaker (a speaker's resumed-segment files are summed),
//!    write `recording.json`, delete the copies. Streams one segment at a time, so memory
//!    stays small for long meetings; safe to re-run after a crash.

use crate::capture::{RecordingInfo, TrackInfo};
use crate::oggopus::create_file;
use anyhow::{Context, Result, bail, ensure};
use muesli_core::import::{ImportPlan, ImportSpeaker, MAX_DURATION_MS, RATE, TRACK_SLACK_S, invalid};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Raw segment length for imports: short, so cleaning N tracks at once stays small in memory.
pub const SEGMENT_SECS: u32 = 60;
const BITRATE: i32 = 24_000;
const STAGE_DIR: &str = "import";
const STAGE_FILE: &str = "stage.json";
/// Biggest acceptable source file: 12 h of 16 kHz 16-bit mono plus slack and headers.
const MAX_TRACK_BYTES: u64 = (MAX_DURATION_MS as u64 / 1000 + TRACK_SLACK_S as u64) * RATE as u64 * 2 + (1 << 20);

/// `raw/import/stage.json`: what [`stage`] copied, for [`finalize`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stage {
    duration_s: f64,
    speakers: Vec<ImportSpeaker>,
    tracks: Vec<StagedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StagedTrack {
    label: String,
    file: String,
    samples: u64,
}

/// What a WAV header says, and where its samples are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavInfo {
    /// 1 = integer PCM, 3 = float (WAVE_FORMAT_EXTENSIBLE is resolved to its subformat).
    pub format: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub data_offset: u64,
    pub data_len: u64,
}

impl WavInfo {
    /// Samples per channel.
    pub fn frames(&self) -> u64 {
        self.data_len / (self.channels.max(1) as u64 * (self.bits.max(8) / 8) as u64)
    }
}

/// Parse a WAV header without reading the samples.
pub fn read_wav_header(path: &Path) -> Result<WavInfo> {
    let shown = path.display();
    let mut f = File::open(path).with_context(|| format!("open {shown}"))?;
    let file_len = f.metadata()?.len();
    let mut head = [0u8; 12];
    f.read_exact(&mut head).map_err(|_| invalid(format!("{shown} is not a WAV file")))?;
    ensure!(&head[..4] == b"RIFF" && &head[8..] == b"WAVE", invalid(format!("{shown} is not a WAV file")));
    let mut fmt: Option<Vec<u8>> = None;
    let mut pos = 12u64;
    loop {
        ensure!(pos + 8 <= file_len, invalid(format!("{shown} has no audio data")));
        f.seek(SeekFrom::Start(pos))?;
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch)?;
        let len = u32::from_le_bytes(ch[4..].try_into()?) as u64;
        let body = pos + 8;
        match &ch[..4] {
            b"fmt " => {
                ensure!((16..=1024).contains(&len), invalid(format!("{shown} has a bad fmt chunk")));
                let mut b = vec![0u8; len as usize];
                f.read_exact(&mut b).map_err(|_| invalid(format!("{shown} is truncated")))?;
                fmt = Some(b);
            }
            b"data" => {
                let fmt = fmt.ok_or_else(|| invalid(format!("{shown}: data before fmt")))?;
                let avail = file_len - body;
                // Streaming writers leave 0 or 0xFFFFFFFF when they can't seek back.
                let data_len = match len {
                    0 | 0xFFFF_FFFF => avail,
                    n => n,
                };
                ensure!(data_len <= avail, invalid(format!("{shown} is truncated ({avail} of {data_len} data bytes)")));
                let u16_at = |i: usize| u16::from_le_bytes([fmt[i], fmt[i + 1]]);
                let tag = u16_at(0);
                let format = match (tag, fmt.len() >= 26) {
                    (0xFFFE, true) => u16_at(24), // WAVE_FORMAT_EXTENSIBLE: first bytes of the subformat GUID
                    _ => tag,
                };
                return Ok(WavInfo {
                    format,
                    channels: u16_at(2),
                    rate: u32::from_le_bytes(fmt[4..8].try_into()?),
                    bits: u16_at(14),
                    data_offset: body,
                    data_len,
                });
            }
            _ => {}
        }
        pos = body + len + (len & 1);
    }
}

fn check_format(path: &Path, w: &WavInfo) -> Result<()> {
    let ok = w.format == 1 && w.bits == 16 && w.channels == 1 && w.rate == RATE;
    ensure!(
        ok,
        invalid(format!(
            "{}: expected 16 kHz mono 16-bit PCM WAV, got {} Hz, {} channel(s), {}-bit (format {})",
            path.display(),
            w.rate,
            w.channels,
            w.bits,
            w.format
        ))
    );
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("sync {}", dir.display()))
}

/// Write `bytes` to `path` atomically and durably (temp file, fsync, rename, fsync dir).
fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Phase 1: copy the plan's tracks into `raw_dir/import/` and validate the copies.
/// Every track must resolve (after symlinks) inside `root`, the manifest's directory.
/// On error the caller removes `raw_dir`. Returns the total bytes copied.
pub fn stage(raw_dir: &Path, root: &Path, plan: &ImportPlan) -> Result<u64> {
    let dir = raw_dir.join(STAGE_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let root = root.canonicalize().with_context(|| format!("resolve {}", root.display()))?;
    let max_s = plan.duration_s() + TRACK_SLACK_S;
    let mut bytes = 0;
    let tracks = plan
        .tracks
        .iter()
        .enumerate()
        .map(|(i, (label, src))| {
            let shown = src.display();
            let real = src.canonicalize().map_err(|e| invalid(format!("track {shown}: {e}")))?;
            ensure!(real.starts_with(&root), invalid(format!("track {shown} is outside the manifest's folder {}", root.display())));
            let meta = std::fs::metadata(&real)?;
            ensure!(meta.is_file(), invalid(format!("track {shown} is not a regular file")));
            ensure!(meta.len() <= MAX_TRACK_BYTES, invalid(format!("track {shown} is larger than 12 hours of audio")));
            let file = format!("src-{i:02}.wav");
            let dst = dir.join(&file);
            std::fs::copy(&real, &dst).with_context(|| format!("copy {shown}"))?;
            File::open(&dst).and_then(|f| f.sync_all())?;
            // Validate our copy, not the source: it can't change under us any more.
            let w = read_wav_header(&dst).map_err(|e| invalid(format!("track {shown}: {}", e.to_string().trim_start_matches(muesli_core::import::INVALID))))?;
            check_format(src, &w)?;
            let secs = w.frames() as f64 / RATE as f64;
            ensure!(
                secs <= max_s,
                invalid(format!("track {shown} is {secs:.1} s long but the meeting is {:.1} s (ended_at − started_at)", plan.duration_s()))
            );
            bytes += meta.len();
            Ok(StagedTrack { label: label.clone(), file, samples: w.frames() })
        })
        .collect::<Result<Vec<_>>>()?;
    let st = Stage { duration_s: plan.duration_s(), speakers: plan.speakers.clone(), tracks };
    write_durable(&dir.join(STAGE_FILE), &serde_json::to_vec_pretty(&st)?)?;
    sync_dir(raw_dir)?;
    for d in raw_dir.ancestors().skip(1).take(2) {
        sync_dir(d)?;
    }
    Ok(bytes)
}

/// Is there staged import audio that [`finalize`] hasn't converted yet?
pub fn is_pending(raw_dir: &Path) -> bool {
    raw_dir.join(STAGE_DIR).join(STAGE_FILE).exists() && !raw_dir.join("recording.json").exists()
}

/// Sequential PCM16 reader over a WAV's data chunk.
struct Pcm16 {
    r: BufReader<File>,
    left: u64,
    buf: Vec<u8>,
}

impl Pcm16 {
    fn open(path: &Path) -> Result<Self> {
        let w = read_wav_header(path)?;
        check_format(path, &w)?;
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(w.data_offset))?;
        Ok(Pcm16 { r: BufReader::with_capacity(1 << 16, f), left: w.frames(), buf: vec![0; 1 << 16] })
    }

    /// Add the next `out.len()` samples (or what's left) into `out`.
    fn add_into(&mut self, out: &mut [f32]) -> Result<()> {
        let mut at = 0;
        while at < out.len() && self.left > 0 {
            let n = (out.len() - at).min(self.left as usize).min(self.buf.len() / 2);
            self.r.read_exact(&mut self.buf[..n * 2])?;
            for (o, b) in out[at..at + n].iter_mut().zip(self.buf[..n * 2].chunks_exact(2)) {
                *o += i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0;
            }
            at += n;
            self.left -= n as u64;
        }
        Ok(())
    }
}

/// Phase 2: staged WAVs → `{label}-NNN.opus` segments + `recording.json`. All tracks are
/// padded to the same length (the meeting duration or the longest track, whichever is longer).
/// A no-op that returns the existing info once done.
pub fn finalize(raw_dir: &Path) -> Result<RecordingInfo> {
    finalize_with(raw_dir, SEGMENT_SECS)
}

fn finalize_with(raw_dir: &Path, segment_secs: u32) -> Result<RecordingInfo> {
    let stage_dir = raw_dir.join(STAGE_DIR);
    if raw_dir.join("recording.json").exists() {
        let _ = std::fs::remove_dir_all(&stage_dir);
        return RecordingInfo::load(raw_dir);
    }
    let st: Stage = serde_json::from_str(
        &std::fs::read_to_string(stage_dir.join(STAGE_FILE)).context("the imported audio is missing")?,
    )?;
    let total = st
        .tracks
        .iter()
        .map(|t| t.samples)
        .chain([(st.duration_s * RATE as f64).round() as u64])
        .max()
        .unwrap_or(0);
    let seg_len = (segment_secs * RATE) as u64;
    let n_segs = total.div_ceil(seg_len).max(1);
    let mut bytes = 0;
    let mut tracks = vec![];
    for sp in &st.speakers {
        let mut sources = st
            .tracks
            .iter()
            .filter(|t| t.label == sp.label)
            .map(|t| Pcm16::open(&stage_dir.join(&t.file)))
            .collect::<Result<Vec<_>>>()?;
        if sources.is_empty() {
            continue;
        }
        let segments = (0..n_segs)
            .map(|k| {
                let n = seg_len.min(total.saturating_sub(k * seg_len)) as usize;
                let mut block = vec![0f32; n];
                for s in sources.iter_mut() {
                    s.add_into(&mut block)?;
                }
                block.iter_mut().for_each(|x| *x = x.clamp(-1.0, 1.0));
                let name = format!("{}-{k:03}.opus", sp.label);
                let path = raw_dir.join(&name);
                let mut w = create_file(&path, RATE, BITRATE)?;
                w.write(&block)?;
                let f = w.finish()?.into_inner().map_err(|e| anyhow::anyhow!("flush {name}: {e}"))?;
                f.sync_all()?;
                bytes += f.metadata()?.len();
                Ok(name)
            })
            .collect::<Result<Vec<_>>>()?;
        tracks.push(TrackInfo { label: sp.label.clone(), name: sp.name.clone(), segments });
    }
    ensure!(!tracks.is_empty(), "the import has no tracks");
    let info = RecordingInfo {
        rate: RATE,
        segment_secs,
        duration_s: total as f64 / RATE as f64,
        mic_segments: vec![],
        sys_segments: vec![],
        bytes,
        tracks,
    };
    write_durable(&raw_dir.join("recording.json"), serde_json::to_string_pretty(&info)?.as_bytes())?;
    std::fs::remove_dir_all(&stage_dir)?;
    Ok(info)
}

/// The recording in `raw_dir`: finishes a staged import first, rebuilds a crashed capture.
pub fn recording_info(raw_dir: &Path) -> Result<RecordingInfo> {
    match raw_dir.join(STAGE_DIR).exists() {
        true => finalize(raw_dir),
        false => RecordingInfo::load(raw_dir).or_else(|_| RecordingInfo::scan(raw_dir)),
    }
}

/// Where staged copies live (for tests and cleanup).
pub fn stage_dir(raw_dir: &Path) -> PathBuf {
    raw_dir.join(STAGE_DIR)
}

/// Error while an import is staged but not converted yet (cleaning must wait for it).
pub fn bail_if_pending(raw_dir: &Path) -> Result<()> {
    if is_pending(raw_dir) {
        bail!("the imported audio is still being saved");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use muesli_core::import::{ImportSpeaker, Manifest, ManifestTrack};

    pub fn wav(rate: u32, channels: u16, format: u16, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend((36 + data.len() as u32).to_le_bytes());
        v.extend(b"WAVEfmt ");
        v.extend(16u32.to_le_bytes());
        v.extend(format.to_le_bytes());
        v.extend(channels.to_le_bytes());
        v.extend(rate.to_le_bytes());
        v.extend((rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        v.extend((channels * bits / 8).to_le_bytes());
        v.extend(bits.to_le_bytes());
        v.extend(b"LIST");
        v.extend(4u32.to_le_bytes());
        v.extend(b"INFO");
        v.extend(b"data");
        v.extend((data.len() as u32).to_le_bytes());
        v.extend(data);
        v
    }

    /// 16 kHz mono PCM16: a 440 Hz tone during `[a, b)` seconds, silence elsewhere, `len` s long.
    pub fn tone_wav(len: f64, a: f64, b: f64) -> Vec<u8> {
        let n = (len * RATE as f64) as usize;
        let data: Vec<u8> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                match (a..b).contains(&t) {
                    true => ((t * 440.0 * std::f64::consts::TAU).sin() * 0.3 * 32_767.0) as i16,
                    false => 0,
                }
            })
            .flat_map(i16::to_le_bytes)
            .collect();
        wav(RATE, 1, 1, 16, &data)
    }

    pub fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("muesli-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn plan(dir: &Path, tracks: &[(Option<&str>, &str)], duration_ms: i64) -> ImportPlan {
        let started_at = 1_760_000_000_000;
        Manifest {
            version: 1,
            source_id: "discord:1:test".into(),
            app: Some("discord".into()),
            hint: None,
            started_at,
            ended_at: started_at + duration_ms,
            tracks: tracks.iter().map(|(n, f)| ManifestTrack { name: n.map(Into::into), path: dir.join(f) }).collect(),
        }
        .validate(started_at + duration_ms)
        .unwrap()
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
    }

    #[test]
    fn wav_headers() {
        let d = tmp("hdr");
        let check = |name: &str, bytes: Vec<u8>| {
            let p = d.join(name);
            std::fs::write(&p, bytes).unwrap();
            read_wav_header(&p).and_then(|w| check_format(&p, &w).map(|_| w))
        };
        let ok = check("ok.wav", wav(16_000, 1, 1, 16, &[0; 320])).unwrap();
        assert_eq!((ok.frames(), ok.data_offset), (160, 56), "skips the LIST chunk");
        assert!(check("stereo.wav", wav(16_000, 2, 1, 16, &[0; 320])).unwrap_err().to_string().contains("2 channel"));
        assert!(check("48k.wav", wav(48_000, 1, 1, 16, &[0; 320])).unwrap_err().to_string().contains("48000 Hz"));
        assert!(check("f32.wav", wav(16_000, 1, 3, 32, &[0; 320])).is_err());
        let mut cut = wav(16_000, 1, 1, 16, &[0; 320]);
        cut.truncate(cut.len() - 100);
        assert!(check("cut.wav", cut).unwrap_err().to_string().contains("truncated"));
        assert!(check("junk.wav", b"hello world, not audio".to_vec()).unwrap_err().to_string().starts_with(muesli_core::import::INVALID));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stage_then_finalize_aligns_and_merges_speakers() {
        let d = tmp("align");
        // Matt speaks 0.5–1.5 s (first file) and 2.5–3.5 s (resumed file, padded from zero),
        // Lena 1.6–2.4 s, plus a mixed room track. Meeting is 4 s; segments 1 s for the test.
        std::fs::write(d.join("matt-1.wav"), tone_wav(1.6, 0.5, 1.5)).unwrap();
        std::fs::write(d.join("matt-2.wav"), tone_wav(3.6, 2.5, 3.5)).unwrap();
        std::fs::write(d.join("lena.wav"), tone_wav(2.4, 1.6, 2.4)).unwrap();
        std::fs::write(d.join("room.wav"), tone_wav(0.5, 0.0, 0.0)).unwrap();
        let p = plan(
            &d,
            &[(Some("Matt Rosenthal"), "matt-1.wav"), (Some("Lena"), "lena.wav"), (Some("Matt Rosenthal"), "matt-2.wav"), (None, "room.wav")],
            4_000,
        );
        let raw = d.join("meeting/raw");
        stage(&raw, &d, &p).unwrap();
        assert!(is_pending(&raw));
        assert!(bail_if_pending(&raw).is_err(), "cleaning waits for finalize");
        // Sources can go away once staged.
        ["matt-1.wav", "matt-2.wav", "lena.wav", "room.wav"].iter().for_each(|f| std::fs::remove_file(d.join(f)).unwrap());
        let info = finalize_with(&raw, 1).unwrap();
        assert!(!stage_dir(&raw).exists() && !is_pending(&raw));
        assert!((info.duration_s - 4.0).abs() < 1e-9);
        let who: Vec<_> = info.tracks.iter().map(|t| (t.label.as_str(), t.name.as_deref(), t.segments.len())).collect();
        assert_eq!(who, [("t0", Some("Matt Rosenthal"), 4), ("t1", Some("Lena"), 4), ("m0", None, 4)]);
        let seg = |label: &str, k: usize| crate::oggopus::decode_file(&raw.join(format!("{label}-{k:03}.opus")), RATE).unwrap();
        let window = |v: &[f32], a: f64, b: f64| rms(&v[(a * RATE as f64) as usize..(b * RATE as f64) as usize]);
        let matt: Vec<f32> = (0..4).flat_map(|k| seg("t0", k)).collect();
        let lena: Vec<f32> = (0..4).flat_map(|k| seg("t1", k)).collect();
        assert!((matt.len() as i64 - 4 * RATE as i64).abs() < 1_000, "padded to the meeting: {}", matt.len());
        assert!(window(&matt, 0.7, 1.3) > 0.1 && window(&matt, 2.7, 3.3) > 0.1, "both of Matt's files land in place");
        assert!(window(&matt, 1.7, 2.3) < 0.01 && window(&lena, 1.7, 2.3) > 0.1 && window(&lena, 0.6, 1.4) < 0.01);
        // Idempotent: a second run just loads the result.
        assert_eq!(finalize_with(&raw, 1).unwrap().tracks, info.tracks);
        assert_eq!(recording_info(&raw).unwrap().tracks, info.tracks);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stage_rejects_bad_tracks() {
        let d = tmp("reject");
        let root = d.join("session");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(d.join("outside.wav"), tone_wav(1.0, 0.0, 0.5)).unwrap();
        std::os::unix::fs::symlink(d.join("outside.wav"), root.join("link.wav")).unwrap();
        std::fs::write(root.join("long.wav"), tone_wav(40.0, 0.0, 0.0)).unwrap();
        std::fs::write(root.join("stereo.wav"), wav(RATE, 2, 1, 16, &[0; 640])).unwrap();
        let try_one = |file: &str| {
            let raw = d.join(format!("m-{file}/raw"));
            stage(&raw, &root, &plan(&root, &[(Some("A"), file)], 5_000)).unwrap_err().to_string()
        };
        assert!(try_one(d.join("outside.wav").to_str().unwrap()).contains("outside the manifest"));
        assert!(try_one("link.wav").contains("outside the manifest"), "symlink out of the folder");
        assert!(try_one("missing.wav").contains("No such file"));
        assert!(try_one("long.wav").contains("40.0 s long"));
        assert!(try_one("stereo.wav").contains("mono"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn speakers_keep_manifest_order() {
        let sp = |l: &str, n: Option<&str>| ImportSpeaker { label: l.into(), name: n.map(Into::into) };
        let p = plan(Path::new("/x"), &[(Some("B"), "b.wav"), (None, "m.wav"), (Some("A"), "a.wav")], 1_000);
        assert_eq!(p.speakers, [sp("t0", Some("B")), sp("m0", None), sp("t1", Some("A"))]);
    }
}
