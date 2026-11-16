//! Chromaprint + AcoustID verification.
//!
//! Fingerprinting is built in: FFmpeg decodes the finished track and the
//! bundled Chromaprint port produces the fingerprint. The duration check is
//! always possible; AcoustID matching additionally needs a free API key.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chromaprint::{Algorithm, Fingerprinter};
use ffmpeg::format::Sample;
use ffmpeg::{codec, format, frame, media, software};
use ffmpeg_next as ffmpeg;
use serde::Deserialize;

use crate::audio;
use crate::util::form_encode;

/// `fpcalc` fingerprints the first two minutes by default; AcoustID expects
/// fingerprints of that length.
const FINGERPRINT_SECS: u64 = 120;
/// Abandon a pathological decode instead of blocking verification forever.
const FINGERPRINT_TIMEOUT: Duration = Duration::from_secs(60);

const LOOKUP_URL: &str = "https://api.acoustid.org/v2/lookup";
const TRACK_LIST_URL: &str = "https://api.acoustid.org/v2/track/list_by_mbid";
const USER_AGENT: &str = concat!("soulsucker/", env!("CARGO_PKG_VERSION"));

static FFMPEG_START: std::sync::Once = std::sync::Once::new();

fn init() {
    FFMPEG_START.call_once(|| {
        let _ = ffmpeg::init();
        // The TUI owns the terminal; FFmpeg must not write diagnostics to it.
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
    });
}

#[derive(Clone)]
pub struct VerifySpec {
    pub expected_secs: Option<f64>,
    pub tolerance_secs: f64,
    pub recording_mbid: String,
    pub release_group_mbid: String,
    pub api_key: String,
    pub min_score: f64,
    /// Reject lossy sources below this bitrate (used by MP3 320 mode).
    pub min_mp3_bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct VerifyResult {
    pub accepted: bool,
    pub note: String,
    /// The file's actual length, when it could be read.
    pub duration_secs: Option<f64>,
    /// Rejected only because the length differs from the expected one. The
    /// file may still be right for another pressing of the same album.
    pub length_mismatch: bool,
}

impl VerifyResult {
    fn accept(note: impl Into<String>) -> Self {
        Self {
            accepted: true,
            note: note.into(),
            ..Self::default()
        }
    }

