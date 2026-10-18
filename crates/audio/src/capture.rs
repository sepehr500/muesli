//! Dual-channel capture: default mic ("me") + monitor of the default sink ("them").
//!
//! One PipeWire main-loop thread owns both streams; each stream feeds a writer thread that
//! encodes rolling Ogg Opus segments. Writers place every buffer on a shared monotonic
//! timeline (gap-filling with silence), so mic and sys stay sample-aligned for echo
//! cancellation even if a stream stalls (e.g. a suspended sink).

use crate::dsp::{meter, rms};
use crate::oggopus::create_file;
use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::{properties::properties, spa};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

pub const RATE: u32 = 48_000;
pub const SEGMENT_SECS: u32 = 300;
const BITRATE: i32 = 32_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Mic,
    Sys,
}

impl Channel {
    pub fn prefix(self) -> &'static str {
        match self {
            Channel::Mic => "mic",
            Channel::Sys => "sys",
        }
    }
}

/// Written to `raw/recording.json` when a recording stops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub rate: u32,
    pub segment_secs: u32,
    pub duration_s: f64,
    pub mic_segments: Vec<String>,
    pub sys_segments: Vec<String>,
    pub bytes: u64,
}

impl RecordingInfo {
    pub fn load(raw_dir: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(raw_dir.join("recording.json"))?)?)
    }

    /// Rebuild from segment files on disk (e.g. after a crash mid-recording).
    pub fn scan(raw_dir: &Path) -> Result<Self> {
        let list = |prefix: &str| -> Result<Vec<String>> {
            let mut v: Vec<String> = std::fs::read_dir(raw_dir)?
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with(prefix) && n.ends_with(".opus"))
                .collect();
            v.sort();
            Ok(v)
        };
        let (mic, sys) = (list("mic-")?, list("sys-")?);
        let bytes = mic.iter().chain(&sys).filter_map(|n| std::fs::metadata(raw_dir.join(n)).ok()).map(|m| m.len()).sum();
        let duration_s = mic
            .last()
            .and_then(|last| crate::oggopus::decode_file(&raw_dir.join(last), 16_000).ok())
            .map(|tail| (mic.len().saturating_sub(1) * SEGMENT_SECS as usize) as f64 + tail.len() as f64 / 16_000.0)
            .unwrap_or(0.0);
        Ok(RecordingInfo { rate: RATE, segment_secs: SEGMENT_SECS, duration_s, mic_segments: mic, sys_segments: sys, bytes })
    }
}

/// Live input levels (0..1) for meters.
#[derive(Default)]
pub struct Levels {
    me: AtomicU32,
    them: AtomicU32,
}

impl Levels {
    pub fn get(&self) -> (f32, f32) {
        (f32::from_bits(self.me.load(Ordering::Relaxed)), f32::from_bits(self.them.load(Ordering::Relaxed)))
    }
    fn set(&self, ch: Channel, v: f32) {
        let slot = match ch {
            Channel::Mic => &self.me,
            Channel::Sys => &self.them,
        };
        let prev = f32::from_bits(slot.load(Ordering::Relaxed));
        // Fast attack, slow release.
        let next = if v > prev { v } else { prev * 0.85 + v * 0.15 };
        slot.store(next.to_bits(), Ordering::Relaxed);
    }
}

type Buf = (Instant, Vec<f32>);
type WriterResult = Result<(Channel, Vec<String>, u64)>;

pub struct Recorder {
    quit: pw::channel::Sender<()>,
    pw_thread: Option<JoinHandle<Result<()>>>,
    writers: Vec<JoinHandle<WriterResult>>,
    stop_at: Arc<std::sync::Mutex<Option<Instant>>>,
    t0: Instant,
    dir: PathBuf,
    pub levels: Arc<Levels>,
}

/// Writer: places buffers on the shared timeline, rotates segments, tracks levels.
fn writer(
    ch: Channel,
    dir: PathBuf,
    t0: Instant,
    rx: mpsc::Receiver<Buf>,
    levels: Arc<Levels>,
    stop_at: Arc<std::sync::Mutex<Option<Instant>>>,
) -> WriterResult {
    let seg_len = (SEGMENT_SECS * RATE) as u64;
    let mut names: Vec<String> = vec![];
    let mut cur: Option<crate::oggopus::OggOpusWriter<std::io::BufWriter<std::fs::File>>> = None;
    let mut written: u64 = 0;

    let put = |pcm: &[f32], names: &mut Vec<String>, cur: &mut Option<_>, written: &mut u64| -> Result<()> {
        let mut rest = pcm;
        while !rest.is_empty() {
            if cur.is_none() {
                let name = format!("{}-{:03}.opus", ch.prefix(), names.len());
                *cur = Some(create_file(&dir.join(&name), RATE, BITRATE)?);
                names.push(name);
            }
            let room = (seg_len - *written % seg_len) as usize;
            let n = room.min(rest.len());
            let w: &mut crate::oggopus::OggOpusWriter<_> = cur.as_mut().expect("segment open");
            w.write(&rest[..n])?;
            *written += n as u64;
            rest = &rest[n..];
            if (*written).is_multiple_of(seg_len) {
                cur.take().expect("segment open").finish()?;
            }
        }
        Ok(())
    };

    let slack = (RATE / 10) as u64; // 100 ms
    for (ts, pcm) in rx {
        let end = (ts.saturating_duration_since(t0).as_secs_f64() * RATE as f64) as u64;
        let start = end.saturating_sub(pcm.len() as u64);
        if start > written + slack {
            let gap = vec![0f32; (start - written) as usize];
            put(&gap, &mut names, &mut cur, &mut written)?;
        }
        levels.set(ch, meter(rms(&pcm)));
        put(&pcm, &mut names, &mut cur, &mut written)?;
    }
    // Pad to the common stop time so both channels have equal length.
    let stop = stop_at.lock().ok().and_then(|g| *g).unwrap_or_else(Instant::now);
    let target = (stop.saturating_duration_since(t0).as_secs_f64() * RATE as f64) as u64;
    if target > written {
        let pad = vec![0f32; (target - written) as usize];
        put(&pad, &mut names, &mut cur, &mut written)?;
    }
    if let Some(w) = cur.take() {
        w.finish()?;
    }
    Ok((ch, names, written))
}

