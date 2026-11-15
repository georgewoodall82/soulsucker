//! Persistent settings and credentials.
//!
//! The config is a small TOML file, written with mode 0600 on Unix because it
//! contains the Soulseek password and the AcoustID key.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::util::{default_music_dir, expand_tilde};

pub const APP_NAME: &str = "soulsucker";

/// Format written to the music folder.
///
/// MP3 mode still accepts FLAC sources and converts them after verification;
/// FLAC mode only downloads FLAC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioQuality {
    #[default]
    Flac,
    Mp3,
}

impl AudioQuality {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Mp3 => "MP3 320",
        }
    }

    /// Describes acceptable Soulseek sources in logs and status messages.
    pub fn source_label(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Mp3 => "MP3 320 or FLAC",
        }
    }

    pub fn is_mp3(self) -> bool {
        matches!(self, Self::Mp3)
    }

    /// Serialized config value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3_320",
        }
    }

    /// Parse a config value or a Settings label.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        match value.as_str() {
            "flac" | "lossless" => Some(Self::Flac),
            "mp3" | "mp3_320" | "320" | "mp3_320kbps" => Some(Self::Mp3),
            _ => None,
        }
    }
}

impl Serialize for AudioQuality {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AudioQuality {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // A typo in a hand-edited config should fall back to FLAC instead of
        // discarding the whole file (and the stored credentials with it).
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value).unwrap_or_default())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub username: String,
    pub password: String,
    pub acoustid_key: String,

    /// Root folder that album folders are created inside.
    pub music_dir: String,
    /// Output format: lossless FLAC or 320 kbps MP3.
    pub audio_quality: AudioQuality,

    /// Maximum number of tracks downloading at once.
    pub max_concurrent: usize,
    /// How long a Soulseek search may collect results.
    pub search_timeout_secs: u64,
    /// How long a transfer may sit in a peer's queue before we try another.
    pub queue_timeout_secs: u64,
    /// How long a transfer may make no progress before we try another.
    pub stall_timeout_secs: u64,
    /// Accept a duration within this many seconds of MusicBrainz.
    pub length_tolerance_secs: f64,
    /// Chromaprint/AcoustID verification for finished tracks.
    pub fingerprint: bool,
    pub fingerprint_min_score: f64,
    /// Extra passes over exhausted source lists.
    pub last_resort_rounds: u32,
    /// Stall patience during a last-resort pass.
    pub last_resort_stall_timeout_secs: u64,
    /// Maximum source files tried per track.
    pub candidates_per_track: usize,
    /// Soulseek listener port; 0 asks the OS for a free port.
    pub listen_port: u16,
    /// Number of MusicBrainz release candidates to request.
    pub mb_limit: usize,
    /// Write disc-prefixed track numbers (101, 201, ...) for players that
    /// ignore the disc tag, such as Tesla's USB player.
    pub disc_prefix_track_numbers: bool,
    /// Write the album artist into every track's ARTIST tag so players that
    /// group by track artist (Tesla and similar car stereos) keep the album
    /// together. The original track credit is kept in ARTISTS.
    pub album_artist_for_tracks: bool,
    /// Two-pass EBU R128 normalize downloaded tracks before tagging.
    pub normalize_loudness: bool,

    /// Runtime-only: enable verbose logging from the Soulseek library.
    #[serde(skip)]
    pub verbose_lib: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            acoustid_key: String::new(),
            music_dir: default_music_dir().to_string_lossy().into_owned(),
            audio_quality: AudioQuality::Flac,
            max_concurrent: 4,
            search_timeout_secs: 20,
            queue_timeout_secs: 30,
            stall_timeout_secs: 15,
            length_tolerance_secs: 5.0,
            fingerprint: true,
            fingerprint_min_score: 0.8,
            last_resort_rounds: 2,
            last_resort_stall_timeout_secs: 60,
            candidates_per_track: 12,
            listen_port: 0,
            mb_limit: 25,
            disc_prefix_track_numbers: false,
            album_artist_for_tracks: false,
            normalize_loudness: false,
            verbose_lib: false,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("acoustid_key", &"<redacted>")
            .field("music_dir", &self.music_dir)
            .field("audio_quality", &self.audio_quality)
            .field("max_concurrent", &self.max_concurrent)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Clamp values from a hand-edited config or an over-enthusiastic CLI
    /// into the ranges the app can actually run with.
    pub fn normalize(&mut self) {
        let defaults = Config::default();
        self.username = self.username.trim().to_string();
        self.acoustid_key = self.acoustid_key.trim().to_string();
        if self.music_dir.trim().is_empty() {
            self.music_dir = defaults.music_dir;
        }
        if !(1..=16).contains(&self.max_concurrent) {
            self.max_concurrent = defaults.max_concurrent;
        }
        if self.search_timeout_secs == 0 || self.search_timeout_secs > 300 {
            self.search_timeout_secs = defaults.search_timeout_secs;
        }
        if self.queue_timeout_secs == 0 || self.queue_timeout_secs > 3600 {
            self.queue_timeout_secs = defaults.queue_timeout_secs;
        }
        if self.stall_timeout_secs == 0 || self.stall_timeout_secs > 3600 {
            self.stall_timeout_secs = defaults.stall_timeout_secs;
        }
        if !self.length_tolerance_secs.is_finite() || self.length_tolerance_secs < 0.0 {
            self.length_tolerance_secs = defaults.length_tolerance_secs;
        }
        if !self.fingerprint_min_score.is_finite()
            || !(0.0..=1.0).contains(&self.fingerprint_min_score)
        {
            self.fingerprint_min_score = defaults.fingerprint_min_score;
        }
        if self.last_resort_rounds > 20 {
            self.last_resort_rounds = defaults.last_resort_rounds;
        }
        if self.last_resort_stall_timeout_secs == 0 || self.last_resort_stall_timeout_secs > 3600 {
            self.last_resort_stall_timeout_secs = defaults.last_resort_stall_timeout_secs;
        }
        if !(1..=50).contains(&self.candidates_per_track) {
            self.candidates_per_track = defaults.candidates_per_track;
        }
        if !(1..=100).contains(&self.mb_limit) {
            self.mb_limit = defaults.mb_limit;
        }
    }

    pub fn music_dir_path(&self) -> PathBuf {
        expand_tilde(&self.music_dir)
    }

    pub fn has_credentials(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.is_empty()
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.username.trim().is_empty() {
            errors.push("Soulseek username is required".to_string());
        }
        if self.password.is_empty() {
            errors.push("Soulseek password is required".to_string());
        }
        if self.music_dir.trim().is_empty() {
            errors.push("Music folder is required".to_string());
        }
        if !(1..=16).contains(&self.max_concurrent) {
            errors.push("Parallel downloads must be between 1 and 16".to_string());
        }
        if self.search_timeout_secs == 0 || self.search_timeout_secs > 300 {
            errors.push("Search timeout must be between 1 and 300 seconds".to_string());
        }
        if self.queue_timeout_secs == 0 || self.queue_timeout_secs > 3600 {
            errors.push("Queue timeout must be between 1 and 3600 seconds".to_string());
        }
        if self.stall_timeout_secs == 0 || self.stall_timeout_secs > 3600 {
            errors.push("Stall timeout must be between 1 and 3600 seconds".to_string());
        }
        if !self.length_tolerance_secs.is_finite() || self.length_tolerance_secs < 0.0 {
            errors.push("Length tolerance must be 0 or greater".to_string());
        }
        if !(0.0..=1.0).contains(&self.fingerprint_min_score) {
            errors.push("Fingerprint score must be between 0 and 1".to_string());
        }
        if self.last_resort_rounds > 20 {
            errors.push("Last-resort passes must be 20 or fewer".to_string());
        }
        if self.last_resort_stall_timeout_secs == 0 || self.last_resort_stall_timeout_secs > 3600 {
            errors.push("Last-resort patience must be between 1 and 3600 seconds".to_string());
        }
        if !(1..=50).contains(&self.candidates_per_track) {
            errors.push("Sources per track must be between 1 and 50".to_string());
        }
        if !(1..=100).contains(&self.mb_limit) {
            errors.push("MusicBrainz result limit must be between 1 and 100".to_string());
        }
        errors
    }
}