    pub fn reject(note: impl Into<String>) -> Self {
        Self {
            accepted: false,
            note: note.into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FingerprintVerdict {
    Match(f64),
    MatchReleaseGroup(f64, String),
    Mismatch(f64, String),
    Unknown(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustidCluster {
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub recordings: Vec<AcoustidRecording>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustidRecording {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub releasegroups: Vec<AcoustidReleaseGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustidReleaseGroup {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Deserialize)]
struct LookupResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<ApiError>,
    #[serde(default)]
    results: Vec<AcoustidCluster>,
}

#[derive(Debug, Deserialize)]
struct TrackListResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<ApiError>,
    #[serde(default)]
    tracks: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeyError {
    Invalid,
    Unavailable(String),
}

/// Confirm that AcoustID accepts the client key.
///
/// The request deliberately uses an invalid fingerprint. AcoustID validates
/// the client key first, so a good key returns error code 3 ("invalid
/// fingerprint") while a bad key returns code 4 ("invalid API key"). This
/// avoids generating and fingerprinting audio just to check the key.
pub fn validate_api_key(api_key: &str) -> std::result::Result<(), ApiKeyError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(ApiKeyError::Invalid);
    }
    let body = format!(
        "format=json&client={}&meta=recordings&duration=3&fingerprint=AQAAAAAA",
        form_encode(api_key)
    );
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    throttle_requests();
    let mut response = agent
        .post(LOOKUP_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|err| ApiKeyError::Unavailable(err.to_string()))?;
    let data: LookupResponse = response.body_mut().read_json().map_err(|err| {
        ApiKeyError::Unavailable(format!("could not parse AcoustID response: {err}"))
    })?;
    classify_api_key_response(&data)
}

fn classify_api_key_response(data: &LookupResponse) -> std::result::Result<(), ApiKeyError> {
    if data.status == "ok" {
        return Ok(());
    }
    match data.error.as_ref().map(|err| err.code).unwrap_or_default() {
        // The key was accepted; only our deliberately fake fingerprint was
        // rejected.
        3 => Ok(()),
        4 => Err(ApiKeyError::Invalid),
        _ => Err(ApiKeyError::Unavailable(
            data.error
                .as_ref()
                .and_then(|err| err.message.clone())
                .unwrap_or_else(|| "unknown AcoustID error".to_string()),
        )),
    }
}

static LAST_REQUEST: Mutex<Option<Instant>> = Mutex::new(None);

fn throttle_requests() {
    let mut last = LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = *last {
        let elapsed = previous.elapsed();
        let spacing = Duration::from_millis(350);
        if elapsed < spacing {
            std::thread::sleep(spacing - elapsed);
        }
    }
    *last = Some(Instant::now());
}

fn http() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(25)))
        .user_agent(USER_AGENT)
        .build();
    ureq::Agent::new_with_config(config)
}

#[derive(Debug)]
enum FingerprintFailure {
    Timeout,
    Other(String),
}

/// Fingerprint the first two minutes on a worker thread so a decode that
/// wedges cannot block verification forever. A timed-out worker is abandoned
/// and the file is accepted on duration alone.
fn compute_fingerprint(path: &Path) -> std::result::Result<String, FingerprintFailure> {
    let path = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("chromaprint".to_string())
        .spawn(move || {
            let _ = tx.send(fingerprint_file(&path));
        })
        .map_err(|err| FingerprintFailure::Other(format!("could not start Chromaprint: {err}")))?;
    match rx.recv_timeout(FINGERPRINT_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(FingerprintFailure::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(FingerprintFailure::Other(
            "the Chromaprint worker stopped unexpectedly".to_string(),
        )),
    }
}

fn fingerprint_file(path: &Path) -> std::result::Result<String, FingerprintFailure> {
    init();
    let mut ictx = format::input(path).map_err(|err| {
        FingerprintFailure::Other(format!("could not open {}: {err}", path.display()))
    })?;
    let stream = ictx.streams().best(media::Type::Audio).ok_or_else(|| {
        FingerprintFailure::Other(format!("{} has no audio stream", path.display()))
    })?;
    let stream_index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())
        .and_then(|context| context.decoder().audio())
        .map_err(|err| {
            FingerprintFailure::Other(format!("could not open the audio decoder: {err}"))
        })?;
    decoder.set_parameters(stream.parameters()).map_err(|err| {
        FingerprintFailure::Other(format!("could not configure the audio decoder: {err}"))
    })?;

    let mut sink: Option<FingerprintSink> = None;
    let mut decoded = frame::Audio::empty();
    'audio: for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(|err| {
            FingerprintFailure::Other(format!("could not decode the audio: {err}"))
        })?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            // PCM streams often report an unspecified layout. Give the frame
            // the default layout for its channel count so the converter sees
            // the same layout it was configured with.
            if decoded.channel_layout().is_empty() {
                let channels = decoded.channels();
                if channels > 0 {
                    decoded.set_channel_layout(ffmpeg::ChannelLayout::default(i32::from(channels)));
                }
            }
            let sink = match sink.as_mut() {
                Some(sink) => sink,
                None => sink.insert(FingerprintSink::new(
                    decoded.rate(),
                    decoded.channel_layout(),
                    decoded.format(),
                )?),
            };
            if sink.push(&decoded)? {
                break 'audio;
            }
        }
    }
    let sink = sink.ok_or_else(|| {
        FingerprintFailure::Other("the audio stream decoded no samples".to_string())
    })?;
    sink.finish()
}

/// Streams decoded PCM into Chromaprint, stopping once the fingerprint has
/// all the audio it needs.
struct FingerprintSink {
    converter: software::resampling::Context,
    fingerprinter: Fingerprinter,
    converted: frame::Audio,
    channels: usize,
    limit: u64,
    fed: u64,
}

impl FingerprintSink {
    fn new(
        rate: u32,
        layout: ffmpeg::ChannelLayout,
        format: Sample,
    ) -> std::result::Result<Self, FingerprintFailure> {
        if rate == 0 || layout.channels() == 0 || format == Sample::None {
            return Err(FingerprintFailure::Other(
                "the audio stream has no usable sample format".to_string(),
            ));
        }
        let converter = software::resampling::Context::get(
            format,
            layout,
            rate,
            Sample::I16(ffmpeg::format::sample::Type::Packed),
            layout,
            rate,
        )
        .map_err(|err| {
            FingerprintFailure::Other(format!("could not open the audio converter: {err}"))
        })?;
        let mut fingerprinter = Fingerprinter::new(Algorithm::default());
        fingerprinter
            .start(rate, layout.channels() as u16)
            .map_err(|err| {
                FingerprintFailure::Other(format!("could not start Chromaprint: {err}"))
            })?;
        Ok(Self {
            converter,
            fingerprinter,
            converted: frame::Audio::empty(),
            channels: layout.channels() as usize,
            limit: FINGERPRINT_SECS * u64::from(rate),
            fed: 0,
        })
    }

