//! Application state, screen flow and keyboard handling.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::acoustid;
use crate::cli::Args;
use crate::config::{self, Config};
use crate::input::TextInput;
use crate::musicbrainz::{self, ReleaseCandidate, ReleaseInfo};
use crate::session::{
    self, DownloadOptions, DownloadRequest, LogLevel, SessionHandle, SlskEvent, TrackMeta,
    TrackStatus, TrackUpdate,
};
use crate::update::{self, UpdateOutcome};
use crate::util;

const TOAST_DURATION: Duration = Duration::from_secs(4);
const MAX_LOG_LINES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Setup,
    Search,
    Tracks,
    Download,
    Settings,
}

#[derive(Debug, Clone)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Online { username: String, port: Option<u16> },
    Error(String),
}

#[derive(Debug)]
pub enum UiEvent {
    MbSearch {
        generation: u64,
        result: Result<Vec<ReleaseCandidate>, String>,
    },
    MbRelease {
        generation: u64,
        result: Result<ReleaseInfo, String>,
    },
    Slsk {
        generation: u64,
        event: SlskEvent,
    },
    UpdateStarted(String),
    UpdateDone(Result<UpdateOutcome, String>),
}

#[derive(Debug, Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Downloading {
        version: String,
    },
    Done(UpdateOutcome),
    Failed(String),
}

impl UpdateState {
    pub fn busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "check",
            Self::Checking => "checking…",
            Self::Downloading { .. } => "updating…",
            Self::Done(UpdateOutcome::Updated { .. }) => "restart",
            Self::Done(UpdateOutcome::UpToDate { .. }) => "up to date",
            Self::Done(UpdateOutcome::NewerLocal { .. }) => "newer",
            Self::Failed(_) => "failed",
        }
    }

    pub fn level(&self) -> LogLevel {
        match self {
            Self::Failed(_) => LogLevel::Error,
            Self::Done(UpdateOutcome::Updated { .. })
            | Self::Done(UpdateOutcome::UpToDate { .. }) => LogLevel::Success,
            _ => LogLevel::Info,
        }
    }

    pub fn status(&self) -> Option<(String, LogLevel)> {
        match self {
            Self::Idle => None,
            Self::Checking => Some((
                "Checking GitHub for the latest release…".to_string(),
                LogLevel::Info,
            )),
            Self::Downloading { version } => {
                Some((format!("Downloading soulsucker {version}…"), LogLevel::Info))
            }
            Self::Done(UpdateOutcome::UpToDate { current, latest }) => Some((
                format!("soulsucker {current} is up to date (latest {latest})"),
                LogLevel::Success,
            )),
            Self::Done(UpdateOutcome::NewerLocal { current, latest }) => Some((
                format!("Local build {current} is newer than the latest release ({latest})"),
                LogLevel::Info,
            )),
            Self::Done(UpdateOutcome::Updated { version }) => Some((
                format!("Updated to {version} — restart soulsucker to use it"),
                LogLevel::Success,
            )),
            Self::Failed(error) => Some((format!("Update failed: {error}"), LogLevel::Error)),
        }
    }
}

pub struct LogEntry {
    pub at: Instant,
    pub level: LogLevel,
    pub message: String,
}

pub struct Toast {
    pub message: String,
    pub level: LogLevel,
    pub at: Instant,
}

// -------------------------------------------------------------------------
// Setup
// -------------------------------------------------------------------------

pub struct SetupForm {
    pub username: TextInput,
    pub password: TextInput,
    pub acoustid: TextInput,
    pub focus: usize,
    pub error: Option<String>,
}

impl SetupForm {
    fn from_config(config: &Config) -> Self {
        Self {
            username: TextInput::new(&config.username),
            password: TextInput::new(&config.password),
            acoustid: TextInput::new(&config.acoustid_key),
            focus: if config.username.is_empty() { 0 } else { 1 },
            error: None,
        }
    }
}

// -------------------------------------------------------------------------
// Search
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFocus {
    Artist,
    Album,
    Results,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    Idle,
    Searching { started: Instant },
    Fetching { started: Instant },
}

