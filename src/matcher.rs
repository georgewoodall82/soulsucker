//! Local matching of MusicBrainz track titles against Soulseek filenames.
//!
//! The source files often carry artist/album prefixes, release tags, track
//! numbers and inconsistent punctuation. Matching is deliberately fuzzy, but
//! still conservative enough that a wrong track is worse than no source.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use strsim::normalized_levenshtein;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::config::AudioQuality;
use crate::util::{extension_of, remote_basename, stem};

pub const MIN_MATCH_SCORE: f64 = 0.5;
const MIN_FLAC_BYTES: u64 = 1_000_000;
const MAX_FLAC_BYTES: u64 = 2_000_000_000;
const MIN_MP3_BYTES: u64 = 300_000;
const MAX_MP3_BYTES: u64 = 500_000_000;
const MP3_320_MIN_KBPS: f64 = 300.0;
const MAX_ESTIMATED_MP3_KBPS: f64 = 400.0;
const MAX_FILES_PER_USER_PER_TRACK: usize = 10;
const TRACK_NUMBER_BONUS: f64 = 0.05;
const WRONG_TRACK_PENALTY: f64 = 0.10;
const DISC_NUMBER_BONUS: f64 = 0.03;
const WRONG_DISC_PENALTY: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFormat {
    Flac,
    Mp3,
}

