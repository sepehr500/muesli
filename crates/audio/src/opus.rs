//! Thin safe wrapper over system libopus (mono, float).

use anyhow::{Result, bail};
use opusic_sys as ffi;
use std::os::raw::c_int;

pub struct Encoder {
    st: *mut ffi::OpusEncoder,
    pub rate: u32,
    /// Encoder lookahead in samples at `rate` (Ogg pre-skip is this at 48 kHz).
    pub lookahead: u32,
}

// The encoder state is only touched through &mut self.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(rate: u32, bitrate: i32) -> Result<Self> {
        let mut err: c_int = 0;
        let st = unsafe { ffi::opus_encoder_create(rate as i32, 1, ffi::OPUS_APPLICATION_VOIP, &mut err) };
        if st.is_null() || err != ffi::OPUS_OK {
            bail!("opus_encoder_create failed: {err}");
        }
        let mut lookahead: i32 = 0;
        unsafe {
            ffi::opus_encoder_ctl(st, ffi::OPUS_SET_BITRATE_REQUEST, bitrate as c_int);
            ffi::opus_encoder_ctl(st, ffi::OPUS_SET_SIGNAL_REQUEST, ffi::OPUS_SIGNAL_VOICE);
            ffi::opus_encoder_ctl(st, ffi::OPUS_SET_COMPLEXITY_REQUEST, 5 as c_int);
            ffi::opus_encoder_ctl(st, ffi::OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead as *mut i32);
        }
        Ok(Encoder { st, rate, lookahead: lookahead.max(0) as u32 })
    }

    /// Samples per 20 ms frame.
    pub fn frame_len(&self) -> usize {
        self.rate as usize / 50
    }

    /// Encode exactly one 20 ms frame.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        debug_assert_eq!(pcm.len(), self.frame_len());
        let mut out = vec![0u8; 1500];
        let n = unsafe {
            ffi::opus_encode_float(self.st, pcm.as_ptr(), pcm.len() as c_int, out.as_mut_ptr(), out.len() as i32)
        };
        if n < 0 {
            bail!("opus_encode_float failed: {n}");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.st) }
    }
}

pub struct Decoder {
    st: *mut ffi::OpusDecoder,
    pub rate: u32,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(rate: u32) -> Result<Self> {
        let mut err: c_int = 0;
        let st = unsafe { ffi::opus_decoder_create(rate as i32, 1, &mut err) };
        if st.is_null() || err != ffi::OPUS_OK {
            bail!("opus_decoder_create failed: {err}");
        }
        Ok(Decoder { st, rate })
    }

    /// Decode one packet (up to 120 ms).
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        let max = self.rate as usize * 120 / 1000;
        let mut out = vec![0f32; max];
        let n = unsafe {
            ffi::opus_decode_float(self.st, packet.as_ptr(), packet.len() as i32, out.as_mut_ptr(), max as c_int, 0)
        };
        if n < 0 {
            bail!("opus_decode_float failed: {n}");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.st) }
    }
}