    fn is_full(&self) -> bool {
        self.fed >= self.limit
    }

    fn push(&mut self, input: &frame::Audio) -> std::result::Result<bool, FingerprintFailure> {
        if self.is_full() {
            return Ok(true);
        }
        // Each decoder frame can have a different size (MP3 trims its first
        // frame), so let the converter allocate an output frame that fits it.
        self.converted = frame::Audio::empty();
        self.converter
            .run(input, &mut self.converted)
            .map_err(|err| {
                FingerprintFailure::Other(format!("could not convert the audio: {err}"))
            })?;
        self.feed()
    }

    fn feed(&mut self) -> std::result::Result<bool, FingerprintFailure> {
        let samples = self.converted.samples();
        if samples == 0 {
            return Ok(self.is_full());
        }
        let remaining = self.limit.saturating_sub(self.fed);
        let data = self.converted.data(0);
        let values = (samples as u64 * self.channels as u64)
            .min(data.len() as u64 / 2)
            .min(remaining * self.channels as u64) as usize;
        let values = values - values % self.channels;
        if values > 0 {
            // The converter writes packed i16 samples into plane 0.
            let pcm = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<i16>(), values) };
            self.fingerprinter.feed(pcm).map_err(|err| {
                FingerprintFailure::Other(format!("Chromaprint rejected the audio: {err}"))
            })?;
            self.fed += (values / self.channels) as u64;
        }
        Ok(self.is_full())
    }

    fn finish(mut self) -> std::result::Result<String, FingerprintFailure> {
        // Same-rate conversion has no resampling delay, but flush any buffered
        // tail so the last samples still reach the fingerprint.
        self.converter.flush(&mut self.converted).map_err(|err| {
            FingerprintFailure::Other(format!("could not flush the audio converter: {err}"))
        })?;
        self.feed()?;
        self.fingerprinter.finish().map_err(|err| {
            FingerprintFailure::Other(format!("could not finish Chromaprint: {err}"))
        })?;
        let raw = self.fingerprinter.fingerprint();
        if raw.is_empty() {
            return Err(FingerprintFailure::Other(
                "Chromaprint produced an empty fingerprint".to_string(),
            ));
        }
        Ok(self.fingerprinter.encode())
    }
}

fn acoustid_lookup(
    fingerprint: &str,
    duration: f64,
    api_key: &str,
) -> Result<Vec<AcoustidCluster>> {
    let body = format!(
        "format=json&client={}&meta=recordings+releasegroups&duration={}&fingerprint={}",
        form_encode(api_key),
        duration.round().max(0.0) as u64,
        form_encode(fingerprint)
    );
    throttle_requests();
    let mut response = http()
        .post(LOOKUP_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .context("AcoustID lookup request failed")?;
    let data: LookupResponse = response
        .body_mut()
        .read_json()
        .context("could not parse the AcoustID response")?;
    if data.status != "ok" {
        let message = data
            .error
            .and_then(|err| err.message)
            .unwrap_or_else(|| "unknown AcoustID error".to_string());
        return Err(anyhow!("AcoustID error: {message}"));
    }
    Ok(data.results)
}

fn acoustid_track_count(mbid: &str, api_key: &str) -> Result<usize> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    throttle_requests();
    let mut response = agent
        .get(TRACK_LIST_URL)
        .query("format", "json")
        .query("client", api_key)
        .query("mbid", mbid)
        .call()
        .context("AcoustID track lookup failed")?;
    let data: TrackListResponse = response
        .body_mut()
        .read_json()
        .context("could not parse the AcoustID track response")?;
    if data.status != "ok" {
        let message = data
            .error
            .and_then(|err| err.message)
            .unwrap_or_else(|| "unknown AcoustID error".to_string());
        return Err(anyhow!("AcoustID error: {message}"));
    }
    Ok(data.tracks.len())
}

