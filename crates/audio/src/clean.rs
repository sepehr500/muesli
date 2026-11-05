//! Post-call cleaning: decode → echo cancellation (mic vs sys) → VAD → speech-only chunks.
//! Streams one 5-minute segment pair at a time, so memory stays small for long meetings.

use crate::capture::RecordingInfo;
use crate::dsp::{Chunker, ChunkInfo, Vad};
use crate::oggopus::decode_file;
use crate::playback::{Leveler, PlaybackIndex, PlaybackWriter, load_index, mix_into};
use anyhow::Result;
use std::path::Path;

pub const RATE: u32 = 16_000;
const FRAME: usize = (RATE / 100) as usize; // 10 ms

#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub echo_cancellation: bool,
    /// Close a chunk at the next pause after this much speech.
    pub max_chunk_speech_s: f64,
    /// Debug: also write the cleaned mic channel here (16 kHz Ogg Opus).
    pub dump_clean_mic: Option<std::path::PathBuf>,
    /// Also write a mixed, level-matched playback copy (you + the call) here.
    pub playback_dir: Option<std::path::PathBuf>,
}

impl Default for CleanOptions {
    fn default() -> Self {
        CleanOptions { echo_cancellation: true, max_chunk_speech_s: 240.0, dump_clean_mic: None, playback_dir: None }
    }
}