impl SourceFormat {
    fn from_path(path: &str) -> Option<Self> {
        let extension = extension_of(remote_basename(path));
        if extension.eq_ignore_ascii_case("flac") {
            Some(Self::Flac)
        } else if extension.eq_ignore_ascii_case("mp3") {
            Some(Self::Mp3)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub path: String,
    pub size: u64,
    /// Soulseek file attribute 1: duration in seconds, when the peer sent it.
    pub duration_secs: Option<u32>,
    /// Soulseek file attribute 0: bitrate in kbps, when the peer sent it.
    pub bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub username: String,
    pub free_slot: bool,
    pub speed: u32,
    pub files: Vec<RemoteFile>,
}

#[derive(Debug, Clone)]
pub struct TrackQuery {
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub expected_secs: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub username: String,
    pub path: String,
    pub size: u64,
    pub duration_secs: Option<u32>,
    pub match_score: f64,
    pub free_slot: bool,
    pub user_speed: u32,
}

#[derive(Debug, Clone)]
pub struct CandidateOptions {
    pub per_track: usize,
    pub length_tolerance: f64,
    pub artist: String,
    pub album: String,
    pub quality: AudioQuality,
}

/// One peer's copy of the whole album: a single folder (disc subfolders such
/// as `CD1`/`CD2` count as one folder) with a file for every needed track.
/// Taking every track from one rip keeps gapless transitions intact; mixing
/// rips of different masters can cut or repeat audio between tracks.
#[derive(Debug, Clone)]
pub struct AlbumSource {
    pub username: String,
    /// The album folder on the peer, as the peer shared it.
    pub folder: String,
    /// One file per track, in track order. `None` only for tracks that were
    /// not needed (already on disk).
    pub files: Vec<Option<Candidate>>,
    pub average_score: f64,
    pub free_slot: bool,
    pub user_speed: u32,
}

#[derive(Debug, Clone)]
struct PreparedFile {
    username: String,
    path: String,
    /// The album folder: the parent folder, or its parent for disc folders.
    album_root: String,
    size: u64,
    duration_secs: Option<u32>,
    free_slot: bool,
    user_speed: u32,
    stem_norm: String,
    stem_tokens: HashSet<String>,
    path_norm: String,
    file_track: Option<u32>,
    file_disc: Option<u32>,
}

struct FileMatch {
    track: usize,
    file: usize,
    score: f64,
    duration_distance: f64,
}

/// Build a ranked source list for every track from one pooled Soulseek search.
pub fn build_candidates(
    tracks: &[TrackQuery],
    hits: &[SearchHit],
    options: &CandidateOptions,
) -> Vec<Vec<Candidate>> {
    let prepared = prepare_files(hits, options);
    let matches = score_matches(tracks, &prepared, options);

    let mut per_track: Vec<Vec<Candidate>> = tracks.iter().map(|_| Vec::new()).collect();
    let mut assigned: Vec<Option<usize>> = vec![None; prepared.len()];
    let mut per_user: Vec<HashMap<String, usize>> = tracks.iter().map(|_| HashMap::new()).collect();

    for candidate in matches {
        if assigned[candidate.file].is_some()
            || per_track[candidate.track].len() >= options.per_track.max(1)
        {
            continue;
        }
        let file = &prepared[candidate.file];
        let count = per_user[candidate.track]
            .entry(file.username.clone())
            .or_default();
        if *count >= MAX_FILES_PER_USER_PER_TRACK {
            continue;
        }
        *count += 1;
        assigned[candidate.file] = Some(candidate.track);
        per_track[candidate.track].push(file.candidate(candidate.score));
    }

    for (track, candidates) in tracks.iter().zip(per_track.iter_mut()) {
        candidates.sort_by(|a, b| {
            let score_a = (a.match_score * 100.0).round() as i32;
            let score_b = (b.match_score * 100.0).round() as i32;
            let duration_a = duration_distance(track.expected_secs, a.duration_secs);
            let duration_b = duration_distance(track.expected_secs, b.duration_secs);
            score_b
                .cmp(&score_a)
                .then_with(|| {
                    let a_mp3 = SourceFormat::from_path(&a.path) == Some(SourceFormat::Mp3);
                    let b_mp3 = SourceFormat::from_path(&b.path) == Some(SourceFormat::Mp3);
                    b_mp3.cmp(&a_mp3)
                })
                .then_with(|| b.free_slot.cmp(&a.free_slot))
                .then_with(|| b.user_speed.cmp(&a.user_speed))
                .then_with(|| {
                    duration_a
                        .partial_cmp(&duration_b)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| b.size.cmp(&a.size))
        });
    }

    per_track
}

/// Find peers sharing the whole album in one folder, best first.
///
/// `needed[i]` marks the tracks that still have to be downloaded; a source
/// must have a matching file for every one of them. At most
/// `options.per_track` sources are returned, since each is checked by
/// downloading a track.
pub fn build_album_sources(
    tracks: &[TrackQuery],
    needed: &[bool],
    hits: &[SearchHit],
    options: &CandidateOptions,
) -> Vec<AlbumSource> {
    let prepared = prepare_files(hits, options);
    let matches = score_matches(tracks, &prepared, options);

    // Group by peer and album folder, preserving the global best-first order
    // inside each group so assignment matches build_candidates.
    let mut groups: HashMap<(&str, &str), Vec<&FileMatch>> = HashMap::new();
    for candidate in &matches {
        let file = &prepared[candidate.file];
        groups
            .entry((file.username.as_str(), file.album_root.as_str()))
            .or_default()
            .push(candidate);
    }

    let mut sources = Vec::new();
    for ((username, _), group) in groups {
        let mut files: Vec<Option<Candidate>> = vec![None; tracks.len()];
        let mut used: HashSet<usize> = HashSet::new();
        for candidate in group {
            if files[candidate.track].is_some() || !used.insert(candidate.file) {
                continue;
            }
            files[candidate.track] = Some(prepared[candidate.file].candidate(candidate.score));
        }
        let complete = tracks.iter().enumerate().all(|(index, _)| {
            !needed.get(index).copied().unwrap_or(true) || files[index].is_some()
        });
        let needed_count = needed.iter().filter(|needed| **needed).count();
        if !complete || needed_count == 0 {
            continue;
        }
        let scores: Vec<f64> = files
            .iter()
            .zip(needed)
            .filter(|(_, needed)| **needed)
            .filter_map(|(file, _)| file.as_ref().map(|file| file.match_score))
            .collect();
        let average_score = scores.iter().sum::<f64>() / scores.len().max(1) as f64;
        let first = files.iter().flatten().next().cloned();
        let Some(first) = first else {
            continue;
        };
        let folder = album_root(&first.path).to_string();
        sources.push(AlbumSource {
            username: username.to_string(),
            folder,
            files,
            average_score,
            free_slot: first.free_slot,
            user_speed: first.user_speed,
        });
    }

    let mp3_share = |source: &AlbumSource| {
        let files: Vec<&Candidate> = source.files.iter().flatten().collect();
        let mp3 = files
            .iter()
            .filter(|file| SourceFormat::from_path(&file.path) == Some(SourceFormat::Mp3))
            .count();
        (mp3 * 100 / files.len().max(1)) as i32
    };
    sources.sort_by(|a, b| {
        let score_a = (a.average_score * 100.0).round() as i32;
        let score_b = (b.average_score * 100.0).round() as i32;
        score_b
            .cmp(&score_a)
            .then_with(|| {
                if options.quality.is_mp3() {
                    mp3_share(b).cmp(&mp3_share(a))
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| b.free_slot.cmp(&a.free_slot))
            .then_with(|| b.user_speed.cmp(&a.user_speed))
            .then_with(|| a.username.cmp(&b.username))
            .then_with(|| a.folder.cmp(&b.folder))
    });
    sources.truncate(options.per_track.max(1));
    sources
}

fn prepare_files(hits: &[SearchHit], options: &CandidateOptions) -> Vec<PreparedFile> {
    let mut prepared: Vec<PreparedFile> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for hit in hits {
        for file in &hit.files {
            let base = remote_basename(&file.path);
            let Some(format) = SourceFormat::from_path(&file.path) else {
                continue;
            };
            match format {
                SourceFormat::Flac => {}
                SourceFormat::Mp3
                    if options.quality.is_mp3() && mp3_bitrate_is_320(file, &file.path) => {}
                SourceFormat::Mp3 => continue,
            }
            let (min_bytes, max_bytes) = match format {
                SourceFormat::Flac => (MIN_FLAC_BYTES, MAX_FLAC_BYTES),
                SourceFormat::Mp3 => (MIN_MP3_BYTES, MAX_MP3_BYTES),
            };
            let lower = base.to_ascii_lowercase();
            if lower.contains("sample") || lower.contains("preview") || lower.contains("snippet") {
                continue;
            }
            if file.size > 0 && (file.size < min_bytes || file.size > max_bytes) {
                continue;
            }
            if !seen.insert((hit.username.clone(), file.path.clone())) {
                continue;
            }

            let raw_stem = stem(base);
            let (file_disc, file_track) = leading_track_number(raw_stem);
            // "Album/CD2/01 - Song.flac" carries its disc in the folder name.
            let file_disc = file_disc.or_else(|| folder_disc_number(&file.path));
            let clean_stem = strip_release_noise(raw_stem);
            let stem_norm = normalize_text(&clean_stem);
            if stem_norm.is_empty() {
                continue;
            }
            let path_norm = normalize_text(&file.path);

            prepared.push(PreparedFile {
                username: hit.username.clone(),
                path: file.path.clone(),
                album_root: album_root(&file.path).to_lowercase(),
                size: file.size,
                duration_secs: file.duration_secs.filter(|seconds| *seconds > 0),
                free_slot: hit.free_slot,
                user_speed: hit.speed,
                stem_tokens: token_set(&stem_norm),
                stem_norm,
                path_norm,
                file_track,
                file_disc,
            });
        }
    }
    prepared
}

/// Score every file against every track, best match first.
fn score_matches(
    tracks: &[TrackQuery],
    prepared: &[PreparedFile],
    options: &CandidateOptions,
) -> Vec<FileMatch> {
    let artist_tokens = token_set(&normalize_text(&options.artist));
    let album_tokens = token_set(&normalize_text(&options.album));

    // Score every file against every track first. Assigning greedily per track
    // in album order lets an earlier track consume a similarly named later
    // track's files ("In the Flesh?" vs "In the Flesh").
    let mut matches: Vec<FileMatch> = Vec::new();
    for (track_index, track) in tracks.iter().enumerate() {
        let track_norm = normalize_text(&track.title);
        let track_tokens = token_set(&track_norm);

        for (file_index, file) in prepared.iter().enumerate() {
            let Some(mut score) = title_score(&track_norm, &track_tokens, file) else {
                continue;
            };

            if let (Some(expected), Some(reported)) =
                (track.expected_secs, file.duration_secs.map(f64::from))
            {
                // Reported FLAC lengths are normally accurate. A file belonging
                // to a different track or edit is usually far enough away to
                // reject here instead of spending bandwidth on it.
                let allowed = allowed_duration_drift(expected, options.length_tolerance);
                if (expected - reported).abs() > allowed {
                    continue;
                }
            }

            if file.file_track == Some(track.position) {
                score += TRACK_NUMBER_BONUS;
            } else if file.file_track.is_some() && track.position > 0 {
                score -= WRONG_TRACK_PENALTY;
            }
            if let Some(file_disc) = file.file_disc {
                if file_disc == track.disc {
                    score += DISC_NUMBER_BONUS;
                } else {
                    score -= WRONG_DISC_PENALTY;
                }
            }

            if token_subset(&album_tokens, &file.stem_tokens)
                || token_subset(&album_tokens, &token_set(&file.path_norm))
            {
                score += 0.02;
            }
            if token_subset(&artist_tokens, &token_set(&file.path_norm)) {
                score += 0.01;
            }

            score = score.clamp(0.0, 1.0);
            if score < MIN_MATCH_SCORE {
                continue;
            }
            matches.push(FileMatch {
                track: track_index,
                file: file_index,
                score,
                duration_distance: duration_distance(track.expected_secs, file.duration_secs),
            });
        }
    }

    // Best match wins. A remote file is still only ever assigned to one track:
    // soulseek-rs-lib keys transfers by username+filename, so two tracks
    // sharing one file can cancel each other.
    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                a.duration_distance
                    .partial_cmp(&b.duration_distance)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.track.cmp(&b.track))
    });
    matches
}

impl PreparedFile {
    fn candidate(&self, match_score: f64) -> Candidate {
        Candidate {
            username: self.username.clone(),
            path: self.path.clone(),
            size: self.size,
            duration_secs: self.duration_secs,
            match_score,
            free_slot: self.free_slot,
            user_speed: self.user_speed,
        }
    }
}

/// The folders of a remote path, without the filename.
fn remote_folders(path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = path.split(['/', '\\']).collect();
    parts.pop();
    parts
}

/// The disc number a folder such as `CD2`, `Disc 2` or `The Wall (Disc 2)`
/// names, if any.
fn disc_folder_number(folder: &str) -> Option<u32> {
    let normalized = normalize_text(folder);
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        for prefix in ["cd", "disc", "disk"] {
            if *token == prefix {
                if let Some(number) = tokens.get(index + 1).and_then(|next| next.parse().ok())
                    && (1..=99).contains(&number)
                {
                    return Some(number);
                }
            } else if let Some(rest) = token.strip_prefix(prefix)
                && let Ok(number) = rest.parse::<u32>()
                && (1..=99).contains(&number)
            {
                return Some(number);
            }
        }
    }
    None
}

fn folder_disc_number(path: &str) -> Option<u32> {
    remote_folders(path)
        .last()
        .and_then(|folder| disc_folder_number(folder))
}

/// The album folder of a remote file: its parent folder, or the parent's
/// parent when the file sits in a disc folder (`Album/CD1/01 - Song.flac`).
pub(crate) fn album_root(path: &str) -> &str {
    let folders = remote_folders(path);
    let depth = if folders.len() >= 2
        && folders
            .last()
            .is_some_and(|folder| disc_folder_number(folder).is_some())
    {
        folders.len() - 1
    } else {
        folders.len()
    };
    let length: usize = folders[..depth].iter().map(|part| part.len() + 1).sum();
    &path[..length.saturating_sub(1)]
}

fn allowed_duration_drift(expected: f64, tolerance: f64) -> f64 {
    ((expected * 0.1).max(tolerance * 5.0)).clamp(15.0, 60.0)
}

fn duration_distance(expected: Option<f64>, reported: Option<u32>) -> f64 {
    match (expected, reported) {
        (Some(expected), Some(reported)) => (expected - f64::from(reported)).abs(),
        _ => f64::MAX,
    }
}

/// A 320 kbps MP3 can be identified from Soulseek's bitrate attribute, or
/// estimated from size and duration. Peers that send neither must at least put
/// "320" in the filename; a 128 kbps file is worse than falling back to FLAC.
fn mp3_bitrate_is_320(file: &RemoteFile, path: &str) -> bool {
    if let Some(kbps) = file.bitrate_kbps.filter(|kbps| *kbps > 0) {
        return f64::from(kbps) >= MP3_320_MIN_KBPS;
    }
    if let Some(seconds) = file.duration_secs.filter(|seconds| *seconds > 0)
        && file.size > 0
    {
        let estimated = file.size as f64 * 8.0 / f64::from(seconds) / 1000.0;
        return (MP3_320_MIN_KBPS..=MAX_ESTIMATED_MP3_KBPS).contains(&estimated);
    }
    path.to_ascii_lowercase().contains("320")
}

/// Similarity between a track title and a candidate file stem.
fn title_score(
    track_norm: &str,
    track_tokens: &HashSet<String>,
    file: &PreparedFile,
) -> Option<f64> {
    if track_norm.is_empty() || file.stem_norm.is_empty() {
        return None;
    }
    if track_norm == file.stem_norm {
        return Some(1.0);
    }

    let padded_track = format!(" {track_norm} ");
    let padded_file = format!(" {} ", file.stem_norm);
    let contains = padded_file.contains(&padded_track);
    let lev = normalized_levenshtein(track_norm, &file.stem_norm);
    let jaccard = jaccard(track_tokens, &file.stem_tokens);

    // A single word must be a whole-word match; "one" should not match
    // "someone" just because a fuzzy ratio crosses the threshold.
    if track_tokens.len() == 1 && !contains {
        return None;
    }

    if contains {
        Some(lev.max(jaccard).max(0.85))
    } else {
        Some(lev.max(jaccard))
    }
}

/// Lowercase, strip accents, replace punctuation with spaces and collapse.
pub fn normalize_text(s: &str) -> String {
    let lowered = s.to_lowercase().replace('&', " and ");
    let mut out = String::with_capacity(lowered.len());
    let mut last_space = true;
    for ch in lowered.nfkd() {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            for c in ch.to_lowercase() {
                out.push(c);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Remove `[FLAC]`, `{WEB}`, and format-parentheses before matching.
pub(crate) fn strip_release_noise(s: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "flac",
        "mp3",
        "320",
        "v0",
        "v2",
        "24bit",
        "24-bit",
        "16bit",
        "16-bit",
        "vinyl",
        "web",
        "cd",
        "remaster",
        "remastered",
        "edition",
        "deluxe",
        "bonus",
        "lossless",
        "rip",
        "scene",
    ];

    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '[' | '{' => {
                let close = if chars[i] == '[' { ']' } else { '}' };
                if let Some(offset) = chars[i + 1..].iter().position(|&c| c == close) {
                    i += offset + 2;
                } else {
                    i += 1;
                }
            }
            '(' => {
                if let Some(offset) = chars[i + 1..].iter().position(|&c| c == ')') {
                    let inside: String = chars[i + 1..i + 1 + offset].iter().collect();
                    let lowered = inside.to_ascii_lowercase();
                    if !KEYWORDS.iter().any(|kw| lowered.contains(kw)) {
                        out.push('(');
                        out.push_str(&inside);
                        out.push(')');
                    }
                    i += offset + 2;
                } else {
                    i += 1;
                }
            }
            ']' | '}' | ')' => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    strip_leading_track_number(out.trim())
}

fn strip_leading_track_number(s: &str) -> String {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 2 {
        i += 1;
    }
    if i == 0 || i >= bytes.len() {
        return trimmed.to_string();
    }
    let sep = bytes[i] as char;
    if !matches!(sep, ' ' | '-' | '.' | '_') {
        return trimmed.to_string();
    }
    let mut end = i + 1;
    // Multi-disc prefixes such as "1-01" or "2.03".
    if matches!(sep, '-' | '.' | '_') {
        let mut j = end;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - end < 2 {
            j += 1;
        }
        if j > end && (j == bytes.len() || matches!(bytes[j] as char, ' ' | '-' | '.' | '_')) {
            end = j;
        }
    }
    let rest = trimmed[end..].trim_start_matches([' ', '-', '.', '_']);
    if rest.is_empty() {
        trimmed.to_string()
    } else {
        rest.to_string()
    }
}

fn leading_track_number(stem: &str) -> (Option<u32>, Option<u32>) {
    let trimmed = stem.trim_start();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 2 {
        i += 1;
    }
    if i == 0 || i >= bytes.len() {
        return (None, None);
    }
    let first: u32 = trimmed[..i].parse().unwrap_or(0);
    let sep = bytes[i] as char;
    if !matches!(sep, ' ' | '-' | '.' | '_') {
        return (None, None);
    }
    if matches!(sep, '-' | '.' | '_') {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - (i + 1) < 2 {
            j += 1;
        }
        if j > i + 1 && (j == bytes.len() || matches!(bytes[j] as char, ' ' | '-' | '.' | '_')) {
            let second: u32 = trimmed[i + 1..j].parse().unwrap_or(0);
            return (Some(first), Some(second));
        }
    }
    (None, Some(first))
}

fn token_set(s: &str) -> HashSet<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn token_subset(needle: &HashSet<String>, haystack: &HashSet<String>) -> bool {
    !needle.is_empty() && needle.len() > 1 && needle.is_subset(haystack)
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    intersection as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(_user: &str, path: &str, size: u64) -> RemoteFile {
        RemoteFile {
            path: path.to_string(),
            size,
            duration_secs: None,
            bitrate_kbps: None,
        }
    }

    #[test]
    fn normalization_handles_punctuation_accents_and_ampersands() {
        assert_eq!(normalize_text("Beyoncé — Déjà Vu"), "beyonce deja vu");
        assert_eq!(normalize_text("AC/DC & Friends"), "ac dc and friends");
    }

    #[test]
    fn release_noise_is_stripped() {
        assert_eq!(strip_release_noise("[FLAC] 01 - Song (Remastered)"), "Song");
        assert_eq!(strip_release_noise("04. Song (Live)"), "Song (Live)");
        assert_eq!(strip_release_noise("1-03 Song"), "Song");
        assert_eq!(strip_release_noise("1979"), "1979");
    }

    #[test]
    fn title_scoring_prefers_exact_and_containment() {
        let f = PreparedFile {
            username: "u".into(),
            path: "x\\Metallica - Ride the Lightning - 01 - Fight Fire With Fire.flac".into(),
            album_root: "x".into(),
            size: 30_000_000,
            duration_secs: Some(285),
            free_slot: true,
            user_speed: 100,
            stem_norm: normalize_text(&strip_release_noise(
                "Metallica - Ride the Lightning - 01 - Fight Fire With Fire",
            )),
            stem_tokens: token_set(&normalize_text(
                "Metallica Ride the Lightning 01 Fight Fire With Fire",
            )),
            path_norm: normalize_text(
                "Metallica - Ride the Lightning - 01 - Fight Fire With Fire.flac",
            ),
            file_track: Some(1),
            file_disc: None,
        };
        let title = normalize_text("Fight Fire With Fire");
        let score = title_score(&title, &token_set(&title), &f).unwrap();
        assert!(score >= 0.85, "{score}");
    }

    #[test]
    fn a_short_single_word_does_not_match_a_longer_word() {
        let f = PreparedFile {
            username: "u".into(),
            path: "someone.flac".into(),
            album_root: String::new(),
            size: 20_000_000,
            duration_secs: None,
            free_slot: true,
            user_speed: 1,
            stem_norm: "someone".into(),
            stem_tokens: token_set("someone"),
            path_norm: "someone flac".into(),
            file_track: None,
            file_disc: None,
        };
        let title = normalize_text("One");
        assert!(title_score(&title, &token_set(&title), &f).is_none());
    }

    #[test]
    fn candidates_are_ranked_and_junk_is_ignored() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Fight Fire With Fire".into(),
            expected_secs: Some(285.0),
        }];
        let hits = vec![SearchHit {
            username: "fast".into(),
            free_slot: true,
            speed: 5000,
            files: vec![
                file("fast", "Metal\\01 - Fight Fire With Fire.flac", 30_000_000),
                file(
                    "fast",
                    "Metal\\sample - Fight Fire With Fire.flac",
                    30_000_000,
                ),
                file("fast", "Metal\\01 - Fight Fire With Fire.mp3", 8_000_000),
            ],
        }];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Metallica".into(),
            album: "Ride the Lightning".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), 1);
        assert_eq!(out[0][0].username, "fast");
    }

    #[test]
    fn one_remote_file_is_never_assigned_to_two_tracks() {
        let tracks = vec![
            TrackQuery {
                disc: 1,
                position: 1,
                title: "Intro".into(),
                expected_secs: None,
            },
            TrackQuery {
                disc: 1,
                position: 2,
                title: "Intro".into(),
                expected_secs: None,
            },
        ];
        let hits = vec![SearchHit {
            username: "one".into(),
            free_slot: true,
            speed: 100,
            files: vec![file("one", "Album\\01 - Intro.flac", 20_000_000)],
        }];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Artist".into(),
            album: "Album".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len() + out[1].len(), 1);
    }

    fn file_with_duration(path: &str, size: u64, duration_secs: u32) -> RemoteFile {
        RemoteFile {
            path: path.to_string(),
            size,
            duration_secs: Some(duration_secs),
            bitrate_kbps: None,
        }
    }

    #[test]
    fn mp3_mode_prefers_a_verified_320_over_flac() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Fight Fire With Fire".into(),
            expected_secs: Some(240.0),
        }];
        let hits = vec![SearchHit {
            username: "peer".into(),
            free_slot: true,
            speed: 1000,
            files: vec![
                file_with_duration("Metal\\01 - Fight Fire With Fire.flac", 40_000_000, 240),
                file_with_duration("Metal\\01 - Fight Fire With Fire.mp3", 9_600_000, 240),
            ],
        }];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Mp3,
            artist: "Metallica".into(),
            album: "Ride the Lightning".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), 2);
        assert!(out[0][0].path.ends_with(".mp3"));
        assert!(out[0][1].path.ends_with(".flac"));
    }

    #[test]
    fn mp3_mode_rejects_low_bitrate_mp3_and_falls_back_to_flac() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Fight Fire With Fire".into(),
            expected_secs: Some(240.0),
        }];
        let hits = vec![SearchHit {
            username: "peer".into(),
            free_slot: true,
            speed: 1000,
            files: vec![
                file_with_duration("Metal\\01 - Fight Fire With Fire.mp3", 3_840_000, 240),
                file_with_duration("Metal\\01 - Fight Fire With Fire.flac", 40_000_000, 240),
            ],
        }];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Mp3,
            artist: "Metallica".into(),
            album: "Ride the Lightning".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), 1);
        assert!(out[0][0].path.ends_with(".flac"));
    }

    #[test]
    fn reported_mp3_bitrate_can_select_a_file_without_duration() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Song".into(),
            expected_secs: None,
        }];
        let hits = vec![SearchHit {
            username: "peer".into(),
            free_slot: true,
            speed: 1000,
            files: vec![RemoteFile {
                path: "Album\\01 - Song.mp3".into(),
                size: 9_000_000,
                duration_secs: None,
                bitrate_kbps: Some(320),
            }],
        }];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Mp3,
            artist: "Artist".into(),
            album: "Album".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), 1);
        assert_eq!(out[0][0].path, "Album\\01 - Song.mp3");
    }

    #[test]
    fn similarly_named_tracks_get_their_own_sources() {
        let tracks = vec![
            TrackQuery {
                disc: 1,
                position: 1,
                title: "In the Flesh?".into(),
                expected_secs: None,
            },
            TrackQuery {
                disc: 2,
                position: 8,
                title: "In the Flesh".into(),
                expected_secs: None,
            },
        ];
        let hits = vec![
            SearchHit {
                username: "a".into(),
                free_slot: true,
                speed: 100,
                files: vec![
                    file(
                        "a",
                        "Pink Floyd - The Wall\\1-01 - In the Flesh_.flac",
                        20_000_000,
                    ),
                    file(
                        "a",
                        "Pink Floyd - The Wall\\2-08 - In the Flesh.flac",
                        25_000_000,
                    ),
                ],
            },
            SearchHit {
                username: "b".into(),
                free_slot: true,
                speed: 100,
                files: vec![file(
                    "b",
                    "Pink Floyd - The Wall\\2-08 - In the Flesh.flac",
                    25_000_000,
                )],
            },
        ];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Pink Floyd".into(),
            album: "The Wall".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);

        assert_eq!(out[0].len(), 1);
        assert!(out[0][0].path.contains("1-01"));
        assert_eq!(out[1].len(), 2);
        assert!(
            out[1]
                .iter()
                .all(|candidate| candidate.path.contains("2-08"))
        );
    }

    #[test]
    fn duration_keeps_similarly_named_tracks_apart() {
        let tracks = vec![
            TrackQuery {
                disc: 1,
                position: 1,
                title: "In the Flesh?".into(),
                expected_secs: Some(199.6),
            },
            TrackQuery {
                disc: 2,
                position: 8,
                title: "In the Flesh".into(),
                expected_secs: Some(256.7),
            },
        ];
        let hits = vec![
            SearchHit {
                username: "a".into(),
                free_slot: true,
                speed: 100,
                files: vec![
                    file_with_duration("The Wall\\In the Flesh_.flac", 20_000_000, 199),
                    file_with_duration("The Wall\\In the Flesh.flac", 25_000_000, 257),
                ],
            },
            SearchHit {
                username: "b".into(),
                free_slot: true,
                speed: 100,
                files: vec![file_with_duration(
                    "The Wall\\In the Flesh.flac",
                    25_000_000,
                    257,
                )],
            },
        ];
        let options = CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Pink Floyd".into(),
            album: "The Wall".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);

        assert_eq!(out[0].len(), 1);
        assert_eq!(out[0][0].duration_secs, Some(199));
        assert_eq!(out[1].len(), 2);
        assert!(
            out[1]
                .iter()
                .all(|candidate| candidate.duration_secs == Some(257))
        );
    }

    #[test]
    fn one_peer_is_limited_to_ten_tries() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Song".into(),
            expected_secs: None,
        }];
        let files: Vec<_> = (0..15)
            .map(|i| file("one", &format!("Album\\{i:02} - Song.flac"), 20_000_000))
            .collect();
        let hits = vec![SearchHit {
            username: "one".into(),
            free_slot: true,
            speed: 100,
            files,
        }];
        let options = CandidateOptions {
            per_track: 30,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Artist".into(),
            album: "Album".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), MAX_FILES_PER_USER_PER_TRACK);
    }

    #[test]
    fn per_track_limit_keeps_the_thirty_best_sources() {
        let tracks = vec![TrackQuery {
            disc: 1,
            position: 1,
            title: "Song".into(),
            expected_secs: None,
        }];
        let hits: Vec<_> = (0..4)
            .map(|peer| SearchHit {
                username: format!("peer{peer}"),
                free_slot: true,
                speed: 100,
                files: (0..10)
                    .map(|i| {
                        file(
                            &format!("peer{peer}"),
                            &format!("Album\\{i:02} - Song.flac"),
                            20_000_000,
                        )
                    })
                    .collect(),
            })
            .collect();
        let options = CandidateOptions {
            per_track: 30,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Artist".into(),
            album: "Album".into(),
        };
        let out = build_candidates(&tracks, &hits, &options);
        assert_eq!(out[0].len(), 30);
    }

    fn timed(path: &str, size: u64, secs: u32) -> RemoteFile {
        RemoteFile {
            path: path.to_string(),
            size,
            duration_secs: Some(secs),
            bitrate_kbps: None,
        }
    }

    fn wall_tracks() -> Vec<TrackQuery> {
        vec![
            TrackQuery {
                disc: 1,
                position: 1,
                title: "In the Flesh?".into(),
                expected_secs: Some(199.0),
            },
            TrackQuery {
                disc: 1,
                position: 2,
                title: "The Thin Ice".into(),
                expected_secs: Some(150.0),
            },
            TrackQuery {
                disc: 2,
                position: 1,
                title: "Hey You".into(),
                expected_secs: Some(284.0),
            },
        ]
    }

    fn wall_options() -> CandidateOptions {
        CandidateOptions {
            per_track: 12,
            length_tolerance: 5.0,
            quality: AudioQuality::Flac,
            artist: "Pink Floyd".into(),
            album: "The Wall".into(),
        }
    }

    #[test]
    fn disc_folders_belong_to_their_album_folder() {
        assert_eq!(
            album_root("Music\\The Wall\\CD1\\01 - x.flac"),
            "Music\\The Wall"
        );
        assert_eq!(
            album_root("Music/The Wall/Disc 2/01 - x.flac"),
            "Music/The Wall"
        );
        assert_eq!(
            album_root("Music\\The Wall\\01 - x.flac"),
            "Music\\The Wall"
        );
        assert_eq!(album_root("01 - x.flac"), "");
        assert_eq!(
            folder_disc_number("A\\The Wall (Disc 2)\\01 - x.flac"),
            Some(2)
        );
        assert_eq!(folder_disc_number("A\\CD2\\01 - x.flac"), Some(2));
        assert_eq!(folder_disc_number("A\\The Wall [CD]\\01 - x.flac"), None);
        assert_eq!(folder_disc_number("A\\Discography\\01 - x.flac"), None);
    }

    #[test]
    fn a_peer_with_the_whole_album_in_disc_folders_is_one_source() {
        let hits = vec![
            SearchHit {
                username: "complete".into(),
                free_slot: false,
                speed: 100,
                files: vec![
                    timed(
                        "Rips\\The Wall\\CD1\\01 - In the Flesh.flac",
                        30_000_000,
                        199,
                    ),
                    timed(
                        "Rips\\The Wall\\CD1\\02 - The Thin Ice.flac",
                        25_000_000,
                        150,
                    ),
                    timed("Rips\\The Wall\\CD2\\01 - Hey You.flac", 40_000_000, 284),
                ],
            },
            SearchHit {
                // Faster, but missing a track.
                username: "partial".into(),
                free_slot: true,
                speed: 9_000,
                files: vec![
                    timed("The Wall\\01 - In the Flesh.flac", 30_000_000, 199),
                    timed("The Wall\\02 - The Thin Ice.flac", 25_000_000, 150),
                ],
            },
        ];
        let sources =
            build_album_sources(&wall_tracks(), &[true, true, true], &hits, &wall_options());
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].username, "complete");
        assert_eq!(sources[0].folder, "Rips\\The Wall");
        let paths: Vec<&str> = sources[0]
            .files
            .iter()
            .map(|file| file.as_ref().unwrap().path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "Rips\\The Wall\\CD1\\01 - In the Flesh.flac",
                "Rips\\The Wall\\CD1\\02 - The Thin Ice.flac",
                "Rips\\The Wall\\CD2\\01 - Hey You.flac",
            ]
        );

        // Only the tracks still needed have to be present.
        let sources =
            build_album_sources(&wall_tracks(), &[true, true, false], &hits, &wall_options());
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].username, "partial");
        assert!(sources[0].files[2].is_none());
    }

    #[test]
    fn two_rips_from_one_peer_are_separate_sources() {
        let hits = vec![SearchHit {
            username: "collector".into(),
            free_slot: true,
            speed: 100,
            files: vec![
                timed("1979 CD\\01 - In the Flesh.flac", 30_000_000, 199),
                timed("1979 CD\\02 - The Thin Ice.flac", 25_000_000, 150),
                timed("2011 Remaster\\01 - In the Flesh.flac", 30_000_000, 199),
                timed("2011 Remaster\\02 - The Thin Ice.flac", 25_000_000, 150),
                timed("2011 Remaster\\03 - Hey You.flac", 40_000_000, 284),
            ],
        }];
        let sources =
            build_album_sources(&wall_tracks(), &[true, true, true], &hits, &wall_options());
        assert_eq!(sources.len(), 1, "the 1979 folder is incomplete");
        assert_eq!(sources[0].folder, "2011 Remaster");
        assert!(
            sources[0]
                .files
                .iter()
                .flatten()
                .all(|file| file.path.starts_with("2011 Remaster"))
        );
    }
}
