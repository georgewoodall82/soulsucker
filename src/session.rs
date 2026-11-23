//! Soulseek session and the album download engine.
//!
//! One background thread owns the library client. Tracks are downloaded into
//! per-attempt staging folders (the library flattens files by basename), then
//! verified, optionally normalized/converted, tagged and moved to their final
//! numbered names.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use soulseek_rs::{Client, ClientSettings, DownloadStatus, PeerAddress, SoulseekRs};

use crate::acoustid::{self, VerifyResult, VerifySpec};
use crate::audio::{self, ReplayGain, TrackTags};
use crate::config::{AudioQuality, Config, NormalizeMethod};
use crate::cover;
use crate::matcher::{
    self, AlbumSource, Candidate, CandidateOptions, RemoteFile, SearchHit, TrackQuery,
};
use crate::musicbrainz::{self, PressingTrack, ReleaseInfo};
use crate::normalize::{self, Loudness};
use crate::util;

const STAGING_DIR: &str = ".soulsucker-staging";
const LAST_RESORT_BACKOFF: Duration = Duration::from_secs(5);
const MAX_AUTO_RECONNECTS: u32 = 3;
const VERIFY_DEADLINE: Duration = Duration::from_secs(150);
/// How many times one album may switch to a better-fitting pressing. Each
/// switch is driven by a real file's length, so more than this means the
/// files do not belong to any single pressing.
const MAX_PRESSING_SWITCHES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackStatus {
    Queued,
    Downloading,
    Verifying,
    Retrying,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct TrackUpdate {
    pub job_id: u64,
    pub index: usize,
    pub status: TrackStatus,
    pub bytes_done: u64,
    pub total: u64,
    pub speed: f64,
    pub source: String,
    pub attempt: usize,
    pub attempts_total: usize,
    pub note: String,
}

#[derive(Debug, Clone)]
pub enum SlskEvent {
    Connecting,
    LoginOk {
        username: String,
        port: Option<u16>,
    },
    LoginFailed(String),
    Disconnected(String),
    AlbumSearchStarted {
        job_id: u64,
        query: String,
        timeout_secs: u64,
    },
    AlbumSearchProgress {
        job_id: u64,
        peers: usize,
    },
    SearchInterrupted {
        job_id: u64,
    },
    SourcesReady {
        job_id: u64,
        per_track: Vec<usize>,
        peers: usize,
    },
    Track(TrackUpdate),
    AlbumComplete {
        job_id: u64,
        completed: usize,
        failed: usize,
        cancelled: usize,
        elapsed: Duration,
    },
    Log(LogLevel, String),
    SessionClosed,
}

#[derive(Clone)]
pub struct DownloadOptions {
    pub max_concurrent: usize,
    /// Checks, loudness measurements and conversions running at once.
    pub max_processing: usize,
    pub search_timeout: Duration,
    pub queue_timeout: Duration,
    pub stall_timeout: Duration,
    pub length_tolerance: f64,
    pub api_key: String,
    pub fingerprint_min_score: f64,
    pub last_resort_rounds: u32,
    pub last_resort_stall: Duration,
    pub candidates_per_track: usize,
    pub disc_prefix_track_numbers: bool,
    pub album_artist_for_tracks: bool,
    pub normalize_loudness: bool,
    /// Loudness the album should play at, in LUFS.
    pub normalize_target_lufs: f64,
    /// Whether the gain changes the audio or only becomes ReplayGain tags.
    pub normalize_method: NormalizeMethod,
    /// Save a 500x500 `cover.jpg` beside the tracks.
    pub folder_cover: bool,
    pub audio_quality: AudioQuality,
}

#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub index: usize,
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub artist: String,
    pub expected_secs: Option<f64>,
    pub recording_mbid: String,
    pub track_mbid: String,
    pub final_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub job_id: u64,
    pub artist: String,
    pub album: String,
    pub date: String,
    pub release_mbid: String,
    pub release_group_mbid: String,
    pub album_dir: PathBuf,
    /// The tracks are loose files in the artist folder (a single), so the
    /// folder does not belong to this release and gets no `cover.jpg`.
    pub loose_files: bool,
    pub tracks: Vec<TrackMeta>,
    pub options: DownloadOptions,
    pub force: bool,
}

impl std::fmt::Debug for DownloadOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadOptions")
            .field("max_concurrent", &self.max_concurrent)
            .field("max_processing", &self.max_processing)
            .field("search_timeout", &self.search_timeout)
            .field("queue_timeout", &self.queue_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("length_tolerance", &self.length_tolerance)
            .field("api_key", &"<redacted>")
            .field("fingerprint_min_score", &self.fingerprint_min_score)
            .field("last_resort_rounds", &self.last_resort_rounds)
            .field("last_resort_stall", &self.last_resort_stall)
            .field("candidates_per_track", &self.candidates_per_track)
            .field("disc_prefix_track_numbers", &self.disc_prefix_track_numbers)
            .field("album_artist_for_tracks", &self.album_artist_for_tracks)
            .field("normalize_loudness", &self.normalize_loudness)
            .field("normalize_target_lufs", &self.normalize_target_lufs)
            .field("normalize_method", &self.normalize_method)
            .field("folder_cover", &self.folder_cover)
            .field("audio_quality", &self.audio_quality)
            .finish()
    }
}

impl DownloadRequest {
    pub fn disc_total(&self) -> u32 {
        self.tracks
            .iter()
            .map(|track| track.disc)
            .max()
            .unwrap_or(1)
    }
}

#[derive(Debug)]
enum SessionCommand {
    StartDownload(Box<DownloadRequest>),
    CancelTrack {
        job_id: u64,
        index: usize,
    },
    RetryFailed {
        job_id: u64,
    },
    CancelAll {
        job_id: u64,
    },
    Reconnect,
    Shutdown,
    SearchDone {
        job_id: u64,
        search_attempt: u64,
        result: Result<Vec<SearchHit>, String>,
    },
    Verified {
        job_id: u64,
        index: usize,
        attempt_no: u32,
        result: VerifyResult,
        path: PathBuf,
        loudness: Option<Loudness>,
    },
    /// The probe track passed its duration/fingerprint check. Sent before
    /// loudness measurement or conversion so the rest of the album can start
    /// downloading while the probe file is still being processed.
    ProbeVerified {
        job_id: u64,
        index: usize,
        attempt_no: u32,
    },
    AlbumGainDone {
        job_id: u64,
        index: usize,
        attempt_no: u32,
        result: Result<PathBuf, String>,
    },
    PressingsFetched {
        job_id: u64,
        search_id: u64,
        result: Result<Vec<ReleaseInfo>, String>,
    },
    CoverReady {
        job_id: u64,
        result: Result<Option<FetchedCover>, String>,
    },
}

/// A downloaded cover, plus the `cover.jpg` made from it when that option is
/// on (the resize happens in the cover worker, off the session loop).
#[derive(Debug)]
struct FetchedCover {
    art: musicbrainz::CoverArt,
    folder_jpeg: Option<Result<Vec<u8>, String>>,
}

pub struct SessionHandle {
    tx: Sender<SessionCommand>,
    join: Option<thread::JoinHandle<()>>,
}

impl SessionHandle {
    pub fn spawn(config: &Config, events: Sender<SlskEvent>) -> Self {
        let (tx, rx) = mpsc::channel();
        let config = config.clone();
        let inner_tx = tx.clone();
        let join = thread::Builder::new()
            .name("soulseek-session".to_string())
            .spawn(move || run_session(config, events, rx, inner_tx))
            .expect("could not spawn the Soulseek session thread");
        Self {
            tx,
            join: Some(join),
        }
    }

    pub fn start_download(&self, request: DownloadRequest) {
        let _ = self
            .tx
            .send(SessionCommand::StartDownload(Box::new(request)));
    }

    pub fn cancel_track(&self, job_id: u64, index: usize) {
        let _ = self.tx.send(SessionCommand::CancelTrack { job_id, index });
    }

    pub fn retry_failed(&self, job_id: u64) {
        let _ = self.tx.send(SessionCommand::RetryFailed { job_id });
    }

    pub fn cancel_all(&self, job_id: u64) {
        let _ = self.tx.send(SessionCommand::CancelAll { job_id });
    }

