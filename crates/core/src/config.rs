//! `~/.config/muesli/config.toml`. Every field has a default; a missing file is fine.

use crate::detect::App;
use crate::paths;
use crate::session::Policy;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub openrouter: OpenRouter,
    pub recording: Recording,
    pub apps: Apps,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouter {
    pub base_url: String,
    /// Tried in order; next one on timeout/error.
    pub stt_models: Vec<String>,
    /// Titles, speaker linking, Q&A.
    pub chat_model: String,
    /// Note enhancement.
    pub enhance_model: String,
    /// Route only to zero-data-retention endpoints.
    pub zdr: bool,
    /// Optional ISO-639-1 language hint for transcription.
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recording {
    pub auto_record: bool,
    /// Seconds a call must be present before recording/asking.
    pub start_debounce_s: u32,
    /// Seconds a call must be gone before auto-stop.
    pub end_grace_s: u32,
    /// 0 = keep audio forever.
    pub audio_retention_days: u32,
    pub echo_cancellation: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Apps {
    pub zoom: AppConfig,
    pub discord: AppConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub enabled: bool,
    /// Overrides `recording.auto_record` for this app.
    pub auto_record: Option<bool>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig { enabled: true, auto_record: None }
    }
}

impl Default for OpenRouter {
    fn default() -> Self {
        OpenRouter {
            base_url: "https://openrouter.ai/api/v1".into(),
            stt_models: vec![
                "elevenlabs/scribe-v2".into(),
                "assemblyai/universal-3-6-pro".into(),
                "fish-audio/transcribe-1-pro".into(),
            ],
            chat_model: "google/gemini-2.5-flash".into(),
            enhance_model: "anthropic/claude-sonnet-4.5".into(),
            zdr: false,
            language: None,
        }
    }
}

impl Default for Recording {
    fn default() -> Self {
        Recording {
            auto_record: true,
            start_debounce_s: 5,
            end_grace_s: 20,
            audio_retention_days: 0,
            echo_cancellation: true,
        }
    }
}


impl Config {
    pub fn load() -> Self {
        Self::load_from(&paths::config_file())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| {
                toml::from_str(&s)
                    .map_err(|e| tracing::warn!("ignoring invalid {}: {e}", path.display()))
                    .ok()
            })
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = paths::config_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn app(&self, app: App) -> &AppConfig {
        match app {
            App::Zoom => &self.apps.zoom,
            App::Discord => &self.apps.discord,
        }
    }

    pub fn policy(&self) -> Policy {
        let per_app = [App::Zoom, App::Discord]
            .into_iter()
            .filter_map(|a| self.app(a).auto_record.map(|v| (a, v)))
            .collect();
        Policy {
            auto_record: self.recording.auto_record,
            per_app,
            start_debounce_ms: self.recording.start_debounce_s as i64 * 1000,
            end_grace_ms: self.recording.end_grace_s as i64 * 1000,
        }
    }
}

/// API key: `OPENROUTER_API_KEY`, else `~/.config/muesli/openrouter.key` (mode 600).
pub fn api_key() -> Option<String> {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| std::fs::read_to_string(paths::key_file()).ok())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

pub fn save_api_key(key: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = paths::key_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, key.trim())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_toml_uses_defaults() {
        let c: Config = toml::from_str("[recording]\nauto_record = false\n[apps.discord]\nauto_record = true").unwrap();
        assert!(!c.recording.auto_record);
        assert_eq!(c.recording.end_grace_s, 20);
        assert_eq!(c.openrouter.stt_models[0], "elevenlabs/scribe-v2");
        let p = c.policy();
        assert!(p.auto_for(App::Discord));
        assert!(!p.auto_for(App::Zoom));
    }

    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&s).unwrap(), c);
    }
}
