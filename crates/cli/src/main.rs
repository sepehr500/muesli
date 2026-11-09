//! `muesli-cli` — scriptable control of muesli-daemon (also used by the Omarchy plugin).

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use muesli_core::ipc::{self, Event, Request, SessionView, Status};
use muesli_core::{config, detect, import, model, store, transcript};

#[derive(Parser)]
#[command(name = "muesli-cli", version, about = "Control muesli, the local meeting notes app")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Current daemon status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stream status/level events (one JSON object per line with --json).
    Watch {
        #[arg(long)]
        json: bool,
    },
    /// Start recording now (manual), or accept a pending prompt.
    Start,
    /// Accept the "Record this call?" prompt.
    Record,
    /// Decline the prompt for this call.
    Ignore,
    /// Stop recording and process it.
    Stop,
    /// Stop recording and throw it away.
    Discard,
    /// Back-to-back meetings: end this meeting and keep recording into a new one.
    Next,
    /// Toggle recording: start if idle/prompting, stop if recording.
    Toggle,
    /// Turn auto-record on/off (persists to config.toml).
    AutoRecord { value: OnOff },
    /// Rename a meeting.
    Rename { id: String, title: String },
    /// Re-run the post-call pipeline.
    Reprocess { id: String },
    /// Re-run note enhancement only.
    Enhance { id: String },
    /// Delete a meeting and its audio.
    Delete { id: String },
    /// Rename a speaker everywhere in a meeting, e.g. `rename-speaker ID "Speaker 2" Barry`.
    /// Same name twice merges two speakers; an empty name resets to "Speaker N".
    RenameSpeaker { id: String, from: String, to: String },
    /// List recent meetings (reads the database directly).
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print a meeting's notes and transcript as markdown.
    Show { id: String },
    /// Open the notes app (optionally at a meeting).
    Open { id: Option<String> },
    /// Store the OpenRouter API key (~/.config/muesli/openrouter.key, mode 600).
    SetKey {
        /// Omit to be prompted (keeps the key out of shell history); `-` reads stdin.
        key: Option<String>,
    },
    /// Import an external recording (e.g. a Discord bot's per-speaker WAVs) from an
    /// `import.json` manifest. Returns once the audio is stored; notes follow in the background.
    /// Idempotent per `source_id`. Exit codes: 0 stored (or already stored), 2 invalid
    /// manifest/audio (don't retry), 1 other error (retry later). See docs/import.md.
    Import {
        /// Absolute path to the manifest.
        #[arg(long)]
        manifest: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Hook scripts in ~/.config/muesli/hooks/<event>.d/ (see docs/hooks.md).
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Debug: what does detection see right now?
    Detect,
}