    pub fn reconnect(&self) {
        let _ = self.tx.send(SessionCommand::Reconnect);
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(SessionCommand::Shutdown);
        if let Some(join) = self.join.take() {
            // Give the session a moment to cancel transfers and remove its
            // staging files, but never block the UI on a slow login.
            let deadline = Instant::now() + Duration::from_millis(1200);
            while !join.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                let _ = thread::Builder::new()
                    .name("soulseek-session-join".to_string())
                    .spawn(move || {
                        let _ = join.join();
                    });
            }
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// -------------------------------------------------------------------------
// Session thread
// -------------------------------------------------------------------------

enum ConnResult {
    Connected(Arc<Client>),
    Failed { message: String, auth: bool },
}

#[derive(Debug)]
pub enum LoginCheckError {
    Rejected(String),
    Unavailable(String),
}

/// Validate a username/password without leaving a session or listener behind.
pub fn verify_login(config: &Config) -> Result<(), LoginCheckError> {
    match connect_client_with(config, false) {
        ConnResult::Connected(client) => {
            if let Some(mut client) = Arc::into_inner(client) {
                client.disconnect();
            }
            Ok(())
        }
        ConnResult::Failed { message, auth } => {
            if auth {
                Err(LoginCheckError::Rejected(message))
            } else {
                Err(LoginCheckError::Unavailable(message))
            }
        }
    }
}

fn run_session(
    config: Config,
    events: Sender<SlskEvent>,
    commands: Receiver<SessionCommand>,
    inner_tx: Sender<SessionCommand>,
) {
    Session::new(config, events, commands, inner_tx).run();
}

/// A secondary Soulseek login used for parallel downloads. Peers accept one
/// transfer per username at a time, so each concurrent download runs on its
/// own account. The name is the main username plus the slot number and the
/// password is shared; the server creates unknown accounts on first login.
struct ExtraAccount {
    username: String,
    client: Option<Arc<Client>>,
    connector: Option<Receiver<ConnResult>>,
    retry_at: Option<Instant>,
    failures: u32,
    /// The server refused this login; leave it alone until the session
    /// restarts.
    rejected: bool,
}

/// Username of the `slot`th extra account (slot 0 is the main account).
fn extra_username(base: &str, slot: usize) -> String {
    format!("{base}{slot}")
}

struct Session {
    config: Config,
    events: Sender<SlskEvent>,
    commands: Receiver<SessionCommand>,
    inner_tx: Sender<SessionCommand>,
    client: Option<Arc<Client>>,
    /// Extra logins (`<username>1`, `<username>2`, …) so parallel downloads
    /// from one peer do not share an account. Slot `n` is `extras[n - 1]`.
    extras: Vec<ExtraAccount>,
    connector: Option<Receiver<ConnResult>>,
    connecting: bool,
    reconnect_at: Option<Instant>,
    connect_failures: u32,
    pending_download: Option<DownloadRequest>,
    album: Option<AlbumState>,
    last_session_check: Instant,
}

impl Session {
    fn new(
        config: Config,
        events: Sender<SlskEvent>,
        commands: Receiver<SessionCommand>,
        inner_tx: Sender<SessionCommand>,
    ) -> Self {
        let mut session = Self {
            config,
            events,
            commands,
            inner_tx,
            client: None,
            extras: Vec::new(),
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: None,
            last_session_check: Instant::now(),
        };
        session.start_connect();
        session
    }

    fn run(mut self) {
        loop {
            self.poll_connector();
            if self.reconnect_at.is_some_and(|at| Instant::now() >= at) {
                self.reconnect_at = None;
                self.start_connect();
            }

            match self.commands.recv_timeout(Duration::from_millis(80)) {
                Ok(command) => {
                    if !self.handle(command) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
        self.cleanup_shutdown();
    }

    fn log(&self, level: LogLevel, message: impl Into<String>) {
        let _ = self.events.send(SlskEvent::Log(level, message.into()));
    }

    fn start_connect(&mut self) {
        if self.connecting {
            return;
        }
        self.reconnect_at = None;
        self.connecting = true;
        let _ = self.events.send(SlskEvent::Connecting);
        let (tx, rx) = mpsc::channel();
        self.connector = Some(rx);
        let config = self.config.clone();
        thread::Builder::new()
            .name("soulseek-connect".to_string())
            .spawn(move || {
                let _ = tx.send(connect_client(&config));
            })
            .ok();
    }

    fn poll_connector(&mut self) {
        let Some(rx) = &self.connector else {
            return;
        };
        match rx.try_recv() {
            Ok(ConnResult::Connected(client)) => {
                self.connector = None;
                self.connecting = false;
                self.connect_failures = 0;
                self.reconnect_at = None;
                let port = client.listen_port();
                let username = client.username().to_string();
                self.client = Some(client);
                self.last_session_check = Instant::now();
                let _ = self.events.send(SlskEvent::LoginOk { username, port });
                if let Some(request) = self.pending_download.take() {
                    self.start_album(request);
                } else if self.album.as_ref().is_some_and(|album| {
                    album.phase == AlbumPhase::Searching && album.search_restart_pending
                }) {
                    self.begin_search();
                }
            }
            Ok(ConnResult::Failed { message, auth }) => {
                self.connector = None;
                self.connecting = false;
                let _ = self.events.send(SlskEvent::LoginFailed(message.clone()));
                let hint = if auth {
                    "Check the username and password in Settings (F2)."
                } else if self.connect_failures < MAX_AUTO_RECONNECTS {
                    "Retrying in a few seconds; F5 also retries now."
                } else {
                    "Press F5 to try connecting again."
                };
                self.log(
                    LogLevel::Error,
                    format!("Soulseek login failed: {message}. {hint}"),
                );
                if !auth && self.connect_failures < MAX_AUTO_RECONNECTS {
                    self.connect_failures += 1;
                    self.reconnect_at = Some(Instant::now() + Duration::from_secs(8));
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.connector = None;
                self.connecting = false;
                let message = "the connection attempt ended unexpectedly".to_string();
                let _ = self.events.send(SlskEvent::LoginFailed(message.clone()));
                self.log(LogLevel::Error, format!("Soulseek login failed: {message}"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn handle(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::StartDownload(request) => {
                if self.client.is_some() {
                    self.start_album(*request);
                } else {
                    self.pending_download = Some(*request);
                    if !self.connecting {
                        self.start_connect();
                    }
                }
            }
            SessionCommand::CancelTrack { job_id, index } => self.cancel_track(job_id, index),
            SessionCommand::RetryFailed { job_id } => self.retry_failed(job_id),
            SessionCommand::CancelAll { job_id } => {
                if self
                    .pending_download
                    .as_ref()
                    .is_some_and(|request| request.job_id == job_id)
                {
                    self.pending_download = None;
                }
                if self
                    .album
                    .as_ref()
                    .is_some_and(|album| album.job.job_id == job_id)
                {
                    self.cancel_album();
                }
            }
            SessionCommand::Reconnect => {
                if self.connecting {
                    self.log(LogLevel::Info, "Already connecting to Soulseek.");
                    return true;
                }
                let searching = self
                    .album
                    .as_ref()
                    .is_some_and(|album| album.phase == AlbumPhase::Searching);
                if searching {
                    let job_id = self
                        .album
                        .as_ref()
                        .map(|album| album.job.job_id)
                        .unwrap_or(0);
                    if let Some(album) = self.album.as_mut() {
                        album.search_cancel.store(true, Ordering::Relaxed);
                        album.search_restart_pending = true;
                    }
                    let _ = self.events.send(SlskEvent::SearchInterrupted { job_id });
                    self.log(
                        LogLevel::Warn,
                        "Soulseek search interrupted; it will restart after reconnect.",
                    );
                } else {
                    self.fail_active_tracks("reconnecting to Soulseek", true);
                    self.check_completion();
                }
                if let Some(mut client) = self.client.take()
                    && let Some(client) = Arc::get_mut(&mut client)
                {
                    client.disconnect();
                }
                // Dropping a client logs it out. The extra accounts come back
                // once the main one is connected again.
                let wanted = self.extras.len();
                self.set_wanted_extras(0);
                self.set_wanted_extras(wanted);
                self.connecting = false;
                self.connect_failures = 0;
                self.start_connect();
            }
            SessionCommand::Shutdown => return false,
            SessionCommand::SearchDone {
                job_id,
                search_attempt,
                result,
            } => self.search_done(job_id, search_attempt, result),
            SessionCommand::Verified {
                job_id,
                index,
                attempt_no,
                result,
                path,
                loudness,
            } => self.verified(job_id, index, attempt_no, result, path, loudness),
            SessionCommand::ProbeVerified {
                job_id,
                index,
                attempt_no,
            } => {
                let events = self.events.clone();
                if let Some(album) = self.album.as_mut()
                    && album.job.job_id == job_id
                {
                    commit_probe_if_verified(album, index, attempt_no, &events);
                }
            }
            SessionCommand::AlbumGainDone {
                job_id,
                index,
                attempt_no,
                result,
            } => self.album_gain_done(job_id, index, attempt_no, result),
            SessionCommand::PressingsFetched {
                job_id,
                search_id,
                result,
            } => self.pressings_fetched(job_id, search_id, result),
            SessionCommand::CoverReady { job_id, result } => {
                self.cover_ready(job_id, result);
            }
        }
        true
    }

    fn tick(&mut self) {
        self.check_session();
        self.manage_extras();
        // Nobody is waiting for the pressing lookup any more (the held tracks
        // were cancelled): stop retrying MusicBrainz.
        if let Some(album) = self.album.as_ref()
            && album.pressing_search_running
            && !album
                .tracks
                .iter()
                .any(|track| track.pressing_hold.is_some())
        {
            album.pressing_cancel.store(true, Ordering::Relaxed);
        }
        // Album gain works on local files and does not need the session.
        self.check_album_gain_deadlines();
        self.run_verification_queue();
        self.run_album_gain();
        if self.client.is_none() {
            self.check_completion();
            return;
        }
        let Some(phase) = self.album.as_ref().map(|album| album.phase) else {
            return;
        };
        match phase {
            AlbumPhase::Searching => {
                let events = self.events.clone();
                let client = self.client.clone();
                let command_tx = self.inner_tx.clone();
                let Some(album) = self.album.as_mut() else {
                    return;
                };
                if !album.search_timeout_sent
                    && album.search_started.elapsed()
                        > album.job.options.search_timeout + Duration::from_secs(15)
                {
                    album.search_timeout_sent = true;
                    album.search_cancel.store(true, Ordering::Relaxed);
                    let _ = command_tx.send(SessionCommand::SearchDone {
                        job_id: album.job.job_id,
                        search_attempt: album.search_attempt,
                        result: Err("Soulseek search timed out".to_string()),
                    });
                }
                if album.last_progress_emit.elapsed() >= Duration::from_millis(500) {
                    album.last_progress_emit = Instant::now();
                    let peers = client
                        .as_ref()
                        .map(|client| client.get_search_results_count(&album.query))
                        .unwrap_or(0);
                    if peers != album.last_peer_count {
                        album.last_peer_count = peers;
                        let _ = events.send(SlskEvent::AlbumSearchProgress {
                            job_id: album.job.job_id,
                            peers,
                        });
                    }
                }
            }
            AlbumPhase::Downloading => {
                self.poll_downloads();
                self.check_verification_deadlines();
                self.schedule();
                self.check_completion();
            }
            AlbumPhase::Complete => {}
        }
    }

    fn check_session(&mut self) {
        let Some(client) = &self.client else {
            return;
        };
        let Some(loss) = client.session_loss() else {
            return;
        };
        let _ = self.events.send(SlskEvent::Disconnected(loss.to_string()));
        self.log(LogLevel::Error, format!("Soulseek session ended: {loss}"));
        self.client = None;
        if self
            .album
            .as_ref()
            .is_some_and(|album| album.phase == AlbumPhase::Searching)
        {
            let job_id = self
                .album
                .as_ref()
                .map(|album| album.job.job_id)
                .unwrap_or(0);
            if let Some(album) = self.album.as_mut() {
                album.search_cancel.store(true, Ordering::Relaxed);
                album.search_restart_pending = true;
            }
            let _ = self.events.send(SlskEvent::SearchInterrupted { job_id });
            self.log(
                LogLevel::Warn,
                "Soulseek search interrupted; it will restart after reconnect.",
            );
        } else {
            self.fail_active_tracks("Soulseek session lost", true);
            self.check_completion();
        }
        self.connect_failures = 0;
        if !self.connecting {
            self.start_connect();
        }
    }

    fn start_album(&mut self, request: DownloadRequest) {
        if self.album.is_some() {
            self.cancel_album();
        }
        let _ = fs::remove_dir_all(request.album_dir.join(STAGING_DIR));

        let started = Instant::now();
        let job_id = request.job_id;
        let release_mbid = request.release_mbid.clone();
        let cover_artist = request.artist.clone();
        let cover_album = request.album.clone();
        let wants_folder_cover = request.options.folder_cover && !request.loose_files;
        let folder_cover_missing = wants_folder_cover
            && (request.force || !request.album_dir.join(cover::FOLDER_COVER_NAME).exists());
        let mut tracks: Vec<TrackState> = request
            .tracks
            .iter()
            .map(|meta| {
                let existing = if request.force {
                    None
                } else {
                    existing_file_is_valid(meta, request.options.length_tolerance)
                };
                TrackState::new(meta.clone(), existing)
            })
            .collect();

        let query = format!("{} {}", request.artist, request.album)
            .trim()
            .to_string();
        let all_done = tracks.iter().all(|track| track.status == TrackStatus::Done);
        let events = self.events.clone();

        for track in &mut tracks {
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
        }

        let mut album = AlbumState {
            job: request,
            phase: AlbumPhase::Searching,
            tracks,
            cover: None,
            search_cancel: Arc::new(AtomicBool::new(false)),
            query: query.clone(),
            last_peer_count: 0,
            last_progress_emit: Instant::now(),
            search_started: Instant::now(),
            search_timeout_sent: false,
            search_attempt: 0,
            search_restart_pending: false,
            completion_announced: false,
            started,
            album_sources: Vec::new(),
            fallback: Vec::new(),
            source_mode: SourceMode::Mixed,
            peer_activity: HashMap::new(),
            album_gain: None,
            replay_album: None,
            off_source_level: None,
            folder_cover: None,
            folder_cover_done: false,
            observed_secs: Vec::new(),
            pressings: None,
            pressing_search_running: false,
            pressing_cancel: Arc::new(AtomicBool::new(false)),
            pressing_search_id: 0,
            pressing_switches: 0,
        };
        album.observed_secs = vec![None; album.tracks.len()];

        if all_done {
            album.phase = AlbumPhase::Complete;
            album.completion_announced = true;
            let completed = album.tracks.len();
            self.album = Some(album);
            let _ = events.send(SlskEvent::AlbumComplete {
                job_id,
                completed,
                failed: 0,
                cancelled: 0,
                elapsed: started.elapsed(),
            });
            self.log(
                LogLevel::Success,
                "All tracks were already on disk; nothing to download.",
            );
            if folder_cover_missing {
                // The tracks are there but the folder cover is not.
                self.fetch_cover(job_id, cover_artist, cover_album, release_mbid, true);
            }
            return;
        }

        let wanted_extras = album.job.options.max_concurrent.max(1) - 1;
        self.album = Some(album);
        self.set_wanted_extras(wanted_extras);
        self.fetch_cover(
            job_id,
            cover_artist,
            cover_album,
            release_mbid,
            wants_folder_cover,
        );
        self.begin_search();
    }

    /// Every logged-in account by slot: the main one first, then the extras.
    fn slot_clients(&self) -> Vec<Option<Arc<Client>>> {
        let mut clients = Vec::with_capacity(self.extras.len() + 1);
        clients.push(self.client.clone());
        clients.extend(self.extras.iter().map(|extra| extra.client.clone()));
        clients
    }

    /// Keep `wanted` extra accounts: add missing ones (they connect from
    /// `manage_extras`) and log out surplus ones.
    fn set_wanted_extras(&mut self, wanted: usize) {
        self.extras.truncate(wanted);
        while self.extras.len() < wanted {
            let slot = self.extras.len() + 1;
            self.extras.push(ExtraAccount {
                username: extra_username(&self.config.username, slot),
                client: None,
                connector: None,
                retry_at: None,
                failures: 0,
                rejected: false,
            });
        }
    }

    /// Connect the extra accounts, pick up finished logins and notice lost
    /// sessions. Downloads start on whichever accounts are ready, so a slow
    /// or refused extra login never holds up the album.
    fn manage_extras(&mut self) {
        if self.extras.is_empty() {
            return;
        }
        let now = Instant::now();
        let primary_up = self.client.is_some();
        let mut messages: Vec<(LogLevel, String)> = Vec::new();
        let mut lost_slots: Vec<usize> = Vec::new();
        for (index, extra) in self.extras.iter_mut().enumerate() {
            let slot = index + 1;
            if let Some(loss) = extra
                .client
                .as_ref()
                .and_then(|client| client.session_loss())
            {
                messages.push((
                    LogLevel::Warn,
                    format!("Download account {} disconnected: {loss}", extra.username),
                ));
                extra.client = None;
                extra.retry_at = Some(now + Duration::from_secs(8));
                lost_slots.push(slot);
            }
            if let Some(rx) = &extra.connector {
                match rx.try_recv() {
                    Ok(ConnResult::Connected(client)) => {
                        extra.connector = None;
                        extra.failures = 0;
                        extra.client = Some(client);
                        messages.push((
                            LogLevel::Info,
                            format!("Download account {} is connected.", extra.username),
                        ));
                    }
                    Ok(ConnResult::Failed { message, auth }) => {
                        extra.connector = None;
                        if auth {
                            extra.rejected = true;
                            messages.push((
                                LogLevel::Warn,
                                format!(
                                    "Download account {} was rejected ({message}); \
                                     parallel downloads will use fewer accounts.",
                                    extra.username
                                ),
                            ));
                        } else {
                            extra.failures += 1;
                            extra.retry_at = Some(now + Duration::from_secs(8));
                            messages.push((
                                LogLevel::Warn,
                                format!(
                                    "Download account {} could not connect: {message}",
                                    extra.username
                                ),
                            ));
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        extra.connector = None;
                        extra.failures += 1;
                        extra.retry_at = Some(now + Duration::from_secs(8));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            let due = extra.retry_at.is_none_or(|at| now >= at);
            if primary_up
                && extra.client.is_none()
                && extra.connector.is_none()
                && !extra.rejected
                && extra.failures <= MAX_AUTO_RECONNECTS
                && due
            {
                extra.retry_at = None;
                extra.connector = spawn_extra_connect(&self.config, &extra.username, slot);
                if extra.connector.is_none() {
                    extra.failures += 1;
                }
            }
        }
        for (level, message) in messages {
            self.log(level, message);
        }
        for slot in lost_slots {
            let affected: Vec<usize> = self
                .album
                .as_ref()
                .map(|album| {
                    album
                        .tracks
                        .iter()
                        .enumerate()
                        .filter(|(_, track)| {
                            track
                                .current
                                .as_ref()
                                .is_some_and(|active| active.slot == slot)
                        })
                        .map(|(index, _)| index)
                        .collect()
                })
                .unwrap_or_default();
            for index in affected {
                self.fail_attempt(index, "download account disconnected");
            }
        }
    }

    fn fetch_cover(
        &self,
        job_id: u64,
        artist: String,
        album: String,
        release_mbid: String,
        folder_cover: bool,
    ) {
        let tx = self.inner_tx.clone();
        let spawned = thread::Builder::new()
            .name("cover-art".to_string())
            .spawn(move || {
                let result = musicbrainz::fetch_cover(&artist, &album, &release_mbid)
                    .map_err(|err| err.to_string())
                    .map(|art| {
                        art.map(|art| {
                            let folder_jpeg = folder_cover.then(|| {
                                cover::folder_jpeg(&art.data).map_err(|err| format!("{err:#}"))
                            });
                            FetchedCover { art, folder_jpeg }
                        })
                    });
                let _ = tx.send(SessionCommand::CoverReady { job_id, result });
            });
        if let Err(err) = spawned {
            self.log(
                LogLevel::Warn,
                format!("Could not start the cover art worker: {err}"),
            );
        }
    }

    fn begin_search(&mut self) {
        let Some(client) = self.client.clone() else {
            self.fail_active_tracks("Soulseek is not connected", true);
            self.check_completion();
            return;
        };
        let events = self.events.clone();
        let tx = self.inner_tx.clone();
        let (job_id, search_attempt, query, timeout, cancel) = {
            let Some(album) = self.album.as_mut() else {
                return;
            };
            album.phase = AlbumPhase::Searching;
            album.search_attempt = album.search_attempt.wrapping_add(1);
            let search_attempt = album.search_attempt;
            let cancel = Arc::new(AtomicBool::new(false));
            album.search_cancel = cancel.clone();
            album.last_peer_count = 0;
            album.last_progress_emit = Instant::now();
            album.search_started = Instant::now();
            album.search_timeout_sent = false;
            album.search_restart_pending = false;
            (
                album.job.job_id,
                search_attempt,
                album.query.clone(),
                album.job.options.search_timeout,
                cancel,
            )
        };
        let _ = events.send(SlskEvent::AlbumSearchStarted {
            job_id,
            query: query.clone(),
            timeout_secs: timeout.as_secs(),
        });
        let spawned = thread::Builder::new()
            .name("soulseek-search".to_string())
            .spawn(move || {
                let result = client
                    .search_with_cancel(&query, timeout, Some(cancel))
                    .map(convert_hits)
                    .map_err(|err| err.to_string());
                let _ = tx.send(SessionCommand::SearchDone {
                    job_id,
                    search_attempt,
                    result,
                });
            });
        if spawned.is_err() {
            self.fail_active_tracks("could not start the Soulseek search worker", true);
            self.check_completion();
        }
    }

    fn search_done(
        &mut self,
        job_id: u64,
        search_attempt: u64,
        result: Result<Vec<SearchHit>, String>,
    ) {
        let Some(mut album) = self.album.take() else {
            return;
        };
        if album.job.job_id != job_id
            || album.phase != AlbumPhase::Searching
            || album.search_attempt != search_attempt
            || album.search_restart_pending
        {
            self.album = Some(album);
            return;
        }

        let result = match result {
            Ok(hits) if self.client.is_some() => Ok(hits),
            Ok(_) => Err("Soulseek session lost during the search".to_string()),
            Err(err) => Err(err),
        };
        let events = self.events.clone();

        match result {
            Ok(hits) => {
                if let Some(client) = self.client.clone() {
                    let _ = client.forget_search(&album.query);
                }
                let queries: Vec<TrackQuery> = album
                    .tracks
                    .iter()
                    .map(|track| TrackQuery {
                        disc: track.meta.disc,
                        position: track.meta.position,
                        title: track.meta.title.clone(),
                        expected_secs: track.meta.expected_secs,
                    })
                    .collect();
                let options = CandidateOptions {
                    per_track: album.job.options.candidates_per_track,
                    length_tolerance: album.job.options.length_tolerance,
                    artist: album.job.artist.clone(),
                    album: album.job.album.clone(),
                    quality: album.job.options.audio_quality,
                };
                let candidates = matcher::build_candidates(&queries, &hits, &options);
                let needed: Vec<bool> = album
                    .tracks
                    .iter()
                    .map(|track| track.status != TrackStatus::Done)
                    .collect();
                let album_sources =
                    matcher::build_album_sources(&queries, &needed, &hits, &options);
                let per_track: Vec<usize> = candidates
                    .iter()
                    .zip(&needed)
                    .map(|(sources, needed)| if *needed { sources.len() } else { 0 })
                    .enumerate()
                    .map(|(index, count)| {
                        // A full-album copy may have a file the pooled
                        // assignment gave to a similarly named track.
                        let covered = needed[index]
                            && album_sources
                                .iter()
                                .any(|source| source.files[index].is_some());
                        count.max(usize::from(covered))
                    })
                    .collect();
                let total_candidates: usize = per_track.iter().sum();
                let matched = per_track.iter().filter(|count| **count > 0).count();
                let _ = events.send(SlskEvent::SourcesReady {
                    job_id,
                    per_track,
                    peers: hits.len(),
                });
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Info,
                    format!(
                        "Found {total_candidates} matching {} source(s) from {} peer(s); {matched}/{} tracks have a source.",
                        album.job.options.audio_quality.source_label(),
                        hits.len(),
                        album.tracks.len()
                    ),
                ));
                plan_sources(&mut album, album_sources, candidates, &events);
                album.phase = AlbumPhase::Downloading;
                self.album = Some(album);
            }
            Err(err) => {
                if let Some(client) = self.client.clone() {
                    let _ = client.forget_search(&album.query);
                }
                for track in &mut album.tracks {
                    if track.status == TrackStatus::Done {
                        continue;
                    }
                    track.status = TrackStatus::Failed;
                    track.speed = 0.0;
                    track.note = format!("Soulseek search failed: {err}");
                    let _ = events.send(SlskEvent::Track(track.update(job_id)));
                }
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Error,
                    format!("Soulseek search failed: {err}"),
                ));
                album.phase = AlbumPhase::Downloading;
                self.album = Some(album);
            }
        }
        self.check_completion();
    }

    fn poll_downloads(&mut self) {
        if self.client.is_none() {
            return;
        }
        let events = self.events.clone();
        // Each account only knows its own transfers.
        let downloads: Vec<Vec<soulseek_rs::types::Download>> = self
            .slot_clients()
            .iter()
            .map(|client| {
                client
                    .as_ref()
                    .map(|client| client.get_all_downloads())
                    .unwrap_or_default()
            })
            .collect();

        let (finished, rejected) = {
            let Some(album) = self.album.as_mut() else {
                return;
            };
            if album.phase != AlbumPhase::Downloading {
                return;
            }
            let job_id = album.job.job_id;
            let mut finished = Vec::new();
            let mut rejected: Vec<(usize, String)> = Vec::new();
            let now = Instant::now();
            // A peer that is busy sending us one of this album's tracks is not
            // ignoring the others waiting in its queue; those only wait on our
            // own transfers. Note this first so the order of the tracks does
            // not matter.
            for track in &album.tracks {
                let Some(active) = track.current.as_ref() else {
                    continue;
                };
                let busy = downloads
                    .get(active.slot)
                    .and_then(|list| find_live_download(list, &active.username, &active.path))
                    .is_some_and(|download| {
                        matches!(
                            download.status,
                            DownloadStatus::InProgress { .. } | DownloadStatus::Completed
                        )
                    });
                if busy {
                    album.peer_activity.insert(active.username.clone(), now);
                }
            }
            let peer_activity = album.peer_activity.clone();

            for (index, track) in album.tracks.iter_mut().enumerate() {
                let Some(active) = track.current.as_mut() else {
                    continue;
                };
                // The library replaces the download token when the peer sends
                // its TransferRequest, so tokens are not stable. The remote
                // username+path is.
                let Some(download) = downloads
                    .get(active.slot)
                    .and_then(|list| find_live_download(list, &active.username, &active.path))
                else {
                    rejected.push((index, "transfer vanished".to_string()));
                    continue;
                };
                match &download.status {
                    DownloadStatus::Queued => {
                        track.note = match download.queue_position {
                            Some(0) | None => "queued at peer".to_string(),
                            Some(place) => format!("in peer queue · {place} ahead"),
                        };
                        track.speed = 0.0;
                        let waiting_since = queue_wait_started(
                            active.started,
                            peer_activity.get(&active.username).copied(),
                        );
                        if now.duration_since(waiting_since) >= active.queue_patience {
                            rejected.push((
                                index,
                                format!("queue timeout after {}s", active.queue_patience.as_secs()),
                            ));
                        } else {
                            track.emit_if_due(&events, job_id);
                        }
                    }
                    DownloadStatus::InProgress {
                        bytes_downloaded,
                        total_bytes,
                        speed_bytes_per_sec,
                    } => {
                        // Time spent in the peer's queue is governed by
                        // queue_timeout, not the transfer stall watchdog.
                        if !active.in_progress {
                            active.in_progress = true;
                            active.last_progress = now;
                            active.last_bytes = *bytes_downloaded;
                        } else if *bytes_downloaded != active.last_bytes {
                            active.last_bytes = *bytes_downloaded;
                            active.last_progress = now;
                        }
                        track.bytes_done = *bytes_downloaded;
                        track.total = (*total_bytes).max(track.total);
                        track.speed = *speed_bytes_per_sec;
                        track.note.clear();
                        if now.duration_since(active.last_progress) >= active.stall_patience {
                            rejected.push((
                                index,
                                format!("stalled for {}s", active.stall_patience.as_secs()),
                            ));
                        } else {
                            track.emit_if_due(&events, job_id);
                        }
                    }
                    DownloadStatus::Completed => finished.push(index),
                    DownloadStatus::Failed(reason) => rejected.push((
                        index,
                        reason
                            .clone()
                            .unwrap_or_else(|| "source rejected the transfer".into()),
                    )),
                    DownloadStatus::TimedOut => {
                        rejected.push((index, "transfer timed out".to_string()));
                    }
                    DownloadStatus::Cancelled => {
                        rejected.push((index, "transfer cancelled".to_string()));
                    }
                    DownloadStatus::Paused { .. } => {
                        rejected.push((index, "transfer paused".to_string()));
                    }
                }
            }
            (finished, rejected)
        };

        for (index, reason) in rejected {
            self.fail_attempt(index, &reason);
        }
        for index in finished {
            self.begin_verification(index);
        }
    }

    fn check_verification_deadlines(&mut self) {
        let now = Instant::now();
        let stuck: Vec<usize> = self
            .album
            .as_ref()
            .map(|album| {
                album
                    .tracks
                    .iter()
                    .enumerate()
                    .filter(|(_, track)| {
                        track
                            .verifying
                            .as_ref()
                            .is_some_and(|verifying| now >= verifying.deadline)
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        for index in stuck {
            self.fail_attempt(index, "verification timed out");
        }
    }

    fn schedule(&mut self) {
        let events = self.events.clone();
        let now = Instant::now();
        let Some(album) = self.album.as_ref() else {
            return;
        };
        if album.phase != AlbumPhase::Downloading {
            return;
        }
        if matches!(album.source_mode, SourceMode::Probing { .. })
            && let Some(album) = self.album.as_mut()
        {
            ensure_probe_track(album, &events);
        }
        let slots = self.slot_clients().iter().flatten().count();
        let Some(album) = self.album.as_ref() else {
            return;
        };
        let job_id = album.job.job_id;
        let to_start = tracks_to_start(album, now, slots);

        for index in to_start {
            let is_retry = self
                .album
                .as_ref()
                .and_then(|album| album.tracks.get(index))
                .is_some_and(|track| track.status == TrackStatus::Retrying);
            if is_retry && let Some(album) = self.album.as_mut() {
                album.tracks[index].status = TrackStatus::Queued;
                album.tracks[index].retry_at = None;
                album.tracks[index].note = "last-resort retry".to_string();
                let _ = events.send(SlskEvent::Track(album.tracks[index].update(job_id)));
            }
            self.start_candidate(index);
        }
    }

    fn start_candidate(&mut self, index: usize) -> bool {
        let clients = self.slot_clients();
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return false;
        };
        // Peers serve one transfer per username, so every live download gets
        // an account of its own.
        let Some((slot, client)) = free_slot(&clients, album) else {
            return false;
        };
        loop {
            let Some((
                candidate,
                attempt_no,
                staging,
                stall_patience,
                queue_patience,
                attempts_started,
                attempts_total,
                position,
                title,
            )) = (|| {
                let track = album.tracks.get_mut(index)?;
                if track.next_candidate >= track.candidates.len() {
                    return None;
                }
                let candidate = track.candidates[track.next_candidate].clone();
                track.next_candidate += 1;
                track.attempts_started += 1;
                let attempt_no = track.attempts_started as u32;
                let last_resort = track.rounds_left < album.job.options.last_resort_rounds;
                let stall_patience = if last_resort {
                    album.job.options.last_resort_stall
                } else {
                    album.job.options.stall_timeout
                };
                let queue_patience = if last_resort {
                    album
                        .job
                        .options
                        .queue_timeout
                        .max(album.job.options.last_resort_stall)
                } else {
                    album.job.options.queue_timeout
                };
                let staging = album.job.album_dir.join(STAGING_DIR).join(format!(
                    "s{:x}-j{}-t{index:03}-a{attempt_no}",
                    staging_session_id(),
                    album.job.job_id
                ));
                Some((
                    candidate,
                    attempt_no,
                    staging,
                    stall_patience,
                    queue_patience,
                    track.attempts_started,
                    track.attempts_total,
                    track.meta.position,
                    track.meta.title.clone(),
                ))
            })()
            else {
                advance_after_failure(album, index, &events);
                return false;
            };

            let duplicate_active = album.tracks.iter().enumerate().any(|(other_index, other)| {
                other_index != index
                    && other.current.as_ref().is_some_and(|active| {
                        active.username == candidate.username && active.path == candidate.path
                    })
            });
            if duplicate_active {
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Warn,
                    format!(
                        "{position:02} {title}: skipping a source already used by another track"
                    ),
                ));
                continue;
            }

            if let Err(err) = fs::create_dir_all(&staging) {
                if let Some(track) = album.tracks.get_mut(index) {
                    track.status = TrackStatus::Failed;
                    track.speed = 0.0;
                    track.note = format!("could not create staging folder: {err}");
                    let _ = events.send(SlskEvent::Track(track.update(album.job.job_id)));
                }
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Error,
                    format!("{position:02} {title}: could not create staging folder ({err})"),
                ));
                return false;
            }

            match client.download(
                candidate.path.clone(),
                candidate.username.clone(),
                candidate.size,
                staging.to_string_lossy().into_owned(),
            ) {
                Ok((_download, _receiver)) => {
                    let job_id = album.job.job_id;
                    if let Some(track) = album.tracks.get_mut(index) {
                        track.status = TrackStatus::Downloading;
                        track.source = candidate.username.clone();
                        track.bytes_done = 0;
                        track.total = candidate.size;
                        track.speed = 0.0;
                        track.note.clear();
                        track.current = Some(DownloadActive {
                            slot,
                            username: candidate.username.clone(),
                            path: candidate.path.clone(),
                            staging: staging.clone(),
                            started: Instant::now(),
                            last_progress: Instant::now(),
                            last_bytes: 0,
                            in_progress: false,
                            stall_patience,
                            queue_patience,
                            attempt_no,
                        });
                        let _ = events.send(SlskEvent::Track(track.update(job_id)));
                    }
                    let _ = events.send(SlskEvent::Log(
                        LogLevel::Info,
                        format!(
                            "Downloading {position:02} {title} ← {} (try {attempts_started}/{})",
                            candidate.username,
                            attempts_total.max(1)
                        ),
                    ));
                    return true;
                }
                Err(err) => {
                    let _ = fs::remove_dir_all(&staging);
                    if let Some(track) = album.tracks.get_mut(index) {
                        track.speed = 0.0;
                        track.note = format!("could not queue transfer: {err}");
                        let _ = events.send(SlskEvent::Track(track.update(album.job.job_id)));
                    }
                    let _ = events.send(SlskEvent::Log(
                        LogLevel::Warn,
                        format!(
                            "{position:02} {title}: could not queue from {} ({err}); trying next",
                            candidate.username
                        ),
                    ));
                }
            }
        }
    }

    fn fail_attempt(&mut self, index: usize, reason: &str) {
        // Without a session there is no transfer to cancel, but the track
        // must still move on (album gain can fail while disconnected).
        let clients = self.slot_clients();
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        let (position, title) = {
            let Some(track) = album.tracks.get_mut(index) else {
                return;
            };
            if let Some(active) = track.current.take() {
                cancel_transfer(&clients, &active);
                let _ = fs::remove_dir_all(&active.staging);
            }
            if let Some(verifying) = track.verifying.take() {
                let _ = fs::remove_dir_all(&verifying.staging);
            }
            if let Some(pending) = track.pending.take() {
                let _ = fs::remove_dir_all(&pending.staging);
            }
            if let Some(hold) = track.pressing_hold.take() {
                let _ = fs::remove_dir_all(&hold.staging);
            }
            if let Some(queued) = track.queued_check.take() {
                let _ = fs::remove_dir_all(&queued.staging);
            }
            track.speed = 0.0;
            track.note = reason.to_string();
            (track.meta.position, track.meta.title.clone())
        };
        let _ = events.send(SlskEvent::Log(
            LogLevel::Warn,
            format!("{position:02} {title}: {reason}"),
        ));
        advance_after_failure(album, index, &events);
    }

    fn begin_verification(&mut self, index: usize) {
        if self.client.is_none() {
            return;
        }
        let clients = self.slot_clients();
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        let Some(active) = album.tracks[index].current.take() else {
            return;
        };
        if let Some(Some(client)) = clients.get(active.slot) {
            let _ = client.remove_download(&active.username, &active.path);
        }
        let Some(staged) = staged_file(&active.staging) else {
            let _ = fs::remove_dir_all(&active.staging);
            if let Some(track) = album.tracks.get_mut(index) {
                track.note = "downloaded file was missing".to_string();
            }
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!(
                    "{:02} {}: downloaded file was missing; trying next",
                    album.tracks[index].meta.position, album.tracks[index].meta.title
                ),
            ));
            advance_after_failure(album, index, &events);
            return;
        };
        self.queue_verification(
            index,
            QueuedCheck {
                attempt_no: active.attempt_no,
                staged,
                staging: active.staging,
                username: active.username,
                remote_path: active.path,
            },
        );
    }

    /// Line a downloaded file up for checking. The transfer is over, so the
    /// download slot is free; the check waits for a processing slot.
    fn queue_verification(&mut self, index: usize, check: QueuedCheck) {
        let events = self.events.clone();
        if let Some(album) = self.album.as_mut() {
            let job_id = album.job.job_id;
            if let Some(track) = album.tracks.get_mut(index) {
                track.status = TrackStatus::Verifying;
                track.speed = 0.0;
                track.note = "waiting to check".to_string();
                track.queued_check = Some(check);
                let _ = events.send(SlskEvent::Track(track.update(job_id)));
            }
        }
        self.run_verification_queue();
    }

    /// Start queued checks while processing slots are free. Album-gain
    /// workers and verification workers share the same limit.
    fn run_verification_queue(&mut self) {
        let Some(album) = self.album.as_ref() else {
            return;
        };
        let mut free = album
            .job
            .options
            .max_processing
            .max(1)
            .saturating_sub(processing_in_use(album));
        let queued: Vec<usize> = album
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| track.queued_check.is_some())
            .map(|(index, _)| index)
            .collect();
        for index in queued {
            if free == 0 {
                break;
            }
            let Some(check) = self
                .album
                .as_mut()
                .and_then(|album| album.tracks.get_mut(index))
                .and_then(|track| track.queued_check.take())
            else {
                continue;
            };
            free -= 1;
            self.spawn_verification(
                index,
                check.staged,
                check.staging,
                check.attempt_no,
                check.username,
                check.remote_path,
            );
        }
    }

    /// Verify (and, without album gain, convert) a staged file in a worker.
    fn spawn_verification(
        &mut self,
        index: usize,
        staged: PathBuf,
        staging: PathBuf,
        attempt_no: u32,
        username: String,
        remote_path: String,
    ) {
        let events = self.events.clone();
        let tx = self.inner_tx.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        let job_id = album.job.job_id;
        let is_probe =
            matches!(album.source_mode, SourceMode::Probing { track, .. } if track == index);
        let (update, spec, position, title, source, normalize_loudness, audio_quality) = {
            let Some(track) = album.tracks.get_mut(index) else {
                return;
            };
            track.status = TrackStatus::Verifying;
            track.note = "checking duration…".to_string();
            track.speed = 0.0;
            let deadline = verification_deadline(
                Instant::now(),
                track.meta.expected_secs,
                album.job.options.normalize_loudness || album.job.options.audio_quality.is_lossy(),
            );
            track.verifying = Some(VerifyingAttempt {
                attempt_no,
                staging: staging.clone(),
                deadline,
                username,
                remote_path,
            });
            let update = track.update(job_id);
            let source_is_mp3 = normalize::source_is_mp3(&staged);
            let spec = VerifySpec {
                expected_secs: track.meta.expected_secs,
                tolerance_secs: album.job.options.length_tolerance,
                recording_mbid: track.meta.recording_mbid.clone(),
                release_group_mbid: album.job.release_group_mbid.clone(),
                api_key: album.job.options.api_key.clone(),
                min_score: album.job.options.fingerprint_min_score,
                min_mp3_bitrate_kbps: (album.job.options.audio_quality.is_mp3() && source_is_mp3)
                    .then_some(300),
            };
            (
                update,
                spec,
                track.meta.position,
                track.meta.title.clone(),
                track.source.clone(),
                album.job.options.normalize_loudness,
                album.job.options.audio_quality,
            )
        };
        let _ = events.send(SlskEvent::Track(update.clone()));
        let _ = events.send(SlskEvent::Log(
            LogLevel::Info,
            format!("Verifying {position:02} {title} ← {source}"),
        ));

        let path = staged;
        let spawned = thread::Builder::new()
            .name("soulsucker-verify".to_string())
            .spawn(move || {
                let on_phase = |phase: &str| {
                    let mut phase_update = update.clone();
                    phase_update.status = TrackStatus::Verifying;
                    phase_update.note = phase.to_string();
                    let _ = events.send(SlskEvent::Track(phase_update));
                };
                let mut result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    acoustid::verify_file(&path, &spec, &on_phase)
                }))
                .unwrap_or_else(|_| VerifyResult::reject("verification worker panicked"));
                if result.accepted && !normalize::source_compatible(&path, audio_quality) {
                    result = VerifyResult::reject(format!(
                        "downloaded file is not a {} source",
                        audio_quality.label()
                    ));
                }
                if result.accepted && is_probe {
                    // The right album copy is confirmed; the rest of the
                    // album need not wait for measuring or conversion.
                    let _ = tx.send(SessionCommand::ProbeVerified {
                        job_id,
                        index,
                        attempt_no,
                    });
                }
                let mut result_path = path.clone();
                let mut loudness = None;
                if result.accepted && normalize_loudness {
                    // The gain is applied once the whole album is in, so
                    // only measure here. MP3 conversion waits for that pass
                    // too, so the audio is encoded exactly once.
                    on_phase("measuring loudness…");
                    match normalize::measure_loudness(&path) {
                        Ok(measured) => loudness = Some(measured),
                        Err(err) => {
                            let _ = events.send(SlskEvent::Log(
                                LogLevel::Warn,
                                format!("{position:02} {title}: loudness not measured ({err})"),
                            ));
                        }
                    }
                } else if result.accepted && normalize::needs_conversion(&path, audio_quality) {
                    on_phase(normalize::processing_phase(&path, audio_quality, false));
                    match normalize::process_file(&path, audio_quality, None) {
                        Ok(processed) => result_path = processed,
                        Err(err) => {
                            result = VerifyResult::reject(format!(
                                "could not convert to {} ({err})",
                                audio_quality.label()
                            ));
                        }
                    }
                }
                let _ = tx.send(SessionCommand::Verified {
                    job_id,
                    index,
                    attempt_no,
                    result,
                    path: result_path,
                    loudness,
                });
            });
        if spawned.is_err() {
            if let Some(album) = self.album.as_mut()
                && let Some(verifying) = album.tracks[index].verifying.take()
            {
                let _ = fs::remove_dir_all(&verifying.staging);
            }
            self.fail_attempt(index, "could not start the verification worker");
        }
    }

    fn verified(
        &mut self,
        job_id: u64,
        index: usize,
        attempt_no: u32,
        result: VerifyResult,
        normalized_path: PathBuf,
        loudness: Option<Loudness>,
    ) {
        let Some(mut album) = self.album.take() else {
            return;
        };
        if album.job.job_id != job_id {
            self.album = Some(album);
            return;
        }
        let Some(track) = album.tracks.get_mut(index) else {
            self.album = Some(album);
            return;
        };
        let matches_current_attempt = track.status == TrackStatus::Verifying
            && track
                .verifying
                .as_ref()
                .is_some_and(|verifying| verifying.attempt_no == attempt_no);
        if !matches_current_attempt {
            // A late result from an earlier attempt must not touch the current
            // attempt's staging folder.
            self.album = Some(album);
            return;
        }
        let Some(verifying) = track.verifying.take() else {
            self.album = Some(album);
            return;
        };

        let from_album_source =
            album_source_file(&album, index, &verifying.username, &verifying.remote_path);
        if from_album_source
            && (result.accepted || result.length_mismatch)
            && let Some(secs) = result.duration_secs
            && let Some(observed) = album.observed_secs.get_mut(index)
        {
            *observed = Some(secs);
        }
        if !result.accepted
            && result.length_mismatch
            && from_album_source
            && album.pressing_switches < MAX_PRESSING_SWITCHES
        {
            // The file may be right for another pressing of the album:
            // check that before giving up on the peer's copy.
            self.hold_for_pressing(album, index, verifying, normalized_path, result.note);
            return;
        }

        if !result.accepted {
            let _ = fs::remove_dir_all(&verifying.staging);
            let reason = result.note;
            self.album = Some(album);
            self.fail_attempt(index, &reason);
            return;
        }

        if album.job.options.normalize_loudness {
            let pending = PendingAlbumGain {
                attempt_no,
                staging: verifying.staging,
                path: normalized_path,
                note: result.note,
                loudness,
                from_album_source,
                replay_gain: None,
                deadline: None,
            };
            self.hold_for_album_gain(album, index, pending);
            return;
        }
        self.finish_track(
            album,
            index,
            normalized_path,
            verifying.staging,
            result.note,
            None,
        );
    }

    /// A verified track waits, still staged, until every track is in and the
    /// album gain is known.
    fn hold_for_album_gain(
        &mut self,
        mut album: AlbumState,
        index: usize,
        pending: PendingAlbumGain,
    ) {
        let events = self.events.clone();
        let job_id = album.job.job_id;
        if let Some(track) = album.tracks.get_mut(index) {
            track.status = TrackStatus::Verifying;
            track.speed = 0.0;
            track.note = "awaiting album gain".to_string();
            track.pending = Some(pending);
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
            let _ = events.send(SlskEvent::Log(
                LogLevel::Info,
                format!(
                    "✔ {:02} {} ← {} verified; album gain follows once every track is in",
                    track.meta.position, track.meta.title, track.source
                ),
            ));
        }
        if matches!(album.source_mode, SourceMode::Probing { track, .. } if track == index) {
            commit_album_source(&mut album, &events);
        }
        self.album = Some(album);
    }

    /// Promote a verified (and possibly processed) file, tag it and mark the
    /// track done.
    fn finish_track(
        &mut self,
        mut album: AlbumState,
        index: usize,
        normalized_path: PathBuf,
        staging: PathBuf,
        note: String,
        replay_gain: Option<ReplayGain>,
    ) {
        let events = self.events.clone();
        let job_id = album.job.job_id;
        let Some(track) = album.tracks.get_mut(index) else {
            let _ = fs::remove_dir_all(&staging);
            self.album = Some(album);
            return;
        };
        let final_path = track.meta.final_path.clone();
        let Some(parent) = final_path.parent() else {
            let _ = fs::remove_dir_all(&staging);
            self.album = Some(album);
            self.fail_attempt(index, "invalid destination path");
            return;
        };
        if let Err(err) = fs::create_dir_all(parent) {
            let _ = fs::remove_dir_all(&staging);
            self.album = Some(album);
            self.fail_attempt(index, &format!("could not create album folder: {err}"));
            return;
        }

        if final_path.exists()
            && !album.job.force
            && let Some(size) =
                existing_file_is_valid(&track.meta, album.job.options.length_tolerance)
        {
            let _ = fs::remove_dir_all(&staging);
            track.status = TrackStatus::Done;
            track.bytes_done = size;
            track.total = size;
            track.speed = 0.0;
            track.note = "existing file kept".to_string();
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
            self.album = Some(album);
            self.save_folder_cover();
            self.check_completion();
            return;
        }

        if let Err(err) = promote_file(&normalized_path, &final_path) {
            let _ = fs::remove_dir_all(&staging);
            self.album = Some(album);
            self.fail_attempt(index, &format!("could not save file: {err}"));
            return;
        }
        track.downloaded = true;

        if let Err(err) = write_track_tags(
            &album.job,
            &track.meta,
            album.cover.as_deref(),
            replay_gain,
            &final_path,
        ) {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!(
                    "{} — {}: tags not written ({err})",
                    track.meta.position, track.meta.title
                ),
            ));
        }

        let size = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        track.status = TrackStatus::Done;
        track.bytes_done = size;
        track.total = size;
        track.speed = 0.0;
        track.note = note.clone();
        let _ = fs::remove_dir_all(&staging);
        let source = track.source.clone();
        let position = track.meta.position;
        let title = track.meta.title.clone();
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
        let _ = events.send(SlskEvent::Log(
            LogLevel::Success,
            format!("✔ {position:02} {title} ← {source} · {note}"),
        ));
        if matches!(album.source_mode, SourceMode::Probing { track, .. } if track == index) {
            commit_album_source(&mut album, &events);
        }
        self.album = Some(album);
        self.save_folder_cover();
        self.check_completion();
    }

    /// Keep a file rejected only on length while other pressings of the
    /// album are checked against the chosen peer's track lengths.
    fn hold_for_pressing(
        &mut self,
        mut album: AlbumState,
        index: usize,
        verifying: VerifyingAttempt,
        staged: PathBuf,
        reason: String,
    ) {
        let events = self.events.clone();
        let job_id = album.job.job_id;
        if let Some(track) = album.tracks.get_mut(index) {
            track.status = TrackStatus::Verifying;
            track.speed = 0.0;
            track.note = "checking pressings".to_string();
            let _ = events.send(SlskEvent::Log(
                LogLevel::Info,
                format!(
                    "{:02} {}: {reason}; looking for a pressing whose track lengths match {}'s copy",
                    track.meta.position, track.meta.title, verifying.username
                ),
            ));
            track.pressing_hold = Some(HeldForPressing {
                attempt_no: verifying.attempt_no,
                staging: verifying.staging,
                staged,
                username: verifying.username,
                remote_path: verifying.remote_path,
                reason,
            });
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
        }
        if album.pressing_search_running {
            // The running lookup resolves every held track.
            self.album = Some(album);
            return;
        }
        if album.pressings.is_some() {
            self.resolve_pressing_holds(album);
            return;
        }
        album.pressing_search_running = true;
        album.pressing_search_id = album.pressing_search_id.wrapping_add(1);
        let search_id = album.pressing_search_id;
        let group = album.job.release_group_mbid.clone();
        let tx = self.inner_tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        album.pressing_cancel = cancel.clone();
        let log_events = events.clone();
        let spawned = thread::Builder::new()
            .name("pressing-lookup".to_string())
            .spawn(move || {
                // MusicBrainz being unreachable must not be read as "no other
                // pressing exists", so keep trying until it answers.
                let result = musicbrainz::get_group_release_details(
                    &group,
                    &|| cancel.load(Ordering::Relaxed),
                    &|line| {
                        let _ = log_events.send(SlskEvent::Log(LogLevel::Info, line));
                    },
                )
                .map_err(|err| err.to_string());
                let _ = tx.send(SessionCommand::PressingsFetched {
                    job_id,
                    search_id,
                    result,
                });
            });
        if spawned.is_err() {
            album.pressing_search_running = false;
            album.pressings = Some(Arc::new(Vec::new()));
            self.resolve_pressing_holds(album);
            return;
        }
        self.album = Some(album);
    }

    fn pressings_fetched(
        &mut self,
        job_id: u64,
        search_id: u64,
        result: Result<Vec<ReleaseInfo>, String>,
    ) {
        let Some(mut album) = self.album.take() else {
            return;
        };
        if album.job.job_id != job_id
            || album.pressing_search_id != search_id
            || !album.pressing_search_running
        {
            self.album = Some(album);
            return;
        }
        album.pressing_search_running = false;
        match result {
            Ok(pressings) => album.pressings = Some(Arc::new(pressings)),
            Err(_) if album.pressing_cancel.load(Ordering::Relaxed) => {}
            Err(err) => {
                // Not cached: a later length mismatch may try again.
                let _ = self.events.send(SlskEvent::Log(
                    LogLevel::Warn,
                    format!("Could not look up other pressings on MusicBrainz ({err})"),
                ));
            }
        }
        self.resolve_pressing_holds(album);
    }

    /// Switch to a pressing that fits the held files and re-verify them, or
    /// release them to the next source when none fits.
    fn resolve_pressing_holds(&mut self, mut album: AlbumState) {
        let events = self.events.clone();
        let chosen = album.pressings.clone().and_then(|pressings| {
            musicbrainz::best_matching_pressing(
                &pressing_tracks(&album),
                &pressings,
                &album.job.release_mbid,
                album.job.options.length_tolerance,
            )
            .cloned()
        });
        let held: Vec<usize> = album
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| track.pressing_hold.is_some())
            .map(|(index, _)| index)
            .collect();

        match chosen {
            Some(release) => {
                apply_pressing(&mut album, &release);
                album.pressing_switches += 1;
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Success,
                    format!(
                        "Switched to the {} pressing ({}) — its track lengths match the files; re-checking {} track(s).",
                        pressing_label(&release),
                        release.mbid,
                        held.len()
                    ),
                ));
                retag_saved_tracks(&album, &events);
                self.album = Some(album);
                for index in held {
                    let Some(hold) = self
                        .album
                        .as_mut()
                        .and_then(|album| album.tracks.get_mut(index))
                        .and_then(|track| track.pressing_hold.take())
                    else {
                        continue;
                    };
                    self.queue_verification(
                        index,
                        QueuedCheck {
                            attempt_no: hold.attempt_no,
                            staged: hold.staged,
                            staging: hold.staging,
                            username: hold.username,
                            remote_path: hold.remote_path,
                        },
                    );
                }
            }
            None => {
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Warn,
                    "No other pressing matches the chosen copy's track lengths; trying other sources."
                        .to_string(),
                ));
                self.album = Some(album);
                for index in held {
                    let Some(hold) = self
                        .album
                        .as_mut()
                        .and_then(|album| album.tracks.get_mut(index))
                        .and_then(|track| track.pressing_hold.take())
                    else {
                        continue;
                    };
                    let _ = fs::remove_dir_all(&hold.staging);
                    self.fail_attempt(index, &hold.reason);
                }
            }
        }
    }

    /// Once every track is verified (or has given up), work out one gain for
    /// the album. Gain mode applies it to each held track in a worker;
    /// ReplayGain mode leaves the audio alone and writes it as tags (the
    /// worker then only converts, when the output format needs it).
    fn run_album_gain(&mut self) {
        let events = self.events.clone();
        let tx = self.inner_tx.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        if album.phase != AlbumPhase::Downloading || !album.job.options.normalize_loudness {
            return;
        }
        if !album_ready_for_gain(album) {
            return;
        }
        let target = album.job.options.normalize_target_lufs;
        let tags_only = album.job.options.normalize_method == NormalizeMethod::ReplayGain;
        let gain_db = match album.album_gain {
            Some(gain) => gain,
            None => {
                // With a chosen album copy, only its tracks set the album
                // gain; tracks taken from other sources are matched to the
                // result instead of dragging it around.
                let own_tracks = album
                    .tracks
                    .iter()
                    .filter_map(|track| track.pending.as_ref())
                    .filter(|pending| pending.from_album_source && pending.loudness.is_some())
                    .count();
                let split = own_tracks >= 2;
                let measured: Vec<Loudness> = album
                    .tracks
                    .iter()
                    .filter_map(|track| track.pending.as_ref())
                    .filter(|pending| !split || pending.from_album_source)
                    .filter_map(|pending| pending.loudness)
                    .collect();
                let off_source = album
                    .tracks
                    .iter()
                    .filter_map(|track| track.pending.as_ref())
                    .filter(|pending| !pending.from_album_source && pending.loudness.is_some())
                    .count();
                let gain_db = if tags_only {
                    // Nothing is changed in the audio, so there is no gain to
                    // apply; the album values only go into the tags.
                    match normalize::album_loudness(&measured) {
                        Some((integrated, true_peak)) => {
                            album.replay_album = Some((integrated, true_peak));
                            if split && off_source > 0 {
                                let _ = events.send(SlskEvent::Log(
                                    LogLevel::Info,
                                    format!(
                                        "{off_source} track(s) came from another source; their album gain follows their own level."
                                    ),
                                ));
                            }
                            let _ = events.send(SlskEvent::Log(
                                LogLevel::Info,
                                format!(
                                    "Album loudness {:.1} LUFS, peak {:.1} dBTP over {} track(s): writing ReplayGain tags for {:.1} LUFS (album {:+.1} dB).",
                                    integrated,
                                    true_peak,
                                    measured.len(),
                                    target,
                                    target - integrated
                                ),
                            ));
                        }
                        None => {
                            let _ = events.send(SlskEvent::Log(
                                LogLevel::Warn,
                                "Could not measure the album's loudness; no ReplayGain tags written."
                                    .to_string(),
                            ));
                        }
                    }
                    0.0
                } else {
                    match normalize::album_gain(&measured, target) {
                        Some(gain) => {
                            if split && off_source > 0 {
                                album.off_source_level = Some(gain.integrated + gain.gain_db);
                                let _ = events.send(SlskEvent::Log(
                                    LogLevel::Info,
                                    format!(
                                        "{off_source} track(s) came from another source; matching them to the album's level instead."
                                    ),
                                ));
                            }
                            let _ = events.send(SlskEvent::Log(
                                LogLevel::Info,
                                format!(
                                    "Album loudness {:.1} LUFS, peak {:.1} dBTP over {} track(s): applying {:+.1} dB to every track (target {:.1} LUFS).",
                                    gain.integrated,
                                    gain.true_peak,
                                    measured.len(),
                                    gain.gain_db,
                                    target
                                ),
                            ));
                            gain.gain_db
                        }
                        None => {
                            let _ = events.send(SlskEvent::Log(
                                LogLevel::Warn,
                                "Could not measure the album's loudness; saving tracks at their original level."
                                    .to_string(),
                            ));
                            0.0
                        }
                    }
                };
                album.album_gain = Some(gain_db);
                gain_db
            }
        };

        let job_id = album.job.job_id;
        let quality = album.job.options.audio_quality;
        let max = album.job.options.max_processing.max(1);
        let off_source_level = album.off_source_level;
        let replay_album = album.replay_album;
        let mut running = processing_in_use(album);
        for (index, track) in album.tracks.iter_mut().enumerate() {
            if running >= max {
                break;
            }
            let Some(pending) = track.pending.as_mut() else {
                continue;
            };
            if pending.deadline.is_some() {
                continue;
            }
            running += 1;
            let converts = normalize::needs_conversion(&pending.path, quality);
            let (gain_db, label) = if tags_only {
                // A track from another master has its own album gain: the
                // album's would be wrong for it.
                let album_values = if pending.from_album_source {
                    replay_album
                } else {
                    pending
                        .loudness
                        .map(|loudness| (loudness.integrated, loudness.true_peak))
                };
                let tags = normalize::replay_gain(pending.loudness.as_ref(), album_values, target);
                let label = match tags.track_gain_db {
                    Some(gain) => format!("ReplayGain {gain:+.1} dB"),
                    None => "no loudness measured".to_string(),
                };
                pending.replay_gain = Some(tags);
                (0.0, label)
            } else {
                let matched = match (
                    pending.from_album_source,
                    off_source_level,
                    pending.loudness,
                ) {
                    (false, Some(level), Some(loudness)) => {
                        Some(normalize::matched_gain(&loudness, level))
                    }
                    _ => None,
                };
                let gain_db = matched.unwrap_or(gain_db);
                let label = if matched.is_some() {
                    "matched to album"
                } else {
                    "album gain"
                };
                (gain_db, format!("{label} {gain_db:+.1} dB"))
            };
            pending.deadline = Some(verification_deadline(
                Instant::now(),
                track.meta.expected_secs,
                true,
            ));
            let path = pending.path.clone();
            let attempt_no = pending.attempt_no;
            let tx = tx.clone();
            let spawned = thread::Builder::new()
                .name("album-gain".to_string())
                .spawn(move || {
                    let result = std::panic::catch_unwind(|| {
                        if gain_db.abs() < 0.005 && !normalize::needs_conversion(&path, quality) {
                            // Nothing to change: keep the verified file as is.
                            return Ok(path.clone());
                        }
                        normalize::process_file(&path, quality, Some(gain_db))
                            .map_err(|err| err.to_string())
                    })
                    .unwrap_or_else(|_| Err("audio worker panicked".to_string()));
                    let _ = tx.send(SessionCommand::AlbumGainDone {
                        job_id,
                        index,
                        attempt_no,
                        result,
                    });
                });
            if spawned.is_err() {
                // Let the deadline check handle it on the next tick.
                pending.deadline = Some(Instant::now());
            }
            track.note = if converts {
                format!("{label} → {}", quality.format_name())
            } else {
                label
            };
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
        }
    }

    fn album_gain_done(
        &mut self,
        job_id: u64,
        index: usize,
        attempt_no: u32,
        result: Result<PathBuf, String>,
    ) {
        let Some(mut album) = self.album.take() else {
            return;
        };
        let current = album.job.job_id == job_id
            && album.tracks.get(index).is_some_and(|track| {
                track.pending.as_ref().is_some_and(|pending| {
                    pending.attempt_no == attempt_no && pending.deadline.is_some()
                })
            });
        if !current {
            // A late result after a cancel, a timeout or a newer attempt. Its
            // staging folder is gone or belongs to someone else.
            self.album = Some(album);
            return;
        }
        let Some(pending) = album.tracks[index].pending.take() else {
            self.album = Some(album);
            return;
        };
        match result {
            Ok(path) => self.finish_track(
                album,
                index,
                path,
                pending.staging,
                pending.note,
                pending.replay_gain,
            ),
            Err(err) => self.album_gain_failed(album, index, pending, &err),
        }
    }

    /// Processing is best-effort: a file already in the right format is kept
    /// at its original level; a file that still needs converting is rejected
    /// so another source can be tried.
    fn album_gain_failed(
        &mut self,
        album: AlbumState,
        index: usize,
        pending: PendingAlbumGain,
        reason: &str,
    ) {
        let quality = album.job.options.audio_quality;
        let Some(track) = album.tracks.get(index) else {
            let _ = fs::remove_dir_all(&pending.staging);
            self.album = Some(album);
            return;
        };
        let label = format!("{:02} {}", track.meta.position, track.meta.title);
        if !normalize::needs_conversion(&pending.path, quality) {
            self.log(
                LogLevel::Warn,
                format!("{label}: loudness step failed ({reason}); saved unchanged"),
            );
            self.finish_track(
                album,
                index,
                pending.path,
                pending.staging,
                pending.note,
                pending.replay_gain,
            );
            return;
        }
        let _ = fs::remove_dir_all(&pending.staging);
        self.album = Some(album);
        self.fail_attempt(
            index,
            &format!("could not convert to {} ({reason})", quality.label()),
        );
    }

    fn check_album_gain_deadlines(&mut self) {
        let now = Instant::now();
        let stuck: Vec<usize> = self
            .album
            .as_ref()
            .map(|album| {
                album
                    .tracks
                    .iter()
                    .enumerate()
                    .filter(|(_, track)| {
                        track
                            .pending
                            .as_ref()
                            .and_then(|pending| pending.deadline)
                            .is_some_and(|deadline| now >= deadline)
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        for index in stuck {
            let Some(mut album) = self.album.take() else {
                return;
            };
            let Some(pending) = album.tracks[index].pending.take() else {
                self.album = Some(album);
                continue;
            };
            self.album_gain_failed(album, index, pending, "timed out");
        }
    }

    fn cover_ready(&mut self, job_id: u64, result: Result<Option<FetchedCover>, String>) {
        let mut messages = Vec::new();
        {
            let Some(album) = self.album.as_mut() else {
                return;
            };
            if album.job.job_id != job_id {
                return;
            }
            match result {
                Ok(Some(fetched)) => {
                    let FetchedCover { art, folder_jpeg } = fetched;
                    match folder_jpeg {
                        Some(Ok(jpeg)) => album.folder_cover = Some(Arc::new(jpeg)),
                        Some(Err(err)) => messages.push((
                            LogLevel::Warn,
                            format!("Could not make {}: {err}", cover::FOLDER_COVER_NAME),
                        )),
                        None => {}
                    }
                    let source = art.source.label();
                    let cover = Arc::new(art.data);
                    album.cover = Some(cover.clone());
                    let mut updated = 0;
                    let mut failed = 0;
                    for track in &mut album.tracks {
                        if !track.downloaded || track.status != TrackStatus::Done {
                            continue;
                        }
                        match audio::write_cover(&track.meta.final_path, cover.as_slice()) {
                            Ok(()) => updated += 1,
                            Err(_) => failed += 1,
                        }
                    }
                    if updated > 0 {
                        messages.push((
                            LogLevel::Success,
                            format!("Embedded {source} cover art in {updated} finished track(s)."),
                        ));
                    }
                    if failed > 0 {
                        messages.push((
                            LogLevel::Warn,
                            format!("Could not add cover art to {failed} finished track(s)."),
                        ));
                    }
                    if updated == 0 && failed == 0 {
                        messages.push((
                            LogLevel::Info,
                            format!("{source} front cover is ready for new tracks."),
                        ));
                    }
                }
                Ok(None) => messages.push((
                    LogLevel::Info,
                    "No cover found on iTunes or the Cover Art Archive; source art was kept."
                        .to_string(),
                )),
                Err(err) => {
                    messages.push((LogLevel::Warn, format!("Could not fetch cover art: {err}")));
                }
            }
        }
        for (level, message) in messages {
            self.log(level, message);
        }
        self.save_folder_cover();
    }

    /// Write `cover.jpg` into the album folder once both the image and the
    /// folder (with at least one finished track in it) exist. An existing
    /// `cover.jpg` is kept unless the album is being forced.
    fn save_folder_cover(&mut self) {
        let Some(album) = self.album.as_mut() else {
            return;
        };
        if album.folder_cover_done || album.job.loose_files || !album.job.options.folder_cover {
            return;
        }
        let Some(jpeg) = album.folder_cover.clone() else {
            return;
        };
        let has_tracks = album
            .tracks
            .iter()
            .any(|track| track.status == TrackStatus::Done);
        if !has_tracks || !album.job.album_dir.is_dir() {
            return;
        }
        album.folder_cover_done = true;
        let dir = album.job.album_dir.clone();
        let force = album.job.force;
        let message = match cover::write_folder_cover(&dir, jpeg.as_slice(), force) {
            Ok(cover::FolderCoverResult::Written) => (
                LogLevel::Success,
                format!("Saved {} in the album folder.", cover::FOLDER_COVER_NAME),
            ),
            Ok(cover::FolderCoverResult::Kept) => (
                LogLevel::Info,
                format!(
                    "{} already exists; kept it (use --force to replace).",
                    cover::FOLDER_COVER_NAME
                ),
            ),
            Err(err) => (
                LogLevel::Warn,
                format!("Could not save {}: {err:#}", cover::FOLDER_COVER_NAME),
            ),
        };
        self.log(message.0, message.1);
    }

    fn cancel_track(&mut self, job_id: u64, index: usize) {
        if self.client.is_none() {
            return;
        }
        let clients = self.slot_clients();
        let events = self.events.clone();
        {
            let Some(album) = self.album.as_mut() else {
                return;
            };
            if album.job.job_id != job_id {
                return;
            }
            let Some(track) = album.tracks.get_mut(index) else {
                return;
            };
            if matches!(track.status, TrackStatus::Done | TrackStatus::Cancelled) {
                return;
            }
            if let Some(active) = track.current.take() {
                cancel_transfer(&clients, &active);
                let _ = fs::remove_dir_all(&active.staging);
            }
            if let Some(verifying) = track.verifying.take() {
                let _ = fs::remove_dir_all(&verifying.staging);
            }
            if let Some(pending) = track.pending.take() {
                let _ = fs::remove_dir_all(&pending.staging);
            }
            if let Some(hold) = track.pressing_hold.take() {
                let _ = fs::remove_dir_all(&hold.staging);
            }
            if let Some(queued) = track.queued_check.take() {
                let _ = fs::remove_dir_all(&queued.staging);
            }
            track.status = TrackStatus::Cancelled;
            track.speed = 0.0;
            track.note = "cancelled".to_string();
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!("{:02} {}: cancelled", track.meta.position, track.meta.title),
            ));
        }
        self.check_completion();
    }

    fn retry_failed(&mut self, job_id: u64) {
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        if album.job.job_id != job_id {
            return;
        }
        let mut retried = 0;
        for index in 0..album.tracks.len() {
            let track = &mut album.tracks[index];
            if track.status != TrackStatus::Failed {
                continue;
            }
            if track.candidates.is_empty() {
                // Tracks that failed while waiting for the full-album check
                // never received sources of their own.
                track.candidates = album.fallback.get(index).cloned().unwrap_or_default();
                track.attempts_total = track.attempts_started + track.candidates.len();
            }
            if track.candidates.is_empty() {
                continue;
            }
            track.status = TrackStatus::Queued;
            track.next_candidate = 0;
            track.rounds_left = album.job.options.last_resort_rounds;
            track.retry_at = None;
            track.bytes_done = 0;
            track.speed = 0.0;
            track.note = "retrying failed track".to_string();
            retried += 1;
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
        }
        if retried > 0 {
            if matches!(album.source_mode, SourceMode::Probing { .. }) {
                album.source_mode = SourceMode::Mixed;
            }
            album.phase = AlbumPhase::Downloading;
            album.completion_announced = false;
            let _ = events.send(SlskEvent::Log(
                LogLevel::Info,
                format!("Retrying {retried} failed track(s)…"),
            ));
        } else {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Info,
                "No failed tracks with known sources to retry.".to_string(),
            ));
        }
    }

    /// Fail every unfinished track. With `keep_verified`, tracks that are
    /// already verified and only waiting for the album gain are kept: that
    /// work is local and does not need the Soulseek session.
    fn fail_active_tracks(&mut self, reason: &str, keep_verified: bool) {
        let clients = self.slot_clients();
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        if album.phase == AlbumPhase::Searching {
            album.phase = AlbumPhase::Downloading;
            album.completion_announced = false;
        }
        let job_id = album.job.job_id;
        for track in &mut album.tracks {
            if !matches!(
                track.status,
                TrackStatus::Queued
                    | TrackStatus::Downloading
                    | TrackStatus::Verifying
                    | TrackStatus::Retrying
            ) {
                continue;
            }
            if keep_verified
                && (track.pending.is_some()
                    || track.pressing_hold.is_some()
                    || track.queued_check.is_some())
            {
                continue;
            }
            if let Some(queued) = track.queued_check.take() {
                let _ = fs::remove_dir_all(&queued.staging);
            }
            if let Some(pending) = track.pending.take() {
                let _ = fs::remove_dir_all(&pending.staging);
            }
            if let Some(hold) = track.pressing_hold.take() {
                let _ = fs::remove_dir_all(&hold.staging);
            }
            if let Some(active) = track.current.take() {
                cancel_transfer(&clients, &active);
                let _ = fs::remove_dir_all(&active.staging);
            }
            if let Some(verifying) = track.verifying.take() {
                let _ = fs::remove_dir_all(&verifying.staging);
            }
            track.status = TrackStatus::Failed;
            track.speed = 0.0;
            track.note = reason.to_string();
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
        }
    }

    fn check_completion(&mut self) {
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        if album.phase != AlbumPhase::Downloading || album.completion_announced {
            return;
        }
        let all_terminal = album.tracks.iter().all(|track| {
            matches!(
                track.status,
                TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
            )
        });
        if !all_terminal {
            return;
        }
        album.completion_announced = true;
        album.phase = AlbumPhase::Complete;
        let _ = fs::remove_dir_all(album.job.album_dir.join(STAGING_DIR));
        let completed = album
            .tracks
            .iter()
            .filter(|track| track.status == TrackStatus::Done)
            .count();
        let failed = album
            .tracks
            .iter()
            .filter(|track| track.status == TrackStatus::Failed)
            .count();
        let cancelled = album
            .tracks
            .iter()
            .filter(|track| track.status == TrackStatus::Cancelled)
            .count();
        let elapsed = album.started.elapsed();
        let job_id = album.job.job_id;
        let _ = events.send(SlskEvent::AlbumComplete {
            job_id,
            completed,
            failed,
            cancelled,
            elapsed,
        });
        if failed == 0 && cancelled == 0 {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Success,
                format!(
                    "All {completed} tracks saved to {} in {}",
                    album.job.album_dir.display(),
                    util::fmt_elapsed(elapsed)
                ),
            ));
        } else {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!(
                    "Finished: {completed} downloaded, {failed} failed, {cancelled} cancelled in {}",
                    util::fmt_elapsed(elapsed)
                ),
            ));
        }
    }

    fn cancel_album(&mut self) {
        if let Some(album) = self.album.as_mut() {
            album.search_cancel.store(true, Ordering::Relaxed);
            album.pressing_cancel.store(true, Ordering::Relaxed);
        }
        self.fail_active_tracks("download cancelled", false);
        let events = self.events.clone();
        let client = self.client.clone();
        if let Some(album) = self.album.take() {
            if let Some(client) = client {
                let _ = client.forget_search(&album.query);
            }
            let _ = fs::remove_dir_all(album.job.album_dir.join(STAGING_DIR));
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!("Cancelled download of “{}”", album.job.album),
            ));
        }
    }

    fn cleanup_shutdown(&mut self) {
        if let Some(album) = self.album.as_mut() {
            album.search_cancel.store(true, Ordering::Relaxed);
            album.pressing_cancel.store(true, Ordering::Relaxed);
        }
        self.fail_active_tracks("app closed", false);
        if let Some(album) = self.album.take() {
            let _ = fs::remove_dir_all(album.job.album_dir.join(STAGING_DIR));
        }
        self.set_wanted_extras(0);
        if let Some(mut client) = self.client.take()
            && let Some(client) = Arc::get_mut(&mut client)
        {
            client.disconnect();
        }
        let _ = self.events.send(SlskEvent::SessionClosed);
    }
}

