//! Desktop notifications via `notify-send` (libnotify ≥ 0.8), with action buttons.
//! Chosen actions are forwarded to the daemon as commands.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub struct Notifier {
    /// The live "session" notification (recording / ask), replaced in place.
    session_id: Arc<Mutex<Option<u32>>>,
    actions: UnboundedSender<String>,
}

impl Notifier {
    pub fn new(actions: UnboundedSender<String>) -> Self {
        Notifier { session_id: Arc::default(), actions }
    }

    /// Show (or replace) the session notification.
    pub fn session(&self, summary: &str, body: &str, actions: &[(&str, &str)], persistent: bool) {
        let replace = *self.session_id.lock().expect("lock");
        self.spawn(summary, body, actions, replace, persistent, true);
    }

    /// One-off notification (e.g. "Notes ready").
    pub fn info(&self, summary: &str, body: &str, actions: &[(&str, &str)]) {
        self.spawn(summary, body, actions, None, false, false);
    }

    pub fn dismiss(&self) {
        if let Some(id) = self.session_id.lock().expect("lock").take() {
            tokio::spawn(async move {
                let _ = tokio::process::Command::new("gdbus")
                    .args([
                        "call", "--session", "--dest", "org.freedesktop.Notifications",
                        "--object-path", "/org/freedesktop/Notifications",
                        "--method", "org.freedesktop.Notifications.CloseNotification",
                        &id.to_string(),
                    ])
                    .stdout(Stdio::null())
                    .status()
                    .await;
            });
        }
    }

    fn spawn(&self, summary: &str, body: &str, actions: &[(&str, &str)], replace: Option<u32>, persistent: bool, session: bool) {
        let mut cmd = tokio::process::Command::new("notify-send");
        cmd.args(["-a", "muesli", "-i", "audio-input-microphone", "-p"]);
        if let Some(id) = replace {
            cmd.args(["-r", &id.to_string()]);
        }
        if persistent {
            cmd.args(["-u", "critical"]);
        }
        actions.iter().for_each(|(name, label)| {
            cmd.arg(format!("--action={name}={label}"));
        });
        cmd.arg(summary).arg(body).stdout(Stdio::piped()).stderr(Stdio::null());
        let (slot, tx) = (self.session_id.clone(), self.actions.clone());
        tokio::spawn(async move {
            let Ok(mut child) = cmd.spawn() else {
                tracing::warn!("notify-send not available");
                return;
            };
            let Some(out) = child.stdout.take() else { return };
            let mut lines = BufReader::new(out).lines();
            if let Ok(Some(first)) = lines.next_line().await
                && let (true, Ok(id)) = (session, first.trim().parse::<u32>()) {
                    *slot.lock().expect("lock") = Some(id);
                }
            while let Ok(Some(action)) = lines.next_line().await {
                let a = action.trim();
                if !a.is_empty() {
                    let _ = tx.send(a.to_string());
                }
            }
            let _ = child.wait().await;
        });
    }
}
