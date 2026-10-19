//! Meeting detection: a pure function over a PipeWire node snapshot and the
//! current window list. No I/O here; the daemon feeds snapshots in.

use serde::{Deserialize, Serialize};

/// Which meeting app a call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum App {
    Zoom,
    Discord,
}

impl App {
    pub fn label(self) -> &'static str {
        match self {
            App::Zoom => "Zoom",
            App::Discord => "Discord",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            App::Zoom => "zoom",
            App::Discord => "discord",
        }
    }
}

/// The subset of PipeWire node properties detection cares about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PwNode {
    pub id: u32,
    pub media_class: String,
    pub app_name: Option<String>,
    pub binary: Option<String>,
    pub node_name: Option<String>,
    pub pid: Option<u32>,
    /// `pulse.corked`: a paused PulseAudio stream (e.g. muted preview).
    pub corked: bool,
}

/// A toplevel window, as reported by `hyprctl clients -j`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub class: String,
    pub title: String,
    pub pid: u32,
}

/// A call we believe is happening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub app: App,
    /// Human hint for naming, e.g. "Example server · #general".
    pub hint: Option<String>,
    /// PipeWire node id of the mic stream that triggered this.
    pub node_id: u32,
}

fn lc(s: &Option<String>) -> String {
    s.as_deref().unwrap_or_default().to_ascii_lowercase()
}

/// Binaries that are themselves meeting apps (native clients).
fn native_app(binary: &str, app_name: &str, node_name: &str) -> Option<App> {
    let any = |needles: &[&str]| {
        needles
            .iter()
            .any(|n| binary.contains(n) || app_name.contains(n) || node_name.contains(n))
    };
    if any(&["zoom"]) {
        Some(App::Zoom)
    } else if any(&["discord", "vesktop", "webcord", "webrtc voiceengine"]) {
        Some(App::Discord)
    } else {
        None
    }
}

const BROWSERS: &[&str] = &[
    "chrome", "chromium", "brave", "firefox", "librewolf", "zen", "vivaldi", "opera", "msedge",
    "electron",
];

fn is_browser(binary: &str, app_name: &str) -> bool {
    BROWSERS.iter().any(|b| binary.contains(b) || app_name.contains(b))
}

/// Which meeting app a window looks like, if any.
pub fn window_app(w: &Window) -> Option<App> {
    let class = w.class.to_ascii_lowercase();
    let title = w.title.to_ascii_lowercase();
    if class.contains("zoom") || title.contains("zoom meeting") || title.contains("zoom workplace")
    {
        Some(App::Zoom)
    } else if class.contains("discord") || class.contains("vesktop") || title.ends_with("- discord")
        || title.contains("| discord") || title.starts_with("discord |")
        || title.contains(") discord |")
    {
        Some(App::Discord)
    } else {
        None
    }
}

/// Extract a naming hint from a meeting window title.
///
/// Discord web:  "(80) Discord | #general | Example server" → "Example server · #general"
/// Discord app:  "#general | Server - Discord"                → "Server · #general"
/// Zoom:         "Zoom Meeting" → None, other titles kept as-is.
pub fn title_hint(app: App, title: &str) -> Option<String> {
    let t = title.trim();
    match app {
        App::Discord => {
            // Strip unread counter like "(80) ".
            let t = match t.strip_prefix('(') {
                Some(rest) => rest.split_once(") ").map(|(_, r)| r).unwrap_or(t),
                None => t,
            };
            let t = t.trim_end_matches("- Discord").trim();
            let parts: Vec<&str> = t
                .split('|')
                .map(str::trim)
                .filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case("discord"))
                .collect();
            let channel = parts.iter().find(|p| p.starts_with('#') || p.starts_with('@'));
            let server = parts.iter().find(|p| !p.starts_with('#') && !p.starts_with('@'));
            match (server, channel) {
                (Some(s), Some(c)) => Some(format!("{s} · {c}")),
                (Some(s), None) => Some(s.to_string()),
                (None, Some(c)) => Some(c.to_string()),
                _ => None,
            }
        }
        App::Zoom => {
            let l = t.to_ascii_lowercase();
            if l.is_empty() || l == "zoom" || l == "zoom meeting" || l.starts_with("zoom workplace")
            {
                None
            } else {
                Some(t.trim_end_matches("- Zoom").trim().to_string())
            }
        }
    }
}

fn best_window(app: App, windows: &[Window]) -> Option<&Window> {
    // Prefer windows whose title yields a hint.
    let mut ws: Vec<&Window> = windows.iter().filter(|w| window_app(w) == Some(app)).collect();
    ws.sort_by_key(|w| title_hint(app, &w.title).is_none());
    ws.into_iter().next()
}

/// Is this node a live (uncorked) capture stream from an application?
fn is_live_capture(n: &PwNode) -> bool {
    n.media_class == "Stream/Input/Audio" && !n.corked
}

/// Decide whether a call is happening.
///
/// Rules:
/// 1. A native meeting client (Zoom, Discord, Vesktop) has a live mic stream → call.
/// 2. A browser has a live mic stream AND a meeting-app window exists → call
///    (Discord/Zoom running as a web app; the audio service pid differs from the window pid,
///    so we match on window presence rather than pid).
pub fn classify(nodes: &[PwNode], windows: &[Window]) -> Option<Candidate> {
    classify_for(nodes, windows, |_| true)
}

