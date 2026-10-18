//! Talking to muesli-daemon (blocking IPC on worker threads) and OpenRouter.

use gtk::gio;
use muesli_core::ipc::{self, Event, Request, Response, Status};
use muesli_core::openrouter::{Client, Message};
use std::time::Duration;

#[derive(Debug)]
pub enum Msg {
    Status(Status),
    MeetingUpdated(String),
    Disconnected,
}

/// Background thread: initial status, then the event stream; reconnects every few seconds.
/// Level events (8 Hz) are dropped here so the UI thread never wakes for them.
pub fn spawn_watch(tx: async_channel::Sender<Msg>) {
    std::thread::Builder::new()
        .name("muesli-watch".into())
        .spawn(move || {
            loop {
                let status = ipc::request(&Request::Status)
                    .ok()
                    .filter(|r| r.ok)
                    .and_then(|r| r.data)
                    .and_then(|d| serde_json::from_value::<Status>(d).ok());
                if let Some(s) = status {
                    if tx.send_blocking(Msg::Status(s)).is_err() {
                        return;
                    }
                    let _ = ipc::watch(|ev| match ev {
                        Event::Levels { .. } => true,
                        Event::Status(s) => tx.send_blocking(Msg::Status(s)).is_ok(),
                        Event::MeetingUpdated { id } => tx.send_blocking(Msg::MeetingUpdated(id)).is_ok(),
                    });
                }
                if tx.send_blocking(Msg::Disconnected).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
        .expect("spawn watch thread");
}

/// One request off the UI thread. Err(message) when the daemon is down or refuses.
pub async fn request(req: Request) -> Result<serde_json::Value, String> {
    let r: anyhow::Result<Response> = gio::spawn_blocking(move || ipc::request(&req))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("request thread panicked")));
    match r {
        Ok(Response { ok: true, data, .. }) => Ok(data.unwrap_or(serde_json::Value::Null)),
        Ok(Response { error, .. }) => Err(error.unwrap_or_else(|| "request failed".into())),
        Err(e) => Err(e.to_string()),
    }
}

/// Chat completion on a small current-thread runtime in a worker thread.
pub async fn chat(model: String, messages: Vec<Message>) -> Result<String, String> {
    gio::spawn_blocking(move || {
        let cfg = muesli_core::config::Config::load();
        let client = Client::from_config(&cfg).map_err(|e| e.to_string())?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        rt.block_on(client.chat(&model, &messages, false)).map(|r| r.content).map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("chat thread panicked".into()))
}
