//! Soulseek session and the album download engine.
//!
//! One background thread owns the library client. Tracks are downloaded into
//! per-attempt staging folders (the library flattens files by basename), then
//! verified, tagged and moved to their final `01 - Title.flac` names.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use soulseek_rs::{Client, ClientSettings, DownloadStatus, PeerAddress, SoulseekRs};

use crate::acoustid::{self, VerifyResult, VerifySpec};
use crate::audio::{self, TrackTags};
use crate::config::Config;
use crate::matcher::{self, Candidate, CandidateOptions, RemoteFile, SearchHit, TrackQuery};
use crate::musicbrainz;
use crate::util;

const STAGING_DIR: &str = ".soulsucker-staging";
const LAST_RESORT_BACKOFF: Duration = Duration::from_secs(5);
const MAX_AUTO_RECONNECTS: u32 = 3;
const VERIFY_DEADLINE: Duration = Duration::from_secs(150);

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
    pub search_timeout: Duration,
    pub queue_timeout: Duration,
    pub stall_timeout: Duration,
    pub length_tolerance: f64,
    pub api_key: String,
    pub fingerprint_min_score: f64,
    pub last_resort_rounds: u32,
    pub last_resort_stall: Duration,
    pub candidates_per_track: usize,
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
    pub tracks: Vec<TrackMeta>,
    pub options: DownloadOptions,
    pub force: bool,
}

impl std::fmt::Debug for DownloadOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadOptions")
            .field("max_concurrent", &self.max_concurrent)
            .field("search_timeout", &self.search_timeout)
            .field("queue_timeout", &self.queue_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("length_tolerance", &self.length_tolerance)
            .field("api_key", &"<redacted>")
            .field("fingerprint_min_score", &self.fingerprint_min_score)
            .field("last_resort_rounds", &self.last_resort_rounds)
            .field("last_resort_stall", &self.last_resort_stall)
            .field("candidates_per_track", &self.candidates_per_track)
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
    },
    CoverReady {
        job_id: u64,
        result: Result<Option<musicbrainz::CoverArt>, String>,
    },
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

fn run_session(
    config: Config,
    events: Sender<SlskEvent>,
    commands: Receiver<SessionCommand>,
    inner_tx: Sender<SessionCommand>,
) {
    Session::new(config, events, commands, inner_tx).run();
}