impl SearchPhase {
    pub fn is_loading(self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn started(self) -> Option<Instant> {
        match self {
            Self::Idle => None,
            Self::Searching { started } | Self::Fetching { started } => Some(started),
        }
    }
}

pub struct SearchView {
    pub artist: TextInput,
    pub album: TextInput,
    pub focus: SearchFocus,
    pub results: Vec<ReleaseCandidate>,
    pub list_state: ratatui::widgets::ListState,
    pub phase: SearchPhase,
    pub error: Option<String>,
    pub query_label: String,
}

impl SearchView {
    fn new(artist: &str, album: &str) -> Self {
        let focus = if artist.trim().is_empty() {
            SearchFocus::Artist
        } else {
            SearchFocus::Album
        };
        Self {
            artist: TextInput::new(artist),
            album: TextInput::new(album),
            focus,
            results: Vec::new(),
            list_state: ratatui::widgets::ListState::default(),
            phase: SearchPhase::Idle,
            error: None,
            query_label: String::new(),
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn set_selected(&mut self, index: usize) {
        if self.results.is_empty() {
            self.list_state.select(None);
        } else {
            let index = index.min(self.results.len() - 1);
            self.list_state.select(Some(index));
        }
    }
}

// -------------------------------------------------------------------------
// Tracks
// -------------------------------------------------------------------------

#[derive(Clone)]
pub struct PlannedTrack {
    pub meta: TrackMeta,
    pub label: String,
}

pub struct TracksView {
    pub release: ReleaseInfo,
    pub album_dir: PathBuf,
    pub tracks: Vec<PlannedTrack>,
    pub list_state: ratatui::widgets::ListState,
    pub existing_count: usize,
}

impl TracksView {
    pub(crate) fn build(release: ReleaseInfo, config: &Config) -> Self {
        let music_dir = config.music_dir_path();
        let artist_dir = if release.artist.trim().is_empty() {
            music_dir
        } else {
            music_dir.join(util::sanitize_filename(&release.artist))
        };
        let single = release.is_single();
        let album_dir = if single || release.title.trim().is_empty() {
            artist_dir
        } else {
            artist_dir.join(util::sanitize_filename(&release.title))
        };
        let track_count = release.tracks.len() as u32;
        let disc_count = release.disc_count();
        let mut used = HashSet::new();
        let mut tracks = Vec::with_capacity(release.tracks.len());

        for track in &release.tracks {
            let base = if single {
                format!("{}.flac", util::sanitize_filename(&track.title))
            } else {
                session::track_filename(
                    track.disc,
                    track.position,
                    &track.title,
                    disc_count,
                    track_count,
                )
            };
            let mut filename = base.clone();
            if used.contains(&filename) {
                let stem = base.strip_suffix(".flac").unwrap_or(&base);
                for suffix in 2..100 {
                    let candidate = format!("{stem} ({suffix}).flac");
                    if !used.contains(&candidate) {
                        filename = candidate;
                        break;
                    }
                }
            }
            used.insert(filename.clone());
            let final_path = album_dir.join(filename);
            let label = if disc_count > 1 {
                format!("{}-{:02}", track.disc, track.position)
            } else {
                format!("{:02}", track.position)
            };
            tracks.push(PlannedTrack {
                meta: TrackMeta {
                    index: track.index,
                    disc: track.disc,
                    position: track.position,
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    expected_secs: track.length_secs(),
                    recording_mbid: track.recording_mbid.clone(),
                    track_mbid: track.track_mbid.clone(),
                    final_path,
                },
                label,
            });
        }

        let existing_count = tracks
            .iter()
            .filter(|track| {
                session::existing_file_is_valid(&track.meta, config.length_tolerance_secs).is_some()
            })
            .count();

        Self {
            release,
            album_dir,
            tracks,
            list_state: ratatui::widgets::ListState::default(),
            existing_count,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }
}

// -------------------------------------------------------------------------
// Download
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Waiting,
    Searching,
    Downloading,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadFocus {
    Tracks,
    Log,
}

#[derive(Clone)]
pub struct DownloadRow {
    pub index: usize,
    pub label: String,
    pub title: String,
    pub status: TrackStatus,
    pub bytes: u64,
    pub total: u64,
    pub speed: f64,
    pub source: String,
    pub attempt: usize,
    pub attempts_total: usize,
    pub note: String,
}

pub struct DownloadView {
    pub job_id: u64,
    pub artist: String,
    pub album: String,
    pub album_dir: PathBuf,
    pub rows: Vec<DownloadRow>,
    pub list_state: ratatui::widgets::ListState,
    pub focus: DownloadFocus,
    pub phase: DownloadPhase,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub query: String,
    pub peers: usize,
    pub waiting_for_login: bool,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub log_offset: usize,
    pub retry_available: bool,
    pub search_interrupted: bool,
}

impl DownloadView {
    pub(crate) fn new(
        job_id: u64,
        release: &ReleaseInfo,
        album_dir: PathBuf,
        tracks: &[PlannedTrack],
        waiting_for_login: bool,
    ) -> Self {
        let rows = tracks
            .iter()
            .map(|track| DownloadRow {
                index: track.meta.index,
                label: track.label.clone(),
                title: track.meta.title.clone(),
                status: TrackStatus::Queued,
                bytes: 0,
                total: 0,
                speed: 0.0,
                source: String::new(),
                attempt: 0,
                attempts_total: 0,
                note: if waiting_for_login {
                    "waiting for login".to_string()
                } else {
                    "waiting for search".to_string()
                },
            })
            .collect();
        let mut list_state = ratatui::widgets::ListState::default();
        if !tracks.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            job_id,
            artist: release.artist.clone(),
            album: release.title.clone(),
            album_dir,
            rows,
            list_state,
            focus: DownloadFocus::Tracks,
            phase: if waiting_for_login {
                DownloadPhase::Waiting
            } else {
                DownloadPhase::Searching
            },
            started: Instant::now(),
            finished: None,
            query: format!("{} {}", release.artist, release.title),
            peers: 0,
            waiting_for_login,
            completed: 0,
            failed: 0,
            cancelled: 0,
            log_offset: 0,
            retry_available: true,
            search_interrupted: false,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn update_row(&mut self, update: &TrackUpdate) {
        if update.job_id != self.job_id {
            return;
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.index == update.index) {
            // Phase updates from the fingerprint worker are sent directly to
            // the UI. Ignore late ones after a cancel or after a retry moved
            // on to a different attempt.
            if update.status == TrackStatus::Verifying {
                if matches!(
                    row.status,
                    TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
                ) {
                    return;
                }
                if row.attempt > 0 && update.attempt != row.attempt {
                    return;
                }
            }
            row.status = update.status;
            row.bytes = update.bytes_done;
            row.total = update.total;
            row.speed = update.speed;
            row.source = update.source.clone();
            row.attempt = update.attempt;
            row.attempts_total = update.attempts_total;
            row.note = update.note.clone();
        }
    }

    fn set_all_notes(&mut self, note: &str) {
        for row in &mut self.rows {
            if !matches!(
                row.status,
                TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
            ) {
                row.note = note.to_string();
            }
        }
    }

    pub fn terminated(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| {
                matches!(
                    row.status,
                    TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
                )
            })
            .count()
    }
}

// -------------------------------------------------------------------------
// Settings
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    Username,
    Password,
    AcoustId,
    Fingerprint,
    MusicDir,
    MaxConcurrent,
    SearchTimeout,
    QueueTimeout,
    StallTimeout,
    LengthTolerance,
    MinScore,
    LastResortRounds,
    LastResortStall,
    Candidates,
    ListenPort,
    MbLimit,
    DiscPrefixTracks,
    AlbumArtistTracks,
    CheckUpdates,
}

#[derive(Debug, Clone, Copy)]
pub enum SettingKind {
    Text,
    Secret,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
    Bool,
    Action,
}

impl SettingKind {
    pub fn is_secret(self) -> bool {
        matches!(self, Self::Secret)
    }

    pub fn is_bool(self) -> bool {
        matches!(self, Self::Bool)
    }

    pub fn is_action(self) -> bool {
        matches!(self, Self::Action)
    }
}

pub struct SettingField {
    pub key: SettingKey,
    pub label: &'static str,
    pub hint: &'static str,
    pub value: String,
    pub kind: SettingKind,
}

pub struct EditState {
    pub field: usize,
    pub input: TextInput,
}

pub struct SettingsForm {
    pub fields: Vec<SettingField>,
    pub selected: usize,
    pub list_state: ratatui::widgets::ListState,
    pub editing: Option<EditState>,
    pub error: Option<String>,
}

impl SettingsForm {
    pub fn from_config(config: &Config) -> Self {
        let fields = vec![
            field(
                SettingKey::Username,
                "Soulseek username",
                "account name",
                config.username.clone(),
                SettingKind::Text,
            ),
            field(
                SettingKey::Password,
                "Soulseek password",
                "stored with mode 0600",
                config.password.clone(),
                SettingKind::Secret,
            ),
            field(
                SettingKey::AcoustId,
                "AcoustID key",
                "optional · acoustid.org/api",
                config.acoustid_key.clone(),
                SettingKind::Secret,
            ),
            field(
                SettingKey::Fingerprint,
                "Fingerprint check",
                "Chromaprint + AcoustID",
                if config.fingerprint { "on" } else { "off" }.to_string(),
                SettingKind::Bool,
            ),
            field(
                SettingKey::MusicDir,
                "Music folder",
                "album folders go here",
                config.music_dir.clone(),
                SettingKind::Text,
            ),
            field(
                SettingKey::MaxConcurrent,
                "Parallel downloads",
                "1–16",
                config.max_concurrent.to_string(),
                SettingKind::Int { min: 1, max: 16 },
            ),
            field(
                SettingKey::SearchTimeout,
                "Search timeout",
                "seconds",
                config.search_timeout_secs.to_string(),
                SettingKind::Int { min: 1, max: 300 },
            ),
            field(
                SettingKey::QueueTimeout,
                "Queue timeout",
                "seconds in a peer queue",
                config.queue_timeout_secs.to_string(),
                SettingKind::Int { min: 1, max: 3600 },
            ),
            field(
                SettingKey::StallTimeout,
                "Stall timeout",
                "seconds without progress",
                config.stall_timeout_secs.to_string(),
                SettingKind::Int { min: 1, max: 3600 },
            ),
            field(
                SettingKey::LengthTolerance,
                "Length tolerance",
                "seconds; 0 disables",
                format_number(config.length_tolerance_secs),
                SettingKind::Float {
                    min: 0.0,
                    max: 600.0,
                },
            ),
            field(
                SettingKey::MinScore,
                "Fingerprint score",
                "0–1 minimum match",
                format!("{:.2}", config.fingerprint_min_score),
                SettingKind::Float { min: 0.0, max: 1.0 },
            ),
            field(
                SettingKey::LastResortRounds,
                "Last-resort passes",
                "extra passes per track",
                config.last_resort_rounds.to_string(),
                SettingKind::Int { min: 0, max: 20 },
            ),
            field(
                SettingKey::LastResortStall,
                "Last-resort patience",
                "seconds",
                config.last_resort_stall_timeout_secs.to_string(),
                SettingKind::Int { min: 1, max: 3600 },
            ),
            field(
                SettingKey::Candidates,
                "Sources per track",
                "maximum tries",
                config.candidates_per_track.to_string(),
                SettingKind::Int { min: 1, max: 50 },
            ),
            field(
                SettingKey::ListenPort,
                "Listener port",
                "0 = automatic",
                config.listen_port.to_string(),
                SettingKind::Int { min: 0, max: 65535 },
            ),
            field(
                SettingKey::MbLimit,
                "MusicBrainz results",
                "release candidates",
                config.mb_limit.to_string(),
                SettingKind::Int { min: 1, max: 100 },
            ),
            field(
                SettingKey::DiscPrefixTracks,
                "Disc-prefixed tracks",
                "Tesla-friendly 101, 201",
                if config.disc_prefix_track_numbers {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                SettingKind::Bool,
            ),
            field(
                SettingKey::AlbumArtistTracks,
                "Album artist on tracks",
                "car-stereo grouping",
                if config.album_artist_for_tracks {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                SettingKind::Bool,
            ),
            field(
                SettingKey::CheckUpdates,
                "Check for updates",
                "GitHub latest release",
                "check".to_string(),
                SettingKind::Action,
            ),
        ];
        let mut list_state = ratatui::widgets::ListState::default();
        list_state.select(Some(0));
        Self {
            fields,
            selected: 0,
            list_state,
            editing: None,
            error: None,
        }
    }

    pub fn selected_field(&self) -> Option<&SettingField> {
        self.fields.get(self.selected)
    }

    fn set_selected(&mut self, index: usize) {
        if self.fields.is_empty() {
            return;
        }
        self.selected = index.min(self.fields.len() - 1);
        self.list_state.select(Some(self.selected));
    }

    fn begin_edit(&mut self) {
        let Some(field) = self.fields.get(self.selected) else {
            return;
        };
        if field.kind.is_bool() || field.kind.is_action() {
            return;
        }
        self.editing = Some(EditState {
            field: self.selected,
            input: TextInput::new(&field.value),
        });
        self.error = None;
    }

    fn commit_edit(&mut self) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        let Some(field) = self.fields.get_mut(edit.field) else {
            return;
        };
        let value = edit.input.value();
        match validate_field(field.kind, &value) {
            Ok(normalized) => {
                field.value = normalized;
                self.error = None;
                self.set_selected((edit.field + 1).min(self.fields.len().saturating_sub(1)));
            }
            Err(err) => {
                self.error = Some(err);
                self.editing = Some(edit);
            }
        }
    }

    fn toggle_selected(&mut self) {
        let Some(field) = self.fields.get_mut(self.selected) else {
            return;
        };
        if field.kind.is_bool() {
            field.value = if field.value == "on" { "off" } else { "on" }.to_string();
        }
    }

    fn index_of(&self, key: SettingKey) -> usize {
        self.fields
            .iter()
            .position(|field| field.key == key)
            .unwrap_or(0)
    }

    pub fn to_config(&self, base: &Config) -> Result<Config, (usize, String)> {
        let mut config = base.clone();
        for (index, field) in self.fields.iter().enumerate() {
            let value = validate_field(field.kind, &field.value)
                .map_err(|err| (index, format!("{}: {err}", field.label)))?;
            match field.key {
                SettingKey::Username => config.username = value.trim().to_string(),
                SettingKey::Password => config.password = value,
                SettingKey::AcoustId => config.acoustid_key = value.trim().to_string(),
                SettingKey::Fingerprint => config.fingerprint = value == "on",
                SettingKey::MusicDir => config.music_dir = value.trim().to_string(),
                SettingKey::MaxConcurrent => {
                    config.max_concurrent = parse_usize(&value, index, field)?
                }
                SettingKey::SearchTimeout => {
                    config.search_timeout_secs = parse_u64(&value, index, field)?
                }
                SettingKey::QueueTimeout => {
                    config.queue_timeout_secs = parse_u64(&value, index, field)?
                }
                SettingKey::StallTimeout => {
                    config.stall_timeout_secs = parse_u64(&value, index, field)?
                }
                SettingKey::LengthTolerance => {
                    config.length_tolerance_secs = parse_f64(&value, index, field)?
                }
                SettingKey::MinScore => {
                    config.fingerprint_min_score = parse_f64(&value, index, field)?
                }
                SettingKey::LastResortRounds => {
                    config.last_resort_rounds = parse_u32(&value, index, field)?
                }
                SettingKey::LastResortStall => {
                    config.last_resort_stall_timeout_secs = parse_u64(&value, index, field)?
                }
                SettingKey::Candidates => {
                    config.candidates_per_track = parse_usize(&value, index, field)?
                }
                SettingKey::ListenPort => config.listen_port = parse_u16(&value, index, field)?,
                SettingKey::MbLimit => config.mb_limit = parse_usize(&value, index, field)?,
                SettingKey::DiscPrefixTracks => {
                    config.disc_prefix_track_numbers = value == "on";
                }
                SettingKey::AlbumArtistTracks => {
                    config.album_artist_for_tracks = value == "on";
                }
                SettingKey::CheckUpdates => {}
            }
        }
        if let Some(error) = config.validate().first() {
            let index = if error.contains("Music folder") {
                self.index_of(SettingKey::MusicDir)
            } else if error.contains("username") {
                self.index_of(SettingKey::Username)
            } else if error.contains("password") {
                self.index_of(SettingKey::Password)
            } else {
                0
            };
            return Err((index, error.clone()));
        }
        Ok(config)
    }
}

fn field(
    key: SettingKey,
    label: &'static str,
    hint: &'static str,
    value: String,
    kind: SettingKind,
) -> SettingField {
    SettingField {
        key,
        label,
        hint,
        value,
        kind,
    }
}

fn validate_field(kind: SettingKind, value: &str) -> Result<String, String> {
    match kind {
        SettingKind::Text | SettingKind::Secret => Ok(value.to_string()),
        SettingKind::Int { min, max } => {
            let parsed: i64 = value
                .trim()
                .parse()
                .map_err(|_| "enter a whole number".to_string())?;
            if parsed < min || parsed > max {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(parsed.to_string())
        }
        SettingKind::Float { min, max } => {
            let parsed: f64 = value
                .trim()
                .parse()
                .map_err(|_| "enter a number".to_string())?;
            if !parsed.is_finite() || parsed < min || parsed > max {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(format_number(parsed))
        }
        SettingKind::Bool => Ok(if value == "on" { "on" } else { "off" }.to_string()),
        SettingKind::Action => Ok(value.to_string()),
    }
}

fn format_number(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

fn parse_usize(value: &str, index: usize, field: &SettingField) -> Result<usize, (usize, String)> {
    value
        .parse()
        .map_err(|_| (index, format!("{}: invalid number", field.label)))
}

fn parse_u64(value: &str, index: usize, field: &SettingField) -> Result<u64, (usize, String)> {
    value
        .parse()
        .map_err(|_| (index, format!("{}: invalid number", field.label)))
}

fn parse_u32(value: &str, index: usize, field: &SettingField) -> Result<u32, (usize, String)> {
    value
        .parse()
        .map_err(|_| (index, format!("{}: invalid number", field.label)))
}

fn parse_u16(value: &str, index: usize, field: &SettingField) -> Result<u16, (usize, String)> {
    value
        .parse()
        .map_err(|_| (index, format!("{}: invalid number", field.label)))
}

fn parse_f64(value: &str, index: usize, field: &SettingField) -> Result<f64, (usize, String)> {
    value
        .parse()
        .map_err(|_| (index, format!("{}: invalid number", field.label)))
}

// -------------------------------------------------------------------------
// Confirm
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Quit,
    NewSearch,
}

pub struct ConfirmDialog {
    pub title: String,
    pub lines: Vec<String>,
    pub action: ConfirmAction,
}

// -------------------------------------------------------------------------
// App
// -------------------------------------------------------------------------

pub struct App {
    pub cfg: Config,
    pub persisted_cfg: Config,
    pub cfg_path: PathBuf,
    pub screen: Screen,
    pub previous_screen: Screen,
    pub setup: SetupForm,
    pub search: SearchView,
    pub tracks: Option<TracksView>,
    pub download: Option<DownloadView>,
    pub settings: SettingsForm,
    pub connection: ConnState,
    pub session: Option<SessionHandle>,
    pub events_tx: Sender<UiEvent>,
    pub events_rx: Receiver<UiEvent>,
    pub logs: Vec<LogEntry>,
    pub spinner: usize,
    pub toast: Option<Toast>,
    pub confirm: Option<ConfirmDialog>,
    pub help_open: bool,
    pub quit: bool,
    pub fingerprint_override: Option<bool>,
    pub force: bool,
    pub auto_yes: bool,
    pub pending_start: Option<DownloadRequest>,
    pub generation: u64,
    pub job_counter: u64,
    pub started: Instant,
    pub fpcalc_available: bool,
    pub config_path_label: String,
    pub session_generation: u64,
    pub update_state: UpdateState,
}

impl App {
    pub fn new(
        cfg: Config,
        persisted_cfg: Config,
        cfg_path: PathBuf,
        args: &Args,
        config_warning: Option<String>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let setup_needed = !cfg.has_credentials() || args.setup;
        let setup = SetupForm::from_config(&persisted_cfg);
        let search = SearchView::new(
            args.artist().unwrap_or_default().as_str(),
            args.album().unwrap_or_default().as_str(),
        );
        let settings = SettingsForm::from_config(&persisted_cfg);
        let config_path_label = cfg_path.display().to_string();
        let mut app = Self {
            cfg,
            persisted_cfg,
            cfg_path,
            screen: if setup_needed {
                Screen::Setup
            } else {
                Screen::Search
            },
            previous_screen: Screen::Search,
            setup,
            search,
            tracks: None,
            download: None,
            settings,
            connection: ConnState::Disconnected,
            session: None,
            events_tx,
            events_rx,
            logs: Vec::new(),
            spinner: 0,
            toast: None,
            confirm: None,
            help_open: false,
            quit: false,
            fingerprint_override: args.no_fingerprint.then_some(false),
            force: args.force,
            auto_yes: args.yes,
            pending_start: None,
            generation: 0,
            job_counter: 0,
            started: Instant::now(),
            fpcalc_available: acoustid::fpcalc_path().is_some(),
            config_path_label,
            session_generation: 0,
            update_state: UpdateState::Idle,
        };

        if let Some(warning) = config_warning {
            app.push_log(LogLevel::Warn, warning.clone());
            if app.screen == Screen::Setup {
                app.setup.error = Some(warning.clone());
            }
            app.toast(LogLevel::Warn, warning);
        }
        app.push_log(
            LogLevel::Info,
            format!("Config: {}", app.cfg_path.display()),
        );
        if app.fpcalc_available {
            app.push_log(
                LogLevel::Info,
                "fpcalc found — fingerprint verification available.",
            );
        } else {
            app.push_log(
                LogLevel::Warn,
                "fpcalc not found — tracks will be checked by duration only (install chromaprint-tools for AcoustID).",
            );
        }

        if !setup_needed {
            app.spawn_session();
            if !app.search.artist.is_empty() && !app.search.album.is_empty() {
                app.start_mb_search();
            } else if app.search.artist.is_empty() {
                app.search.focus = SearchFocus::Artist;
            }
        }
        app
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        while !self.quit {
            self.drain_events();
            terminal.draw(|frame| crate::ui::render(self, frame))?;
            if crossterm::event::poll(Duration::from_millis(100))? {
                match crossterm::event::read()? {
                    crossterm::event::Event::Key(key)
                        if matches!(
                            key.kind,
                            crossterm::event::KeyEventKind::Press
                                | crossterm::event::KeyEventKind::Repeat
                        ) =>
                    {
                        self.handle_key(key);
                    }
                    crossterm::event::Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            self.tick();
        }
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
        Ok(())
    }

    fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1) % 10_000;
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.at.elapsed() > TOAST_DURATION)
        {
            self.toast = None;
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.handle_ui_event(event);
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::MbSearch { generation, result } => {
                if generation != self.generation {
                    return;
                }
                self.search.phase = SearchPhase::Idle;
                match result {
                    Ok(results) => {
                        let count = results.len();
                        self.search.results = results;
                        self.search.error = None;
                        if count == 0 {
                            self.search.list_state.select(None);
                            self.search.focus = SearchFocus::Album;
                            self.auto_yes = false;
                            self.push_log(
                                LogLevel::Warn,
                                "No MusicBrainz releases found for that artist and album.",
                            );
                            self.toast(LogLevel::Warn, "No releases found");
                        } else {
                            self.search.set_selected(0);
                            self.search.focus = SearchFocus::Results;
                            self.push_log(
                                LogLevel::Info,
                                format!("MusicBrainz returned {count} release(s)."),
                            );
                            if self.auto_yes {
                                self.choose_release();
                            }
                        }
                    }
                    Err(err) => {
                        self.search.error = Some(err.clone());
                        self.search.focus = SearchFocus::Album;
                        self.push_log(LogLevel::Error, format!("MusicBrainz search failed: {err}"));
                        self.toast(LogLevel::Error, "MusicBrainz search failed");
                    }
                }
            }
            UiEvent::Slsk { generation, event } => {
                if generation == self.session_generation {
                    self.handle_slsk_event(event);
                }
            }
            UiEvent::MbRelease { generation, result } => {
                if generation != self.generation {
                    return;
                }
                self.search.phase = SearchPhase::Idle;
                match result {
                    Ok(release) => {
                        let count = release.tracks.len();
                        self.push_log(
                            LogLevel::Info,
                            format!(
                                "Loaded “{} — {}” ({count} audio track(s), MBID {}).",
                                release.artist, release.title, release.mbid
                            ),
                        );
                        self.tracks = Some(TracksView::build(release, &self.cfg));
                        if let Some(tracks) = &mut self.tracks
                            && !tracks.tracks.is_empty()
                        {
                            tracks.list_state.select(Some(0));
                        }
                        self.screen = Screen::Tracks;
                        if self.auto_yes {
                            self.auto_yes = false;
                            self.start_download();
                        }
                    }
                    Err(err) => {
                        self.search.error = Some(err.clone());
                        self.push_log(LogLevel::Error, format!("Could not load release: {err}"));
                        self.toast(LogLevel::Error, "Could not load tracklist");
                    }
                }
            }
            UiEvent::UpdateStarted(version) => {
                self.update_state = UpdateState::Downloading {
                    version: version.clone(),
                };
                self.push_log(LogLevel::Info, format!("Downloading soulsucker {version}…"));
                self.toast(LogLevel::Info, format!("Updating to {version}…"));
            }
            UiEvent::UpdateDone(result) => match result {
                Ok(outcome) => {
                    let (level, message) = match &outcome {
                        UpdateOutcome::UpToDate { current, latest } => (
                            LogLevel::Success,
                            format!("soulsucker {current} is up to date (latest {latest})"),
                        ),
                        UpdateOutcome::NewerLocal { current, latest } => (
                            LogLevel::Info,
                            format!(
                                "Local build {current} is newer than the latest release ({latest})"
                            ),
                        ),
                        UpdateOutcome::Updated { version } => (
                            LogLevel::Success,
                            format!("Updated to soulsucker {version} — restart to use it"),
                        ),
                    };
                    self.push_log(level, message.clone());
                    self.toast(level, message);
                    self.update_state = UpdateState::Done(outcome);
                }
                Err(error) => {
                    self.push_log(LogLevel::Error, format!("Update failed: {error}"));
                    self.toast(LogLevel::Error, "Update failed");
                    self.update_state = UpdateState::Failed(error);
                }
            },
        }
    }

    fn handle_slsk_event(&mut self, event: SlskEvent) {
        match event {
            SlskEvent::Connecting => {
                self.connection = ConnState::Connecting;
            }
            SlskEvent::LoginOk { username, port } => {
                self.connection = ConnState::Online {
                    username: username.clone(),
                    port,
                };
                let detail = port
                    .map(|port| format!(", listening on {port}"))
                    .unwrap_or_default();
                self.push_log(
                    LogLevel::Success,
                    format!("Logged in to Soulseek as {username}{detail}."),
                );
                self.send_pending_download();
            }
            SlskEvent::LoginFailed(message) => {
                // The session thread logs the detailed reason and hint.
                self.connection = ConnState::Error(message.clone());
                self.toast(LogLevel::Error, format!("Soulseek login failed: {message}"));
            }
            SlskEvent::Disconnected(message) => {
                // The session thread logs this with a hint.
                self.connection = ConnState::Error(message.clone());
                self.toast(LogLevel::Warn, format!("Soulseek disconnected: {message}"));
            }
            SlskEvent::AlbumSearchStarted {
                job_id,
                query,
                timeout_secs,
            } => {
                if let Some(view) = &mut self.download
                    && view.job_id == job_id
                {
                    view.phase = DownloadPhase::Searching;
                    view.waiting_for_login = false;
                    view.retry_available = true;
                    view.search_interrupted = false;
                    view.query = query.clone();
                    view.set_all_notes("searching Soulseek…");
                }
                self.push_log(
                    LogLevel::Info,
                    format!("Searching Soulseek for “{query}” ({timeout_secs}s)…"),
                );
            }
            SlskEvent::AlbumSearchProgress { job_id, peers } => {
                if let Some(view) = &mut self.download
                    && view.job_id == job_id
                {
                    view.peers = peers;
                    view.set_all_notes(&format!("searching Soulseek · {peers} peer(s) responded"));
                }
            }
            SlskEvent::SearchInterrupted { job_id } => {
                if let Some(view) = &mut self.download
                    && view.job_id == job_id
                {
                    view.peers = 0;
                    view.search_interrupted = true;
                    view.set_all_notes("search interrupted — reconnecting");
                }
            }
            SlskEvent::SourcesReady {
                job_id,
                per_track,
                peers,
            } => {
                if let Some(view) = &mut self.download
                    && view.job_id == job_id
                {
                    view.peers = peers;
                    view.phase = DownloadPhase::Downloading;
                    for (index, row) in view.rows.iter_mut().enumerate() {
                        let count = per_track.get(index).copied().unwrap_or(0);
                        row.attempts_total = count;
                        if count == 0
                            && !matches!(
                                row.status,
                                TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
                            )
                        {
                            row.note = "no matching FLAC source".to_string();
                        } else if row.status == TrackStatus::Queued {
                            row.note = format!("{count} source(s) available");
                        }
                    }
                }
            }
            SlskEvent::Track(update) => {
                if let Some(view) = &mut self.download
                    && view.job_id == update.job_id
                {
                    if matches!(
                        view.phase,
                        DownloadPhase::Waiting | DownloadPhase::Searching
                    ) && update.status != TrackStatus::Queued
                    {
                        view.phase = DownloadPhase::Downloading;
                    }
                    view.update_row(&update);
                }
            }
            SlskEvent::AlbumComplete {
                job_id,
                completed,
                failed,
                cancelled,
                elapsed,
            } => {
                if let Some(view) = &mut self.download
                    && view.job_id == job_id
                {
                    view.phase = DownloadPhase::Complete;
                    view.finished = Some(Instant::now());
                    view.completed = completed;
                    view.failed = failed;
                    view.cancelled = cancelled;
                    if failed == 0 && cancelled == 0 {
                        self.toast(
                            LogLevel::Success,
                            format!(
                                "Album complete — {completed} track(s) in {}",
                                util::fmt_elapsed(elapsed)
                            ),
                        );
                    } else {
                        self.toast(
                                LogLevel::Warn,
                                format!(
                                    "Finished: {completed} downloaded, {failed} failed, {cancelled} cancelled"
                                ),
                            );
                    }
                }
                self.send_pending_download();
            }
            SlskEvent::Log(level, message) => {
                self.push_log(level, message);
            }
            SlskEvent::SessionClosed => {
                self.connection = ConnState::Disconnected;
            }
        }
    }

    pub fn push_log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push(LogEntry {
            at: Instant::now(),
            level,
            message: message.into(),
        });
        if self.logs.len() > MAX_LOG_LINES {
            let excess = self.logs.len() - MAX_LOG_LINES;
            self.logs.drain(..excess);
        }
    }

    pub fn toast(&mut self, level: LogLevel, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            level,
            at: Instant::now(),
        });
    }

    pub fn effective_fingerprint(&self) -> bool {
        self.fingerprint_override.unwrap_or(self.cfg.fingerprint)
    }

    pub fn fingerprint_state(&self) -> (&'static str, bool) {
        if !self.effective_fingerprint() {
            ("off", true)
        } else if self.cfg.acoustid_key.trim().is_empty() {
            ("on, but no AcoustID key — duration only", false)
        } else if !self.fpcalc_available {
            ("on, but fpcalc missing — duration only", false)
        } else {
            ("on — Chromaprint + AcoustID", true)
        }
    }

    fn spawn_session(&mut self) {
        if !self.cfg.has_credentials() {
            return;
        }
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
        if let Some(view) = &mut self.download {
            // A new manager has no album state for an already-finished job.
            // A waiting job gets this switched back on when it is submitted.
            view.retry_available = false;
        }
        self.connection = ConnState::Connecting;
        self.session_generation += 1;
        let generation = self.session_generation;
        let (slsk_tx, slsk_rx) = mpsc::channel();
        let ui_tx = self.events_tx.clone();
        thread::Builder::new()
            .name("session-events".to_string())
            .spawn(move || {
                while let Ok(event) = slsk_rx.recv() {
                    if ui_tx.send(UiEvent::Slsk { generation, event }).is_err() {
                        break;
                    }
                }
            })
            .ok();
        self.session = Some(SessionHandle::spawn(&self.cfg, slsk_tx));
    }

    fn reconnect(&mut self) {
        if self.cfg.has_credentials() {
            if let Some(session) = &self.session {
                session.reconnect();
                self.connection = ConnState::Connecting;
                self.toast(LogLevel::Info, "Reconnecting to Soulseek…");
            } else {
                self.spawn_session();
            }
        } else {
            self.screen = Screen::Setup;
            self.setup = SetupForm::from_config(&self.cfg);
        }
    }

    fn check_for_updates(&mut self) {
        if self.update_state.busy() {
            return;
        }
        if matches!(
            self.update_state,
            UpdateState::Done(UpdateOutcome::Updated { .. })
        ) {
            self.toast(
                LogLevel::Warn,
                "Restart soulsucker to finish the previous update",
            );
            return;
        }
        self.update_state = UpdateState::Checking;
        self.settings.error = None;
        self.push_log(
            LogLevel::Info,
            "Checking GitHub for the latest soulsucker release…",
        );
        self.toast(LogLevel::Info, "Checking for updates…");
        let tx = self.events_tx.clone();
        let current = env!("CARGO_PKG_VERSION").to_string();
        let spawned = thread::Builder::new()
            .name("soulsucker-update".to_string())
            .spawn(move || {
                let result = (|| -> anyhow::Result<UpdateOutcome> {
                    let release = update::latest_release()?;
                    match update::compare_versions(&release.version, &current)? {
                        Ordering::Greater => {
                            let asset = update::platform_asset(&release)?;
                            let version = release.version.clone();
                            let _ = tx.send(UiEvent::UpdateStarted(version.clone()));
                            update::install(asset)?;
                            Ok(UpdateOutcome::Updated { version })
                        }
                        Ordering::Equal => Ok(UpdateOutcome::UpToDate {
                            current: current.clone(),
                            latest: release.version,
                        }),
                        Ordering::Less => Ok(UpdateOutcome::NewerLocal {
                            current: current.clone(),
                            latest: release.version,
                        }),
                    }
                })();
                let _ = tx.send(UiEvent::UpdateDone(result.map_err(|err| err.to_string())));
            });
        if spawned.is_err() {
            self.update_state = UpdateState::Failed("could not start the update worker".into());
            self.toast(LogLevel::Error, "Could not start the update worker");
        }
    }

    fn send_pending_download(&mut self) {
        let Some(request) = self.pending_start.take() else {
            return;
        };
        let Some(view) = &mut self.download else {
            return;
        };
        if view.phase != DownloadPhase::Waiting {
            return;
        }
        let Some(session) = &self.session else {
            self.pending_start = Some(request);
            return;
        };
        if !matches!(self.connection, ConnState::Online { .. }) {
            self.pending_start = Some(request);
            return;
        }
        session.start_download(request);
        view.phase = DownloadPhase::Searching;
        view.waiting_for_login = false;
        view.retry_available = true;
        view.set_all_notes("searching Soulseek…");
    }

    fn start_mb_search(&mut self) {
        let artist = self.search.artist.value().trim().to_string();
        let album = self.search.album.value().trim().to_string();
        if artist.is_empty() || album.is_empty() {
            self.search.focus = if artist.is_empty() {
                SearchFocus::Artist
            } else {
                SearchFocus::Album
            };
            self.toast(LogLevel::Warn, "Enter both an artist and an album");
            return;
        }
        if self.search.phase.is_loading() {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.search.phase = SearchPhase::Searching {
            started: Instant::now(),
        };
        self.search.error = None;
        self.search.results.clear();
        self.search.list_state.select(None);
        self.search.query_label = format!("{artist} — {album}");
        self.push_log(
            LogLevel::Info,
            format!("Searching MusicBrainz for “{artist} — {album}”…"),
        );
        self.toast(LogLevel::Info, "Searching MusicBrainz…");
        let tx = self.events_tx.clone();
        let limit = self.cfg.mb_limit;
        let spawned = thread::Builder::new()
            .name("musicbrainz-search".to_string())
            .spawn(move || {
                let result = musicbrainz::search_releases(&artist, &album, limit)
                    .map_err(|err| err.to_string());
                let _ = tx.send(UiEvent::MbSearch { generation, result });
            });
        if spawned.is_err() {
            self.search.phase = SearchPhase::Idle;
            self.search.error = Some("Could not start a background search worker".into());
            self.toast(LogLevel::Error, "Could not start the search worker");
        }
    }

    fn choose_release(&mut self) {
        if self.search.phase.is_loading() {
            return;
        }
        let Some(index) = self.search.selected() else {
            return;
        };
        let Some(candidate) = self.search.results.get(index).cloned() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.search.phase = SearchPhase::Fetching {
            started: Instant::now(),
        };
        self.search.error = None;
        self.push_log(
            LogLevel::Info,
            format!(
                "Fetching tracklist for “{} — {}”…",
                candidate.artist, candidate.title
            ),
        );
        self.toast(LogLevel::Info, "Fetching tracklist…");
        let mbid = candidate.mbid.clone();
        let tx = self.events_tx.clone();
        let spawned = thread::Builder::new()
            .name("musicbrainz-release".to_string())
            .spawn(move || {
                let result = musicbrainz::get_release(&mbid).map_err(|err| err.to_string());
                let _ = tx.send(UiEvent::MbRelease { generation, result });
            });
        if spawned.is_err() {
            self.search.phase = SearchPhase::Idle;
            self.search.error = Some("Could not start the tracklist worker".into());
            self.toast(LogLevel::Error, "Could not start the tracklist worker");
        }
    }

    fn start_download(&mut self) {
        let Some(tracks_view) = &self.tracks else {
            return;
        };
        if tracks_view.tracks.is_empty() {
            self.toast(LogLevel::Warn, "This release has no audio tracks");
            return;
        }
        self.job_counter += 1;
        let job_id = self.job_counter;
        let release = tracks_view.release.clone();
        let album_dir = tracks_view.album_dir.clone();
        let planned = tracks_view.tracks.clone();
        if let Err(err) = std::fs::create_dir_all(&album_dir) {
            self.toast(
                LogLevel::Error,
                format!("Could not create album folder: {err}"),
            );
            return;
        }
        let online = matches!(self.connection, ConnState::Online { .. });
        let options = DownloadOptions {
            max_concurrent: self.cfg.max_concurrent,
            search_timeout: Duration::from_secs(self.cfg.search_timeout_secs),
            queue_timeout: Duration::from_secs(self.cfg.queue_timeout_secs),
            stall_timeout: Duration::from_secs(self.cfg.stall_timeout_secs),
            length_tolerance: self.cfg.length_tolerance_secs,
            api_key: if self.effective_fingerprint() {
                self.cfg.acoustid_key.clone()
            } else {
                String::new()
            },
            fingerprint_min_score: self.cfg.fingerprint_min_score,
            last_resort_rounds: self.cfg.last_resort_rounds,
            last_resort_stall: Duration::from_secs(self.cfg.last_resort_stall_timeout_secs),
            candidates_per_track: self.cfg.candidates_per_track,
            disc_prefix_track_numbers: self.cfg.disc_prefix_track_numbers,
            album_artist_for_tracks: self.cfg.album_artist_for_tracks,
        };
        let request = DownloadRequest {
            job_id,
            artist: release.artist.clone(),
            album: release.title.clone(),
            date: release.date.clone(),
            release_mbid: release.mbid.clone(),
            release_group_mbid: release.release_group_mbid.clone(),
            album_dir: album_dir.clone(),
            tracks: planned.iter().map(|track| track.meta.clone()).collect(),
            options,
            force: self.force,
        };
        let waiting = !online || self.session.is_none();
        let view = DownloadView::new(job_id, &release, album_dir.clone(), &planned, waiting);
        self.download = Some(view);
        self.screen = Screen::Download;
        self.push_log(LogLevel::Info, format!("Saving to {}", album_dir.display()));
        if waiting {
            self.pending_start = Some(request);
            self.toast(LogLevel::Info, "Waiting for Soulseek login…");
        } else {
            self.pending_start = None;
            if let Some(session) = &self.session {
                session.start_download(request);
            }
            self.toast(LogLevel::Info, "Starting download…");
        }
    }

    fn download_running(&self) -> bool {
        self.download.as_ref().is_some_and(|view| {
            matches!(
                view.phase,
                DownloadPhase::Searching | DownloadPhase::Downloading
            )
        })
    }

    fn request_quit(&mut self) {
        if self.download_running() {
            self.confirm = Some(ConfirmDialog {
                title: "Quit soulsucker?".to_string(),
                lines: vec![
                    "Downloads are still running.".to_string(),
                    "Quitting will cancel them and remove partial files.".to_string(),
                ],
                action: ConfirmAction::Quit,
            });
        } else {
            self.quit = true;
        }
    }

    fn request_new_search(&mut self) {
        if self.download_running() {
            self.confirm = Some(ConfirmDialog {
                title: "Cancel downloads and start over?".to_string(),
                lines: vec![
                    "Every active transfer will be cancelled.".to_string(),
                    "Tracks already saved will stay on disk.".to_string(),
                ],
                action: ConfirmAction::NewSearch,
            });
        } else {
            self.do_new_search();
        }
    }

    fn do_new_search(&mut self) {
        if let (Some(view), Some(session)) = (&self.download, &self.session) {
            session.cancel_all(view.job_id);
        }
        self.download = None;
        self.pending_start = None;
        self.search.results.clear();
        self.search.list_state.select(None);
        self.search.phase = SearchPhase::Idle;
        self.search.error = None;
        self.search.focus = if self.search.artist.is_empty() {
            SearchFocus::Artist
        } else {
            SearchFocus::Album
        };
        self.screen = Screen::Search;
    }

    fn open_settings(&mut self) {
        if matches!(self.screen, Screen::Setup | Screen::Settings) {
            return;
        }
        if self.download_running() {
            self.toast(
                LogLevel::Warn,
                "Finish or cancel the current download before opening settings",
            );
            return;
        }
        if self.search.phase.is_loading() {
            self.toast(
                LogLevel::Info,
                "Wait for the MusicBrainz request to finish (Esc cancels it)",
            );
            return;
        }
        self.previous_screen = self.screen;
        self.settings = SettingsForm::from_config(&self.persisted_cfg);
        self.screen = Screen::Settings;
    }

    fn save_settings(&mut self) {
        if self.download_running() {
            self.toast(
                LogLevel::Warn,
                "A download started while settings were open; finish or cancel it first",
            );
            return;
        }
        let old = self.cfg.clone();
        let pending_waiting = self.pending_start.is_some()
            && self
                .download
                .as_ref()
                .is_some_and(|view| view.phase == DownloadPhase::Waiting);
        let new_config = match self.settings.to_config(&self.persisted_cfg) {
            Ok(config) => config,
            Err((index, message)) => {
                self.settings.set_selected(index);
                self.settings.error = Some(message.clone());
                self.toast(LogLevel::Error, message);
                return;
            }
        };
        if let Err(err) = config::save(&new_config, &self.cfg_path) {
            self.settings.error = Some(format!("Could not save settings: {err}"));
            self.toast(LogLevel::Error, format!("Could not save settings: {err}"));
            return;
        }
        let restart = old.username != new_config.username
            || old.password != new_config.password
            || old.listen_port != new_config.listen_port;
        let folder_changed = old.music_dir != new_config.music_dir;
        self.persisted_cfg = new_config.clone();
        self.cfg = new_config;
        self.fingerprint_override = None;
        self.settings.error = None;
        self.push_log(
            LogLevel::Success,
            format!("Settings saved to {}", self.cfg_path.display()),
        );
        if restart || self.session.is_none() {
            self.spawn_session();
        }
        if self.previous_screen == Screen::Tracks
            && let Some(view) = &self.tracks
        {
            let release = view.release.clone();
            self.tracks = Some(TracksView::build(release, &self.cfg));
            if let Some(tracks) = &mut self.tracks
                && !tracks.tracks.is_empty()
            {
                tracks.list_state.select(Some(0));
            }
        }
        self.screen = self.previous_screen;
        if pending_waiting && folder_changed {
            self.toast(
                LogLevel::Warn,
                "Settings saved; the pending download keeps the old music folder",
            );
        } else {
            self.toast(LogLevel::Success, "Settings saved");
        }
    }

    fn save_setup(&mut self) {
        let username = self.setup.username.value().trim().to_string();
        let password = self.setup.password.value();
        if username.is_empty() {
            self.setup.error = Some("Soulseek username is required".to_string());
            self.setup.focus = 0;
            return;
        }
        if password.is_empty() {
            self.setup.error = Some("Soulseek password is required".to_string());
            self.setup.focus = 1;
            return;
        }
        let acoustid = self.setup.acoustid.value().trim().to_string();
        self.persisted_cfg.username = username.clone();
        self.persisted_cfg.password = password.clone();
        self.persisted_cfg.acoustid_key = acoustid.clone();
        if let Err(err) = config::save(&self.persisted_cfg, &self.cfg_path) {
            self.setup.error = Some(format!("Could not save credentials: {err}"));
            return;
        }
        // Keep any runtime-only CLI overrides, but use the new credentials.
        self.cfg.username = username;
        self.cfg.password = password;
        self.cfg.acoustid_key = acoustid;
        self.setup.error = None;
        self.push_log(
            LogLevel::Success,
            format!("Credentials saved to {}", self.cfg_path.display()),
        );
        self.screen = Screen::Search;
        self.spawn_session();
        if !self.search.artist.is_empty() && !self.search.album.is_empty() {
            self.start_mb_search();
        } else {
            self.search.focus = if self.search.artist.is_empty() {
                SearchFocus::Artist
            } else {
                SearchFocus::Album
            };
        }
        self.toast(LogLevel::Success, "Credentials saved");
    }

    fn cancel_selected_track(&mut self) {
        let Some(view) = &self.download else {
            return;
        };
        if view.phase == DownloadPhase::Waiting {
            self.toast(LogLevel::Info, "Wait for Soulseek login first");
            return;
        }
        let Some(selected) = view.selected() else {
            return;
        };
        let Some(row) = view.rows.get(selected) else {
            return;
        };
        if matches!(
            row.status,
            TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
        ) {
            self.toast(LogLevel::Warn, "That track is not active");
            return;
        }
        if let Some(session) = &self.session {
            session.cancel_track(view.job_id, row.index);
            self.toast(LogLevel::Info, "Cancelling track…");
        }
    }

    fn retry_failed(&mut self) {
        let Some(view) = &self.download else {
            return;
        };
        if !view.retry_available {
            self.toast(
                LogLevel::Warn,
                "The Soulseek session was restarted — press n for a new search",
            );
            return;
        }
        let failed_rows = view
            .rows
            .iter()
            .filter(|row| row.status == TrackStatus::Failed)
            .count();
        if failed_rows == 0 {
            self.toast(LogLevel::Info, "No failed tracks to retry");
            return;
        }
        let retryable = view
            .rows
            .iter()
            .any(|row| row.status == TrackStatus::Failed && row.attempts_total > 0);
        if !retryable {
            self.toast(
                LogLevel::Warn,
                "Failed tracks had no matching sources — run a new search",
            );
            return;
        }
        if let Some(session) = &self.session {
            session.retry_failed(view.job_id);
            if let Some(view) = &mut self.download {
                view.phase = DownloadPhase::Downloading;
                view.finished = None;
                view.completed = 0;
                view.failed = 0;
                view.cancelled = 0;
            }
            self.toast(LogLevel::Info, "Retrying failed tracks…");
        }
    }

    fn open_album_folder(&mut self) {
        if let Some(view) = &self.download {
            match util::open_in_file_manager(&view.album_dir) {
                Ok(()) => self.toast(LogLevel::Info, "Opened album folder"),
                Err(err) => self.toast(LogLevel::Warn, format!("Could not open folder: {err}")),
            }
        }
    }

    // ---------------------------------------------------------------------
    // Keyboard
    // ---------------------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        if self.help_open {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') | KeyCode::F(1)
            ) || key.code == KeyCode::Char('q')
            {
                self.help_open = false;
            }
            return;
        }
        if self.confirm.is_some() {
            self.handle_confirm_key(key);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.request_quit();
                    return;
                }
                KeyCode::Char('s') if self.screen == Screen::Setup => {
                    self.save_setup();
                    return;
                }
                KeyCode::Char('s') if self.screen == Screen::Settings => {
                    if self.settings.editing.is_some() {
                        self.settings.commit_edit();
                        if self.settings.editing.is_some() {
                            return;
                        }
                    }
                    self.save_settings();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::F(1) => {
                self.help_open = true;
                return;
            }
            KeyCode::F(2) => {
                self.open_settings();
                return;
            }
            KeyCode::F(5) => {
                self.reconnect();
                return;
            }
            _ => {}
        }

