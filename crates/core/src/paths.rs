//! XDG locations. `MUESLI_HOME` overrides everything (tests, dev runs).

use std::path::PathBuf;

fn home_override() -> Option<PathBuf> {
    std::env::var_os("MUESLI_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn dirs() -> directories::ProjectDirs {
    directories::ProjectDirs::from("", "", "muesli").expect("no home directory")
}

/// Create `dir` (and parents) and make it private to the user (0700): config holds the API key,
/// data holds recordings and transcripts.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub fn config_dir() -> PathBuf {
    home_override().map(|h| h.join("config")).unwrap_or_else(|| dirs().config_dir().to_path_buf())
}

pub fn data_dir() -> PathBuf {
    home_override().map(|h| h.join("data")).unwrap_or_else(|| dirs().data_dir().to_path_buf())
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn key_file() -> PathBuf {
    config_dir().join("openrouter.key")
}

pub fn templates_dir() -> PathBuf {
    config_dir().join("templates")
}

pub fn db_file() -> PathBuf {
    data_dir().join("muesli.db")
}

pub fn meetings_dir() -> PathBuf {
    data_dir().join("meetings")
}

pub fn meeting_dir(id: &str) -> PathBuf {
    meetings_dir().join(id)
}

/// Hook scripts: `hooks/<event>.d/<script>`.
pub fn hooks_dir() -> PathBuf {
    config_dir().join("hooks")
}

/// Files written for hook scripts (notes, transcript, meeting.json).
pub fn export_dir(id: &str) -> PathBuf {
    meeting_dir(id).join("export")
}

/// Mixed playback pieces for a meeting (see `muesli_audio::playback`).
pub fn playback_dir(id: &str) -> PathBuf {
    meeting_dir(id).join("playback")
}

pub fn socket_path() -> PathBuf {
    if let Some(h) = home_override() {
        return h.join("muesli.sock");
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("muesli.sock")
}