fn mbid_known(mbid: &str, api_key: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = mbid.to_ascii_lowercase();
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied()
    {
        return known;
    }
    // On a transient API error, assume the recording is known. That keeps a
    // mismatch verdict conservative instead of silently weakening the check,
    // but do not cache an error as if it were a real answer.
    match acoustid_track_count(mbid, api_key) {
        Ok(count) => {
            let known = count > 0;
            cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, known);
            known
        }
        Err(_) => true,
    }
}

fn recording_label(recording: &AcoustidRecording) -> String {
    if recording.title.is_empty() {
        format!("[{}]", &recording.id[..recording.id.len().min(8)])
    } else {
        format!(
            "“{}” [{}]",
            recording.title,
            &recording.id[..recording.id.len().min(8)]
        )
    }
}

/// Decide what a set of AcoustID clusters means for an expected recording.
pub fn evaluate_lookup(
    clusters: &[AcoustidCluster],
    expected_mbid: &str,
    expected_release_group: &str,
    min_score: f64,
    expected_known: bool,
) -> FingerprintVerdict {
    let identified: Vec<&AcoustidCluster> = clusters
        .iter()
        .filter(|cluster| !cluster.recordings.is_empty())
        .collect();
    if identified.is_empty() {
        return FingerprintVerdict::Unknown(
            "fingerprint unknown to AcoustID — kept on duration".to_string(),
        );
    }
    let top = identified
        .iter()
        .map(|cluster| cluster.score)
        .fold(f64::NEG_INFINITY, f64::max);
    if top < min_score {
        return FingerprintVerdict::Unknown(format!(
            "weak AcoustID evidence (best {top:.2}) — kept on duration"
        ));
    }

    let expected = expected_mbid.to_ascii_lowercase();
    let tier: Vec<&AcoustidCluster> = identified
        .iter()
        .copied()
        .filter(|cluster| cluster.score >= top - 1e-9)
        .collect();
    if tier
        .iter()
        .flat_map(|cluster| cluster.recordings.iter())
        .any(|recording| recording.id.eq_ignore_ascii_case(&expected))
    {
        return FingerprintVerdict::Match(top);
    }

    // Reissues and compilations sometimes use a different recording ID for
    // the same performance (an album edit, a differently indexed pressing, or
    // a remaster). If the fingerprint points at another recording from the
    // selected album, the audio is still the track we asked for.
    let release_group = expected_release_group.trim();
    if !release_group.is_empty()
        && let Some(recording) = tier
            .iter()
            .flat_map(|cluster| cluster.recordings.iter())
            .find(|recording| {
                recording
                    .releasegroups
                    .iter()
                    .any(|group| group.id.eq_ignore_ascii_case(release_group))
            })
    {
        return FingerprintVerdict::MatchReleaseGroup(top, recording_label(recording));
    }

    if !expected_known {
        return FingerprintVerdict::Unknown(
            "expected recording unknown to AcoustID — kept on duration".to_string(),
        );
    }

    let label = tier
        .first()
        .and_then(|cluster| cluster.recordings.first())
        .map(recording_label)
        .unwrap_or_else(|| "another recording".to_string());
    FingerprintVerdict::Mismatch(top, label)
}

/// Check a finished file. The duration check runs even without fingerprinting.
pub fn verify_file(path: &Path, spec: &VerifySpec, on_phase: &dyn Fn(&str)) -> VerifyResult {
    let duration = match audio::duration_secs(path) {
        Ok(duration) => duration,
        Err(err) => {
            return VerifyResult::reject(format!("not a readable audio file ({err})"));
        }
    };
    let mut result = verify_with_duration(path, spec, on_phase, duration);
    result.duration_secs = Some(duration);
    result
}