// -------------------------------------------------------------------------
// Connecting
// -------------------------------------------------------------------------

fn connect_client(config: &Config) -> ConnResult {
    connect_client_with(config, true)
}

/// Log an extra account in on a worker thread. It listens on the next port
/// after the configured one so port forwarding for a small range keeps
/// working, and falls back to any free port.
fn spawn_extra_connect(
    config: &Config,
    username: &str,
    slot: usize,
) -> Option<Receiver<ConnResult>> {
    let mut config = config.clone();
    config.username = username.to_string();
    let preferred = match u16::try_from(slot) {
        Ok(offset) if config.listen_port != 0 => config.listen_port.checked_add(offset),
        _ => None,
    }
    .unwrap_or(0);
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name(format!("soulseek-connect-{slot}"))
        .spawn(move || {
            config.listen_port = preferred;
            let mut result = connect_client(&config);
            if preferred != 0 && matches!(result, ConnResult::Failed { auth: false, .. }) {
                config.listen_port = 0;
                result = connect_client(&config);
            }
            let _ = tx.send(result);
        })
        .ok()?;
    Some(rx)
}

/// The first logged-in account with no live transfer of this album.
fn free_slot(clients: &[Option<Arc<Client>>], album: &AlbumState) -> Option<(usize, Arc<Client>)> {
    clients.iter().enumerate().find_map(|(slot, client)| {
        let client = client.as_ref()?;
        let busy = album.tracks.iter().any(|track| {
            track
                .current
                .as_ref()
                .is_some_and(|active| active.slot == slot)
        });
        (!busy).then(|| (slot, client.clone()))
    })
}

