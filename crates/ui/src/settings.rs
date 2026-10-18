//! Settings: an `adw::PreferencesDialog` over `config.toml` (+ the API key file).

use crate::daemon;
use crate::vm;
use adw::prelude::*;
use gtk::glib;
use muesli_core::config::{self, AppConfig, Config};
use muesli_core::ipc::Request;

/// Option<bool> per-app override ⇄ combo index.
fn override_index(v: Option<bool>) -> u32 {
    match v {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    }
}

fn override_value(i: u32) -> Option<bool> {
    match i {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

pub fn reload_daemon() {
    glib::spawn_future_local(async {
        let _ = daemon::request(Request::ReloadConfig).await;
    });
}

struct AppRows {
    enabled: adw::SwitchRow,
    auto: adw::ComboRow,
}

impl AppRows {
    fn new(group: &adw::PreferencesGroup, label: &str, cfg: &AppConfig) -> Self {
        let enabled = adw::SwitchRow::builder()
            .title(format!("Detect {label} calls"))
            .active(cfg.enabled)
            .build();
        let auto = adw::ComboRow::builder()
            .title("Auto-record")
            .subtitle("Override the default for this app")
            .model(&gtk::StringList::new(&["Use default", "Always record", "Ask first"]))
            .selected(override_index(cfg.auto_record))
            .build();
        group.add(&enabled);
        group.add(&auto);
        AppRows { enabled, auto }
    }

    fn read(&self) -> AppConfig {
        AppConfig { enabled: self.enabled.is_active(), auto_record: override_value(self.auto.selected()) }
    }
}

pub fn show(parent: &impl IsA<gtk::Widget>, on_saved: impl Fn() + 'static) {
    let cfg = Config::load();
    let dlg = adw::PreferencesDialog::new();
    dlg.set_title("Settings");
    dlg.set_search_enabled(false);

    // General
    let general = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").build();
    let key_group = adw::PreferencesGroup::builder()
        .title("OpenRouter")
        .description(match config::api_key().is_some() {
            true => "A key is saved. Paste a new one to replace it.",
            false => "Needed for transcripts, notes and questions. Create one at openrouter.ai/keys.",
        })
        .build();
    let key = adw::PasswordEntryRow::builder().title("API key").show_apply_button(true).build();
    key.connect_apply({
        let key_group = key_group.clone();
        move |row| {
            let text = row.text().to_string();
            match config::save_api_key(&text) {
                Ok(()) => {
                    key_group.set_description(Some("Key saved."));
                    row.set_text("");
                    reload_daemon();
                }
                Err(e) => key_group.set_description(Some(&format!("Couldn't save the key: {e}"))),
            }
        }
    });
    key_group.add(&key);
    general.add(&key_group);

    let rec_group = adw::PreferencesGroup::builder().title("Recording").build();
    let auto = adw::SwitchRow::builder()
        .title("Auto-record calls")
        .subtitle("Start recording when a call is detected, instead of asking")
        .active(cfg.recording.auto_record)
        .build();
    auto.connect_active_notify(|row| {
        let value = row.is_active();
        glib::spawn_future_local(async move {
            let _ = daemon::request(Request::SetAutoRecord { value }).await;
        });
    });
    let aec = adw::SwitchRow::builder()
        .title("Echo cancellation")
        .subtitle("Remove what your speakers played from the mic track")
        .active(cfg.recording.echo_cancellation)
        .build();
    let keep = adw::SpinRow::with_range(0.0, 3650.0, 1.0);
    keep.set_title("Keep audio for");
    keep.set_subtitle("Days. 0 keeps recordings forever");
    keep.set_value(cfg.recording.audio_retention_days as f64);
    [auto.upcast_ref::<gtk::Widget>(), aec.upcast_ref(), keep.upcast_ref()].iter().for_each(|w| rec_group.add(*w));
    general.add(&rec_group);

    // Apps
    let apps = adw::PreferencesPage::builder().title("Apps").icon_name("video-display-symbolic").build();
    let zoom_group = adw::PreferencesGroup::builder().title("Zoom").build();
    let zoom = AppRows::new(&zoom_group, "Zoom", &cfg.apps.zoom);
    let discord_group = adw::PreferencesGroup::builder().title("Discord").build();
    let discord = AppRows::new(&discord_group, "Discord", &cfg.apps.discord);
    apps.add(&zoom_group);
    apps.add(&discord_group);

    // Models
    let models = adw::PreferencesPage::builder().title("Models").icon_name("applications-science-symbolic").build();
    let mgroup = adw::PreferencesGroup::builder()
        .title("Models")
        .description("Any OpenRouter model id, e.g. anthropic/claude-sonnet-5.5")
        .build();
    let entry = |title: &str, text: &str| {
        let r = adw::EntryRow::builder().title(title).text(text).build();
        r.add_css_class("mono-row");
        mgroup.add(&r);
        r
    };
    let chat = entry("Questions and titles", &cfg.openrouter.chat_model);
    let enhance = entry("Note enhancement", &cfg.openrouter.enhance_model);
    let stt = entry("Transcription (comma separated, tried in order)", &cfg.openrouter.stt_models.join(", "));
    models.add(&mgroup);

    dlg.add(&general);
    dlg.add(&apps);
    dlg.add(&models);

    dlg.connect_closed(move |_| {
        // Start from the file on disk: the daemon may have written auto_record meanwhile.
        let before = Config::load();
        let mut next = before.clone();
        next.recording.auto_record = auto.is_active();
        next.recording.echo_cancellation = aec.is_active();
        next.recording.audio_retention_days = keep.value().max(0.0) as u32;
        next.apps.zoom = zoom.read();
        next.apps.discord = discord.read();
        let or = |s: glib::GString, old: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(old.into());
        next.openrouter.chat_model = or(chat.text(), &before.openrouter.chat_model);
        next.openrouter.enhance_model = or(enhance.text(), &before.openrouter.enhance_model);
        next.openrouter.stt_models = Some(vm::parse_list(&stt.text()))
            .filter(|v| !v.is_empty())
            .unwrap_or(before.openrouter.stt_models.clone());
        if next != before && next.save().is_ok() {
            reload_daemon();
        }
        on_saved();
    });
    dlg.present(Some(parent));
}