        match self.screen {
            Screen::Setup => self.handle_setup_key(key),
            Screen::Search => self.handle_search_key(key),
            Screen::Tracks => self.handle_tracks_key(key),
            Screen::Download => self.handle_download_key(key),
            Screen::Settings => self.handle_settings_key(key),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(confirm) = self.confirm.take() {
                    match confirm.action {
                        ConfirmAction::Quit => self.quit = true,
                        ConfirmAction::NewSearch => self.do_new_search(),
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.confirm = None;
            }
            _ => {}
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.setup.focus = (self.setup.focus + 1) % 3;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.setup.focus = (self.setup.focus + 2) % 3;
            }
            KeyCode::Enter => {
                if self.setup.focus < 2 {
                    self.setup.focus += 1;
                } else {
                    self.save_setup();
                }
            }
            KeyCode::Esc => {
                self.requests_quit_without_download();
            }
            _ => {
                let input = match self.setup.focus {
                    0 => &mut self.setup.username,
                    1 => &mut self.setup.password,
                    _ => &mut self.setup.acoustid,
                };
                input.handle_key(key);
            }
        }
    }

    fn requests_quit_without_download(&mut self) {
        self.quit = true;
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        let loading = self.search.phase.is_loading();
        match key.code {
            KeyCode::Tab => {
                self.cycle_search_focus(true);
            }
            KeyCode::BackTab => {
                self.cycle_search_focus(false);
            }
            KeyCode::Esc => {
                if loading {
                    self.generation += 1;
                    self.search.phase = SearchPhase::Idle;
                    self.search.error = None;
                    self.auto_yes = false;
                    self.toast(LogLevel::Info, "Search cancelled");
                } else if self.search.focus != SearchFocus::Results {
                    if !self.search.results.is_empty() {
                        self.search.focus = SearchFocus::Results;
                    } else {
                        self.search.focus = SearchFocus::Artist;
                    }
                } else {
                    self.search.focus = SearchFocus::Album;
                }
            }
            KeyCode::Enter => match self.search.focus {
                SearchFocus::Artist => self.search.focus = SearchFocus::Album,
                SearchFocus::Album => {
                    if !loading {
                        self.start_mb_search();
                    }
                }
                SearchFocus::Results => {
                    if !loading {
                        self.choose_release();
                    }
                }
            },
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
                if self.search.focus == SearchFocus::Results =>
            {
                self.move_search_selection(key.code);
            }
            _ => {
                if loading {
                    return;
                }
                match self.search.focus {
                    SearchFocus::Artist => {
                        self.search.artist.handle_key(key);
                    }
                    SearchFocus::Album => {
                        self.search.album.handle_key(key);
                    }
                    SearchFocus::Results => match key.code {
                        KeyCode::Char('s') => self.open_settings(),
                        KeyCode::Char('r') => self.start_mb_search(),
                        KeyCode::Char('q') => self.request_quit(),
                        KeyCode::Char('/') | KeyCode::Char('e') | KeyCode::Char('i') => {
                            self.search.focus = SearchFocus::Album;
                        }
                        KeyCode::Char('?') => self.help_open = true,
                        _ => {}
                    },
                }
            }
        }
    }

