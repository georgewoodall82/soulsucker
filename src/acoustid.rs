//! Chromaprint + AcoustID verification.
//!
//! A duration check is always possible. Fingerprint verification is optional:
//! it needs `fpcalc` (chromaprint-tools) and a free AcoustID API key.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::audio;
use crate::util::form_encode;

const LOOKUP_URL: &str = "https://api.acoustid.org/v2/lookup";
const TRACK_LIST_URL: &str = "https://api.acoustid.org/v2/track/list_by_mbid";
const USER_AGENT: &str = concat!("soulsucker/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct VerifySpec {
    pub expected_secs: Option<f64>,
    pub tolerance_secs: f64,
    pub recording_mbid: String,
    pub release_group_mbid: String,
    pub api_key: String,
    pub min_score: f64,
}

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub accepted: bool,
    pub note: String,
}

impl VerifyResult {
    fn accept(note: impl Into<String>) -> Self {
        Self {
            accepted: true,
            note: note.into(),
        }
    }

    fn reject(note: impl Into<String>) -> Self {
        Self {
            accepted: false,
            note: note.into(),
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
    message: Option<String>,
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

/// Locate `fpcalc` on PATH.
pub fn fpcalc_path() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "fpcalc.exe"
    } else {
        "fpcalc"
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

enum FingerprintFailure {
    Timeout,
    Other(String),
}

fn compute_fingerprint(path: &Path) -> std::result::Result<(f64, String), FingerprintFailure> {
    let Some(binary) = fpcalc_path() else {
        return Err(FingerprintFailure::Other(
            "fpcalc was not found on PATH".to_string(),
        ));
    };
    run_fpcalc(&binary, path)
}

fn run_fpcalc(
    binary: &Path,
    path: &Path,
) -> std::result::Result<(f64, String), FingerprintFailure> {
    let mut child = Command::new(binary)
        .arg("-json")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            FingerprintFailure::Other(format!("could not run fpcalc on {}: {err}", path.display()))
        })?;
    let mut stdout_pipe = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FingerprintFailure::Other(
                "fpcalc produced no output stream".to_string(),
            ));
        }
    };
    let mut stderr_pipe = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FingerprintFailure::Other(
                "fpcalc produced no error stream".to_string(),
            ));
        }
    };
    // Drain the pipes on their own threads so a large fingerprint cannot
    // block the child while we wait for it to exit.
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(FingerprintFailure::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(FingerprintFailure::Other(format!(
                    "could not wait for fpcalc: {err}"
                )));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(FingerprintFailure::Other(format!(
            "fpcalc failed: {}",
            stderr.trim().chars().take(200).collect::<String>()
        )));
    }
    let data: serde_json::Value = serde_json::from_slice(&stdout)
        .map_err(|err| FingerprintFailure::Other(format!("fpcalc returned invalid JSON: {err}")))?;
    let duration = data
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let fingerprint = data
        .get("fingerprint")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| FingerprintFailure::Other("fpcalc returned no fingerprint".to_string()))?;
    if fingerprint.is_empty() {
        return Err(FingerprintFailure::Other(
            "fpcalc returned an empty fingerprint".to_string(),
        ));
    }
    Ok((duration, fingerprint.to_string()))
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

    if let Some(expected) = spec.expected_secs
        && spec.tolerance_secs > 0.0
    {
        let diff = (duration - expected).abs();
        if diff > spec.tolerance_secs {
            return VerifyResult::reject(format!(
                "length {} vs expected {} (±{:.0}s)",
                crate::util::fmt_duration(duration),
                crate::util::fmt_duration(expected),
                spec.tolerance_secs
            ));
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

    if spec.recording_mbid.is_empty() {
        return VerifyResult::accept(format!(
            "duration {duration_note} · no recording ID for fingerprinting"
        ));
    }
    if spec.api_key.trim().is_empty() {
        return VerifyResult::accept(format!("duration {duration_note} · fingerprint off"));
    }
    if fpcalc_path().is_none() {
        return VerifyResult::accept(format!("duration {duration_note} · fpcalc not installed"));
    }

    on_phase("computing Chromaprint fingerprint…");
    let (fp_duration, fingerprint) = match compute_fingerprint(path) {
        Ok(result) => result,
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

    if let Some(expected) = spec.expected_secs
        && fp_duration > 0.0
        && spec.tolerance_secs > 0.0
        && (fp_duration - expected).abs() > spec.tolerance_secs + 1.0
    {
        return VerifyResult::reject(format!(
            "decoded length {} vs expected {} (±{:.0}s)",
            crate::util::fmt_duration(fp_duration),
            crate::util::fmt_duration(expected),
            spec.tolerance_secs
        ));
    }

    on_phase("querying AcoustID…");
    let clusters = match acoustid_lookup(&fingerprint, fp_duration, &spec.api_key) {
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
}