/// `classify`, considering only the apps `watch` allows (the user can turn apps off).
pub fn classify_for(nodes: &[PwNode], windows: &[Window], watch: impl Fn(App) -> bool) -> Option<Candidate> {
    let hint_for = |app: App| best_window(app, windows).and_then(|w| title_hint(app, &w.title));

    let native = nodes.iter().filter(|n| is_live_capture(n)).find_map(|n| {
        native_app(&lc(&n.binary), &lc(&n.app_name), &lc(&n.node_name)).filter(|a| watch(*a)).map(|app| (app, n.id))
    });
    if let Some((app, node_id)) = native {
        return Some(Candidate { app, hint: hint_for(app), node_id });
    }

    let browser_mic =
        nodes.iter().find(|n| is_live_capture(n) && is_browser(&lc(&n.binary), &lc(&n.app_name)))?;
    // Zoom first: if both windows exist, a Zoom web call is the likelier mic user.
    [App::Zoom, App::Discord].into_iter().filter(|a| watch(*a)).find_map(|app| {
        best_window(app, windows)
            .map(|w| Candidate { app, hint: title_hint(app, &w.title), node_id: browser_mic.id })
    })
}

/// Parse `pw-dump` JSON into nodes (used for fixtures and the CLI `detect` debug command).
pub fn nodes_from_pw_dump(json: &serde_json::Value) -> Vec<PwNode> {
    json.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["type"] == "PipeWire:Interface:Node")
        .map(|o| {
            let p = &o["info"]["props"];
            let s = |k: &str| p[k].as_str().map(str::to_owned);
            PwNode {
                id: o["id"].as_u64().unwrap_or_default() as u32,
                media_class: s("media.class").unwrap_or_default(),
                app_name: s("application.name"),
                binary: s("application.process.binary"),
                node_name: s("node.name"),
                pid: p["application.process.id"].as_u64().map(|v| v as u32),
                corked: p["pulse.corked"].as_bool().unwrap_or(false),
            }
        })
        .collect()
}

/// Parse `hyprctl clients -j` JSON into windows.
pub fn windows_from_hyprctl(json: &serde_json::Value) -> Vec<Window> {
    json.as_array()
        .into_iter()
        .flatten()
        .map(|c| Window {
            class: c["class"].as_str().unwrap_or_default().to_owned(),
            title: c["title"].as_str().unwrap_or_default().to_owned(),
            pid: c["pid"].as_u64().unwrap_or_default() as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> serde_json::Value {
        let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn discord_web_app_in_call_is_detected() {
        let nodes = nodes_from_pw_dump(&fixture("discord-active.json"));
        let windows = windows_from_hyprctl(&fixture("discord-active.clients.json"));
        let c = classify(&nodes, &windows).expect("call");
        assert_eq!(c.app, App::Discord);
        assert_eq!(c.hint.as_deref(), Some("Example server · #general"));
    }

    #[test]
    fn browser_mic_without_meeting_window_is_ignored() {
        let nodes = nodes_from_pw_dump(&fixture("discord-active.json"));
        let windows = vec![Window { class: "google-chrome".into(), title: "Docs".into(), pid: 1 }];
        assert_eq!(classify(&nodes, &windows), None);
    }

    #[test]
    fn turned_off_apps_are_skipped() {
        let nodes = nodes_from_pw_dump(&fixture("discord-active.json"));
        let mut windows = windows_from_hyprctl(&fixture("discord-active.clients.json"));
        assert_eq!(classify_for(&nodes, &windows, |a| a != App::Discord), None);
        // A Zoom tab would normally win; with Zoom off, the Discord call is still found.
        windows.push(Window { class: "google-chrome".into(), title: "Zoom Meeting - Google Chrome".into(), pid: 2 });
        assert_eq!(classify(&nodes, &windows).map(|c| c.app), Some(App::Zoom));
        assert_eq!(classify_for(&nodes, &windows, |a| a != App::Zoom).map(|c| c.app), Some(App::Discord));
    }

    #[test]
    fn corked_stream_is_ignored() {
        let nodes = vec![PwNode {
            id: 1,
            media_class: "Stream/Input/Audio".into(),
            binary: Some("zoom".into()),
            corked: true,
            ..Default::default()
        }];
        assert_eq!(classify(&nodes, &[]), None);
    }

    #[test]
    fn native_zoom_is_detected_without_window() {
        let nodes = vec![PwNode {
            id: 7,
            media_class: "Stream/Input/Audio".into(),
            app_name: Some("ZOOM VoiceEngine".into()),
            binary: Some("zoom".into()),
            ..Default::default()
        }];
        let c = classify(&nodes, &[]).unwrap();
        assert_eq!((c.app, c.node_id, c.hint), (App::Zoom, 7, None));
    }

    #[test]
    fn output_streams_do_not_count() {
        let nodes = vec![PwNode {
            id: 3,
            media_class: "Stream/Output/Audio".into(),
            binary: Some("Discord".into()),
            ..Default::default()
        }];
        assert_eq!(classify(&nodes, &[]), None);
    }

    #[test]
    fn hints() {
        assert_eq!(
            title_hint(App::Discord, "#planning | Hiking crew - Discord").as_deref(),
            Some("Hiking crew · #planning")
        );
        assert_eq!(title_hint(App::Zoom, "Zoom Meeting"), None);
        assert_eq!(title_hint(App::Zoom, "Q4 roadmap - Zoom").as_deref(), Some("Q4 roadmap"));
    }
}