pub struct LoadedConfig {
    pub config: Config,
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// Resolve the config file location. `SOULSUCKER_CONFIG` overrides it.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SOULSUCKER_CONFIG")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", APP_NAME)
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("soulsucker.toml"))
}

pub fn load(path_override: Option<&Path>) -> Result<LoadedConfig> {
    let path = path_override
        .map(Path::to_path_buf)
        .unwrap_or_else(config_path);

    if !path.exists() {
        return Ok(LoadedConfig {
            config: Config::default(),
            path,
            warning: None,
        });
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read config file {}", path.display()))?;
    tighten_file_permissions(&path);
    match toml::from_str::<Config>(&text) {
        Ok(mut config) => {
            // Hand-edited configs should not be able to lock the user out of
            // the setup screen with an unrelated out-of-range value.
            config.normalize();
            Ok(LoadedConfig {
                config,
                path,
                warning: None,
            })
        }
        Err(err) => {
            let backup = backup_config(&path, &text);
            let backup_note = backup
                .map(|backup| format!(" (backup at {})", backup.display()))
                .unwrap_or_default();
            Ok(LoadedConfig {
                config: Config::default(),
                path,
                warning: Some(format!(
                    "Config could not be parsed: {err}. Starting fresh{backup_note}."
                )),
            })
        }
    }
}

/// A uniquely named temporary file in the target's directory, removed on Drop
/// unless it was successfully renamed into place.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl TempFile {
    fn create_near(target: &Path, body: &[u8]) -> Result<Self> {
        for attempt in 0..4 {
            let path = unique_sibling(target, "tmp");
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let guard = Self { path, keep: false };
                    file.write_all(body)
                        .with_context(|| format!("could not write {}", guard.path().display()))?;
                    file.sync_all().ok();
                    return Ok(guard);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 3 => {
                    continue;
                }
                Err(err) => {
                    return Err(err).context("could not create a temporary config file");
                }
            }
        }
        Err(anyhow!("could not create a unique temporary config file"))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn unique_sibling(target: &Path, suffix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.{}.{}.{suffix}",
        std::process::id(),
        nonce ^ u128::from(sequence)
    ))
}