struct Session {
    config: Config,
    events: Sender<SlskEvent>,
    commands: Receiver<SessionCommand>,
    inner_tx: Sender<SessionCommand>,
    client: Option<Arc<Client>>,
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
                    self.fail_active_tracks("reconnecting to Soulseek");
                    self.check_completion();
                }
                if let Some(mut client) = self.client.take()
                    && let Some(client) = Arc::get_mut(&mut client)
                {
                    client.disconnect();
                }
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
            } => self.verified(job_id, index, attempt_no, result),
            SessionCommand::CoverReady { job_id, result } => {
                self.cover_ready(job_id, result);
            }
        }
        true
    }

    fn tick(&mut self) {
        self.check_session();
        if self.client.is_none() {
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
            self.fail_active_tracks("Soulseek session lost");
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
        };

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
            return;
        }

        self.album = Some(album);
        self.fetch_cover(job_id, cover_artist, cover_album, release_mbid);
        self.begin_search();
    }

    fn fetch_cover(&self, job_id: u64, artist: String, album: String, release_mbid: String) {
        let tx = self.inner_tx.clone();
        let spawned = thread::Builder::new()
            .name("cover-art".to_string())
            .spawn(move || {
                let result = musicbrainz::fetch_cover(&artist, &album, &release_mbid)
                    .map_err(|err| err.to_string());
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
            self.fail_active_tracks("Soulseek is not connected");
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
            self.fail_active_tracks("could not start the Soulseek search worker");
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
                };
                let candidates = matcher::build_candidates(&queries, &hits, &options);
                let mut per_track = Vec::with_capacity(candidates.len());
                let mut total_candidates = 0usize;
                for (index, track) in album.tracks.iter_mut().enumerate() {
                    if track.status == TrackStatus::Done {
                        per_track.push(0);
                        continue;
                    }
                    let sources = candidates.get(index).cloned().unwrap_or_default();
                    total_candidates += sources.len();
                    per_track.push(sources.len());
                    track.attempts_total = sources.len();
                    if sources.is_empty() {
                        track.status = TrackStatus::Failed;
                        track.speed = 0.0;
                        track.note = "no matching FLAC source found".to_string();
                        let _ = events.send(SlskEvent::Track(track.update(job_id)));
                    } else {
                        track.candidates = sources;
                        track.status = TrackStatus::Queued;
                        track.next_candidate = 0;
                        track.rounds_left = album.job.options.last_resort_rounds;
                        track.note = "waiting for a download slot".to_string();
                        let _ = events.send(SlskEvent::Track(track.update(job_id)));
                    }
                }
                let matched = per_track.iter().filter(|count| **count > 0).count();
                let _ = events.send(SlskEvent::SourcesReady {
                    job_id,
                    per_track,
                    peers: hits.len(),
                });
                let _ = events.send(SlskEvent::Log(
                    LogLevel::Info,
                    format!(
                        "Found {total_candidates} matching FLAC source(s) from {} peer(s); {matched}/{} tracks have a source.",
                        hits.len(),
                        album.tracks.len()
                    ),
                ));
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
        let Some(client) = self.client.clone() else {
            return;
        };
        let events = self.events.clone();
        let downloads = client.get_all_downloads();

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

            for (index, track) in album.tracks.iter_mut().enumerate() {
                let Some(active) = track.current.as_mut() else {
                    continue;
                };
                // The library replaces the download token when the peer sends
                // its TransferRequest, so tokens are not stable. The remote
                // username+path is.
                let Some(download) = find_live_download(&downloads, &active.username, &active.path)
                else {
                    rejected.push((index, "transfer vanished".to_string()));
                    continue;
                };
                let now = Instant::now();
                match &download.status {
                    DownloadStatus::Queued => {
                        track.note = match download.queue_position {
                            Some(0) | None => "queued at peer".to_string(),
                            Some(place) => format!("in peer queue · {place} ahead"),
                        };
                        track.speed = 0.0;
                        if now.duration_since(active.started) >= active.queue_patience {
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
                        track.verifying.as_ref().is_some_and(|verifying| {
                            now.duration_since(verifying.started) >= VERIFY_DEADLINE
                        })
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
        let max = album.job.options.max_concurrent.max(1);
        let mut active = album
            .tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.status,
                    TrackStatus::Downloading | TrackStatus::Verifying
                )
            })
            .count();
        let job_id = album.job.job_id;
        let mut to_start = Vec::new();
        for index in 0..album.tracks.len() {
            if active >= max {
                break;
            }
            let track = &album.tracks[index];
            let retrying_due =
                track.status == TrackStatus::Retrying && track.retry_at.is_some_and(|at| now >= at);
            if retrying_due || track.status == TrackStatus::Queued {
                to_start.push(index);
                active += 1;
            }
        }

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
        let Some(client) = self.client.clone() else {
            return false;
        };
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
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
        let Some(client) = self.client.clone() else {
            return;
        };
        let events = self.events.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        let (position, title) = {
            let Some(track) = album.tracks.get_mut(index) else {
                return;
            };
            if let Some(active) = track.current.take() {
                let cancelled = client.cancel_download(&active.username, &active.path);
                if !cancelled {
                    let _ = client.remove_download(&active.username, &active.path);
                }
                let _ = fs::remove_dir_all(&active.staging);
            }
            if let Some(verifying) = track.verifying.take() {
                let _ = fs::remove_dir_all(&verifying.staging);
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
        let Some(client) = self.client.clone() else {
            return;
        };
        let events = self.events.clone();
        let tx = self.inner_tx.clone();
        let Some(album) = self.album.as_mut() else {
            return;
        };
        let job_id = album.job.job_id;
        let Some(active) = album.tracks[index].current.take() else {
            return;
        };
        let _ = client.remove_download(&active.username, &active.path);
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

        let (update, spec, position, title, source) = {
            let Some(track) = album.tracks.get_mut(index) else {
                return;
            };
            let attempt_no = active.attempt_no;
            track.status = TrackStatus::Verifying;
            track.note = "checking duration…".to_string();
            track.speed = 0.0;
            track.verifying = Some(VerifyingAttempt {
                attempt_no,
                staging: active.staging.clone(),
                path: staged.clone(),
                started: Instant::now(),
            });
            let update = track.update(job_id);
            let spec = VerifySpec {
                expected_secs: track.meta.expected_secs,
                tolerance_secs: album.job.options.length_tolerance,
                recording_mbid: track.meta.recording_mbid.clone(),
                release_group_mbid: album.job.release_group_mbid.clone(),
                api_key: album.job.options.api_key.clone(),
                min_score: album.job.options.fingerprint_min_score,
            };
            (
                update,
                spec,
                track.meta.position,
                track.meta.title.clone(),
                track.source.clone(),
            )
        };
        let _ = events.send(SlskEvent::Track(update.clone()));
        let _ = events.send(SlskEvent::Log(
            LogLevel::Info,
            format!("Verifying {position:02} {title} ← {source}"),
        ));

        let attempt_no = active.attempt_no;
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
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    acoustid::verify_file(&path, &spec, &on_phase)
                }))
                .unwrap_or_else(|_| VerifyResult {
                    accepted: false,
                    note: "verification worker panicked".to_string(),
                });
                let _ = tx.send(SessionCommand::Verified {
                    job_id,
                    index,
                    attempt_no,
                    result,
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

    fn verified(&mut self, job_id: u64, index: usize, attempt_no: u32, result: VerifyResult) {
        let events = self.events.clone();
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

        if !result.accepted {
            let _ = fs::remove_dir_all(&verifying.staging);
            let reason = result.note;
            self.album = Some(album);
            self.fail_attempt(index, &reason);
            return;
        }

        let final_path = track.meta.final_path.clone();
        let Some(parent) = final_path.parent() else {
            let _ = fs::remove_dir_all(&verifying.staging);
            self.album = Some(album);
            self.fail_attempt(index, "invalid destination path");
            return;
        };
        if let Err(err) = fs::create_dir_all(parent) {
            let _ = fs::remove_dir_all(&verifying.staging);
            self.album = Some(album);
            self.fail_attempt(index, &format!("could not create album folder: {err}"));
            return;
        }

        if final_path.exists()
            && !album.job.force
            && let Some(size) =
                existing_file_is_valid(&track.meta, album.job.options.length_tolerance)
        {
            let _ = fs::remove_dir_all(&verifying.staging);
            track.status = TrackStatus::Done;
            track.bytes_done = size;
            track.total = size;
            track.speed = 0.0;
            track.note = "existing file kept".to_string();
            let _ = events.send(SlskEvent::Track(track.update(job_id)));
            self.album = Some(album);
            self.check_completion();
            return;
        }

        if let Err(err) = promote_file(&verifying.path, &final_path) {
            let _ = fs::remove_dir_all(&verifying.staging);
            self.album = Some(album);
            self.fail_attempt(index, &format!("could not save file: {err}"));
            return;
        }
        track.downloaded = true;

        let date = (!album.job.date.trim().is_empty()).then_some(album.job.date.as_str());
        let disc_track_total = album
            .job
            .tracks
            .iter()
            .filter(|meta| meta.disc == track.meta.disc)
            .count() as u32;
        let tags = TrackTags {
            title: &track.meta.title,
            artist: &track.meta.artist,
            album_artist: &album.job.artist,
            album: &album.job.album,
            date,
            track: track.meta.position,
            track_total: disc_track_total,
            disc: track.meta.disc,
            disc_total: album.job.disc_total(),
            release_mbid: &album.job.release_mbid,
            release_group_mbid: &album.job.release_group_mbid,
            recording_mbid: &track.meta.recording_mbid,
            track_mbid: &track.meta.track_mbid,
            cover: album.cover.as_ref().map(|cover| cover.as_slice()),
        };
        if let Err(err) = audio::write_tags(&final_path, &tags) {
            let _ = events.send(SlskEvent::Log(
                LogLevel::Warn,
                format!(
                    "{} — {}: tags not written ({err})",
                    track.meta.position, track.meta.title
                ),
            ));
        }

        let size = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        let note = result.note;
        track.status = TrackStatus::Done;
        track.bytes_done = size;
        track.total = size;
        track.speed = 0.0;
        track.note = note.clone();
        let _ = fs::remove_dir_all(&verifying.staging);
        let source = track.source.clone();
        let position = track.meta.position;
        let title = track.meta.title.clone();
        let _ = events.send(SlskEvent::Track(track.update(job_id)));
        let _ = events.send(SlskEvent::Log(
            LogLevel::Success,
            format!("✔ {position:02} {title} ← {source} · {note}"),
        ));
        self.album = Some(album);
        self.check_completion();
    }

    fn cover_ready(&mut self, job_id: u64, result: Result<Option<musicbrainz::CoverArt>, String>) {
        let mut messages = Vec::new();
        {
            let Some(album) = self.album.as_mut() else {
                return;
            };
            if album.job.job_id != job_id {
                return;
            }
            match result {
                Ok(Some(cover)) => {
                    let source = cover.source.label();
                    let cover = Arc::new(cover.data);
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
    }

    fn cancel_track(&mut self, job_id: u64, index: usize) {
        let Some(client) = self.client.clone() else {
            return;
        };
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
                let cancelled = client.cancel_download(&active.username, &active.path);
                if !cancelled {
                    let _ = client.remove_download(&active.username, &active.path);
                }
                let _ = fs::remove_dir_all(&active.staging);
            }
            if let Some(verifying) = track.verifying.take() {
                let _ = fs::remove_dir_all(&verifying.staging);
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
            if track.status != TrackStatus::Failed || track.candidates.is_empty() {
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

    fn fail_active_tracks(&mut self, reason: &str) {
        let client = self.client.clone();
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
            if let Some(active) = track.current.take() {
                if let Some(client) = &client {
                    let cancelled = client.cancel_download(&active.username, &active.path);
                    if !cancelled {
                        let _ = client.remove_download(&active.username, &active.path);
                    }
                }
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
        }
        self.fail_active_tracks("download cancelled");
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
        }
        self.fail_active_tracks("app closed");
        if let Some(album) = self.album.take() {
            let _ = fs::remove_dir_all(album.job.album_dir.join(STAGING_DIR));
        }
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
    let mut settings = ClientSettings::new(config.username.clone(), config.password.clone());
    settings.server_address = PeerAddress::new("server.slsknet.org".to_string(), 2416);
    settings.enable_listen = true;
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
}

struct DownloadActive {
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
    path: PathBuf,
    started: Instant,
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
        "no matching FLAC source found".to_string()
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
        if util::extension_of(&name.to_string_lossy()).eq_ignore_ascii_case("flac") {
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
        .unwrap_or("track.flac");
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

/// Build a final track filename. Multi-disc releases get a `1-01` prefix.
pub fn track_filename(
    disc: u32,
    position: u32,
    title: &str,
    disc_count: u32,
    track_count: u32,
) -> String {
    let width = track_count.to_string().len().max(2);
    let title = util::sanitize_filename(title);
    if disc_count > 1 {
        format!("{disc}-{position:0width$} - {title}.flac")
    } else {
        format!("{position:0width$} - {title}.flac")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_zero_padded_and_disc_aware() {
        assert_eq!(
            track_filename(1, 1, "Fight Fire With Fire", 1, 8),
            "01 - Fight Fire With Fire.flac"
        );
        assert_eq!(
            track_filename(2, 10, "Song/Two", 2, 10),
            "2-10 - Song_Two.flac"
        );
    }

    #[test]
    fn wide_track_numbers_use_more_padding() {
        assert_eq!(track_filename(1, 3, "Track", 1, 120), "003 - Track.flac");
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
                    search_timeout: Duration::from_secs(1),
                    queue_timeout: Duration::from_secs(1),
                    stall_timeout: Duration::from_secs(1),
                    length_tolerance: 5.0,
                    api_key: String::new(),
                    fingerprint_min_score: 0.8,
                    last_resort_rounds: 1,
                    last_resort_stall: Duration::from_secs(10),
                    candidates_per_track: 12,
                },
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
        }
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
            path: PathBuf::from("/tmp/soulsucker-nonexistent.flac"),
            started: Instant::now(),
        });
        let (events_tx, _events_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut session = Session {
            config: Config::default(),
            events: events_tx,
            commands: cmd_rx,
            inner_tx: cmd_tx,
            client: None,
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
            },
        );
        let verifying = session.album.as_ref().unwrap().tracks[0]
            .verifying
            .as_ref()
            .expect("the newer attempt must still be verifying");
        assert_eq!(verifying.attempt_no, 2);
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
            connector: None,
            connecting: false,
            reconnect_at: None,
            connect_failures: 0,
            pending_download: None,
            album: Some(album),
            last_session_check: Instant::now(),
        };
        session.fail_active_tracks("Soulseek session lost");
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
    fn staged_file_ignores_part_files() {
        let dir =
            std::env::temp_dir().join(format!("soulsucker-staged-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("song.flac.part"), b"partial").unwrap();
        assert!(staged_file(&dir).is_none());
        fs::write(dir.join("song.flac"), b"done").unwrap();
        assert_eq!(staged_file(&dir).unwrap().file_name().unwrap(), "song.flac");
        let _ = fs::remove_dir_all(&dir);
    }
}