/// Cancel a transfer on the account that carries it. The library keeps a
/// cancelled record when it wants to deny a late peer offer; only drop the
/// record when cancelling found nothing.
fn cancel_transfer(clients: &[Option<Arc<Client>>], active: &DownloadActive) {
    if let Some(Some(client)) = clients.get(active.slot) {
        let cancelled = client.cancel_download(&active.username, &active.path);
        if !cancelled {
            let _ = client.remove_download(&active.username, &active.path);
        }
    }
}

fn connect_client_with(config: &Config, enable_listen: bool) -> ConnResult {
    let mut settings = ClientSettings::new(config.username.clone(), config.password.clone());
    settings.server_address = PeerAddress::new("server.slsknet.org".to_string(), 2416);
    settings.enable_listen = enable_listen;
    settings.listen_port = config.listen_port;
    settings.shared_directories = Vec::new();
    settings.accept_children = false;

    let mut client = Client::with_settings(settings);
    // `Client::with_settings` initializes the library logger from the
    // environment; set the level we actually want after that.
    soulseek_rs::utils::logger::set_log_level(if config.verbose_lib {
        soulseek_rs::utils::logger::LogLevel::Info
    } else {
        soulseek_rs::utils::logger::LogLevel::Error
    });
    if let Err(err) = client.connect() {
        return ConnResult::Failed {
            message: err.to_string(),
            auth: false,
        };
    }
    match client.login() {
        Ok(true) => ConnResult::Connected(Arc::new(client)),
        Ok(false) => ConnResult::Failed {
            message: "the server rejected the login".to_string(),
            auth: true,
        },
        Err(SoulseekRs::AuthenticationFailed) => ConnResult::Failed {
            message: "the username or password was rejected".to_string(),
            auth: true,
        },
        Err(err) => ConnResult::Failed {
            message: err.to_string(),
            auth: false,
        },
    }
}

