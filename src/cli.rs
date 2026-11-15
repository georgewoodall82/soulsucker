//! Command-line interface.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Config;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "soulsucker",
    version,
    about = "Find an album on MusicBrainz and download it from Soulseek",
    long_about = "A polished terminal album downloader. Run without arguments for the TUI; \
                  artist and album prefill the search. Credentials are asked for once and saved.",
    after_help = "Credential flags are saved to the config file. Other options override the stored \
                  settings for this run only; use Settings (F2) to change saved defaults."
)]
pub struct Args {
    /// Artist name (positional)
    #[arg(value_name = "ARTIST")]
    pub artist_pos: Option<String>,
    /// Album title (positional)
    #[arg(value_name = "ALBUM")]
    pub album_pos: Option<String>,

    /// Artist name
    #[arg(short = 'a', long = "artist", value_name = "ARTIST")]
    pub artist: Option<String>,
    /// Album title
    #[arg(short = 'A', long = "album", value_name = "ALBUM")]
    pub album: Option<String>,

    /// Soulseek username (saved to the config file)
    #[arg(short = 'u', long = "username", value_name = "USER")]
    pub username: Option<String>,
    /// Soulseek password (saved to the config file; visible in shell history)
    #[arg(short = 'p', long = "password", value_name = "PASS")]
    pub password: Option<String>,
    /// AcoustID API key (saved to the config file)
    #[arg(long = "acoustid-key", value_name = "KEY")]
    pub acoustid_key: Option<String>,

    /// Root music folder for this run only (use Settings to save it)
    #[arg(short = 'o', long = "output", value_name = "DIR")]
    pub output: Option<String>,
    /// Force re-download of files that already exist
    #[arg(long)]
    pub force: bool,
    /// Auto-pick the recommended album, release and start downloading
    #[arg(long)]
    pub yes: bool,

    /// Disable Chromaprint/AcoustID verification for this run
    #[arg(long)]
    pub no_fingerprint: bool,
    /// Seconds to collect Soulseek results (default: from config)
    #[arg(long, value_name = "SECS")]
    pub search_timeout: Option<u64>,
    /// Seconds a transfer may sit in a peer queue
    #[arg(long, value_name = "SECS")]
    pub queue_timeout: Option<u64>,
    /// Seconds without progress before another source is tried
    #[arg(long, value_name = "SECS")]
    pub stall_timeout: Option<u64>,
    /// Parallel track downloads
    #[arg(long, value_name = "N")]
    pub max_concurrent: Option<usize>,
    /// Allowed duration difference from MusicBrainz, in seconds (0 disables)
    #[arg(long, value_name = "SECS")]
    pub length_tolerance: Option<f64>,
    /// Minimum AcoustID score to accept a fingerprint match
    #[arg(long, value_name = "SCORE")]
    pub fingerprint_min_score: Option<f64>,
    /// Extra passes over exhausted source lists
    #[arg(long, value_name = "N")]
    pub last_resort_rounds: Option<u32>,
    /// Stall patience during a last-resort pass
    #[arg(long, value_name = "SECS")]
    pub last_resort_stall_timeout: Option<u64>,
    /// Maximum source files tried per track
    #[arg(long, value_name = "N")]
    pub candidates: Option<usize>,
    /// Soulseek listener port (0 = automatic)
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
    /// Number of MusicBrainz album/EP matches to show
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Use a different config file
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Force the first-run credential screen even if a config exists
    #[arg(long)]
    pub setup: bool,
    /// Verbose raw library logging (printed to stderr after the TUI exits)
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

impl Args {
    pub fn artist(&self) -> Option<String> {
        self.artist
            .clone()
            .or_else(|| self.artist_pos.clone())
            .filter(|value| !value.trim().is_empty())
    }

    pub fn album(&self) -> Option<String> {
        self.album
            .clone()
            .or_else(|| self.album_pos.clone())
            .filter(|value| !value.trim().is_empty())
    }

    /// Credential flags are persisted; they are the one-time bootstrap path.
    pub fn apply_credential_overrides(&self, config: &mut Config) {
        if let Some(username) = &self.username {
            config.username = username.trim().to_string();
        }
        if let Some(password) = &self.password {
            config.password = password.clone();
        }
        if let Some(key) = &self.acoustid_key {
            config.acoustid_key = key.trim().to_string();
        }
    }

    /// Non-credential flags override stored settings for this run only.
    pub fn apply_runtime_overrides(&self, config: &mut Config) {
        if let Some(output) = &self.output {
            config.music_dir = output.trim().to_string();
        }
        if let Some(value) = self.search_timeout {
            config.search_timeout_secs = value;
        }
        if let Some(value) = self.queue_timeout {
            config.queue_timeout_secs = value;
        }
        if let Some(value) = self.stall_timeout {
            config.stall_timeout_secs = value;
        }
        if let Some(value) = self.max_concurrent {
            config.max_concurrent = value;
        }
        if let Some(value) = self.length_tolerance {
            config.length_tolerance_secs = value;
        }
        if let Some(value) = self.fingerprint_min_score {
            config.fingerprint_min_score = value;
        }
        if let Some(value) = self.last_resort_rounds {
            config.last_resort_rounds = value;
        }
        if let Some(value) = self.last_resort_stall_timeout {
            config.last_resort_stall_timeout_secs = value;
        }
        if let Some(value) = self.candidates {
            config.candidates_per_track = value;
        }
        if let Some(value) = self.port {
            config.listen_port = value;
        }
        if let Some(value) = self.limit {
            config.mb_limit = value;
        }
        config.verbose_lib = self.verbose;
    }

    pub fn credential_override(&self) -> bool {
        self.username.is_some() || self.password.is_some() || self.acoustid_key.is_some()
    }
}
