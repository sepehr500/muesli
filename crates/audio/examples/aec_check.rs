//! Measure echo before/after cleaning on two 48 kHz f32 mono WAVs:
//!   cargo run -p muesli-audio --example aec_check -- mic.wav sys.wav
use muesli_audio::{capture, clean, oggopus};
use std::path::Path;

fn read_wav_f32(p: &str) -> Vec<f32> {
    let d = std::fs::read(p).unwrap();
    let i = d.windows(4).position(|w| w == b"data").unwrap();
    let n = u32::from_le_bytes(d[i + 4..i + 8].try_into().unwrap()) as usize;
    d[i + 8..i + 8 + n].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (mic, sys) = (read_wav_f32(&args[1]), read_wav_f32(&args[2]));
    let dir = std::env::temp_dir().join("muesli-aec-check");
    let _ = std::fs::remove_dir_all(&dir);
    let raw = dir.join("raw");
    std::fs::create_dir_all(&raw)?;
    for (name, pcm) in [("mic-000.opus", &mic), ("sys-000.opus", &sys)] {
        let mut w = oggopus::create_file(&raw.join(name), 48_000, 32_000)?;
        w.write(pcm)?;
        w.finish()?;
    }
    std::fs::write(raw.join("recording.json"), serde_json::to_string(&capture::RecordingInfo {
        rate: 48_000, segment_secs: 300, duration_s: mic.len() as f64 / 48_000.0,
        mic_segments: vec!["mic-000.opus".into()], sys_segments: vec!["sys-000.opus".into()], bytes: 0,
    })?)?;
    for aec in [false, true] {
        let dump = dir.join(format!("clean-mic-aec{aec}.opus"));
        let r = clean::clean(&raw, &dir.join(format!("chunks-{aec}")), &clean::CleanOptions {
            echo_cancellation: aec, dump_clean_mic: Some(dump.clone()), ..Default::default()
        })?;
        let m = oggopus::decode_file(&dump, 16_000)?;
        let s = oggopus::decode_file(&raw.join("sys-000.opus"), 16_000)?;
        let skip = 16_000 * 3; // let the canceller converge
        let corr = clean::echo_correlation(&m[skip..], &s[skip..], 16_000 / 5);
        let mic_speech: f64 = r.mic_chunks.iter().map(|c| c.speech_s).sum();
        let sys_speech: f64 = r.sys_chunks.iter().map(|c| c.speech_s).sum();
        println!("aec={aec:<5} applied={} echo-corr={corr:.3} rms={:.5} mic-speech={mic_speech:.1}s sys-speech={sys_speech:.1}s silence={:.1}s of {:.1}s",
            r.echo_cancelled, muesli_audio::dsp::rms(&m[skip..]), r.silence_s, r.duration_s);
    }
    let _ = Path::new("");
    Ok(())
}