// -------------------------------------------------------------------------
// Album / track state
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlbumPhase {
    Searching,
    Downloading,
    Complete,
}

struct AlbumState {
    job: DownloadRequest,
    phase: AlbumPhase,
    tracks: Vec<TrackState>,
    cover: Option<Arc<Vec<u8>>>,
    search_cancel: Arc<AtomicBool>,
    query: String,
    last_peer_count: usize,
    last_progress_emit: Instant,
    search_started: Instant,
    search_timeout_sent: bool,
    search_attempt: u64,
    search_restart_pending: bool,
    completion_announced: bool,
    started: Instant,
    /// Peers sharing the whole album in one folder, best first.
    album_sources: Vec<AlbumSource>,
    /// Per-track sources pooled from every peer, used once no single peer's
    /// copy works out.
    fallback: Vec<Vec<Candidate>>,
    source_mode: SourceMode,
    /// Last time each peer (by username) was sending us a track of this album.
    /// A track waiting in a peer's queue is only waiting for our own other
    /// downloads from that peer, so its queue timeout runs from this moment.
    peer_activity: HashMap<String, Instant>,
    /// The one gain applied to every track, once computed. Tracks finished
    /// later (a retry) reuse it so the album stays consistent.
    album_gain: Option<f64>,
    /// ReplayGain mode: the album's loudness (LUFS) and loudest true peak
    /// (dBTP), measured once like `album_gain`.
    replay_album: Option<(f64, f64)>,
    /// Loudness (LUFS, after the album gain) that tracks from outside the
    /// chosen album copy are matched to. Another master of a song can be many
    /// dB louder, and one static gain would leave it sticking out and let its
    /// peak hold the whole album down. Set together with `album_gain`.
    off_source_level: Option<f64>,
    /// The 500x500 `cover.jpg` for the album folder, once fetched.
    folder_cover: Option<Arc<Vec<u8>>>,
    /// `cover.jpg` was written (or deliberately kept); do not look again.
    folder_cover_done: bool,
    /// Actual lengths of the chosen full-album copy's files, per track, from
    /// verification (including files rejected only on length).
    observed_secs: Vec<Option<f64>>,
    /// Every pressing in the release group, once fetched.
    pressings: Option<Arc<Vec<ReleaseInfo>>>,
    pressing_search_running: bool,
    /// Stops the running pressing lookup, which otherwise keeps retrying an
    /// unreachable MusicBrainz.
    pressing_cancel: Arc<AtomicBool>,
    pressing_search_id: u64,
    pressing_switches: u32,
}

/// A file rejected only on length, kept while other pressings are checked.
struct HeldForPressing {
    attempt_no: u32,
    staging: PathBuf,
    staged: PathBuf,
    username: String,
    remote_path: String,
    /// Why it was rejected, used if no pressing fits.
    reason: String,
}

/// Whether this remote file is the chosen full-album copy's file for the
/// track (while probing or committed).
fn album_source_file(album: &AlbumState, index: usize, username: &str, path: &str) -> bool {
    let source = match album.source_mode {
        SourceMode::Probing { source, .. } | SourceMode::Committed { source } => source,
        SourceMode::Mixed => return false,
    };
    album
        .album_sources
        .get(source)
        .and_then(|source| source.files.get(index))
        .and_then(|file| file.as_ref())
        .is_some_and(|file| file.username == username && file.path == path)
}

/// The album's tracks with the best known length of the chosen copy's file:
/// the measured length when verified, else what the peer reported.
fn pressing_tracks(album: &AlbumState) -> Vec<PressingTrack> {
    let source = match album.source_mode {
        SourceMode::Probing { source, .. } | SourceMode::Committed { source } => {
            album.album_sources.get(source)
        }
        SourceMode::Mixed => None,
    };
    album
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let reported = source
                .and_then(|source| source.files.get(index))
                .and_then(|file| file.as_ref())
                .and_then(|file| file.duration_secs)
                .map(f64::from);
            PressingTrack {
                disc: track.meta.disc,
                position: track.meta.position,
                title: track.meta.title.clone(),
                observed_secs: album
                    .observed_secs
                    .get(index)
                    .copied()
                    .flatten()
                    .or(reported),
            }
        })
        .collect()
}

/// Make another pressing the album's release: expected lengths and IDs for
/// every track, plus the release ID and date used in tags. Titles, paths and
/// numbering stay as chosen.
fn apply_pressing(album: &mut AlbumState, release: &ReleaseInfo) {
    for (index, info) in release.tracks.iter().enumerate() {
        for meta in album
            .tracks
            .get_mut(index)
            .map(|track| &mut track.meta)
            .into_iter()
            .chain(album.job.tracks.get_mut(index))
        {
            if let Some(length) = info.length_secs() {
                meta.expected_secs = Some(length);
            }
            meta.recording_mbid = info.recording_mbid.clone();
            meta.track_mbid = info.track_mbid.clone();
        }
    }
    album.job.release_mbid = release.mbid.clone();
    if !release.date.trim().is_empty() {
        album.job.date = release.date.clone();
    }
}

fn pressing_label(release: &ReleaseInfo) -> String {
    let parts: Vec<&str> = [release.date.as_str(), release.country.as_str()]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect();
    if parts.is_empty() {
        "other".to_string()
    } else {
        parts.join(" ")
    }
}

/// Tracks saved before a pressing switch carry the old release's IDs.
fn retag_saved_tracks(album: &AlbumState, events: &Sender<SlskEvent>) {
    for track in &album.tracks {
        if track.status != TrackStatus::Done || !track.downloaded {
            continue;
        }
        // Loudness tags written when the track was saved stay as they are.
        if let Err(err) = write_track_tags(
            &album.job,
            &track.meta,
            album.cover.as_deref(),
            None,
            &track.meta.final_path,
        ) {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!(
                    "{:02} {}: tags not updated for the new pressing ({err})",
                    track.meta.position, track.meta.title
                ),
            ));
        }
    }
}

/// A verified track held in staging until the album gain is applied.
struct PendingAlbumGain {
    attempt_no: u32,
    staging: PathBuf,
    path: PathBuf,
    /// The verification note, shown when the track is done.
    note: String,
    loudness: Option<Loudness>,
    /// Whether the file is the chosen full-album copy's own file. Only those
    /// tracks share the album gain; see `AlbumState::off_source_level`.
    from_album_source: bool,
    /// ReplayGain mode: the tags to write, set when the worker starts.
    replay_gain: Option<ReplayGain>,
    /// Set when the gain worker starts.
    deadline: Option<Instant>,
}

/// Album gain needs every track's measurement: wait until no track is still
/// searching, downloading or verifying.
fn album_ready_for_gain(album: &AlbumState) -> bool {
    album.tracks.iter().any(|track| track.pending.is_some())
        && album.tracks.iter().all(|track| {
            track.pending.is_some()
                || matches!(
                    track.status,
                    TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
                )
        })
}

/// How tracks get their sources.
///
/// Rips of different masters often split tracks at slightly different points,
/// so a gapless album assembled from several peers can cut or repeat audio
/// between tracks. When peers share the whole album, the first needed track
/// is downloaded and verified from the best one alone (`Probing`). If it
/// checks out, every other track comes from that same folder (`Committed`);
/// if not, the next full-album peer is probed. Sources are only mixed
/// (`Mixed`) when no full-album copy works out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceMode {
    Mixed,
    Probing { source: usize, track: usize },
    Committed { source: usize },
}

struct DownloadActive {
    /// Which Soulseek login carries this transfer: 0 is the main account,
    /// `n` is the `n`th extra account (see `ExtraAccount`).
    slot: usize,
    username: String,
    path: String,
    staging: PathBuf,
    started: Instant,
    last_progress: Instant,
    last_bytes: u64,
    in_progress: bool,
    stall_patience: Duration,
    queue_patience: Duration,
    attempt_no: u32,
}

struct VerifyingAttempt {
    attempt_no: u32,
    staging: PathBuf,
    deadline: Instant,
    /// Which remote file this attempt is, to tell full-album source files
    /// from pooled ones.
    username: String,
    remote_path: String,
}

/// A downloaded file waiting for a processing slot to be checked.
struct QueuedCheck {
    attempt_no: u32,
    staged: PathBuf,
    staging: PathBuf,
    username: String,
    remote_path: String,
}

struct TrackState {
    meta: TrackMeta,
    status: TrackStatus,
    candidates: Vec<Candidate>,
    next_candidate: usize,
    attempts_total: usize,
    attempts_started: usize,
    rounds_left: u32,
    retry_at: Option<Instant>,
    current: Option<DownloadActive>,
    verifying: Option<VerifyingAttempt>,
    pending: Option<PendingAlbumGain>,
    pressing_hold: Option<HeldForPressing>,
    queued_check: Option<QueuedCheck>,
    downloaded: bool,
    bytes_done: u64,
    total: u64,
    speed: f64,
    source: String,
    note: String,
    last_emit: Instant,
    last_emit_status: TrackStatus,
    last_emit_note: String,
}

impl TrackState {
    fn new(meta: TrackMeta, existing_size: Option<u64>) -> Self {
        let (status, bytes, total, note) = match existing_size {
            Some(size) => (TrackStatus::Done, size, size, "already on disk".to_string()),
            None => (TrackStatus::Queued, 0, 0, "waiting for search".to_string()),
        };
        Self {
            meta,
            status,
            candidates: Vec::new(),
            next_candidate: 0,
            attempts_total: 0,
            attempts_started: 0,
            rounds_left: 0,
            retry_at: None,
            current: None,
            verifying: None,
            pending: None,
            pressing_hold: None,
            queued_check: None,
            downloaded: false,
            bytes_done: bytes,
            total,
            speed: 0.0,
            source: String::new(),
            note: note.clone(),
            last_emit: Instant::now(),
            last_emit_status: status,
            last_emit_note: note,
        }
    }

    fn update(&self, job_id: u64) -> TrackUpdate {
        TrackUpdate {
            job_id,
            index: self.meta.index,
            status: self.status,
            bytes_done: self.bytes_done,
            total: self.total,
            speed: self.speed,
            source: self.source.clone(),
            attempt: self.attempts_started,
            attempts_total: self.attempts_total,
            note: self.note.clone(),
        }
    }

    fn emit_if_due(&mut self, events: &Sender<SlskEvent>, job_id: u64) {
        let now = Instant::now();
        if self.status != self.last_emit_status
            || self.note != self.last_emit_note
            || now.duration_since(self.last_emit) >= Duration::from_millis(250)
        {
            self.last_emit = now;
            self.last_emit_status = self.status;
            self.last_emit_note = self.note.clone();
            let _ = events.send(SlskEvent::Track(self.update(job_id)));
        }
    }
}