fn audio_format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(1);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .expect("serialize format pod")
        .0
        .into_inner()
}

fn run_pw(
    mic_tx: mpsc::Sender<Buf>,
    sys_tx: mpsc::Sender<Buf>,
    quit_rx: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

    let make = |name: &str, monitor: bool, tx: mpsc::Sender<Buf>| -> Result<_> {
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Production",
            *pw::keys::APP_NAME => "muesli",
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_LATENCY => "2048/48000",
        };
        if monitor {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        let stream = pw::stream::StreamBox::new(&core, name, props)?;
        let listener = stream
            .add_local_listener_with_user_data(tx)
            .process(|stream, tx| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let n = data.chunk().size() as usize / std::mem::size_of::<f32>();
                if let Some(bytes) = data.data() {
                    let pcm: Vec<f32> = bytes[..n * 4]
                        .as_chunks::<4>().0.iter()
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    let _ = tx.send((Instant::now(), pcm));
                }
            })
            .register()?;
        let pod_bytes = audio_format_pod();
        let mut params = [spa::pod::Pod::from_bytes(&pod_bytes).ok_or_else(|| anyhow!("bad format pod"))?];
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        Ok((stream, listener))
    };

    let started = (|| -> Result<_> { Ok((make("muesli-mic", false, mic_tx)?, make("muesli-sys", true, sys_tx)?)) })();
    let streams = match started {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e}")));
            return Err(e);
        }
    };
    mainloop.run();
    let ((mic, _l1), (sys, _l2)) = streams;
    let _ = mic.disconnect();
    let _ = sys.disconnect();
    Ok(())
}

impl Recorder {
    /// Start recording into `meeting_dir/raw/`.
    pub fn start(meeting_dir: &Path) -> Result<Self> {
        let dir = meeting_dir.join("raw");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let t0 = Instant::now();
        let levels = Arc::new(Levels::default());
        let stop_at = Arc::new(std::sync::Mutex::new(None));
        let (mic_tx, mic_rx) = mpsc::channel();
        let (sys_tx, sys_rx) = mpsc::channel();
        let writers = [(Channel::Mic, mic_rx), (Channel::Sys, sys_rx)]
            .into_iter()
            .map(|(ch, rx)| {
                let (dir, levels, stop_at) = (dir.clone(), levels.clone(), stop_at.clone());
                std::thread::Builder::new()
                    .name(format!("muesli-{}", ch.prefix()))
                    .spawn(move || writer(ch, dir, t0, rx, levels, stop_at))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let (quit_tx, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let pw_thread = std::thread::Builder::new()
            .name("muesli-pw".into())
            .spawn(move || run_pw(mic_tx, sys_tx, quit_rx, ready_tx))?;
        ready_rx.recv().map_err(|_| anyhow!("pipewire thread died"))??;
        Ok(Recorder { quit: quit_tx, pw_thread: Some(pw_thread), writers, stop_at, t0, dir, levels })
    }

    pub fn elapsed_s(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    /// Stop, finalize all segment files, write `recording.json`.
    pub fn stop(mut self) -> Result<RecordingInfo> {
        if let Ok(mut g) = self.stop_at.lock() {
            *g = Some(Instant::now());
        }
        let _ = self.quit.send(());
        if let Some(t) = self.pw_thread.take() {
            t.join().map_err(|_| anyhow!("pipewire thread panicked"))??;
        }
        let mut info = RecordingInfo {
            rate: RATE,
            segment_secs: SEGMENT_SECS,
            duration_s: 0.0,
            mic_segments: vec![],
            sys_segments: vec![],
            bytes: 0,
        };
        for w in std::mem::take(&mut self.writers) {
            let (ch, names, samples) = w.join().map_err(|_| anyhow!("writer panicked"))??;
            info.duration_s = info.duration_s.max(samples as f64 / RATE as f64);
            match ch {
                Channel::Mic => info.mic_segments = names,
                Channel::Sys => info.sys_segments = names,
            }
        }
        info.bytes = info
            .mic_segments
            .iter()
            .chain(&info.sys_segments)
            .filter_map(|n| std::fs::metadata(self.dir.join(n)).ok())
            .map(|m| m.len())
            .sum();
        std::fs::write(self.dir.join("recording.json"), serde_json::to_string_pretty(&info)?)?;
        Ok(info)
    }
}