fn backup_config(path: &Path, text: &str) -> Option<PathBuf> {
    let backup = unique_sibling(path, "bak");
    let temp = TempFile::create_near(path, text.as_bytes()).ok()?;
    match fs::rename(temp.path(), &backup) {
        Ok(()) => {
            let mut temp = temp;
            temp.keep = true;
            Some(backup)
        }
        Err(_) => None,
    }
}

#[cfg(unix)]
fn tighten_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_file() && metadata.permissions().mode() & 0o077 != 0 {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(unix))]
fn tighten_file_permissions(_path: &Path) {}

pub fn save(config: &Config, path: &Path) -> Result<()> {
    let errors = config.validate();
    if let Some(first) = errors.first() {
        return Err(anyhow!(first.clone()));
    }
    if let Some(parent) = path.parent() {
        let existed = parent.exists();
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create config directory {}", parent.display()))?;
        #[cfg(unix)]
        if !existed {
            // Only tighten a directory we just created. Never chmod an
            // existing location the user chose (for example /tmp).
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let body = toml::to_string_pretty(config).context("could not serialize config")?;
    let mut temp = TempFile::create_near(path, body.as_bytes())?;
    replace_file(temp.path(), path)?;
    temp.keep = true;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn replace_file(source: &Path, target: &Path) -> Result<()> {
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if !target.exists() {
                return Err(first_error).with_context(|| {
                    format!("could not replace config file {}", target.display())
                });
            }
            // Windows cannot always replace an existing file. Move the old
            // one aside first, and put it back if the new file cannot land.
            let backup = unique_sibling(target, "old");
            fs::rename(target, &backup)
                .with_context(|| format!("could not stage old config file {}", target.display()))?;
            match fs::rename(source, target) {
                Ok(()) => {
                    let _ = fs::remove_file(&backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::rename(&backup, target);
                    Err(error).with_context(|| {
                        format!("could not replace config file {}", target.display())
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_once_credentials_are_filled() {
        let mut config = Config::default();
        assert!(!config.validate().is_empty());
        config.username = "someone".into();
        config.password = "secret".into();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            username: "someone".into(),
            password: "secret".into(),
            acoustid_key: "key".into(),
            max_concurrent: 7,
            audio_quality: AudioQuality::Mp3,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("audio_quality = \"mp3_320\""));
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.username, "someone");
        assert_eq!(back.max_concurrent, 7);
        assert_eq!(back.audio_quality, AudioQuality::Mp3);
        assert_eq!(back.music_dir, config.music_dir);
    }

    #[test]
    fn audio_quality_accepts_common_spellings_and_defaults_to_flac() {
        assert_eq!(AudioQuality::parse("FLAC"), Some(AudioQuality::Flac));
        assert_eq!(AudioQuality::parse("mp3"), Some(AudioQuality::Mp3));
        assert_eq!(AudioQuality::parse("MP3 320"), Some(AudioQuality::Mp3));
        assert_eq!(AudioQuality::parse("mp3_320"), Some(AudioQuality::Mp3));
        assert_eq!(AudioQuality::parse("nonsense"), None);

        let config: Config =
            toml::from_str("username = \"u\"\npassword = \"p\"\naudio_quality = \"nonsense\"\n")
                .unwrap();
        assert_eq!(config.audio_quality, AudioQuality::Flac);
    }

    #[cfg(unix)]
    #[test]
    fn a_predictable_temp_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "soulsucker-config-symlink-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let victim = dir.join("victim");
        fs::write(&victim, "untouched").unwrap();
        symlink(&victim, dir.join("config.toml.tmp")).unwrap();

        let config = Config {
            username: "someone".into(),
            password: "secret".into(),
            ..Config::default()
        };
        save(&config, &path).unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn loading_tightens_a_permissive_config_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "soulsucker-config-mode-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "username = \"u\"\npassword = \"p\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = load(Some(&path)).unwrap();
        assert_eq!(loaded.config.username, "u");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("soulsucker-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let config = Config {
            username: "someone".into(),
            password: "secret".into(),
            ..Config::default()
        };
        save(&config, &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let loaded = load(Some(&path)).unwrap();
        assert_eq!(loaded.config.username, "someone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let config: Config = toml::from_str("username = \"u\"\npassword = \"p\"\n").unwrap();
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.search_timeout_secs, 20);
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn out_of_range_values_are_clamped_to_defaults() {
        let mut config = Config {
            max_concurrent: 0,
            search_timeout_secs: 9999,
            queue_timeout_secs: 0,
            stall_timeout_secs: 0,
            length_tolerance_secs: -1.0,
            fingerprint_min_score: 2.0,
            last_resort_rounds: 99,
            candidates_per_track: 0,
            mb_limit: 0,
            music_dir: String::new(),
            ..Config::default()
        };
        config.normalize();
        let defaults = Config::default();
        assert_eq!(config.max_concurrent, defaults.max_concurrent);
        assert_eq!(config.search_timeout_secs, defaults.search_timeout_secs);
        assert_eq!(config.queue_timeout_secs, defaults.queue_timeout_secs);
        assert_eq!(config.stall_timeout_secs, defaults.stall_timeout_secs);
        assert_eq!(config.length_tolerance_secs, defaults.length_tolerance_secs);
        assert_eq!(config.fingerprint_min_score, defaults.fingerprint_min_score);
        assert_eq!(config.last_resort_rounds, defaults.last_resort_rounds);
        assert_eq!(config.candidates_per_track, defaults.candidates_per_track);
        assert_eq!(config.mb_limit, defaults.mb_limit);
        assert_eq!(config.music_dir, defaults.music_dir);
    }
}
