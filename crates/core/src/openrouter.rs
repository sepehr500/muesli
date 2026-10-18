//! Minimal async OpenRouter client: chat completions + speech-to-text.

use crate::transcript::{RawSegment, RawWord};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Message { role: "system".into(), content: c.into() }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Message { role: "user".into(), content: c.into() }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Message { role: "assistant".into(), content: c.into() }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    key: String,
    zdr: bool,
}

#[derive(Debug, Clone)]
pub struct ChatReply {
    pub content: String,
    pub cost: f64,
}

#[derive(Debug, Clone)]
pub struct SttReply {
    pub model: String,
    pub text: String,
    pub segments: Vec<RawSegment>,
    pub words: Vec<RawWord>,
    pub cost: f64,
}

/// Provider-specific STT options (sent under `provider.options`): turn on speaker
/// diarization and turn off sound-event tags where the provider supports it.
pub fn stt_provider_options(model: &str) -> Option<serde_json::Value> {
    match model.split('/').next()? {
        "elevenlabs" => Some(json!({ "elevenlabs": { "diarize": true, "tag_audio_events": false } })),
        "assemblyai" => Some(json!({ "assemblyai": { "speaker_labels": true } })),
        _ => None,
    }
}

/// Errors worth trying the next STT model for.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("{model}: {msg}")]
    Retryable { model: String, msg: String },
    #[error("{0}")]
    Fatal(String),
}

impl Client {
    pub fn new(cfg: &crate::config::OpenRouter, key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Client { http, base_url: cfg.base_url.trim_end_matches('/').to_string(), key, zdr: cfg.zdr })
    }

    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let key = crate::config::api_key().ok_or_else(|| anyhow!("no OpenRouter API key configured"))?;
        Self::new(&cfg.openrouter, key)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.key)
            .header("HTTP-Referer", "https://github.com/muesli-notes")
            .header("X-Title", "Muesli")
    }

    pub async fn chat(&self, model: &str, messages: &[Message], json_mode: bool) -> Result<ChatReply> {
        let mut body = json!({ "model": model, "messages": messages, "usage": { "include": true } });
        if json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }
        if self.zdr {
            body["provider"] = json!({ "zdr": true });
        }
        let resp = self.post("/chat/completions").json(&body).send().await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("chat: invalid JSON")?;
        if !status.is_success() || v.get("error").is_some() {
            bail!("chat {model} failed ({status}): {}", v["error"]["message"].as_str().unwrap_or("unknown error"));
        }
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
        Ok(ChatReply { content, cost: v["usage"]["cost"].as_f64().unwrap_or(0.0) })
    }

    /// One STT attempt. `verbose` requests segments with timestamps.
    async fn stt_once(
        &self,
        model: &str,
        audio: &[u8],
        format: &str,
        language: Option<&str>,
        verbose: bool,
    ) -> std::result::Result<SttReply, SttError> {
        let retry = |msg: String| SttError::Retryable { model: model.to_string(), msg };
        let mut body = json!({
            "model": model,
            "input_audio": { "data": base64::engine::general_purpose::STANDARD.encode(audio), "format": format },
        });
        if verbose {
            body["response_format"] = json!("verbose_json");
            body["timestamp_granularities"] = json!(["segment", "word"]);
        }
        if let Some(l) = language {
            body["language"] = json!(l);
        }
        if self.zdr {
            body["provider"]["zdr"] = json!(true);
        }
        if let Some(opts) = stt_provider_options(model) {
            body["provider"]["options"] = opts;
        }
        let resp = self
            .post("/audio/transcriptions")
            .timeout(Duration::from_secs(120))
            .json(&body)
            .send()
            .await
            .map_err(|e| retry(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| retry(e.to_string()))?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(json!({ "raw": text }));
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::PAYMENT_REQUIRED {
            return Err(SttError::Fatal(format!("OpenRouter rejected the request ({status})")));
        }
        if !status.is_success() || v.get("error").is_some() {
            let msg = v["error"]["message"].as_str().map(str::to_owned).unwrap_or_else(|| text.chars().take(300).collect());
            return Err(retry(format!("{status}: {msg}")));
        }
        let segments: Vec<RawSegment> =
            serde_json::from_value(v["segments"].clone()).unwrap_or_default();
        let words: Vec<RawWord> = serde_json::from_value(v["words"].clone()).unwrap_or_default();
        Ok(SttReply {
            model: model.to_string(),
            text: v["text"].as_str().unwrap_or_default().to_string(),
            segments,
            words,
            cost: v["usage"]["cost"].as_f64().unwrap_or(0.0),
        })
    }

    /// Try each model in order; within a model, prefer verbose_json and fall back to plain json
    /// (some providers reject verbose_json with a 400).
    pub async fn transcribe(
        &self,
        models: &[String],
        audio: &[u8],
        format: &str,
        language: Option<&str>,
    ) -> Result<SttReply> {
        let mut errors = vec![];
        for model in models {
            for verbose in [true, false] {
                match self.stt_once(model, audio, format, language, verbose).await {
                    Ok(r) => return Ok(r),
                    Err(SttError::Fatal(m)) => bail!(m),
                    Err(e) => {
                        tracing::warn!("transcription attempt failed: {e}");
                        errors.push(e.to_string());
                    }
                }
            }
        }
        bail!("all transcription models failed: {}", errors.join("; "))
    }
}