#[derive(Debug, Clone)]
pub struct CleanResult {
    pub mic_chunks: Vec<ChunkInfo>,
    pub sys_chunks: Vec<ChunkInfo>,
    /// Imported recordings: speech chunks per track (instead of mic/sys).
    pub track_chunks: Vec<TrackChunks>,
    pub duration_s: f64,
    /// Seconds where nobody was speaking (removed from uploads).
    pub silence_s: f64,
    pub echo_cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct TrackChunks {
    /// Speaker label ("t0" named, "m0" mixed).
    pub label: String,
    /// Display name of a named track; None for a mixed track.
    pub name: Option<String>,
    pub chunks: Vec<ChunkInfo>,
}

/// Echo canceller abstraction so the crate builds without WebRTC.
trait Aec {
    fn process(&mut self, mic: &mut [f32], sys: &[f32]);
}

struct NoAec;
impl Aec for NoAec {
    fn process(&mut self, _: &mut [f32], _: &[f32]) {}
}

#[cfg(feature = "aec")]
struct WebRtc(webrtc_audio_processing::Processor);

#[cfg(feature = "aec")]
impl WebRtc {
    fn new() -> Result<Self> {
        use webrtc_audio_processing_config as c;
        let p = webrtc_audio_processing::Processor::new(RATE)
            .map_err(|e| anyhow::anyhow!("webrtc apm: {e:?}"))?;
        p.set_config(c::Config {
            echo_canceller: Some(c::EchoCanceller::default()),
            high_pass_filter: Some(c::HighPassFilter::default()),
            noise_suppression: Some(c::NoiseSuppression {
                level: c::NoiseSuppressionLevel::Moderate,
                ..Default::default()
            }),
            ..Default::default()
        });
        Ok(WebRtc(p))
    }
}

#[cfg(feature = "aec")]
impl Aec for WebRtc {
    fn process(&mut self, mic: &mut [f32], sys: &[f32]) {
        let _ = self.0.analyze_render_frame([sys]);
        let _ = self.0.process_capture_frame([mic]);
    }
}

fn make_aec(enabled: bool) -> (Box<dyn Aec>, bool) {
    #[cfg(feature = "aec")]
    if enabled {
        match WebRtc::new() {
            Ok(a) => return (Box::new(a), true),
            Err(e) => tracing::warn!("echo cancellation unavailable: {e}"),
        }
    }
    let _ = enabled;
    (Box::new(NoAec), false)
}

/// Run the cleaning pass over `raw_dir`, writing chunk files into `out_dir`.
pub fn clean(raw_dir: &Path, out_dir: &Path, opts: &CleanOptions) -> Result<CleanResult> {
    std::fs::create_dir_all(out_dir)?;
    crate::import::bail_if_pending(raw_dir)?;
    let info = RecordingInfo::load(raw_dir).or_else(|_| RecordingInfo::scan(raw_dir))?;
    if !info.tracks.is_empty() {
        return clean_tracks(raw_dir, out_dir, &info, opts);
    }
    let (mut aec, echo_cancelled) = make_aec(opts.echo_cancellation);
    let (mut vad_mic, mut vad_sys) = (Vad::default(), Vad::default());
    let mut mic_ch = Chunker::new(out_dir, "mic", RATE, opts.max_chunk_speech_s);
    let mut sys_ch = Chunker::new(out_dir, "sys", RATE, opts.max_chunk_speech_s);
    let mut t = 0.0f64;
    let mut silent_frames = 0u64;
    let n = info.mic_segments.len().max(info.sys_segments.len());
    let mut dump = match &opts.dump_clean_mic {
        Some(p) => Some(crate::oggopus::create_file(p, RATE, 32_000)?),
        None => None,
    };
    let mut playback = opts.playback_dir.as_deref().map(PlaybackWriter::create).transpose()?;
    let (mut lev_mic, mut lev_sys) = (Leveler::default(), Leveler::default());
    let mut mixed = Vec::with_capacity(FRAME);

    for i in 0..n {
        let load = |names: &[String]| -> Vec<f32> {
            names
                .get(i)
                .and_then(|name| {
                    decode_file(&raw_dir.join(name), RATE)
                        .map_err(|e| tracing::warn!("decode {name}: {e}"))
                        .ok()
                })
                .unwrap_or_default()
        };
        let (mut mic, mut sys) = (load(&info.mic_segments), load(&info.sys_segments));
        let len = mic.len().max(sys.len()).div_ceil(FRAME) * FRAME;
        mic.resize(len, 0.0);
        sys.resize(len, 0.0);
        for (m, s) in mic.as_chunks_mut::<FRAME>().0.iter_mut().zip(sys.as_chunks::<FRAME>().0) {
            aec.process(m, s);
            if let Some(d) = dump.as_mut() {
                d.write(m)?;
            }
            let (sm, ss) = (vad_mic.push(m), vad_sys.push(s));
            if let Some(w) = playback.as_mut() {
                let (mut a, mut b) = (*m, *s);
                lev_mic.apply(&mut a, sm);
                lev_sys.apply(&mut b, ss);
                mix_into(&a, &b, &mut mixed);
                w.write(&mixed)?;
            }
            mic_ch.push(t, m, sm)?;
            sys_ch.push(t, s, ss)?;
            silent_frames += u64::from(!sm && !ss);
            t += FRAME as f64 / RATE as f64;
        }
    }
    if let Some(d) = dump {
        d.finish()?;
    }
    if let Some(w) = playback {
        w.finish()?;
    }
    let (mic_chunks, _) = mic_ch.finish()?;
    let (sys_chunks, _) = sys_ch.finish()?;
    Ok(CleanResult {
        mic_chunks,
        sys_chunks,
        track_chunks: vec![],
        duration_s: t,
        silence_s: silent_frames as f64 * FRAME as f64 / RATE as f64,
        echo_cancelled,
    })
}

/// Imported per-speaker tracks: no echo to cancel (each track is one person's own stream), so
/// every track goes straight to its own VAD and chunker, and playback is the level-matched sum.
/// Loads one raw segment per track at a time.
fn clean_tracks(raw_dir: &Path, out_dir: &Path, info: &RecordingInfo, opts: &CleanOptions) -> Result<CleanResult> {
    let tracks = &info.tracks;
    let mut vads: Vec<Vad> = tracks.iter().map(|_| Vad::default()).collect();
    let mut levelers: Vec<Leveler> = tracks.iter().map(|_| Leveler::default()).collect();
    let mut chunkers: Vec<Chunker> =
        tracks.iter().map(|tr| Chunker::new(out_dir, &tr.label, RATE, opts.max_chunk_speech_s)).collect();
    let mut playback = opts.playback_dir.as_deref().map(PlaybackWriter::create).transpose()?;
    let (mut t, mut silent_frames) = (0.0f64, 0u64);
    let n = tracks.iter().map(|tr| tr.segments.len()).max().unwrap_or(0);
    let mut mix = [0f32; FRAME];
    for i in 0..n {
        let mut pcm: Vec<Vec<f32>> = tracks
            .iter()
            .map(|tr| {
                tr.segments
                    .get(i)
                    .and_then(|name| decode_file(&raw_dir.join(name), RATE).map_err(|e| tracing::warn!("decode {name}: {e}")).ok())
                    .unwrap_or_default()
            })
            .collect();
        let len = pcm.iter().map(Vec::len).max().unwrap_or(0).div_ceil(FRAME) * FRAME;
        pcm.iter_mut().for_each(|p| p.resize(len, 0.0));
        for f in 0..len / FRAME {
            mix.fill(0.0);
            let mut anyone = false;
            for (k, p) in pcm.iter().enumerate() {
                let frame: &[f32; FRAME] = p[f * FRAME..(f + 1) * FRAME].try_into().expect("frame");
                let speech = vads[k].push(frame);
                chunkers[k].push(t, frame, speech)?;
                anyone |= speech;
                if playback.is_some() {
                    let mut lev = *frame;
                    levelers[k].apply(&mut lev, speech);
                    mix.iter_mut().zip(lev).for_each(|(m, x)| *m += x);
                }
            }
            if let Some(w) = playback.as_mut() {
                mix.iter_mut().for_each(|m| *m = m.tanh());
                w.write(&mix)?;
            }
            silent_frames += u64::from(!anyone);
            t += FRAME as f64 / RATE as f64;
        }
    }
    if let Some(w) = playback {
        w.finish()?;
    }
    let track_chunks = tracks
        .iter()
        .zip(chunkers)
        .map(|(tr, c)| Ok(TrackChunks { label: tr.label.clone(), name: tr.name.clone(), chunks: c.finish()?.0 }))
        .collect::<Result<Vec<_>>>()?;
    Ok(CleanResult {
        mic_chunks: vec![],
        sys_chunks: vec![],
        track_chunks,
        duration_s: t,
        silence_s: silent_frames as f64 * FRAME as f64 / RATE as f64,
        echo_cancelled: false,
    })
}

/// Build playback for a meeting recorded before playback existed: a full cleaning pass into a
/// scratch directory (for the echo-cancelled mic), keeping only the playback copy.
pub fn render_playback(raw_dir: &Path, playback_dir: &Path, echo_cancellation: bool) -> Result<PlaybackIndex> {
    let scratch = playback_dir.with_extension("scratch");
    let opts = CleanOptions { echo_cancellation, playback_dir: Some(playback_dir.to_path_buf()), ..Default::default() };
    let r = clean(raw_dir, &scratch, &opts);
    let _ = std::fs::remove_dir_all(&scratch);
    r?;
    load_index(playback_dir)
}

/// Normalized cross-correlation peak of `a` vs `b` within ±max_lag samples (echo metric).
pub fn echo_correlation(a: &[f32], b: &[f32], max_lag: usize) -> f32 {
    let n = a.len().min(b.len());
    let energy = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>();
    let norm = (energy(&a[..n]) * energy(&b[..n])).sqrt().max(1e-9);
    (0..=max_lag)
        .step_by(4)
        .map(|lag| {
            let c: f32 = (lag..n).map(|i| a[i] * b[i - lag]).sum();
            (c / norm).abs()
        })
        .fold(0.0, f32::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::tests::{plan, tmp, tone_wav};

    #[test]
    fn imported_tracks_chunk_per_speaker_and_mix_for_playback() {
        let d = tmp("clean");
        std::fs::write(d.join("a.wav"), tone_wav(1.5, 0.5, 1.5)).unwrap();
        std::fs::write(d.join("b.wav"), tone_wav(3.0, 2.0, 3.0)).unwrap();
        let p = plan(&d, &[(Some("Ann"), "a.wav"), (Some("Bo"), "b.wav")], 3_500);
        let raw = d.join("m/raw");
        crate::import::stage(&raw, &d, &p).unwrap();
        let opts = CleanOptions { playback_dir: Some(d.join("m/playback")), ..Default::default() };
        assert!(clean(&raw, &d.join("m/clean"), &opts).is_err(), "not converted yet");
        crate::import::finalize(&raw).unwrap();
        let r = clean(&raw, &d.join("m/clean"), &opts).unwrap();
        assert!(!r.echo_cancelled && r.mic_chunks.is_empty() && r.sys_chunks.is_empty());
        let got: Vec<_> = r.track_chunks.iter().map(|t| (t.label.as_str(), t.name.as_deref(), t.chunks.len())).collect();
        assert_eq!(got, [("t0", Some("Ann"), 1), ("t1", Some("Bo"), 1)]);
        // Chunk time maps back to meeting time (300 ms pre-roll before each onset).
        let start = |k: usize| r.track_chunks[k].chunks[0].map.to_src(0.0);
        assert!((start(0) - 0.2).abs() < 0.1, "Ann starts at {}", start(0));
        assert!((start(1) - 1.7).abs() < 0.1, "Bo starts at {}", start(1));
        assert!(r.track_chunks[0].chunks[0].path.file_name().unwrap().to_str().unwrap().starts_with("t0-"));
        assert!((r.duration_s - 3.5).abs() < 0.05);
        // Playback: one mixed copy with both speakers in place.
        let idx = crate::playback::load_index(&d.join("m/playback")).unwrap();
        assert!((idx.duration_s - 3.5).abs() < 0.05);
        let mixed = decode_file(&d.join("m/playback/000.opus"), RATE).unwrap();
        let level = |a: f64, b: f64| crate::dsp::rms(&mixed[(a * 16_000.0) as usize..(b * 16_000.0) as usize]);
        assert!(level(0.7, 1.3) > 0.05 && level(2.2, 2.8) > 0.05 && level(1.6, 1.9) < 0.02);
        // Rebuilding playback later (PreparePlayback) works from the same raw tracks.
        let again = render_playback(&raw, &d.join("m/playback2"), true).unwrap();
        assert_eq!(again, idx);
        let _ = std::fs::remove_dir_all(&d);
    }
}