    fn cycle_search_focus(&mut self, forward: bool) {
        let has_results = !self.search.results.is_empty();
        self.search.focus = match (self.search.focus, forward, has_results) {
            (SearchFocus::Artist, true, _) => SearchFocus::Album,
            (SearchFocus::Artist, false, true) => SearchFocus::Results,
            (SearchFocus::Artist, false, false) => SearchFocus::Album,
            (SearchFocus::Album, true, true) => SearchFocus::Results,
            (SearchFocus::Album, true, false) => SearchFocus::Artist,
            (SearchFocus::Album, false, _) => SearchFocus::Artist,
            (SearchFocus::Results, true, _) => SearchFocus::Artist,
            (SearchFocus::Results, false, _) => SearchFocus::Album,
        };
    }

    fn move_search_selection(&mut self, code: KeyCode) {
        let len = self.search.results.len();
        if len == 0 {
            return;
        }
        let current = self.search.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => (current + 1).min(len - 1),
            KeyCode::PageUp => current.saturating_sub(10),
            KeyCode::PageDown => (current + 10).min(len - 1),
            KeyCode::Home => 0,
            KeyCode::End => len - 1,
            _ => current,
        };
        self.search.set_selected(next);
    }

    fn handle_tracks_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                if let Some(tracks) = &mut self.tracks {
                    move_list_selection(&mut tracks.list_state, tracks.tracks.len(), key.code);
                }
            }
            KeyCode::Enter | KeyCode::Char('d') => self.start_download(),
            KeyCode::Char('f') => {
                let current = self.effective_fingerprint();
                self.fingerprint_override = Some(!current);
                if !current {
                    let (state, ok) = self.fingerprint_state();
                    self.toast(
                        if ok {
                            LogLevel::Success
                        } else {
                            LogLevel::Warn
                        },
                        format!("Fingerprint verification {state}"),
                    );
                } else {
                    self.toast(LogLevel::Info, "Fingerprint verification off for this run");
                }
            }
            KeyCode::Char('b') | KeyCode::Esc => {
                self.screen = Screen::Search;
                self.search.focus = if self.search.results.is_empty() {
                    SearchFocus::Album
                } else {
                    SearchFocus::Results
                };
            }
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.help_open = true,
            _ => {}
        }
    }

    fn handle_download_key(&mut self, key: KeyEvent) {
        match self.download.as_ref().map(|view| view.focus) {
            Some(DownloadFocus::Log) => match key.code {
                KeyCode::Up => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = (view.log_offset + 1).min(self.logs.len());
                    }
                }
                KeyCode::Down => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = view.log_offset.saturating_sub(1);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = (view.log_offset + 10).min(self.logs.len());
                    }
                }
                KeyCode::PageDown => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = view.log_offset.saturating_sub(10);
                    }
                }
                KeyCode::Home => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = self.logs.len();
                    }
                }
                KeyCode::End => {
                    if let Some(view) = &mut self.download {
                        view.log_offset = 0;
                    }
                }
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                    if let Some(view) = &mut self.download {
                        view.focus = DownloadFocus::Tracks;
                    }
                }
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Char('n') => self.request_new_search(),
                KeyCode::Char('r') => self.retry_failed(),
                KeyCode::Char('?') => self.help_open = true,
                _ => {}
            },
            _ => match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End => {
                    if let Some(view) = &mut self.download {
                        move_list_selection(&mut view.list_state, view.rows.len(), key.code);
                    }
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    if let Some(view) = &mut self.download {
                        view.focus = DownloadFocus::Log;
                    }
                }
                KeyCode::Char('x') => self.cancel_selected_track(),
                KeyCode::Char('r') => self.retry_failed(),
                KeyCode::Char('n') => self.request_new_search(),
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Char('s') => self.open_settings(),
                KeyCode::Char('o') => self.open_album_folder(),
                KeyCode::Char('?') => self.help_open = true,
                KeyCode::Esc => {
                    let phase = self.download.as_ref().map(|view| view.phase);
                    match phase {
                        Some(DownloadPhase::Complete) => {
                            self.screen = Screen::Search;
                            self.search.focus = if self.search.results.is_empty() {
                                SearchFocus::Album
                            } else {
                                SearchFocus::Results
                            };
                        }
                        Some(DownloadPhase::Waiting) => {
                            self.pending_start = None;
                            self.download = None;
                            self.screen = Screen::Search;
                            self.search.focus = if self.search.results.is_empty() {
                                SearchFocus::Album
                            } else {
                                SearchFocus::Results
                            };
                            self.toast(LogLevel::Info, "Pending download cancelled");
                        }
                        _ => {
                            self.toast(
                                LogLevel::Warn,
                                "Cancel the download first (x per track, or n for a new search)",
                            );
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        if self.settings.editing.is_some() {
            match key.code {
                KeyCode::Enter => self.settings.commit_edit(),
                KeyCode::Esc => {
                    self.settings.editing = None;
                    self.settings.error = None;
                }
                _ => {
                    if let Some(edit) = &mut self.settings.editing {
                        edit.input.handle_key(key);
                    }
                }
            }
            return;
        }

        match key.code {
            KeyCode::Up => {
                let index = self.settings.selected.saturating_sub(1);
                self.settings.set_selected(index);
            }
            KeyCode::Down => {
                let index = (self.settings.selected + 1).min(self.settings.fields.len() - 1);
                self.settings.set_selected(index);
            }
            KeyCode::Home => self.settings.set_selected(0),
            KeyCode::End => {
                if !self.settings.fields.is_empty() {
                    self.settings.set_selected(self.settings.fields.len() - 1);
                }
            }
            KeyCode::Enter => {
                let kind = self.settings.selected_field().map(|field| field.kind);
                if kind.is_some_and(SettingKind::is_bool) {
                    self.settings.toggle_selected();
                } else if kind.is_some_and(SettingKind::is_action) {
                    self.check_for_updates();
                } else {
                    self.settings.begin_edit();
                }
            }
            KeyCode::Char(' ') => {
                if self
                    .settings
                    .selected_field()
                    .is_some_and(|field| field.kind.is_bool())
                {
                    self.settings.toggle_selected();
                }
            }
            KeyCode::Esc => {
                self.screen = self.previous_screen;
            }
            KeyCode::Char('?') => self.help_open = true,
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }
}

fn move_list_selection(state: &mut ratatui::widgets::ListState, len: usize, code: KeyCode) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0);
    let next = match code {
        KeyCode::Up => current.saturating_sub(1),
        KeyCode::Down => (current + 1).min(len - 1),
        KeyCode::PageUp => current.saturating_sub(10),
        KeyCode::PageDown => (current + 10).min(len - 1),
        KeyCode::Home => 0,
        KeyCode::End => len - 1,
        _ => current,
    };
    state.select(Some(next));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Config {
        Config {
            username: "user".into(),
            password: "pass".into(),
            acoustid_key: "key".into(),
            ..Config::default()
        }
    }

    fn release_for_test(primary_type: &str) -> crate::musicbrainz::ReleaseInfo {
        crate::musicbrainz::ReleaseInfo {
            mbid: "release".into(),
            release_group_mbid: "group".into(),
            title: "Album".into(),
            artist: "Artist".into(),
            date: "2020".into(),
            country: "GB".into(),
            status: "Official".into(),
            disambiguation: String::new(),
            primary_type: primary_type.into(),
            tracks: vec![crate::musicbrainz::TrackInfo {
                index: 0,
                disc: 1,
                position: 1,
                title: "Song".into(),
                artist: "Artist".into(),
                length_ms: Some(180_000),
                recording_mbid: "rec".into(),
                track_mbid: "track".into(),
            }],
        }
    }

    fn download_view_for_test() -> DownloadView {
        let release = release_for_test("Album");
        let tracks = TracksView::build(release.clone(), &Config::default());
        let planned = tracks.tracks.clone();
        DownloadView::new(1, &release, tracks.album_dir, &planned, false)
    }

    #[test]
    fn albums_are_saved_under_an_artist_folder() {
        let config = Config {
            music_dir: "/tmp/soulsucker-layout".into(),
            ..Config::default()
        };
        let tracks = TracksView::build(release_for_test("Album"), &config);
        assert_eq!(
            tracks.album_dir,
            PathBuf::from("/tmp/soulsucker-layout/Artist/Album")
        );
        assert_eq!(
            tracks.tracks[0].meta.final_path,
            PathBuf::from("/tmp/soulsucker-layout/Artist/Album/01 - Song.flac")
        );
    }

    #[test]
    fn singles_are_saved_as_loose_files_in_the_artist_folder() {
        let config = Config {
            music_dir: "/tmp/soulsucker-layout".into(),
            ..Config::default()
        };
        let tracks = TracksView::build(release_for_test("Single"), &config);
        assert_eq!(
            tracks.album_dir,
            PathBuf::from("/tmp/soulsucker-layout/Artist")
        );
        assert_eq!(
            tracks.tracks[0].meta.final_path,
            PathBuf::from("/tmp/soulsucker-layout/Artist/Song.flac")
        );
    }

    fn phase_update(attempt: usize) -> TrackUpdate {
        TrackUpdate {
            job_id: 1,
            index: 0,
            status: TrackStatus::Verifying,
            bytes_done: 0,
            total: 1,
            speed: 0.0,
            source: "user".into(),
            attempt,
            attempts_total: 2,
            note: "computing…".into(),
        }
    }

    #[test]
    fn stale_verification_updates_do_not_revive_terminal_rows() {
        let mut view = download_view_for_test();
        view.rows[0].status = TrackStatus::Cancelled;
        view.rows[0].attempt = 1;
        view.update_row(&phase_update(1));
        assert_eq!(view.rows[0].status, TrackStatus::Cancelled);

        view.rows[0].status = TrackStatus::Downloading;
        view.rows[0].attempt = 2;
        view.update_row(&phase_update(1));
        assert_eq!(view.rows[0].status, TrackStatus::Downloading);
    }

    #[test]
    fn settings_round_trip_into_a_config() {
        let base = configured();
        let mut form = SettingsForm::from_config(&base);
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::MaxConcurrent)
            .unwrap()
            .value = "8".into();
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::MusicDir)
            .unwrap()
            .value = "/tmp/music".into();
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::Fingerprint)
            .unwrap()
            .value = "off".into();
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::DiscPrefixTracks)
            .unwrap()
            .value = "on".into();
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::AlbumArtistTracks)
            .unwrap()
            .value = "on".into();

        let config = form.to_config(&base).unwrap();
        assert_eq!(config.max_concurrent, 8);
        assert_eq!(config.music_dir, "/tmp/music");
        assert!(!config.fingerprint);
        assert!(config.disc_prefix_track_numbers);
        assert!(config.album_artist_for_tracks);
        assert_eq!(config.username, "user");
    }

    #[test]
    fn an_out_of_range_setting_is_rejected_with_its_field_index() {
        let base = configured();
        let mut form = SettingsForm::from_config(&base);
        let index = form
            .fields
            .iter()
            .position(|field| field.key == SettingKey::MaxConcurrent)
            .unwrap();
        form.fields[index].value = "99".into();
        let error = form.to_config(&base).unwrap_err();
        assert_eq!(error.0, index);
        assert!(error.1.contains("Parallel downloads"));
    }

    #[test]
    fn settings_cannot_save_empty_credentials() {
        let base = configured();
        let mut form = SettingsForm::from_config(&base);
        form.fields
            .iter_mut()
            .find(|field| field.key == SettingKey::Username)
            .unwrap()
            .value = "  ".into();
        assert!(form.to_config(&base).is_err());
    }
}
