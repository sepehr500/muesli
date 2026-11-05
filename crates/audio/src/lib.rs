//! Audio for muesli: PipeWire capture → Ogg Opus segments; post-call cleaning
//! (echo cancellation, VAD, speech-only chunks for upload); importing external recordings.

pub mod capture;
pub mod clean;
pub mod dsp;
pub mod import;
pub mod oggopus;
pub mod opus;
pub mod playback;
