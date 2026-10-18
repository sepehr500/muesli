//! Playback: a mixed copy of the meeting (you + the call) in short Ogg Opus pieces, and a
//! small PipeWire player that streams them.
//!
//! Pieces are `SEGMENT_S` long, so seeking is "decode one small file and skip ahead"; no
//! Ogg page index needed. Written by the cleaning pass from the echo-cancelled mic, so
//! playback on speakers doesn't sound doubled.

use crate::oggopus::{OggOpusWriter, create_file, decode_file};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub const RATE: u32 = 16_000;
pub const SEGMENT_S: u32 = 30;
const BITRATE: i32 = 24_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackIndex {
    pub rate: u32,
    pub segment_s: u32,
    pub segments: usize,
    pub duration_s: f64,
}

impl PlaybackIndex {
    fn segment_len(&self) -> usize {
        (self.rate * self.segment_s) as usize
    }
}

pub fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn segment_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i:03}.opus"))
}

pub fn load_index(dir: &Path) -> Result<PlaybackIndex> {
    let s = std::fs::read_to_string(index_path(dir)).with_context(|| format!("no playback in {}", dir.display()))?;
    Ok(serde_json::from_str(&s)?)
}

/// (segment, sample offset within it) for meeting time `t`, clamped to the recording.
pub fn locate(t: f64, idx: &PlaybackIndex) -> (usize, usize) {
    let total = (idx.duration_s * idx.rate as f64) as usize;
    let sample = ((t.max(0.0) * idx.rate as f64) as usize).min(total);
    let len = idx.segment_len();
    (sample / len, sample % len)
}

/// Writes mixed audio into consecutive `SEGMENT_S` pieces plus `index.json`.
pub struct PlaybackWriter {
    dir: PathBuf,
    cur: Option<OggOpusWriter<BufWriter<File>>>,
    in_seg: usize,
    segments: usize,
    total: u64,
}

