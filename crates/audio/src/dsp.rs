//! Small DSP pieces: levels, voice activity detection, speech chunking.

use crate::oggopus::{OggOpusWriter, create_file};
use anyhow::Result;
use muesli_core::transcript::TimeMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub fn rms(f: &[f32]) -> f32 {
    match f.len() {
        0 => 0.0,
        n => (f.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt(),
    }
}

pub fn db(f: &[f32]) -> f32 {
    10.0 * (f.iter().map(|x| x * x).sum::<f32>() / f.len().max(1) as f32 + 1e-12).log10()
}

/// Map RMS to a 0..1 meter value (−60 dBFS → 0, 0 dBFS → 1).
pub fn meter(rms: f32) -> f32 {
    ((20.0 * (rms + 1e-9).log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Energy VAD with an adaptive noise floor, onset confirmation and hangover.
/// Feed fixed 10 ms frames.
#[derive(Debug, Clone)]
pub struct Vad {
    floor_db: f32,
    active: bool,
    onset: u32,
    hang: u32,
    pub onset_frames: u32,
    pub hang_frames: u32,
    pub margin_db: f32,
    pub min_db: f32,
}

impl Default for Vad {
    fn default() -> Self {
        Vad {
            floor_db: -70.0,
            active: false,
            onset: 0,
            hang: 0,
            onset_frames: 3,
            hang_frames: 40,
            margin_db: 10.0,
            min_db: -52.0,
        }
    }
}

impl Vad {
    pub fn push(&mut self, frame: &[f32]) -> bool {
        let e = db(frame);
        // Floor: follow drops quickly, rise slowly (~2 dB/s at 10 ms frames).
        self.floor_db = match e < self.floor_db {
            true => 0.8 * self.floor_db + 0.2 * e,
            false => (self.floor_db + 0.02).min(e),
        };
        let voiced = e > (self.floor_db + self.margin_db).max(self.min_db);
        self.onset = if voiced { self.onset + 1 } else { 0 };
        match (self.active, voiced) {
            (false, _) if self.onset >= self.onset_frames => {
                self.active = true;
                self.hang = self.hang_frames;
            }
            (true, true) => self.hang = self.hang_frames,
            (true, false) => {
                self.hang = self.hang.saturating_sub(1);
                self.active = self.hang > 0;
            }
            _ => {}
        }
        self.active
    }
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub path: PathBuf,
    pub map: TimeMap,
    pub speech_s: f64,
}

struct Open {
    w: OggOpusWriter<BufWriter<File>>,
    path: PathBuf,
    kept: Vec<(f64, f64)>,
    speech_s: f64,
}

/// Collects speech frames into Ogg Opus chunk files, skipping silence.
/// Each chunk carries a `TimeMap` from chunk time to meeting time.
pub struct Chunker {
    dir: PathBuf,
    prefix: String,
    rate: u32,
    max_speech_s: f64,
    pre_roll_frames: usize,
    pre_roll: VecDeque<(f64, Vec<f32>)>,
    cur: Option<Open>,
    in_speech: bool,
    pub chunks: Vec<ChunkInfo>,
    pub speech_s: f64,
}

impl Chunker {
    pub fn new(dir: &Path, prefix: &str, rate: u32, max_speech_s: f64) -> Self {
        Chunker {
            dir: dir.to_path_buf(),
            prefix: prefix.to_string(),
            rate,
            max_speech_s,
            pre_roll_frames: 30,
            pre_roll: VecDeque::new(),
            cur: None,
            in_speech: false,
            chunks: vec![],
            speech_s: 0.0,
        }
    }

    fn write(&mut self, t: f64, frame: &[f32]) -> Result<()> {
        let rate = self.rate;
        if self.cur.is_none() {
            let path = self.dir.join(format!("{}-{:03}.opus", self.prefix, self.chunks.len()));
            self.cur = Some(Open { w: create_file(&path, rate, 24_000)?, path, kept: vec![], speech_s: 0.0 });
        }
        let cur = self.cur.as_mut().expect("open chunk");
        let len = frame.len() as f64 / rate as f64;
        cur.w.write(frame)?;
        match cur.kept.last_mut() {
            Some((s, l)) if ((*s + *l) - t).abs() < 1e-6 => *l += len,
            _ => cur.kept.push((t, len)),
        }
        cur.speech_s += len;
        self.speech_s += len;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(c) = self.cur.take() {
            c.w.finish()?;
            match c.speech_s >= 0.6 {
                true => self.chunks.push(ChunkInfo { path: c.path, map: TimeMap::from_kept(&c.kept), speech_s: c.speech_s }),
                false => {
                    let _ = std::fs::remove_file(&c.path);
                    self.speech_s -= c.speech_s;
                }
            }
        }
        Ok(())
    }

    /// `t` = meeting time (s) of the frame start.
    pub fn push(&mut self, t: f64, frame: &[f32], speech: bool) -> Result<()> {
        match speech {
            true => {
                if !self.in_speech {
                    let pre: Vec<_> = self.pre_roll.drain(..).collect();
                    for (pt, pf) in pre {
                        self.write(pt, &pf)?;
                    }
                }
                self.write(t, frame)?;
            }
            false => {
                self.pre_roll.push_back((t, frame.to_vec()));
                if self.pre_roll.len() > self.pre_roll_frames {
                    self.pre_roll.pop_front();
                }
                if self.cur.as_ref().is_some_and(|c| c.speech_s >= self.max_speech_s) {
                    self.close()?;
                }
            }
        }
        self.in_speech = speech;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(Vec<ChunkInfo>, f64)> {
        self.close()?;
        Ok((self.chunks, self.speech_s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(level: f32, n: usize) -> Vec<Vec<f32>> {
        (0..n)
            .map(|k| (0..160).map(|i| ((k * 160 + i) as f32 * 0.07).sin() * level).collect())
            .collect()
    }

    #[test]
    fn vad_detects_speech_with_hangover() {
        let mut v = Vad::default();
        let quiet = frames(0.0005, 100);
        let loud = frames(0.2, 50);
        let q: Vec<bool> = quiet.iter().map(|f| v.push(f)).collect();
        assert!(q.iter().all(|s| !s));
        let l: Vec<bool> = loud.iter().map(|f| v.push(f)).collect();
        assert!(!l[0] && l[5] && l[49]);
        let tail: Vec<bool> = quiet.iter().map(|f| v.push(f)).collect();
        assert!(tail[10], "hangover keeps speech on briefly");
        assert!(!tail[60], "then turns off");
    }

    #[test]
    fn chunker_skips_silence_and_maps_time() {
        let dir = std::env::temp_dir().join(format!("muesli-chunker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = Chunker::new(&dir, "mic", 16_000, 4.0);
        let frame = vec![0.1f32; 160];
        // 0–10 s silence, 10–13 s speech, 13–20 s silence, 20–23 s speech.
        for k in 0..2300 {
            let t = k as f64 * 0.01;
            let speech = (10.0..13.0).contains(&t) || (20.0..23.0).contains(&t);
            c.push(t, &frame, speech).unwrap();
        }
        let (chunks, speech) = c.finish().unwrap();
        // 4 s cap: first chunk closes at the silence after ~3.3 s? No — cap is speech_s ≥ 4,
        // first run is 3.3 s (incl. pre-roll), so both runs land in chunk 0 until the cap.
        assert!(!chunks.is_empty());
        let m = &chunks[0].map;
        assert!((m.to_src(0.0) - 9.7).abs() < 0.02, "pre-roll starts 300 ms early: {}", m.to_src(0.0));
        assert!((speech - 6.6).abs() < 0.05, "speech {speech}");
        let total: f64 = chunks.iter().map(|c| c.speech_s).sum();
        assert!((total - speech).abs() < 1e-6);
        std::fs::remove_dir_all(&dir).ok();
    }
}
