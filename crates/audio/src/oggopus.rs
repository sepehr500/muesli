//! Ogg Opus (RFC 7845) mono writer and reader.
//!
//! The writer ends a page about once a second and flushes, so a crash loses at most ~1 s.
//! Ogg pages are self-delimiting: a truncated file still decodes up to the last full page.

use crate::opus::{Decoder, Encoder};
use anyhow::{Context, Result};
use ogg::reading::PacketReader;
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;

const PAGE_EVERY: u32 = 50; // packets (20 ms each) → 1 s pages

fn opus_head(pre_skip: u16, input_rate: u32) -> Vec<u8> {
    [
        b"OpusHead".as_slice(),
        &[1, 1], // version, channels
        &pre_skip.to_le_bytes(),
        &input_rate.to_le_bytes(),
        &0i16.to_le_bytes(), // output gain
        &[0],                // mapping family
    ]
    .concat()
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"muesli";
    [b"OpusTags".as_slice(), &(vendor.len() as u32).to_le_bytes(), vendor, &0u32.to_le_bytes()].concat()
}

/// Streaming mono Ogg Opus encoder over any `Write`.
pub struct OggOpusWriter<W: Write> {
    pw: PacketWriter<'static, W>,
    enc: Encoder,
    serial: u32,
    /// Pending samples < one frame.
    buf: Vec<f32>,
    /// Granule position in 48 kHz units, including pre-skip.
    granule: u64,
    packets: u32,
    samples_in: u64,
}

impl<W: Write> OggOpusWriter<W> {
    pub fn new(w: W, rate: u32, bitrate: i32) -> Result<Self> {
        let enc = Encoder::new(rate, bitrate)?;
        let pre_skip = enc.lookahead as u64 * 48_000 / rate as u64;
        let serial = std::process::id() ^ (rate << 8) ^ 0x6d75;
        let mut pw = PacketWriter::new(w);
        pw.write_packet(opus_head(pre_skip as u16, rate), serial, PacketWriteEndInfo::EndPage, 0)?;
        pw.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;
        Ok(OggOpusWriter { pw, enc, serial, buf: vec![], granule: pre_skip, packets: 0, samples_in: 0 })
    }

    pub fn rate(&self) -> u32 {
        self.enc.rate
    }

    /// Samples written so far (at the input rate).
    pub fn samples(&self) -> u64 {
        self.samples_in
    }

    pub fn write(&mut self, pcm: &[f32]) -> Result<()> {
        self.samples_in += pcm.len() as u64;
        self.buf.extend_from_slice(pcm);
        let n = self.enc.frame_len();
        while self.buf.len() >= n {
            let frame: Vec<f32> = self.buf.drain(..n).collect();
            self.emit(&frame, false)?;
        }
        Ok(())
    }

    fn emit(&mut self, frame: &[f32], last: bool) -> Result<()> {
        let packet = self.enc.encode(frame)?;
        self.granule += (frame.len() as u64) * 48_000 / self.enc.rate as u64;
        self.packets += 1;
        let info = match (last, self.packets.is_multiple_of(PAGE_EVERY)) {
            (true, _) => PacketWriteEndInfo::EndStream,
            (false, true) => PacketWriteEndInfo::EndPage,
            (false, false) => PacketWriteEndInfo::NormalPacket,
        };
        self.pw.write_packet(packet, self.serial, info, self.granule)?;
        if info != PacketWriteEndInfo::NormalPacket {
            self.pw.inner_mut().flush()?;
        }
        Ok(())
    }

    /// Pad the last partial frame with silence, end the stream, return the writer.
    pub fn finish(mut self) -> Result<W> {
        let n = self.enc.frame_len();
        let mut last: Vec<f32> = std::mem::take(&mut self.buf);
        last.resize(n, 0.0);
        self.emit(&last, true)?;
        let mut w = self.pw.into_inner();
        w.flush()?;
        Ok(w)
    }
}

pub fn create_file(path: &Path, rate: u32, bitrate: i32) -> Result<OggOpusWriter<BufWriter<File>>> {
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    OggOpusWriter::new(BufWriter::new(f), rate, bitrate)
}

/// Encode a whole buffer to Ogg Opus bytes (for uploads).
pub fn encode_to_vec(pcm: &[f32], rate: u32, bitrate: i32) -> Result<Vec<u8>> {
    let mut w = OggOpusWriter::new(Vec::new(), rate, bitrate)?;
    w.write(pcm)?;
    w.finish()
}

/// Decode an Ogg Opus stream to mono f32 at `rate` (8/12/16/24/48 kHz), dropping pre-skip.
/// Tolerates truncated files (returns what decoded cleanly).
pub fn decode<R: Read + Seek>(r: R, rate: u32) -> Result<Vec<f32>> {
    let mut pr = PacketReader::new(r);
    let mut dec = Decoder::new(rate)?;
    let mut out = vec![];
    let mut pre_skip = 0usize;
    let mut idx = 0;
    loop {
        let pkt = match pr.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("ogg read stopped early: {e}");
                break;
            }
        };
        match idx {
            0 => {
                if pkt.data.len() >= 12 && &pkt.data[..8] == b"OpusHead" {
                    let ps = u16::from_le_bytes([pkt.data[10], pkt.data[11]]) as usize;
                    pre_skip = ps * rate as usize / 48_000;
                }
            }
            1 => {} // OpusTags
            _ => out.extend(dec.decode(&pkt.data)?),
        }
        idx += 1;
    }
    Ok(out.split_off(pre_skip.min(out.len())))
}

pub fn decode_file(path: &Path, rate: u32) -> Result<Vec<f32>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    decode(std::io::BufReader::new(f), rate)
}

/// 16-bit PCM WAV bytes (fallback upload format).
pub fn wav_bytes(pcm: &[f32], rate: u32) -> Vec<u8> {
    let data: Vec<u8> = pcm
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let len = data.len() as u32;
    [
        b"RIFF".as_slice(),
        &(36 + len).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &rate.to_le_bytes(),
        &(rate * 2).to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &len.to_le_bytes(),
        &data,
    ]
    .concat()
}

pub fn decode_bytes(bytes: &[u8], rate: u32) -> Result<Vec<f32>> {
    decode(Cursor::new(bytes), rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f32, hz: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin() * 0.3)
            .collect()
    }

    #[test]
    fn roundtrip_preserves_length_and_energy() {
        let pcm = tone(16_000, 2.0, 440.0);
        let bytes = encode_to_vec(&pcm, 16_000, 24_000).unwrap();
        assert!(bytes.len() < 10_000, "2 s at 24 kbps should be ~6 KB, got {}", bytes.len());
        let back = decode_bytes(&bytes, 16_000).unwrap();
        // Length within one frame (padding at the end).
        assert!((back.len() as i64 - pcm.len() as i64).abs() <= 320, "{} vs {}", back.len(), pcm.len());
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let (a, b) = (rms(&pcm[1600..30000]), rms(&back[1600..30000]));
        assert!((a - b).abs() / a < 0.2, "rms {a} vs {b}");
    }

    #[test]
    fn truncated_file_still_decodes() {
        let pcm = tone(48_000, 3.0, 300.0);
        let bytes = encode_to_vec(&pcm, 48_000, 32_000).unwrap();
        let cut = &bytes[..bytes.len() * 2 / 3];
        let back = decode_bytes(cut, 48_000).unwrap();
        assert!(back.len() > 48_000, "decoded {} samples", back.len());
    }

    #[test]
    fn wav_header() {
        let w = wav_bytes(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(w.len(), 44 + 6);
    }
}