impl PlaybackWriter {
    pub fn create(dir: &Path) -> Result<Self> {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir)?;
        Ok(PlaybackWriter { dir: dir.to_path_buf(), cur: None, in_seg: 0, segments: 0, total: 0 })
    }

    pub fn write(&mut self, mut pcm: &[f32]) -> Result<()> {
        let len = (RATE * SEGMENT_S) as usize;
        while !pcm.is_empty() {
            if self.cur.is_none() {
                self.cur = Some(create_file(&segment_path(&self.dir, self.segments), RATE, BITRATE)?);
                self.segments += 1;
                self.in_seg = 0;
            }
            let n = (len - self.in_seg).min(pcm.len());
            self.cur.as_mut().expect("open").write(&pcm[..n])?;
            (self.in_seg, self.total, pcm) = (self.in_seg + n, self.total + n as u64, &pcm[n..]);
            if self.in_seg == len {
                self.cur.take().expect("open").finish()?;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<PlaybackIndex> {
        if let Some(w) = self.cur.take() {
            w.finish()?;
        }
        let idx = PlaybackIndex {
            rate: RATE,
            segment_s: SEGMENT_S,
            segments: self.segments,
            duration_s: self.total as f64 / RATE as f64,
        };
        std::fs::write(index_path(&self.dir), serde_json::to_string_pretty(&idx)?)?;
        Ok(idx)
    }
}

/// Slow automatic gain for one channel: brings speech to a common loudness so a quiet mic
/// and a loud call mix evenly. Gain only adapts on speech frames, so silence isn't boosted.
#[derive(Debug, Clone)]
pub struct Leveler {
    gain: f32,
    speech_rms: Option<f32>,
}

impl Default for Leveler {
    fn default() -> Self {
        Leveler { gain: 1.0, speech_rms: None }
    }
}

impl Leveler {
    const TARGET_RMS: f32 = 0.08;
    const MAX_GAIN: f32 = 40.0;

    pub fn apply(&mut self, frame: &mut [f32], speech: bool) {
        if speech {
            let r = crate::dsp::rms(frame);
            let avg = self.speech_rms.map_or(r, |a| a * 0.97 + r * 0.03);
            self.speech_rms = Some(avg);
            let want = (Self::TARGET_RMS / avg.max(1e-5)).clamp(0.25, Self::MAX_GAIN);
            self.gain += (want - self.gain) * 0.05;
        }
        frame.iter_mut().for_each(|x| *x *= self.gain);
    }
}

/// Sum two channels with a soft limiter (no harsh clipping when both talk at once).
pub fn mix_into(a: &[f32], b: &[f32], out: &mut Vec<f32>) {
    out.clear();
    out.extend(a.iter().zip(b).map(|(x, y)| (x + y).tanh()));
}

/// Streams playback pieces; used by the player and tests.
struct Feed {
    dir: PathBuf,
    idx: PlaybackIndex,
    seg: usize,
    buf: Vec<f32>,
    off: usize,
}

impl Feed {
    fn new(dir: &Path) -> Result<Self> {
        let idx = load_index(dir)?;
        Ok(Feed { dir: dir.to_path_buf(), idx, seg: usize::MAX, buf: vec![], off: 0 })
    }

    fn seek(&mut self, t: f64) {
        let (seg, off) = locate(t, &self.idx);
        if seg != self.seg {
            self.load(seg);
        }
        self.off = off.min(self.buf.len());
    }

    fn load(&mut self, seg: usize) {
        self.seg = seg;
        self.off = 0;
        self.buf = match seg < self.idx.segments {
            true => decode_file(&segment_path(&self.dir, seg), self.idx.rate)
                .map_err(|e| tracing::warn!("playback segment {seg}: {e}"))
                .unwrap_or_default(),
            false => vec![],
        };
    }

    /// Absolute position in samples.
    fn position(&self) -> u64 {
        (self.seg.min(self.idx.segments) * self.idx.segment_len() + self.off) as u64
    }

    /// Fill `out`; returns samples written (fewer than `out.len()` only at the end).
    fn fill(&mut self, out: &mut [f32]) -> usize {
        let mut n = 0;
        while n < out.len() {
            if self.off >= self.buf.len() {
                if self.seg + 1 >= self.idx.segments {
                    break;
                }
                self.load(self.seg + 1);
                continue;
            }
            let k = (out.len() - n).min(self.buf.len() - self.off);
            out[n..n + k].copy_from_slice(&self.buf[self.off..self.off + k]);
            (n, self.off) = (n + k, self.off + k);
        }
        n
    }
}

enum Cmd {
    Play,
    Pause,
    Seek(f64),
    Quit,
}

#[derive(Default)]
struct Shared {
    pos: std::sync::atomic::AtomicU64,
    playing: std::sync::atomic::AtomicBool,
}

/// Plays one meeting's playback pieces through PipeWire on its own thread.
pub struct Player {
    tx: pipewire::channel::Sender<Cmd>,
    shared: std::sync::Arc<Shared>,
    rate: u32,
    pub duration_s: f64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    pub fn open(dir: &Path) -> Result<Self> {
        let feed = Feed::new(dir)?;
        let (rate, duration_s) = (feed.idx.rate, feed.idx.duration_s);
        let shared = std::sync::Arc::new(Shared::default());
        let (tx, rx) = pipewire::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let sh = shared.clone();
        let thread = std::thread::Builder::new().name("muesli-player".into()).spawn(move || {
            if let Err(e) = run_player(feed, sh, rx, &ready_tx) {
                tracing::warn!("player: {e}");
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })?;
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .map_err(|_| anyhow!("audio output didn't start"))?
            .map_err(|e| anyhow!(e))?;
        Ok(Player { tx, shared, rate, duration_s, thread: Some(thread) })
    }

    pub fn play(&self) {
        let _ = self.tx.send(Cmd::Play);
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }
    pub fn seek(&self, t: f64) {
        let _ = self.tx.send(Cmd::Seek(t));
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn position(&self) -> f64 {
        self.shared.pos.load(std::sync::atomic::Ordering::Relaxed) as f64 / self.rate as f64
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run_player(
    feed: Feed,
    shared: std::sync::Arc<Shared>,
    rx: pipewire::channel::Receiver<Cmd>,
    ready: &std::sync::mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::sync::atomic::Ordering::Relaxed;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let rate = feed.idx.rate;
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::APP_NAME => "muesli",
        *pw::keys::NODE_NAME => "muesli-playback",
        *pw::keys::NODE_LATENCY => format!("800/{rate}"),
    };
    let mut props = props;
    // Tests route playback to a null sink instead of the speakers.
    if let Ok(target) = std::env::var("MUESLI_PLAYBACK_TARGET") {
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamRc::new(core, "muesli-playback", props)?;
    let feed = std::rc::Rc::new(std::cell::RefCell::new(feed));
    let (f2, sh2) = (feed.clone(), shared.clone());
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let Some(bytes) = data.data() else { return };
            let frames = (bytes.len() / 4).min(2048);
            let mut pcm = vec![0.0f32; frames];
            if sh2.playing.load(Relaxed) {
                let mut f = f2.borrow_mut();
                let n = f.fill(&mut pcm);
                sh2.pos.store(f.position(), Relaxed);
                if n < frames {
                    sh2.playing.store(false, Relaxed);
                }
            }
            pcm.iter().zip(bytes.as_chunks_mut::<4>().0).for_each(|(x, b)| *b = x.to_le_bytes());
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = (frames * 4) as u32;
        })
        .register()?;
    let pod = crate::capture::format_pod(rate);
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad format pod"))?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::INACTIVE,
        &mut params,
    )?;
    let (ml, st) = (mainloop.clone(), stream.clone());
    let _cmds = rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Cmd::Play => {
            let mut f = feed.borrow_mut();
            // Start from the top when nothing is loaded yet or playback had finished.
            let finished = f.position() as f64 / rate as f64 >= f.idx.duration_s - 0.05;
            if finished || f.seg == usize::MAX {
                f.seek(0.0);
            }
            shared.pos.store(f.position(), Relaxed);
            shared.playing.store(true, Relaxed);
            let _ = st.set_active(true);
        }
        Cmd::Pause => {
            shared.playing.store(false, Relaxed);
            let _ = st.set_active(false);
        }
        Cmd::Seek(t) => {
            let mut f = feed.borrow_mut();
            f.seek(t);
            shared.pos.store(f.position(), Relaxed);
        }
        Cmd::Quit => ml.quit(),
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f64) -> Vec<f32> {
        (0..(secs * RATE as f64) as usize).map(|i| (i as f32 * 0.05).sin() * 0.3).collect()
    }

    #[test]
    fn locate_maps_time_to_segment() {
        let idx = PlaybackIndex { rate: RATE, segment_s: 30, segments: 3, duration_s: 75.0 };
        assert_eq!(locate(0.0, &idx), (0, 0));
        assert_eq!(locate(31.0, &idx), (1, RATE as usize));
        assert_eq!(locate(999.0, &idx), (2, 15 * RATE as usize));
        assert_eq!(locate(-5.0, &idx), (0, 0));
    }

    #[test]
    fn writer_segments_and_feed_streams_across_them() {
        let dir = std::env::temp_dir().join(format!("muesli-pb-{}", std::process::id()));
        let mut w = PlaybackWriter::create(&dir).unwrap();
        tone(65.0).chunks(1600).for_each(|c| w.write(c).unwrap());
        let idx = w.finish().unwrap();
        assert_eq!(idx.segments, 3);
        assert!((idx.duration_s - 65.0).abs() < 1e-6);
        assert_eq!(load_index(&dir).unwrap(), idx);

        let mut f = Feed::new(&dir).unwrap();
        f.seek(29.5);
        let mut out = vec![0.0; RATE as usize]; // 1 s, crosses into segment 1
        assert_eq!(f.fill(&mut out), out.len());
        assert_eq!(f.seg, 1);
        assert!((f.position() as f64 / RATE as f64 - 30.5).abs() < 0.05);
        f.seek(64.0);
        let mut rest = vec![0.0; 3 * RATE as usize];
        let n = f.fill(&mut rest);
        assert!(n < rest.len() && n > 0, "stops at the end: {n}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leveler_lifts_a_quiet_channel_and_leaves_silence_alone() {
        let mut l = Leveler::default();
        (0..400).for_each(|_| l.apply(&mut [0.003f32; 160], true));
        let mut f = vec![0.003f32; 160];
        l.apply(&mut f, true);
        assert!(crate::dsp::rms(&f) > 0.04, "quiet speech is lifted: {}", crate::dsp::rms(&f));
        let mut l2 = Leveler::default();
        let mut silence = vec![0.0005f32; 160];
        l2.apply(&mut silence, false);
        assert!((silence[0] - 0.0005).abs() < 1e-9, "no speech, no gain change");
        let mut out = vec![];
        mix_into(&[0.9, -0.9], &[0.9, -0.9], &mut out);
        assert!(out.iter().all(|x| x.abs() < 1.0));
    }
}
