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
    pub duration_s: f64,
    /// Seconds where nobody was speaking (removed from uploads).
    pub silence_s: f64,
    pub echo_cancelled: bool,
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
    let info = RecordingInfo::load(raw_dir).or_else(|_| RecordingInfo::scan(raw_dir))?;
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
        duration_s: t,
        silence_s: silent_frames as f64 * FRAME as f64 / RATE as f64,
        echo_cancelled,
    })
}

/// Normalized cross-correlation peak of `a` vs `b` within ±max_lag samples (echo metric).
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
