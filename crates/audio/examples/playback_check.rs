//! Dev check: build playback for a recording and drive the player.
//!   cargo run -p muesli-audio --example playback_check -- <raw_dir> <playback_dir>
use muesli_audio::{clean, playback};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (raw, out) = (std::path::Path::new(&args[1]), std::path::Path::new(&args[2]));
    let t = Instant::now();
    let idx = clean::render_playback(raw, out, true)?;
    println!("rendered {:.1}s in {} pieces in {:.2}s", idx.duration_s, idx.segments, t.elapsed().as_secs_f64());
    let p = playback::Player::open(out)?;
    p.play();
    std::thread::sleep(Duration::from_millis(1500));
    println!("after 1.5s: pos {:.2}s playing={}", p.position(), p.is_playing());
    p.seek(60.0);
    std::thread::sleep(Duration::from_millis(1000));
    println!("seek 60 + 1s: pos {:.2}s", p.position());
    p.pause();
    let a = p.position();
    std::thread::sleep(Duration::from_millis(600));
    println!("paused: {:.2}s -> {:.2}s playing={}", a, p.position(), p.is_playing());
    p.seek(idx.duration_s - 0.5);
    p.play();
    std::thread::sleep(Duration::from_millis(1500));
    println!("ran off the end: pos {:.2}s playing={}", p.position(), p.is_playing());
    Ok(())
}