/// Move a track to the next source, a last-resort pass, or the failed state.
fn advance_after_failure(album: &mut AlbumState, index: usize, events: &Sender<SlskEvent>) {
    if let SourceMode::Probing { track, .. } = album.source_mode
        && track == index
    {
        advance_probe(album, events);
        return;
    }
    let job_id = album.job.job_id;
    let Some(track) = album.tracks.get_mut(index) else {
        return;
    };
    track.speed = 0.0;
    if track.next_candidate < track.candidates.len() {
        track.status = TrackStatus::Queued;
        track.note = format!(
            "trying next source ({}/{})",
            track.next_candidate + 1,
            track.candidates.len()
        );
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
        return;
    }
    if track.rounds_left > 0 {
        track.rounds_left -= 1;
        track.next_candidate = 0;
        track.status = TrackStatus::Retrying;
        track.retry_at = Some(Instant::now() + LAST_RESORT_BACKOFF);
        track.note = format!(
            "all sources failed — retrying in {}s ({} pass(es) left)",
            LAST_RESORT_BACKOFF.as_secs(),
            track.rounds_left
        );
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
        let _ = events.send(SlskEvent::Log(
            LogLevel::Warn,
            format!(
                "{:02} {}: all sources failed; last-resort pass scheduled ({} left)",
                track.meta.position, track.meta.title, track.rounds_left
            ),
        ));
        return;
    }
    track.status = TrackStatus::Failed;
    track.note = if track.candidates.is_empty() {
        "no matching source found".to_string()
    } else {
        format!("all {} sources failed", track.candidates.len())
    };
    let _ = events.send(SlskEvent::Track(track.update(job_id)));
    let _ = events.send(SlskEvent::Log(
        LogLevel::Error,
        format!(
            "{:02} {}: {}",
            track.meta.position, track.meta.title, track.note
        ),
    ));
}

/// Write the chosen release's canonical tags (and cover, if known) to a file.
fn write_track_tags(
    job: &DownloadRequest,
    meta: &TrackMeta,
    cover: Option<&Vec<u8>>,
    replay_gain: Option<ReplayGain>,
    path: &Path,
) -> anyhow::Result<()> {
    let date = (!job.date.trim().is_empty()).then_some(job.date.as_str());
    let disc_track_total = job
        .tracks
        .iter()
        .filter(|other| other.disc == meta.disc)
        .count() as u32;
    let disc_total = job.disc_total();
    let prefix_tracks = job.options.disc_prefix_track_numbers && disc_total > 1;
    let track_number = tagged_track_number(meta.disc, meta.position, disc_total, prefix_tracks);
    let track_total = tagged_track_number(meta.disc, disc_track_total, disc_total, prefix_tracks);
    let (tag_artist, track_artists) = tagged_artist(
        &meta.artist,
        &job.artist,
        job.options.album_artist_for_tracks,
    );
    let tags = TrackTags {
        title: &meta.title,
        artist: tag_artist,
        track_artists,
        album_artist: &job.artist,
        album: &job.album,
        date,
        track: track_number,
        track_total,
        disc: meta.disc,
        disc_total,
        release_mbid: &job.release_mbid,
        release_group_mbid: &job.release_group_mbid,
        recording_mbid: &meta.recording_mbid,
        track_mbid: &meta.track_mbid,
        cover: cover.map(|cover| cover.as_slice()),
        replay_gain,
    };
    audio::write_tags(path, &tags)
}

/// Decide where tracks come from after the album search.
fn plan_sources(
    album: &mut AlbumState,
    album_sources: Vec<AlbumSource>,
    fallback: Vec<Vec<Candidate>>,
    events: &Sender<SlskEvent>,
) {
    album.album_sources = album_sources;
    album.fallback = fallback;
    album.source_mode = SourceMode::Mixed;
    album.peer_activity.clear();
    let job_id = album.job.job_id;
    let rounds = album.job.options.last_resort_rounds;
    let needed: Vec<usize> = album
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.status != TrackStatus::Done)
        .map(|(index, _)| index)
        .collect();

    // A single missing track cannot be mixed with anything; just get it.
    if needed.len() < 2 || album.album_sources.is_empty() {
        if needed.len() >= 2 {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Info,
                "No single peer shares the whole album; mixing sources.".to_string(),
            ));
        }
        use_fallback_sources(album, events);
        return;
    }

    for &index in &needed {
        let track = &mut album.tracks[index];
        track.candidates.clear();
        track.next_candidate = 0;
        track.attempts_total = 0;
        track.rounds_left = rounds;
        track.retry_at = None;
        track.status = TrackStatus::Queued;
        track.note = "awaiting source check".to_string();
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
    }
    let probe = needed[0];
    album.source_mode = SourceMode::Probing {
        source: 0,
        track: probe,
    };
    let _ = events.send(SlskEvent::Log(
        LogLevel::Info,
        format!(
            "{} peer(s) share the whole album; checking {}'s copy with {:02} {} first.",
            album.album_sources.len(),
            album.album_sources[0].username,
            album.tracks[probe].meta.position,
            album.tracks[probe].meta.title,
        ),
    ));
    start_probe(album, events);
}

/// Point the probe track at the current full-album source's file.
fn start_probe(album: &mut AlbumState, events: &Sender<SlskEvent>) {
    let SourceMode::Probing { source, track } = album.source_mode else {
        return;
    };
    let job_id = album.job.job_id;
    let total = album.album_sources.len();
    let Some(album_source) = album.album_sources.get(source) else {
        use_fallback_sources(album, events);
        return;
    };
    let Some(file) = album_source.files.get(track).cloned().flatten() else {
        use_fallback_sources(album, events);
        return;
    };
    let Some(state) = album.tracks.get_mut(track) else {
        return;
    };
    state.candidates = vec![file];
    state.next_candidate = 0;
    state.retry_at = None;
    state.attempts_total = state.attempts_started + (total - source);
    state.status = TrackStatus::Queued;
    state.note = format!("full-album check {}/{total}", source + 1);
    let _ = events.send(SlskEvent::Track(state.update(job_id)));
}

/// The probe failed: try the next full-album source, or mix sources.
fn advance_probe(album: &mut AlbumState, events: &Sender<SlskEvent>) {
    let SourceMode::Probing { source, track } = album.source_mode else {
        return;
    };
    let failed = album
        .album_sources
        .get(source)
        .map(|source| source.username.clone())
        .unwrap_or_default();
    let next = source + 1;
    if let Some(next_source) = album.album_sources.get(next) {
        let _ = events.send(SlskEvent::Log(
            LogLevel::Warn,
            format!(
                "{failed}'s copy did not check out; trying {}'s full album ({}/{}).",
                next_source.username,
                next + 1,
                album.album_sources.len()
            ),
        ));
        album.source_mode = SourceMode::Probing {
            source: next,
            track,
        };
        start_probe(album, events);
    } else {
        let _ = events.send(SlskEvent::Log(
            LogLevel::Warn,
            "No full-album copy checked out; mixing sources as a last resort.".to_string(),
        ));
        use_fallback_sources(album, events);
    }
}

/// Commit to the full-album source once the probe's current attempt passed
/// verification. Stale or unrelated results are ignored.
fn commit_probe_if_verified(
    album: &mut AlbumState,
    index: usize,
    attempt_no: u32,
    events: &Sender<SlskEvent>,
) {
    let SourceMode::Probing { track, .. } = album.source_mode else {
        return;
    };
    let current = album.tracks.get(index).is_some_and(|state| {
        state.status == TrackStatus::Verifying
            && state
                .verifying
                .as_ref()
                .is_some_and(|verifying| verifying.attempt_no == attempt_no)
    });
    if track == index && current {
        commit_album_source(album, events);
    }
}

/// The probe track verified: take every other track from the same folder.
fn commit_album_source(album: &mut AlbumState, events: &Sender<SlskEvent>) {
    let SourceMode::Probing {
        source,
        track: probe,
    } = album.source_mode
    else {
        return;
    };
    let Some(album_source) = album.album_sources.get(source).cloned() else {
        return;
    };
    let job_id = album.job.job_id;
    album.source_mode = SourceMode::Committed { source };
    for (index, track) in album.tracks.iter_mut().enumerate() {
        if index == probe || !waiting_for_sources(track) {
            continue;
        }
        let Some(file) = album_source.files.get(index).cloned().flatten() else {
            continue;
        };
        // The committed peer's file comes first; the pooled sources are only
        // a last resort if that file cannot be fetched or fails verification.
        let mut candidates = vec![file.clone()];
        candidates.extend(
            album
                .fallback
                .get(index)
                .into_iter()
                .flatten()
                .filter(|other| !(other.username == file.username && other.path == file.path))
                .cloned(),
        );
        track.attempts_total = track.attempts_started + candidates.len();
        track.candidates = candidates;
        track.next_candidate = 0;
        track.note = "waiting for a slot".to_string();
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
    }
    let _ = events.send(SlskEvent::Log(
        LogLevel::Success,
        format!(
            "{}'s copy checked out; downloading the rest of the album from {}.",
            album_source.username, album_source.folder
        ),
    ));
}

/// Give every waiting track its pooled per-track sources.
fn use_fallback_sources(album: &mut AlbumState, events: &Sender<SlskEvent>) {
    album.source_mode = SourceMode::Mixed;
    let job_id = album.job.job_id;
    let rounds = album.job.options.last_resort_rounds;
    for (index, track) in album.tracks.iter_mut().enumerate() {
        if !waiting_for_sources(track) {
            continue;
        }
        let sources = album.fallback.get(index).cloned().unwrap_or_default();
        track.attempts_total = track.attempts_started + sources.len();
        track.next_candidate = 0;
        track.rounds_left = rounds;
        track.retry_at = None;
        track.speed = 0.0;
        if sources.is_empty() {
            track.candidates.clear();
            track.status = TrackStatus::Failed;
            track.note = "no matching source found".to_string();
        } else {
            track.candidates = sources;
            track.status = TrackStatus::Queued;
            track.note = "waiting for a slot".to_string();
        }
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
    }
}

/// The tracks the scheduler may start now, within `max_concurrent` live
/// transfers and the `slots` logged-in accounts that can carry them.
fn tracks_to_start(album: &AlbumState, now: Instant, slots: usize) -> Vec<usize> {
    let probe_track = match album.source_mode {
        SourceMode::Probing { track, .. } => Some(track),
        _ => None,
    };
    let max = album.job.options.max_concurrent.max(1).min(slots);
    let mut active = album
        .tracks
        .iter()
        .filter(|track| uses_download_slot(track))
        .count();
    let mut to_start = Vec::new();
    for (index, track) in album.tracks.iter().enumerate() {
        if active >= max {
            break;
        }
        if probe_track.is_some_and(|probe| probe != index) {
            // Other tracks wait until the full-album source checks out.
            continue;
        }
        let retrying_due =
            track.status == TrackStatus::Retrying && track.retry_at.is_some_and(|at| now >= at);
        if retrying_due || track.status == TrackStatus::Queued {
            to_start.push(index);
            active += 1;
        }
    }
    to_start
}

/// Whether a track occupies one of the `max_concurrent` slots: only a live
/// transfer does. Once a file is down, checking, measuring and converting it
/// are limited by `max_processing` instead, so they never hold up the next
/// download.
fn uses_download_slot(track: &TrackState) -> bool {
    track.current.is_some() && track.status == TrackStatus::Downloading
}

/// Workers currently checking, measuring or converting files.
fn processing_in_use(album: &AlbumState) -> usize {
    album
        .tracks
        .iter()
        .filter(|track| {
            track.verifying.is_some()
                || track
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.deadline.is_some())
        })
        .count()
}

/// A track that still needs a source and is not transferring or verifying.
/// A probe track whose attempt was just torn down still reports its old
/// `Downloading`/`Verifying` status, so the status alone is not enough.
fn waiting_for_sources(track: &TrackState) -> bool {
    track.current.is_none()
        && track.verifying.is_none()
        && track.pending.is_none()
        && track.pressing_hold.is_none()
        && track.queued_check.is_none()
        && !matches!(
            track.status,
            TrackStatus::Done | TrackStatus::Failed | TrackStatus::Cancelled
        )
}

/// Keep probing with another track if the probe track was cancelled, failed
/// for another reason, or turned out to be on disk already.
fn ensure_probe_track(album: &mut AlbumState, events: &Sender<SlskEvent>) {
    let SourceMode::Probing { source, track } = album.source_mode else {
        return;
    };
    let probe_alive = album.tracks.get(track).is_some_and(|state| {
        matches!(
            state.status,
            TrackStatus::Queued
                | TrackStatus::Downloading
                | TrackStatus::Verifying
                | TrackStatus::Retrying
        )
    });
    if probe_alive {
        return;
    }
    let next = album
        .tracks
        .iter()
        .enumerate()
        .find(|(index, state)| *index != track && waiting_for_sources(state))
        .map(|(index, _)| index);
    match next {
        Some(next) => {
            album.source_mode = SourceMode::Probing {
                source,
                track: next,
            };
            start_probe(album, events);
        }
        None => album.source_mode = SourceMode::Mixed,
    }
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

fn convert_hits(results: Vec<soulseek_rs::SearchResult>) -> Vec<SearchHit> {
    results
        .into_iter()
        .filter_map(|result| {
            let files: Vec<RemoteFile> = result
                .files
                .into_iter()
                .map(|file| RemoteFile {
                    path: file.name,
                    size: file.size,
                    duration_secs: file.attribs.get(&1).copied().filter(|seconds| *seconds > 0),
                    bitrate_kbps: file.attribs.get(&0).copied().filter(|kbps| *kbps > 0),
                })
                .collect();
            if files.is_empty() {
                return None;
            }
            Some(SearchHit {
                username: result.username,
                free_slot: result.slots > 0,
                speed: result.speed,
                files,
            })
        })
        .collect()
}

fn staging_session_id() -> u64 {
    static SESSION: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SESSION.get_or_init(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0)
    })
}

/// When a queued transfer's timeout starts counting: when it began, or later
/// if the same peer was last seen sending us another track.
fn queue_wait_started(started: Instant, peer_activity: Option<Instant>) -> Instant {
    peer_activity.map_or(started, |at| at.max(started))
}

fn find_live_download<'a>(
    downloads: &'a [soulseek_rs::types::Download],
    username: &str,
    filename: &str,
) -> Option<&'a soulseek_rs::types::Download> {
    downloads
        .iter()
        .filter(|download| download.username == username && download.filename == filename)
        .min_by_key(|download| download.status.is_terminal())
}

fn staged_file(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        if util::extension_of(&name.to_string_lossy()).eq_ignore_ascii_case("flac")
            || util::extension_of(&name.to_string_lossy()).eq_ignore_ascii_case("mp3")
        {
            candidates.push(path);
        }
    }
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    });
    candidates.pop()
}

pub(crate) fn existing_file_is_valid(meta: &TrackMeta, tolerance_secs: f64) -> Option<u64> {
    let metadata = fs::metadata(&meta.final_path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let duration = audio::duration_secs(&meta.final_path).ok()?;
    if let Some(expected) = meta.expected_secs
        && tolerance_secs > 0.0
        && (duration - expected).abs() > tolerance_secs + 1.0
    {
        return None;
    }
    Some(metadata.len())
}

fn backup_path(target: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("track");
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.{}.{}.soulsucker-old",
        staging_session_id(),
        sequence
    ))
}

/// Promote a verified staging file into its final name without losing an
/// existing file if the rename fails.
fn promote_file(source: &Path, target: &Path) -> std::io::Result<()> {
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if !target.exists() {
                return Err(first_error);
            }
            let backup = backup_path(target);
            fs::rename(target, &backup)?;
            match fs::rename(source, target) {
                Ok(()) => {
                    let _ = fs::remove_file(&backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::rename(&backup, target);
                    Err(error)
                }
            }
        }
    }
}

/// Some players (notably Tesla's USB player) ignore the disc tag and sort only
/// by track number. Opt-in disc-prefixed numbers keep both discs in order:
/// disc 1 becomes 101..113 and disc 2 becomes 201..213.
/// Choose the ARTIST tag and keep a differing per-track credit in ARTISTS.
fn tagged_artist<'a>(
    track_artist: &'a str,
    album_artist: &'a str,
    album_artist_for_tracks: bool,
) -> (&'a str, Option<&'a str>) {
    if album_artist_for_tracks {
        let track_artists =
            (!track_artist.eq_ignore_ascii_case(album_artist)).then_some(track_artist);
        (album_artist, track_artists)
    } else {
        (track_artist, None)
    }
}

/// Verification normally has a fixed deadline. Two-pass normalization or MP3
/// conversion can take much longer than a fingerprint check, so scale the
/// allowance with the expected track length and cap it at 30 minutes.
fn verification_deadline(
    started: Instant,
    expected_secs: Option<f64>,
    audio_processing: bool,
) -> Instant {
    if audio_processing {
        let expected = expected_secs.unwrap_or(300.0).max(0.0);
        started + VERIFY_DEADLINE + Duration::from_secs_f64((expected * 2.0).min(1800.0))
    } else {
        started + VERIFY_DEADLINE
    }
}

fn tagged_track_number(disc: u32, position: u32, disc_count: u32, prefix: bool) -> u32 {
    if prefix && disc_count > 1 {
        disc.max(1) * 100 + position
    } else {
        position
    }
}