#[derive(Subcommand)]
enum HooksCmd {
    /// Registered scripts per event.
    List,
    /// Run an event's scripts for a meeting now and wait for the results.
    Run { event: String, id: String },
    /// Recent runs (optionally for one meeting).
    Log {
        id: Option<String>,
        #[arg(short, long, default_value_t = 20)]
        n: usize,
        /// Show each run's full output.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Queue an event's failed runs again.
    Retry { event_id: String },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
    Toggle,
}

fn print_run(r: &store::HookRun, verbose: bool) {
    let mark = match r.status.as_str() {
        "ok" => "✓",
        "failed" => "✗",
        _ if r.attempts > 0 => "↻",
        _ => "…",
    };
    let when = chrono::DateTime::from_timestamp_millis(r.updated_at)
        .map(|t| t.with_timezone(&chrono::Local).format("%b %-d %H:%M").to_string())
        .unwrap_or_default();
    let code = r.exit_code.map(|c| format!(" exit {c}")).unwrap_or_default();
    println!("{mark} {when}  {}/{}  {}{code}  [{}]", r.event, r.script, r.meeting_id, r.event_id);
    let lines: Vec<&str> = r.log.lines().collect();
    let shown = match verbose {
        true => &lines[..],
        false => &lines[lines.len().saturating_sub(3)..],
    };
    shown.iter().for_each(|l| println!("    {l}"));
}

fn hooks_cmd(cmd: HooksCmd) -> Result<()> {
    use muesli_core::hooks::{self, HookEvent};
    match cmd {
        HooksCmd::List => {
            println!("{}", muesli_core::paths::hooks_dir().display());
            for e in HookEvent::ALL {
                let scripts = hooks::scripts(e);
                println!("  {}.d/{}", e.name(), if scripts.is_empty() { "  (none)".to_string() } else { String::new() });
                scripts.iter().for_each(|s| println!("    {s}"));
            }
        }
        HooksCmd::Run { event, id } => {
            let r = call(Request::RunHook { event: event.clone(), id })?;
            let Some(ev) = r["event_id"].as_str().map(str::to_string) else {
                bail!("no scripts registered for {event} (add one to {})", hooks::script_dir(HookEvent::parse(&event).expect("checked by daemon")).display());
            };
            let c = store::open_default()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(330);
            let runs = loop {
                let runs = store::hook_runs_for_event(&c, &ev)?;
                let settled = runs.iter().all(|r| r.status != "pending" || r.attempts > 0);
                if settled || std::time::Instant::now() > deadline {
                    break runs;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            };
            runs.iter().for_each(|r| print_run(r, true));
            if runs.iter().any(|r| r.status != "ok") {
                std::process::exit(1);
            }
        }
        HooksCmd::Log { id, n, verbose } => {
            let c = store::open_default()?;
            let runs = store::recent_hook_runs(&c, id.as_deref(), n)?;
            if runs.is_empty() {
                println!("No hook runs yet.");
            }
            runs.iter().for_each(|r| print_run(r, verbose));
        }
        HooksCmd::Retry { event_id } => {
            let r = call(Request::RetryHook { event_id })?;
            println!("requeued {}", r["requeued"]);
        }
    }
    Ok(())
}

fn call(req: Request) -> Result<serde_json::Value> {
    let r = ipc::request(&req)?;
    match r.ok {
        true => Ok(r.data.unwrap_or(serde_json::Value::Null)),
        false => bail!(r.error.unwrap_or_else(|| "request failed".into())),
    }
}

fn status() -> Result<Status> {
    Ok(serde_json::from_value(call(Request::Status)?)?)
}

fn clock(ms: i64) -> String {
    let secs = ((store::now_ms() - ms).max(0) / 1000) as u64;
    match secs >= 3600 {
        true => format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60),
        false => format!("{:02}:{:02}", secs / 60, secs % 60),
    }
}

fn human_status(s: &Status) -> String {
    let session = match &s.session {
        SessionView::Idle => "Listening for calls".to_string(),
        SessionView::Prompting { app, hint, .. } => {
            format!("Call detected in {} {}— record?", app.label(), hint.as_deref().map(|h| format!("({h}) ")).unwrap_or_default())
        }
        SessionView::Recording { app, hint, started_at, .. } => format!(
            "● Recording {} {}{}",
            clock(*started_at),
            app.map(|a| a.label()).unwrap_or("manually"),
            hint.as_deref().map(|h| format!(" · {h}")).unwrap_or_default()
        ),
    };
    let processing = s.processing.iter().map(|p| {
        let active = p
            .pipeline
            .steps
            .iter()
            .find(|st| st.status == model::StepStatus::Active)
            .map(|st| st.kind.active_label())
            .unwrap_or("Queued");
        format!("\n  {} — {active} ({}/4)", p.title, p.pipeline.done_count())
    });
    let auto = if s.auto_record { "auto-record on" } else { "auto-record off" };
    let key = if s.api_key { "" } else { "\n  ⚠ No OpenRouter key — run: muesli-cli set-key" };
    format!("{session} · {auto}{}{key}", processing.collect::<String>())
}

/// Print one event; false if stdout is gone (reader closed the pipe).
fn print_event(e: &Event, json: bool) -> bool {
    use std::io::Write;
    let line = match json {
        true => serde_json::to_string(e).unwrap_or_default(),
        false => match e {
            Event::Status(s) => human_status(s),
            Event::Levels { me, them } => format!("levels  you {me:.2}  them {them:.2}"),
            Event::MeetingUpdated { id } => format!("updated {id}"),
        },
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").and_then(|_| out.flush()).is_ok()
}

/// `import`: validate locally (so a bad manifest fails even with the daemon down), then hand
/// the manifest path to the daemon, which copies the audio and replies.
fn run_import(manifest: &std::path::Path) -> Result<import::ImportReply> {
    anyhow::ensure!(manifest.is_absolute(), import::invalid(format!("manifest path {} is not absolute", manifest.display())));
    let meta = std::fs::metadata(manifest).map_err(|e| import::invalid(format!("manifest {}: {e}", manifest.display())))?;
    anyhow::ensure!(meta.len() <= import::MAX_MANIFEST_BYTES, import::invalid("manifest is larger than 1 MB"));
    let json = std::fs::read_to_string(manifest).map_err(|e| import::invalid(format!("manifest: {e}")))?;
    import::parse(&json)?.validate(store::now_ms())?;
    Ok(serde_json::from_value(call(Request::Import { manifest: manifest.to_path_buf() })?)?)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hooks { cmd } => hooks_cmd(cmd)?,
        Cmd::Import { manifest, json } => match run_import(&manifest) {
            Ok(r) => match json {
                true => println!("{}", serde_json::to_string(&r)?),
                false => println!(
                    "{} {} ({})",
                    if r.duplicate { "already imported as" } else { "imported" },
                    r.id,
                    r.state.as_str()
                ),
            },
            Err(e) => {
                let msg = format!("{e:#}");
                let retryable = !import::is_invalid(&msg);
                match json {
                    true => println!("{}", serde_json::json!({ "error": msg, "retryable": retryable })),
                    false => eprintln!("error: {msg}"),
                }
                std::process::exit(if retryable { 1 } else { 2 });
            }
        },
        Cmd::Status { json } => {
            let s = status()?;
            match json {
                true => println!("{}", serde_json::to_string(&s)?),
                false => println!("{}", human_status(&s)),
            }
        }
        Cmd::Watch { json } => {
            // Reconnect forever so bar widgets survive daemon restarts.
            loop {
                let mut alive = true;
                let res = ipc::watch(|e| {
                    alive = print_event(&e, json);
                    alive
                });
                if !alive {
                    return Ok(());
                }
                if let Err(e) = res {
                    use std::io::Write;
                    let msg = match json {
                        true => format!(r#"{{"event":"disconnected","data":{{"error":{}}}}}"#, serde_json::to_string(&e.to_string())?),
                        false => format!("{e}; retrying…"),
                    };
                    if writeln!(std::io::stdout().lock(), "{msg}").is_err() {
                        return Ok(());
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        Cmd::Start => drop(call(Request::Start)?),
        Cmd::Record => drop(call(Request::Record)?),
        Cmd::Ignore => drop(call(Request::Ignore)?),
        Cmd::Stop => drop(call(Request::Stop)?),
        Cmd::Discard => drop(call(Request::Discard)?),
        Cmd::Next => drop(call(Request::Next)?),
        Cmd::Toggle => {
            let req = match status()?.session {
                SessionView::Recording { .. } => Request::Stop,
                SessionView::Prompting { .. } => Request::Record,
                SessionView::Idle => Request::Start,
            };
            call(req)?;
        }
        Cmd::AutoRecord { value } => {
            let v = match value {
                OnOff::On => true,
                OnOff::Off => false,
                OnOff::Toggle => !status()?.auto_record,
            };
            call(Request::SetAutoRecord { value: v })?;
            println!("auto-record {}", if v { "on" } else { "off" });
        }
        Cmd::Rename { id, title } => {
            drop(call(Request::Rename { id, title, confirm: true })?)
        }
        Cmd::Reprocess { id } => drop(call(Request::Reprocess { id })?),
        Cmd::Enhance { id } => drop(call(Request::Enhance { id })?),
        Cmd::Delete { id } => drop(call(Request::Delete { id })?),
        Cmd::RenameSpeaker { id, from, to } => drop(call(Request::RenameSpeaker { id, from, to })?),
        Cmd::List { limit, json } => {
            let c = store::open_default()?;
            let ms = store::list_meetings(&c, limit)?;
            match json {
                true => println!("{}", serde_json::to_string(&ms)?),
                false => ms.iter().for_each(|m| {
                    let when = chrono::DateTime::from_timestamp_millis(m.started_at)
                        .map(|t| t.with_timezone(&chrono::Local).format("%a %b %-d %H:%M").to_string())
                        .unwrap_or_default();
                    let dur = m.duration_ms().map(model::human_duration).unwrap_or_else(|| "…".into());
                    println!("{}  {when}  {:<8} {:>9}  {}", m.id, m.app.map(|a| a.label()).unwrap_or("—"), dur, m.display_title());
                }),
            }
        }
        Cmd::Show { id } => {
            let c = store::open_default()?;
            let Some(m) = store::get_meeting(&c, &id)? else { bail!("no meeting {id}") };
            let notes = store::get_notes(&c, &id)?;
            let segs = store::segments(&c, &id)?;
            println!("# {}\n", m.display_title());
            if let Some(e) = &notes.enhanced {
                println!("{}\n", e.to_markdown());
            } else if !notes.user_md.is_empty() {
                println!("{}\n", notes.user_md);
            }
            if !segs.is_empty() {
                println!("## Transcript\n");
                segs.iter().for_each(|s| {
                    let t = s.t_start as u64;
                    println!("[{:02}:{:02}] {}: {}", t / 60, t % 60, transcript::display_speaker(&s.speaker, &m.speaker_names), s.text)
                });
            }
        }
        Cmd::Open { id } => {
            let mut cmd = std::process::Command::new("muesli");
            if let Some(id) = id {
                cmd.args(["--meeting", &id]);
            }
            cmd.spawn()?;
        }
        Cmd::SetKey { key } => {
            let key = match key.as_deref() {
                Some("-") => std::io::read_to_string(std::io::stdin())?,
                Some(k) => k.to_string(),
                None => prompt_hidden("OpenRouter API key: ")?,
            };
            anyhow::ensure!(!key.trim().is_empty(), "no key given");
            config::save_api_key(&key)?;
            let _ = ipc::request(&Request::ReloadConfig);
            println!("saved to {}", muesli_core::paths::key_file().display());
        }
        Cmd::Detect => {
            let dump = std::process::Command::new("pw-dump").output()?.stdout;
            let clients = std::process::Command::new("hyprctl").args(["clients", "-j"]).output()?.stdout;
            let nodes = detect::nodes_from_pw_dump(&serde_json::from_slice(&dump)?);
            let windows = detect::windows_from_hyprctl(&serde_json::from_slice(&clients).unwrap_or_default());
            nodes.iter().filter(|n| n.media_class.starts_with("Stream/Input")).for_each(|n| {
                println!("mic stream {:>4}  {:?} ({:?}) corked={}", n.id, n.app_name, n.binary, n.corked)
            });
            println!("→ {:?}", detect::classify(&nodes, &windows));
        }
    }
    Ok(())
}

/// Read a line from the terminal without echoing it. Fails closed: if echo can't be turned
/// off, nothing is read. Piped stdin (not a terminal) is read as-is.
fn prompt_hidden(prompt: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(std::io::read_to_string(std::io::stdin())?);
    }
    let stty = |args: &[&str]| {
        std::process::Command::new("stty").args(args).stdin(std::process::Stdio::inherit()).output().ok().filter(|o| o.status.success())
    };
    // Save the exact terminal state; restore it on every path, including errors and panics.
    let saved = stty(&["-g"]).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let saved = saved.ok_or_else(|| anyhow::anyhow!("can't read terminal state; use `muesli-cli set-key -` and pipe the key"))?;
    struct Restore(String);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::process::Command::new("stty").arg(&self.0).stdin(std::process::Stdio::inherit()).status();
        }
    }
    let _restore = Restore(saved);
    stty(&["-echo"]).ok_or_else(|| anyhow::anyhow!("can't hide input; use `muesli-cli set-key -` and pipe the key"))?;
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    eprintln!();
    read?;
    Ok(line)
}