fn verify_with_duration(
    path: &Path,
    spec: &VerifySpec,
    on_phase: &dyn Fn(&str),
    duration: f64,
) -> VerifyResult {
    if let Some(expected) = spec.expected_secs
        && spec.tolerance_secs > 0.0
    {
        let diff = (duration - expected).abs();
        if diff > spec.tolerance_secs {
            let mut result = VerifyResult::reject(format!(
                "length {} vs expected {} (±{:.0}s)",
                crate::util::fmt_duration(duration),
                crate::util::fmt_duration(expected),
                spec.tolerance_secs
            ));
            result.length_mismatch = true;
            return result;
        }
    }

    let duration_note = match spec.expected_secs {
        Some(expected) => format!(
            "{} (expected {})",
            crate::util::fmt_duration(duration),
            crate::util::fmt_duration(expected)
        ),
        None => crate::util::fmt_duration(duration),
    };

    if let Some(min_kbps) = spec.min_mp3_bitrate_kbps {
        match audio::bitrate_kbps(path) {
            Ok(Some(kbps)) if kbps >= min_kbps => {}
            Ok(Some(kbps)) => {
                return VerifyResult::reject(format!(
                    "MP3 bitrate {kbps} kbps — expected 320 kbps"
                ));
            }
            Ok(None) => {
                return VerifyResult::reject("could not read the MP3 bitrate".to_string());
            }
            Err(err) => {
                return VerifyResult::reject(format!("could not read the MP3 bitrate ({err})"));
            }
        }
    }

    if spec.recording_mbid.is_empty() {
        return VerifyResult::accept(format!(
            "duration {duration_note} · no recording ID for fingerprinting"
        ));
    }
    if spec.api_key.trim().is_empty() {
        return VerifyResult::accept(format!("duration {duration_note} · fingerprint off"));
    }

    on_phase("computing Chromaprint fingerprint…");
    let fingerprint = match compute_fingerprint(path) {
        Ok(fingerprint) => fingerprint,
        Err(FingerprintFailure::Timeout) => {
            return VerifyResult::accept(format!(
                "duration {duration_note} · fingerprint timed out — kept on duration"
            ));
        }
        Err(FingerprintFailure::Other(err)) => {
            return VerifyResult::reject(format!(
                "could not decode the file for fingerprinting ({err})"
            ));
        }
    };

    on_phase("querying AcoustID…");
    let clusters = match acoustid_lookup(&fingerprint, duration, &spec.api_key) {
        Ok(clusters) => clusters,
        Err(err) => {
            return VerifyResult::accept(format!(
                "duration {duration_note} · AcoustID unavailable ({err})"
            ));
        }
    };

    on_phase("resolving recording…");
    let mut expected_known = true;
    // Only ask whether AcoustID knows the expected recording if the top
    // cluster doesn't already contain it.
    let contains_expected = clusters.iter().any(|cluster| {
        cluster
            .recordings
            .iter()
            .any(|recording| recording.id.eq_ignore_ascii_case(&spec.recording_mbid))
    });
    if !contains_expected {
        expected_known = mbid_known(&spec.recording_mbid, &spec.api_key);
    }

    match evaluate_lookup(
        &clusters,
        &spec.recording_mbid,
        &spec.release_group_mbid,
        spec.min_score,
        expected_known,
    ) {
        FingerprintVerdict::Match(score) => VerifyResult::accept(format!(
            "duration {duration_note} · AcoustID match {score:.2}"
        )),
        FingerprintVerdict::MatchReleaseGroup(score, label) => VerifyResult::accept(format!(
            "duration {duration_note} · AcoustID match {score:.2} (alternate pressing {label})"
        )),
        FingerprintVerdict::Unknown(detail) => {
            VerifyResult::accept(format!("duration {duration_note} · {detail}"))
        }
        FingerprintVerdict::Mismatch(score, label) => VerifyResult::reject(format!(
            "AcoustID top hit {label} {score:.2} is a different recording"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(score: f64, ids: &[&str]) -> AcoustidCluster {
        AcoustidCluster {
            score,
            recordings: ids
                .iter()
                .map(|id| AcoustidRecording {
                    id: (*id).to_string(),
                    title: format!("title {id}"),
                    releasegroups: Vec::new(),
                })
                .collect(),
        }
    }

    fn cluster_in_release_group(score: f64, id: &str, release_group: &str) -> AcoustidCluster {
        AcoustidCluster {
            score,
            recordings: vec![AcoustidRecording {
                id: id.to_string(),
                title: format!("title {id}"),
                releasegroups: vec![AcoustidReleaseGroup {
                    id: release_group.to_string(),
                }],
            }],
        }
    }

    fn api_error(code: i32, message: &str) -> LookupResponse {
        LookupResponse {
            status: "error".to_string(),
            error: Some(ApiError {
                code,
                message: Some(message.to_string()),
            }),
            results: Vec::new(),
        }
    }

    #[test]
    fn api_key_validation_classifies_acoustid_errors() {
        assert_eq!(
            classify_api_key_response(&api_error(3, "invalid fingerprint")),
            Ok(())
        );
        assert_eq!(
            classify_api_key_response(&api_error(4, "invalid API key")),
            Err(ApiKeyError::Invalid)
        );
        assert!(matches!(
            classify_api_key_response(&api_error(5, "temporarily unavailable")),
            Err(ApiKeyError::Unavailable(_))
        ));
        assert_eq!(
            classify_api_key_response(&LookupResponse {
                status: "ok".to_string(),
                error: None,
                results: Vec::new(),
            }),
            Ok(())
        );
    }

    #[test]
    fn expected_recording_in_the_top_cluster_is_a_match() {
        let clusters = vec![cluster(0.94, &["expected", "other"]), cluster(0.4, &["x"])];
        assert_eq!(
            evaluate_lookup(&clusters, "expected", "", 0.8, true),
            FingerprintVerdict::Match(0.94)
        );
    }

    #[test]
    fn a_lower_ranked_appearance_does_not_count() {
        let clusters = vec![cluster(0.94, &["other"]), cluster(0.5, &["expected"])];
        assert!(matches!(
            evaluate_lookup(&clusters, "expected", "", 0.8, true),
            FingerprintVerdict::Mismatch(_, _)
        ));
    }

    #[test]
    fn an_alternate_recording_in_the_same_release_group_is_a_match() {
        let clusters = vec![cluster_in_release_group(0.95, "standard", "the-wall")];
        assert!(matches!(
            evaluate_lookup(&clusters, "1985-edit", "the-wall", 0.8, true),
            FingerprintVerdict::MatchReleaseGroup(0.95, _)
        ));
    }

    #[test]
    fn an_alternate_recording_in_another_release_group_is_a_mismatch() {
        let clusters = vec![cluster_in_release_group(0.95, "live", "live-album")];
        assert!(matches!(
            evaluate_lookup(&clusters, "studio", "the-wall", 0.8, true),
            FingerprintVerdict::Mismatch(_, _)
        ));
    }

    #[test]
    fn a_release_group_match_does_not_require_the_expected_recording_to_be_known() {
        let clusters = vec![cluster_in_release_group(0.95, "standard", "the-wall")];
        assert!(matches!(
            evaluate_lookup(&clusters, "1985-edit", "the-wall", 0.8, false),
            FingerprintVerdict::MatchReleaseGroup(_, _)
        ));
    }

    #[test]
    fn an_unknown_expected_recording_is_kept_on_duration() {
        let clusters = vec![cluster(0.94, &["other"])];
        assert!(matches!(
            evaluate_lookup(&clusters, "expected", "", 0.8, false),
            FingerprintVerdict::Unknown(_)
        ));
    }

    #[test]
    fn weak_evidence_is_unknown() {
        let clusters = vec![cluster(0.5, &["expected"])];
        assert!(matches!(
            evaluate_lookup(&clusters, "expected", "", 0.8, true),
            FingerprintVerdict::Unknown(_)
        ));
    }

    #[test]
    fn a_non_ascii_recording_id_does_not_panic_the_label() {
        let clusters = vec![cluster(0.94, &["ééééééééé"])];
        assert!(matches!(
            evaluate_lookup(&clusters, "expected", "", 0.8, true),
            FingerprintVerdict::Mismatch(_, _)
        ));
    }

    #[test]
    fn an_empty_database_answer_is_unknown() {
        assert!(matches!(
            evaluate_lookup(&[], "expected", "", 0.8, true),
            FingerprintVerdict::Unknown(_)
        ));
    }

    #[test]
    fn lookup_response_parses_release_groups() {
        let data: LookupResponse = serde_json::from_str(
            r#"{
                "status": "ok",
                "results": [{
                    "score": 0.95,
                    "recordings": [{
                        "id": "recording",
                        "title": "Song",
                        "releasegroups": [{"id": "group", "title": "Album"}]
                    }]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(data.results[0].recordings[0].releasegroups[0].id, "group");
    }

    fn write_test_wav(path: &Path, seconds: u32) {
        let rate = 44_100u32;
        let channels = 2u16;
        let frames = rate * seconds;
        let data_len = frames * u32::from(channels) * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let t = frame as f32 / rate as f32;
            let sample =
                ((t * 440.0 * std::f32::consts::TAU).sin() * 0.25 * i16::MAX as f32) as i16;
            for _ in 0..channels {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_tone_is_fingerprinted_in_process() {
        let dir =
            std::env::temp_dir().join(format!("soulsucker-chromaprint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        write_test_wav(&path, 3);

        let fingerprint = compute_fingerprint(&path).unwrap();
        assert!(!fingerprint.is_empty());
        // Decoding and fingerprinting the same PCM must be deterministic.
        assert_eq!(compute_fingerprint(&path).unwrap(), fingerprint);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