/// Build a final track filename with a specific extension.
pub fn track_filename_with_ext(
    disc: u32,
    position: u32,
    title: &str,
    disc_count: u32,
    track_count: u32,
    extension: &str,
) -> String {
    let width = track_count.to_string().len().max(2);
    let title = util::sanitize_filename(title);
    if disc_count > 1 {
        format!("{disc}-{position:0width$} - {title}.{extension}")
    } else {
        format!("{position:0width$} - {title}.{extension}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_prefixed_track_numbers_keep_discs_in_order() {
        assert_eq!(tagged_track_number(1, 1, 2, true), 101);
        assert_eq!(tagged_track_number(1, 13, 2, true), 113);
        assert_eq!(tagged_track_number(2, 1, 2, true), 201);
        assert_eq!(tagged_track_number(2, 13, 2, true), 213);
        assert_eq!(tagged_track_number(1, 1, 1, true), 1);
        assert_eq!(tagged_track_number(2, 8, 2, false), 8);
    }

    #[test]
    fn audio_processing_extends_the_verification_deadline() {
        let start = Instant::now();
        assert_eq!(
            verification_deadline(start, Some(300.0), false),
            start + VERIFY_DEADLINE
        );
        assert!(
            verification_deadline(start, Some(300.0), true)
                > verification_deadline(start, Some(300.0), false)
        );
        assert_eq!(
            verification_deadline(start, Some(3600.0), true),
            start + VERIFY_DEADLINE + Duration::from_secs(1800)
        );
    }

    #[test]
    fn album_artist_can_replace_track_artist_for_car_stereos() {
        assert_eq!(tagged_artist("Staind", "Staind", false), ("Staind", None));
        assert_eq!(
            tagged_artist("Aaron Lewis and Fred Durst", "Staind", true),
            ("Staind", Some("Aaron Lewis and Fred Durst"))
        );
        assert_eq!(tagged_artist("Staind", "Staind", true), ("Staind", None));
    }

    #[test]
    fn filenames_are_zero_padded_and_disc_aware() {
        assert_eq!(
            track_filename_with_ext(1, 1, "Fight Fire With Fire", 1, 8, "flac"),
            "01 - Fight Fire With Fire.flac"
        );
        assert_eq!(
            track_filename_with_ext(2, 10, "Song/Two", 2, 10, "flac"),
            "2-10 - Song_Two.flac"
        );
        assert_eq!(
            track_filename_with_ext(1, 1, "Fight Fire With Fire", 1, 8, "mp3"),
            "01 - Fight Fire With Fire.mp3"
        );
    }

    #[test]
    fn wide_track_numbers_use_more_padding() {
        assert_eq!(
            track_filename_with_ext(1, 3, "Track", 1, 120, "flac"),
            "003 - Track.flac"
        );
    }

    fn album_with_exhausted_track(rounds_left: u32) -> AlbumState {
        let meta = TrackMeta {
            index: 0,
            disc: 1,
            position: 1,
            title: "Song".into(),
            artist: "Artist".into(),
            expected_secs: Some(180.0),
            recording_mbid: "rec".into(),
            track_mbid: "track".into(),
            final_path: PathBuf::from("/tmp/song.flac"),
        };
        let mut track = TrackState::new(meta.clone(), None);
        track.candidates = vec![
            Candidate {
                username: "a".into(),
                path: "a\\song.flac".into(),
                size: 20_000_000,
                duration_secs: Some(180),
                match_score: 1.0,
                free_slot: true,
                user_speed: 100,
            },
            Candidate {
                username: "b".into(),
                path: "b\\song.flac".into(),
                size: 20_000_000,
                duration_secs: Some(180),
                match_score: 1.0,
                free_slot: true,
                user_speed: 100,
            },
        ];
        track.next_candidate = 2;
        track.rounds_left = rounds_left;
        track.status = TrackStatus::Downloading;
        AlbumState {
            job: DownloadRequest {
                job_id: 1,
                artist: "Artist".into(),
                album: "Album".into(),
                date: "2000".into(),
                release_mbid: "release".into(),
                release_group_mbid: "group".into(),
                album_dir: PathBuf::from("/tmp/album"),
                tracks: vec![meta],
                options: DownloadOptions {
                    max_concurrent: 1,
                    max_processing: 3,
                    search_timeout: Duration::from_secs(1),
                    queue_timeout: Duration::from_secs(1),
                    stall_timeout: Duration::from_secs(1),
                    length_tolerance: 5.0,
                    api_key: String::new(),
                    fingerprint_min_score: 0.8,
                    last_resort_rounds: 1,
                    last_resort_stall: Duration::from_secs(10),
                    candidates_per_track: 12,
                    disc_prefix_track_numbers: false,
                    album_artist_for_tracks: false,
                    normalize_loudness: false,
                    normalize_target_lufs: -16.0,
                    normalize_method: NormalizeMethod::Gain,
                    folder_cover: false,
                    audio_quality: AudioQuality::Flac,
                },
                loose_files: false,
                force: false,
            },
            phase: AlbumPhase::Downloading,
            tracks: vec![track],
            cover: None,
            search_cancel: Arc::new(AtomicBool::new(false)),
            query: "Artist Album".into(),
            last_peer_count: 0,
            last_progress_emit: Instant::now(),
            search_started: Instant::now(),
            search_timeout_sent: false,
            search_attempt: 0,
            search_restart_pending: false,
            completion_announced: false,
            started: Instant::now(),
            album_sources: Vec::new(),
            fallback: Vec::new(),
            source_mode: SourceMode::Mixed,
            peer_activity: HashMap::new(),
            album_gain: None,
            replay_album: None,
            off_source_level: None,
            folder_cover: None,
            folder_cover_done: false,
            observed_secs: vec![None],
            pressings: None,
            pressing_search_running: false,
            pressing_cancel: Arc::new(AtomicBool::new(false)),
            pressing_search_id: 0,
            pressing_switches: 0,
        }
    }

    fn source_file(username: &str, folder: &str, title: &str) -> Candidate {
        Candidate {
            username: username.into(),
            path: format!("{folder}\\{title}.flac"),
            size: 20_000_000,
            duration_secs: Some(180),
            match_score: 1.0,
            free_slot: true,
            user_speed: 100,
        }
    }

    fn album_source(username: &str, titles: &[&str]) -> AlbumSource {
        AlbumSource {
            username: username.into(),
            folder: format!("{username} rip"),
            files: titles
                .iter()
                .map(|title| Some(source_file(username, &format!("{username} rip"), title)))
                .collect(),
            average_score: 1.0,
            free_slot: true,
            user_speed: 100,
        }
    }

    /// Three queued tracks straight after the search, plus two full-album
    /// peers and a pooled per-track list from a third peer.
    fn album_after_search() -> (AlbumState, Vec<AlbumSource>, Vec<Vec<Candidate>>) {
        let mut album = album_with_exhausted_track(1);
        let template = album.tracks[0].meta.clone();
        album.tracks.clear();
        album.job.tracks.clear();
        let titles = ["One", "Two", "Three"];
        for (index, title) in titles.iter().enumerate() {
            let mut meta = template.clone();
            meta.index = index;
            meta.position = index as u32 + 1;
            meta.title = (*title).into();
            album.job.tracks.push(meta.clone());
            album.tracks.push(TrackState::new(meta, None));
        }
        album.observed_secs = vec![None; titles.len()];
        let sources = vec![
            album_source("first", &titles),
            album_source("second", &titles),
        ];
        let fallback = titles
            .iter()
            .map(|title| {
                vec![
                    source_file("pool", "misc", title),
                    // The committed peer's own file must not be tried twice.
                    source_file("first", "first rip", title),
                ]
            })
            .collect();
        (album, sources, fallback)
    }

    #[test]
    fn a_full_album_source_is_checked_with_the_first_track_alone() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);

        assert_eq!(
            album.source_mode,
            SourceMode::Probing {
                source: 0,
                track: 0
            }
        );
        assert_eq!(album.tracks[0].candidates.len(), 1);
        assert_eq!(album.tracks[0].candidates[0].username, "first");
        for track in &album.tracks[1..] {
            assert_eq!(track.status, TrackStatus::Queued);
            assert!(
                track.candidates.is_empty(),
                "other tracks wait for the check"
            );
        }
    }

    #[test]
    fn a_verified_probe_commits_every_track_to_that_peer() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);
        album.tracks[0].status = TrackStatus::Done;
        commit_album_source(&mut album, &tx);

        assert_eq!(album.source_mode, SourceMode::Committed { source: 0 });
        assert_eq!(album.album_sources[0].username, "first");
        for track in &album.tracks[1..] {
            let users: Vec<&str> = track
                .candidates
                .iter()
                .map(|candidate| candidate.username.as_str())
                .collect();
            assert_eq!(users, ["first", "pool"], "{}", track.meta.title);
            assert!(track.candidates[0].path.starts_with("first rip"));
        }
    }

    #[test]
    fn a_failed_probe_moves_to_the_next_full_album_peer_then_mixes() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);

        album.tracks[0].next_candidate = 1;
        advance_after_failure(&mut album, 0, &tx);
        assert_eq!(
            album.source_mode,
            SourceMode::Probing {
                source: 1,
                track: 0
            }
        );
        assert_eq!(album.tracks[0].status, TrackStatus::Queued);
        assert_eq!(album.tracks[0].candidates[0].username, "second");
        assert_eq!(album.tracks[0].next_candidate, 0);
        assert!(album.tracks[1].candidates.is_empty());

        album.tracks[0].next_candidate = 1;
        advance_after_failure(&mut album, 0, &tx);
        assert_eq!(album.source_mode, SourceMode::Mixed);
        for track in &album.tracks {
            assert_eq!(track.status, TrackStatus::Queued);
            assert_eq!(track.candidates.len(), 2);
            assert_eq!(track.candidates[0].username, "pool");
        }
    }

    #[test]
    fn sources_are_mixed_when_no_peer_has_the_whole_album() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, _, fallback) = album_after_search();
        plan_sources(&mut album, Vec::new(), fallback, &tx);
        assert_eq!(album.source_mode, SourceMode::Mixed);
        assert!(album.tracks.iter().all(|track| track.candidates.len() == 2));
    }

    #[test]
    fn a_single_missing_track_does_not_wait_for_a_probe() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        album.tracks[0].status = TrackStatus::Done;
        album.tracks[1].status = TrackStatus::Done;
        plan_sources(&mut album, sources, fallback, &tx);
        assert_eq!(album.source_mode, SourceMode::Mixed);
        assert_eq!(album.tracks[2].candidates.len(), 2);
    }

    #[test]
    fn cancelling_the_probe_track_probes_with_the_next_track() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);
        album.tracks[0].status = TrackStatus::Cancelled;
        ensure_probe_track(&mut album, &tx);
        assert_eq!(
            album.source_mode,
            SourceMode::Probing {
                source: 0,
                track: 1
            }
        );
        assert_eq!(album.tracks[1].candidates[0].username, "first");
    }

    #[test]
    fn an_exhausted_track_schedules_a_last_resort_pass() {
        let (tx, _rx) = mpsc::channel();
        let mut album = album_with_exhausted_track(1);
        advance_after_failure(&mut album, 0, &tx);
        let track = &album.tracks[0];
        assert_eq!(track.status, TrackStatus::Retrying);
        assert_eq!(track.rounds_left, 0);
        assert_eq!(track.next_candidate, 0);
        assert!(track.retry_at.is_some());
    }

    #[test]
    fn a_failed_last_resort_pass_marks_the_track_failed() {
        let (tx, _rx) = mpsc::channel();
        let mut album = album_with_exhausted_track(0);
        advance_after_failure(&mut album, 0, &tx);
        assert_eq!(album.tracks[0].status, TrackStatus::Failed);
        assert!(album.tracks[0].note.contains("all 2 sources failed"));
    }

    #[test]
    fn an_exhausted_track_with_another_source_is_queued_again() {
        let (tx, _rx) = mpsc::channel();
        let mut album = album_with_exhausted_track(1);
        album.tracks[0].next_candidate = 1;
        advance_after_failure(&mut album, 0, &tx);
        assert_eq!(album.tracks[0].status, TrackStatus::Queued);
        assert_eq!(album.tracks[0].rounds_left, 1);
    }

    #[test]
    fn a_stale_search_result_is_ignored() {
        let mut album = album_with_exhausted_track(1);
        album.phase = AlbumPhase::Searching;
        album.search_attempt = 2;
        let (events_tx, _events_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut session = Session {
            config: Config::default(),
            events: events_tx,
            commands: cmd_rx,
            inner_tx: cmd_tx,
            client: None,
            extras: Vec::new(),
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: Some(album),
            last_session_check: Instant::now(),
        };
        session.search_done(1, 1, Ok(Vec::new()));
        let album = session.album.as_ref().expect("album must be restored");
        assert_eq!(album.phase, AlbumPhase::Searching);
        assert_eq!(album.search_attempt, 2);
        assert_eq!(album.tracks[0].status, TrackStatus::Downloading);

        session.album.as_mut().unwrap().search_restart_pending = true;
        session.search_done(1, 2, Ok(Vec::new()));
        let album = session.album.as_ref().unwrap();
        assert_eq!(album.phase, AlbumPhase::Searching);
        assert_eq!(album.tracks[0].status, TrackStatus::Downloading);
    }

    #[test]
    fn a_stale_verification_result_does_not_touch_a_newer_attempt() {
        let mut album = album_with_exhausted_track(1);
        album.tracks[0].status = TrackStatus::Verifying;
        album.tracks[0].verifying = Some(VerifyingAttempt {
            attempt_no: 2,
            staging: PathBuf::from("/tmp/soulsucker-nonexistent-staging"),
            deadline: Instant::now() + Duration::from_secs(60),
            username: "a".into(),
            remote_path: "a\\song.flac".into(),
        });
        let (events_tx, _events_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut session = Session {
            config: Config::default(),
            events: events_tx,
            commands: cmd_rx,
            inner_tx: cmd_tx,
            client: None,
            extras: Vec::new(),
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: Some(album),
            last_session_check: Instant::now(),
        };
        session.verified(
            1,
            0,
            1,
            VerifyResult {
                accepted: true,
                note: "late result".into(),
                ..VerifyResult::default()
            },
            PathBuf::from("/tmp/soulsucker-late.flac"),
            None,
        );
        let verifying = session.album.as_ref().unwrap().tracks[0]
            .verifying
            .as_ref()
            .expect("the newer attempt must still be verifying");
        assert_eq!(verifying.attempt_no, 2);
    }

    fn offline_session(album: AlbumState) -> (Session, Receiver<SlskEvent>) {
        let (events_tx, events_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let session = Session {
            config: Config::default(),
            events: events_tx,
            commands: cmd_rx,
            inner_tx: cmd_tx,
            client: None,
            extras: Vec::new(),
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: Some(album),
            last_session_check: Instant::now(),
        };
        (session, events_rx)
    }

    fn hold(track: &mut TrackState, integrated: f64, staging: &Path) {
        track.status = TrackStatus::Verifying;
        track.pending = Some(PendingAlbumGain {
            attempt_no: 1,
            staging: staging.to_path_buf(),
            path: staging.join("missing.flac"),
            note: "duration ok".into(),
            loudness: Some(Loudness {
                integrated,
                true_peak: -6.0,
                duration_secs: 180.0,
            }),
            from_album_source: true,
            replay_gain: None,
            deadline: None,
        });
    }

    /// An album in album-gain mode with tracks 0 and 1 verified and held.
    fn album_waiting_for_gain(quality: AudioQuality) -> (AlbumState, PathBuf) {
        let (mut album, _, _) = album_after_search();
        album.job.options.normalize_loudness = true;
        album.job.options.audio_quality = quality;
        album.job.options.max_processing = 1;
        let staging = std::env::temp_dir().join(format!(
            "soulsucker-gain-hold-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        hold(&mut album.tracks[0], -20.0, &staging);
        hold(&mut album.tracks[1], -24.0, &staging);
        album.tracks[2].status = TrackStatus::Downloading;
        (album, staging)
    }

    #[test]
    fn a_track_from_another_source_does_not_set_the_album_gain() {
        let (mut album, staging) = album_waiting_for_gain(AudioQuality::Flac);
        hold(&mut album.tracks[2], -10.0, &staging);
        album.tracks[2].pending.as_mut().unwrap().from_album_source = false;
        let own_only = normalize::album_gain(
            &[
                album.tracks[0].pending.as_ref().unwrap().loudness.unwrap(),
                album.tracks[1].pending.as_ref().unwrap().loudness.unwrap(),
            ],
            -16.0,
        )
        .unwrap();
        let (mut session, _events) = offline_session(album);
        session.run_album_gain();
        let album = session.album.as_ref().unwrap();
        assert_eq!(album.album_gain, Some(own_only.gain_db));
        assert_eq!(
            album.off_source_level,
            Some(own_only.integrated + own_only.gain_db)
        );
    }

    fn release_with_lengths(mbid: &str, lengths: [u64; 3]) -> ReleaseInfo {
        ReleaseInfo {
            mbid: mbid.into(),
            release_group_mbid: "group".into(),
            title: "Album".into(),
            artist: "Artist".into(),
            date: "2011-09-26".into(),
            country: "XE".into(),
            status: "Official".into(),
            disambiguation: String::new(),
            primary_type: "Album".into(),
            tracks: ["One", "Two", "Three"]
                .iter()
                .zip(lengths)
                .enumerate()
                .map(|(index, (title, secs))| musicbrainz::TrackInfo {
                    index,
                    disc: 1,
                    position: index as u32 + 1,
                    title: (*title).into(),
                    artist: "Artist".into(),
                    length_ms: Some(secs * 1000),
                    recording_mbid: format!("{mbid}-rec{index}"),
                    track_mbid: format!("{mbid}-track{index}"),
                })
                .collect(),
        }
    }

    /// Committed to "first"; track 2 from that copy just came back 106 s
    /// long against an expected 180 s.
    fn committed_album_with_long_track() -> (Session, Receiver<SlskEvent>, PathBuf) {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);
        album.tracks[0].status = TrackStatus::Done;
        commit_album_source(&mut album, &tx);
        let staging = std::env::temp_dir().join(format!(
            "soulsucker-pressing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = album.tracks[1].candidates[0].clone();
        album.tracks[1].next_candidate = 1;
        album.tracks[1].status = TrackStatus::Verifying;
        album.tracks[1].verifying = Some(VerifyingAttempt {
            attempt_no: 1,
            staging: staging.clone(),
            deadline: Instant::now() + Duration::from_secs(60),
            username: file.username,
            remote_path: file.path,
        });
        let (session, events) = offline_session(album);
        (session, events, staging)
    }

    fn length_mismatch(secs: f64) -> VerifyResult {
        VerifyResult {
            accepted: false,
            note: "length 1:46 vs expected 3:00 (±5s)".into(),
            duration_secs: Some(secs),
            length_mismatch: true,
        }
    }

    #[test]
    fn a_length_mismatch_switches_to_a_pressing_that_fits_and_rechecks_the_file() {
        let (mut session, _events, staging) = committed_album_with_long_track();
        // Peer-reported lengths for the rest of the copy are 180 s.
        session.album.as_mut().unwrap().pressings = Some(Arc::new(vec![
            release_with_lengths("release", [180, 180, 180]),
            release_with_lengths("wrong", [180, 120, 180]),
            release_with_lengths("fits", [180, 106, 180]),
        ]));
        session.verified(
            1,
            1,
            1,
            length_mismatch(106.2),
            staging.join("song.flac"),
            None,
        );

        let album = session.album.as_ref().unwrap();
        assert_eq!(album.job.release_mbid, "fits");
        assert_eq!(album.job.date, "2011-09-26");
        assert_eq!(album.pressing_switches, 1);
        assert_eq!(album.tracks[1].meta.expected_secs, Some(106.0));
        assert_eq!(album.job.tracks[1].recording_mbid, "fits-rec1");
        assert_eq!(album.tracks[2].meta.track_mbid, "fits-track2");
        let track = &album.tracks[1];
        assert!(track.pressing_hold.is_none());
        let verifying = track
            .verifying
            .as_ref()
            .expect("the same file is re-verified");
        assert_eq!(verifying.attempt_no, 1);
        assert_eq!(verifying.username, "first");
        assert_eq!(track.next_candidate, 1, "no other source was tried");
    }

    #[test]
    fn without_a_fitting_pressing_the_next_source_is_tried() {
        let (mut session, _events, staging) = committed_album_with_long_track();
        session.album.as_mut().unwrap().pressings = Some(Arc::new(vec![release_with_lengths(
            "wrong",
            [180, 120, 180],
        )]));
        session.verified(
            1,
            1,
            1,
            length_mismatch(106.2),
            staging.join("song.flac"),
            None,
        );
        let album = session.album.as_ref().unwrap();
        assert_eq!(album.job.release_mbid, "release");
        let track = &album.tracks[1];
        assert!(track.pressing_hold.is_none() && track.verifying.is_none());
        assert_eq!(track.status, TrackStatus::Queued);
        assert_eq!(track.candidates[track.next_candidate].username, "pool");
    }

    #[test]
    fn a_running_pressing_lookup_holds_the_track_until_it_returns() {
        let (mut session, _events, staging) = committed_album_with_long_track();
        // Use an already-running lookup so the test never touches the network.
        {
            let album = session.album.as_mut().unwrap();
            album.pressing_search_running = true;
            album.pressing_search_id = 7;
        }
        session.verified(
            1,
            1,
            1,
            length_mismatch(106.2),
            staging.join("song.flac"),
            None,
        );
        let album = session.album.as_ref().unwrap();
        assert!(album.tracks[1].pressing_hold.is_some());
        assert_eq!(album.tracks[1].status, TrackStatus::Verifying);
        assert!(!uses_download_slot(&album.tracks[1]));

        // A stale lookup result is ignored.
        session.pressings_fetched(1, 6, Ok(Vec::new()));
        assert!(
            session.album.as_ref().unwrap().tracks[1]
                .pressing_hold
                .is_some()
        );

        session.pressings_fetched(
            1,
            7,
            Ok(vec![release_with_lengths("fits", [180, 106, 180])]),
        );
        let album = session.album.as_ref().unwrap();
        assert!(!album.pressing_search_running);
        assert_eq!(album.job.release_mbid, "fits");
        assert!(album.tracks[1].verifying.is_some());
    }

    #[test]
    fn a_pooled_file_with_the_wrong_length_just_fails() {
        let (mut session, _events, staging) = committed_album_with_long_track();
        if let Some(verifying) = session.album.as_mut().unwrap().tracks[1].verifying.as_mut() {
            verifying.username = "pool".into();
        }
        session.album.as_mut().unwrap().pressings = Some(Arc::new(vec![release_with_lengths(
            "fits",
            [180, 106, 180],
        )]));
        session.verified(
            1,
            1,
            1,
            length_mismatch(106.2),
            staging.join("song.flac"),
            None,
        );
        let album = session.album.as_ref().unwrap();
        assert_eq!(album.job.release_mbid, "release");
        assert!(album.tracks[1].pressing_hold.is_none());
        assert_eq!(album.tracks[1].status, TrackStatus::Queued);
    }

    #[test]
    fn a_track_held_for_album_gain_does_not_use_a_download_slot() {
        // One slot: the verified probe track is held for the album gain and
        // the rest of the album was committed to the same peer.
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        album.job.options.max_concurrent = 1;
        album.job.options.normalize_loudness = true;
        plan_sources(&mut album, sources, fallback, &tx);
        assert_eq!(tracks_to_start(&album, Instant::now(), usize::MAX), [0]);

        let staging = std::env::temp_dir().join("soulsucker-held-slot");
        hold(&mut album.tracks[0], -20.0, &staging);
        commit_album_source(&mut album, &tx);
        assert_eq!(tracks_to_start(&album, Instant::now(), usize::MAX), [1]);

        // A live transfer still uses its slot.
        album.tracks[1].status = TrackStatus::Downloading;
        album.tracks[1].current = Some(DownloadActive {
            slot: 0,
            username: "peer".into(),
            path: "x.flac".into(),
            staging: staging.clone(),
            started: Instant::now(),
            last_progress: Instant::now(),
            last_bytes: 0,
            in_progress: true,
            stall_patience: Duration::from_secs(10),
            queue_patience: Duration::from_secs(10),
            attempt_no: 1,
        });
        assert!(tracks_to_start(&album, Instant::now(), usize::MAX).is_empty());
    }

    #[test]
    fn queue_timeout_runs_from_the_peers_last_activity() {
        let started = Instant::now();
        let later = started + Duration::from_secs(30);
        // No other transfer from this peer: the timeout counts from the start.
        assert_eq!(queue_wait_started(started, None), started);
        // The peer was sending us another track after this one was queued.
        assert_eq!(queue_wait_started(started, Some(later)), later);
        // Activity from before this attempt began does not shorten the wait.
        assert_eq!(queue_wait_started(later, Some(started)), later);
    }

    #[test]
    fn checking_a_downloaded_file_does_not_use_a_download_slot() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        album.job.options.max_concurrent = 1;
        plan_sources(&mut album, sources, fallback, &tx);
        commit_album_source(&mut album, &tx);
        // Track 0 is downloaded and being checked; track 1 can download.
        album.tracks[0].status = TrackStatus::Verifying;
        album.tracks[0].verifying = Some(VerifyingAttempt {
            attempt_no: 1,
            staging: PathBuf::from("/nonexistent"),
            deadline: Instant::now() + Duration::from_secs(60),
            username: "peer".into(),
            remote_path: "x.flac".into(),
        });
        assert_eq!(tracks_to_start(&album, Instant::now(), usize::MAX), [1]);
        // A file waiting for a processing slot does not block it either.
        album.tracks[0].verifying = None;
        album.tracks[0].queued_check = Some(queued_check());
        assert_eq!(tracks_to_start(&album, Instant::now(), usize::MAX), [1]);
    }

    #[test]
    fn parallel_downloads_are_limited_by_logged_in_accounts() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        album.job.options.max_concurrent = 3;
        plan_sources(&mut album, sources, fallback, &tx);
        commit_album_source(&mut album, &tx);
        assert_eq!(tracks_to_start(&album, Instant::now(), 1).len(), 1);
        assert_eq!(tracks_to_start(&album, Instant::now(), 2).len(), 2);
        assert_eq!(tracks_to_start(&album, Instant::now(), 3).len(), 3);
        assert_eq!(tracks_to_start(&album, Instant::now(), 8).len(), 3);
    }

    #[test]
    fn extra_accounts_append_their_slot_number() {
        assert_eq!(extra_username("hippoforestbob", 1), "hippoforestbob1");
        assert_eq!(extra_username("hippoforestbob", 2), "hippoforestbob2");
    }

    #[test]
    fn probe_commits_as_soon_as_it_is_verified_not_after_processing() {
        let (tx, _rx) = mpsc::channel();
        let (mut album, sources, fallback) = album_after_search();
        plan_sources(&mut album, sources, fallback, &tx);
        let SourceMode::Probing { track: probe, .. } = album.source_mode else {
            panic!("expected probing");
        };
        let other = (0..album.tracks.len())
            .find(|index| *index != probe)
            .unwrap();
        album.tracks[probe].status = TrackStatus::Verifying;
        album.tracks[probe].verifying = Some(VerifyingAttempt {
            attempt_no: 2,
            staging: PathBuf::from("/nonexistent"),
            deadline: Instant::now() + Duration::from_secs(60),
            username: "peer".into(),
            remote_path: "x.flac".into(),
        });

        // Stale attempt and non-probe tracks do not commit.
        commit_probe_if_verified(&mut album, probe, 1, &tx);
        commit_probe_if_verified(&mut album, other, 2, &tx);
        assert!(matches!(album.source_mode, SourceMode::Probing { .. }));

        commit_probe_if_verified(&mut album, probe, 2, &tx);
        assert!(matches!(album.source_mode, SourceMode::Committed { .. }));
        assert_eq!(tracks_to_start(&album, Instant::now(), usize::MAX), [other]);
    }

    fn queued_check() -> QueuedCheck {
        QueuedCheck {
            attempt_no: 1,
            staged: PathBuf::from("/nonexistent/staged.flac"),
            staging: PathBuf::from("/nonexistent/staging"),
            username: "peer".into(),
            remote_path: "x.flac".into(),
        }
    }

    #[test]
    fn processing_slots_limit_checks_independently_of_downloads() {
        let (mut album, _, _) = album_after_search();
        album.job.options.max_concurrent = 1;
        album.job.options.max_processing = 2;
        for track in album.tracks.iter_mut().take(3) {
            track.status = TrackStatus::Verifying;
            track.queued_check = Some(queued_check());
        }
        let (mut session, _events) = offline_session(album);
        session.run_verification_queue();
        let album = session.album.as_ref().unwrap();
        let started = album
            .tracks
            .iter()
            .filter(|track| track.verifying.is_some())
            .count();
        let waiting = album
            .tracks
            .iter()
            .filter(|track| track.queued_check.is_some())
            .count();
        assert_eq!((started, waiting), (2, 1));
        assert_eq!(processing_in_use(album), 2);
        // Nothing is downloading, so a download could still start.
        assert!(!album.tracks.iter().any(uses_download_slot));
    }

    #[test]
    fn album_gain_waits_for_every_track_then_uses_one_gain() {
        let (album, _) = album_waiting_for_gain(AudioQuality::Flac);
        let (mut session, _events) = offline_session(album);

        session.run_album_gain();
        let album = session.album.as_ref().unwrap();
        assert!(album.album_gain.is_none(), "track 3 is still downloading");
        assert!(album.tracks[0].pending.as_ref().unwrap().deadline.is_none());

        session.album.as_mut().unwrap().tracks[2].status = TrackStatus::Failed;
        session.run_album_gain();
        let album = session.album.as_ref().unwrap();
        let expected = normalize::album_gain(
            &[
                album.tracks[0].pending.as_ref().unwrap().loudness.unwrap(),
                album.tracks[1].pending.as_ref().unwrap().loudness.unwrap(),
            ],
            -16.0,
        )
        .unwrap()
        .gain_db;
        assert_eq!(album.album_gain, Some(expected));
        // max_processing = 1: one gain worker at a time.
        let started = album
            .tracks
            .iter()
            .filter(|track| {
                track
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.deadline.is_some())
            })
            .count();
        assert_eq!(started, 1);
    }

    #[test]
    fn replaygain_mode_writes_tags_for_the_target_and_leaves_the_audio_alone() {
        let (mut album, staging) = album_waiting_for_gain(AudioQuality::Flac);
        album.job.options.normalize_method = NormalizeMethod::ReplayGain;
        album.job.options.normalize_target_lufs = -14.0;
        album.job.options.max_processing = 8;
        // The third track comes from another master of the song.
        hold(&mut album.tracks[2], -10.0, &staging);
        album.tracks[2].pending.as_mut().unwrap().from_album_source = false;
        let (mut session, _events) = offline_session(album);

        session.run_album_gain();

        let album = session.album.as_ref().unwrap();
        // No gain is baked into the audio.
        assert_eq!(album.album_gain, Some(0.0));
        let (album_integrated, _) = album.replay_album.unwrap();
        let own = normalize::album_loudness(&[
            album.tracks[0].pending.as_ref().unwrap().loudness.unwrap(),
            album.tracks[1].pending.as_ref().unwrap().loudness.unwrap(),
        ])
        .unwrap();
        assert!((album_integrated - own.0).abs() < 1e-9);

        let first = album.tracks[0]
            .pending
            .as_ref()
            .unwrap()
            .replay_gain
            .unwrap();
        assert!((first.track_gain_db.unwrap() - (-14.0 - -20.0)).abs() < 1e-9);
        assert!((first.album_gain_db.unwrap() - (-14.0 - own.0)).abs() < 1e-9);
        // The off-source track's album gain follows its own level.
        let third = album.tracks[2]
            .pending
            .as_ref()
            .unwrap()
            .replay_gain
            .unwrap();
        assert!((third.track_gain_db.unwrap() - (-14.0 - -10.0)).abs() < 1e-9);
        assert_eq!(third.track_gain_db, third.album_gain_db);
        assert!(album.off_source_level.is_none());
        assert!(album.tracks[0].note.starts_with("ReplayGain +6.0 dB"));
    }

    #[test]
    fn opus_mode_converts_flac_even_when_only_tagging() {
        let (mut album, _) = album_waiting_for_gain(AudioQuality::Opus);
        album.job.options.normalize_method = NormalizeMethod::ReplayGain;
        let (session, _events) = offline_session(album);
        let path = session.album.as_ref().unwrap().tracks[0]
            .pending
            .as_ref()
            .unwrap()
            .path
            .clone();
        assert!(normalize::needs_conversion(&path, AudioQuality::Opus));
        assert!(normalize::source_compatible(&path, AudioQuality::Opus));
    }

    fn folder_cover_album(name: &str) -> (AlbumState, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "soulsucker-folder-cover-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut album, _, _) = album_after_search();
        album.job.album_dir = dir.clone();
        album.job.options.folder_cover = true;
        album.folder_cover = Some(Arc::new(b"jpeg-bytes".to_vec()));
        album.tracks[0].status = TrackStatus::Done;
        (album, dir)
    }

    #[test]
    fn the_folder_cover_is_saved_once_a_track_is_in_the_folder() {
        let (mut album, dir) = folder_cover_album("saved");
        album.tracks[0].status = TrackStatus::Downloading;
        let (mut session, _events) = offline_session(album);
        session.save_folder_cover();
        assert!(!dir.join("cover.jpg").exists(), "no finished track yet");

        session.album.as_mut().unwrap().tracks[0].status = TrackStatus::Done;
        session.save_folder_cover();
        assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), b"jpeg-bytes");
        assert!(session.album.as_ref().unwrap().folder_cover_done);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_folder_cover_is_kept_unless_forced() {
        let (album, dir) = folder_cover_album("kept");
        fs::write(dir.join("cover.jpg"), b"mine").unwrap();
        let (mut session, _events) = offline_session(album);
        session.save_folder_cover();
        assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), b"mine");

        let (mut album, _) = folder_cover_album("kept");
        album.job.force = true;
        let (mut session, _events) = offline_session(album);
        session.save_folder_cover();
        assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), b"jpeg-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn singles_and_a_disabled_option_get_no_folder_cover() {
        let (mut album, dir) = folder_cover_album("single");
        album.job.loose_files = true;
        let (mut session, _events) = offline_session(album);
        session.save_folder_cover();
        assert!(!dir.join("cover.jpg").exists());

        let (mut album, _) = folder_cover_album("single");
        album.job.options.folder_cover = false;
        let (mut session, _events) = offline_session(album);
        session.save_folder_cover();
        assert!(!dir.join("cover.jpg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_album_gain_result_is_ignored() {
        let (mut album, _) = album_waiting_for_gain(AudioQuality::Flac);
        album.tracks[0].pending.as_mut().unwrap().deadline = Some(Instant::now());
        let (mut session, _events) = offline_session(album);
        session.album_gain_done(1, 0, 99, Ok(PathBuf::from("/nonexistent")));
        let track = &session.album.as_ref().unwrap().tracks[0];
        assert!(track.pending.is_some());
        assert_eq!(track.status, TrackStatus::Verifying);
    }

    #[test]
    fn a_failed_conversion_during_album_gain_tries_another_source() {
        let (mut album, _) = album_waiting_for_gain(AudioQuality::Mp3);
        album.tracks[0].pending.as_mut().unwrap().deadline = Some(Instant::now());
        album.tracks[0].candidates = vec![source_file("pool", "misc", "One")];
        album.tracks[0].next_candidate = 0;
        let (mut session, _events) = offline_session(album);
        session.album_gain_done(1, 0, 1, Err("encoder broke".into()));
        let track = &session.album.as_ref().unwrap().tracks[0];
        assert!(track.pending.is_none());
        assert_eq!(track.status, TrackStatus::Queued);
    }

    #[test]
    fn session_loss_keeps_tracks_waiting_for_album_gain() {
        let (album, _) = album_waiting_for_gain(AudioQuality::Flac);
        let (mut session, _events) = offline_session(album);
        session.fail_active_tracks("Soulseek session lost", true);
        let album = session.album.as_ref().unwrap();
        assert!(album.tracks[0].pending.is_some());
        assert_eq!(album.tracks[0].status, TrackStatus::Verifying);
        assert_eq!(album.tracks[2].status, TrackStatus::Failed);
        assert!(album_ready_for_gain(album));

        session.fail_active_tracks("download cancelled", false);
        let album = session.album.as_ref().unwrap();
        assert!(album.tracks[0].pending.is_none());
        assert_eq!(album.tracks[0].status, TrackStatus::Failed);
    }

    #[test]
    fn session_loss_fails_tracks_even_after_the_client_is_gone() {
        let mut album = album_with_exhausted_track(1);
        album.tracks[0].status = TrackStatus::Downloading;
        album.tracks[0].current = None;
        let (events_tx, events_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut session = Session {
            config: Config::default(),
            events: events_tx,
            commands: cmd_rx,
            inner_tx: cmd_tx,
            client: None,
            extras: Vec::new(),
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: Some(album),
            last_session_check: Instant::now(),
        };
        session.fail_active_tracks("Soulseek session lost", true);
        assert_eq!(
            session.album.as_ref().unwrap().tracks[0].status,
            TrackStatus::Failed
        );
        let mut saw_failure = false;
        while let Ok(event) = events_rx.try_recv() {
            if matches!(
                event,
                SlskEvent::Track(TrackUpdate {
                    status: TrackStatus::Failed,
                    ..
                })
            ) {
                saw_failure = true;
            }
        }
        assert!(saw_failure);
    }

    #[test]
    fn promoting_over_an_existing_file_keeps_the_new_one() {
        let dir =
            std::env::temp_dir().join(format!("soulsucker-promote-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("new.flac");
        let target = dir.join("track.flac");
        fs::write(&source, b"new").unwrap();
        fs::write(&target, b"old").unwrap();
        promote_file(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_promotion_restores_the_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "soulsucker-promote-restore-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("missing.flac");
        let target = dir.join("track.flac");
        fs::write(&target, b"old").unwrap();
        assert!(promote_file(&source, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        let _ = fs::remove_dir_all(&dir);
    }

    fn stored_download(
        token: u32,
        username: &str,
        filename: &str,
        status: DownloadStatus,
    ) -> soulseek_rs::types::Download {
        soulseek_rs::types::Download {
            username: username.to_string(),
            filename: filename.to_string(),
            token,
            size: 1,
            download_directory: String::new(),
            status,
            sender: mpsc::channel().0,
            queue_position: None,
            metadata: soulseek_rs::types::DownloadMetadata::default(),
        }
    }

    #[test]
    fn download_lookup_ignores_changed_tokens_and_prefers_the_live_entry() {
        let downloads = vec![
            stored_download(1, "peer", "Album\\song.flac", DownloadStatus::Cancelled),
            stored_download(
                99,
                "peer",
                "Album\\song.flac",
                DownloadStatus::InProgress {
                    bytes_downloaded: 10,
                    total_bytes: 100,
                    speed_bytes_per_sec: 1.0,
                },
            ),
            stored_download(2, "peer", "Album\\other.flac", DownloadStatus::Queued),
        ];
        let found = find_live_download(&downloads, "peer", "Album\\song.flac").unwrap();
        assert_eq!(found.token, 99);
        assert!(!found.status.is_terminal());
        assert!(find_live_download(&downloads, "peer", "missing.flac").is_none());
    }

    #[test]
    fn existing_mp3_files_are_validated_by_duration_and_extension() {
        let dir = std::env::temp_dir().join(format!("soulsucker-mp3-valid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.mp3");
        fs::write(&path, include_bytes!("../tests/fixtures/silence.mp3")).unwrap();
        let meta = TrackMeta {
            index: 0,
            disc: 1,
            position: 1,
            title: "Song".into(),
            artist: "Artist".into(),
            expected_secs: Some(1.0),
            recording_mbid: "rec".into(),
            track_mbid: "track".into(),
            final_path: path.clone(),
        };
        assert!(existing_file_is_valid(&meta, 0.1).is_some());

        let wrong_extension = dir.join("song.flac");
        fs::copy(&path, &wrong_extension).unwrap();
        let mut wrong_meta = meta;
        wrong_meta.final_path = wrong_extension;
        assert!(existing_file_is_valid(&wrong_meta, 0.1).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_file_ignores_part_files() {
        let dir =
            std::env::temp_dir().join(format!("soulsucker-staged-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("song.flac.part"), b"partial").unwrap();
        assert!(staged_file(&dir).is_none());
        fs::write(dir.join("song.flac"), b"done").unwrap();
        assert_eq!(staged_file(&dir).unwrap().file_name().unwrap(), "song.flac");
        fs::remove_file(dir.join("song.flac")).unwrap();
        fs::write(dir.join("song.mp3"), b"done").unwrap();
        assert_eq!(staged_file(&dir).unwrap().file_name().unwrap(), "song.mp3");
        let _ = fs::remove_dir_all(&dir);
    }
}
