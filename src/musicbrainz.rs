//! MusicBrainz search, release fetching and standard-edition ranking.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use strsim::normalized_levenshtein;

use crate::matcher::{normalize_text, strip_release_noise};

const MB_BASE: &str = "https://musicbrainz.org/ws/2";
const CAA_BASE: &str = "https://coverartarchive.org";
const ITUNES_SEARCH: &str = "https://itunes.apple.com/search";
const ITUNES_COUNTRY: &str = "US";
const ITUNES_ARTWORK_SIZE: &str = "600x600bb";
const USER_AGENT: &str = concat!(
    "soulsucker/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/example/soulsucker)"
);
const REQUEST_SPACING: Duration = Duration::from_millis(1100);
const MAX_RETRIES: u32 = 3;

static HTTP: OnceLock<ureq::Agent> = OnceLock::new();
static LAST_REQUEST: Mutex<Option<Instant>> = Mutex::new(None);

fn http() -> &'static ureq::Agent {
    HTTP.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(25)))
            .user_agent(USER_AGENT)
            .build();
        ureq::Agent::new_with_config(config)
    })
}

fn throttle() {
    let mut guard = LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(last) = *guard {
        let elapsed = last.elapsed();
        if elapsed < REQUEST_SPACING {
            std::thread::sleep(REQUEST_SPACING - elapsed);
        }
    }
    *guard = Some(Instant::now());
}

fn should_retry(retryable: bool, attempt: u32) -> bool {
    retryable && attempt < MAX_RETRIES
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(2 + attempt as u64 * 2)
}

fn get_json<T: DeserializeOwned>(url: &str, query: &[(&str, String)]) -> Result<T> {
    let mut last_error = None;
    for attempt in 0..=MAX_RETRIES {
        throttle();
        let request = http()
            .get(url)
            .query_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        match request.call() {
            Ok(mut response) => {
                return response
                    .body_mut()
                    .read_json::<T>()
                    .with_context(|| format!("could not parse MusicBrainz response from {url}"));
            }
            Err(err) => {
                let retryable = matches!(
                    err,
                    ureq::Error::StatusCode(429 | 502 | 503 | 504)
                        | ureq::Error::Timeout(_)
                        | ureq::Error::Io(_)
                );
                let message = friendly_http_error(&err);
                last_error = Some(anyhow!(message));
                if !should_retry(retryable, attempt) {
                    break;
                }
                std::thread::sleep(retry_delay(attempt));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("MusicBrainz request failed")))
}

fn friendly_http_error(err: &ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(503) => {
            "MusicBrainz is busy right now; try again in a moment".to_string()
        }
        ureq::Error::StatusCode(429) => {
            "MusicBrainz asked us to slow down; wait a moment and retry".to_string()
        }
        ureq::Error::StatusCode(404) => "MusicBrainz has no such release".to_string(),
        ureq::Error::StatusCode(code) => format!("MusicBrainz returned HTTP {code}"),
        ureq::Error::Timeout(_) => "MusicBrainz timed out".to_string(),
        other => format!("MusicBrainz request failed: {other}"),
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseCandidate {
    pub mbid: String,
    pub title: String,
    pub artist: String,
    pub date: String,
    pub country: String,
    pub status: String,
    pub disambiguation: String,
    pub track_count: u32,
    pub primary_type: String,
    pub secondary_types: Vec<String>,
    pub formats: Vec<String>,
    pub score: i32,
}

impl ReleaseCandidate {
    pub fn year(&self) -> &str {
        self.date.get(..4).unwrap_or("")
    }

    pub fn format_summary(&self) -> String {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for format in &self.formats {
            let lower = format.to_ascii_lowercase();
            if is_video_format(&lower) {
                continue;
            }
            *counts.entry(format.as_str()).or_default() += 1;
        }
        if counts.is_empty() {
            return "format unknown".to_string();
        }
        counts
            .into_iter()
            .map(|(format, count)| {
                if count > 1 {
                    format!("{count}×{format}")
                } else {
                    format.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Short badges such as `Deluxe`, `Live`, `Bootleg`.
    pub fn edition_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        let blob = format!("{} {}", self.title, self.disambiguation).to_ascii_lowercase();
        if blob.contains("super deluxe") {
            tags.push("Super Deluxe".to_string());
        } else if blob.contains("deluxe") {
            tags.push("Deluxe".to_string());
        }
        if blob.contains("anniversary") {
            tags.push("Anniversary".to_string());
        }
        if blob.contains("remaster") {
            tags.push("Remaster".to_string());
        }
        if blob.contains("expanded") {
            tags.push("Expanded".to_string());
        }
        if contains_word(&blob, "live")
            || self
                .secondary_types
                .iter()
                .any(|s| s.eq_ignore_ascii_case("live"))
        {
            tags.push("Live".to_string());
        }
        if contains_word(&blob, "demo") || contains_word(&blob, "demos") {
            tags.push("Demos".to_string());
        }
        for secondary in &self.secondary_types {
            if secondary.eq_ignore_ascii_case("compilation")
                && !tags.iter().any(|t| t == "Compilation")
            {
                tags.push("Compilation".to_string());
            }
            if secondary.eq_ignore_ascii_case("remix") && !tags.iter().any(|t| t == "Remix") {
                tags.push("Remix".to_string());
            }
        }
        if !self.status.is_empty() && !self.status.eq_ignore_ascii_case("official") {
            tags.push(self.status.clone());
        }
        if !self.primary_type.is_empty() && !self.primary_type.eq_ignore_ascii_case("album") {
            tags.push(self.primary_type.clone());
        }
        tags.dedup();
        tags
    }

    /// A remastered pressing, based on the release title or disambiguation
    /// comment (MusicBrainz stores remaster information there).
    pub fn is_remastered(&self) -> bool {
        let blob = format!("{} {}", self.title, self.disambiguation).to_ascii_lowercase();
        blob.contains("remaster")
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseGroupCandidate {
    pub mbid: String,
    pub title: String,
    pub artist: String,
    pub first_release_date: String,
    pub primary_type: String,
    pub secondary_types: Vec<String>,
    pub disambiguation: String,
    pub release_count: u32,
    pub search_score: i32,
    /// Lower is more desirable, like [`ReleaseCandidate::score`].
    pub score: i32,
}

impl ReleaseGroupCandidate {
    pub fn year(&self) -> &str {
        self.first_release_date.get(..4).unwrap_or("")
    }

    /// Short type labels such as `Album`, `EP`, or `Live · EP`.
    pub fn type_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if !self.primary_type.trim().is_empty() {
            tags.push(self.primary_type.trim().to_string());
        }
        for secondary in &self.secondary_types {
            let secondary = secondary.trim();
            if !secondary.is_empty() && !tags.iter().any(|tag| tag.eq_ignore_ascii_case(secondary))
            {
                tags.push(secondary.to_string());
            }
        }
        tags
    }
}

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub index: usize,
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub artist: String,
    pub length_ms: Option<u64>,
    pub recording_mbid: String,
    pub track_mbid: String,
}

impl TrackInfo {
    pub fn length_secs(&self) -> Option<f64> {
        self.length_ms.map(|ms| ms as f64 / 1000.0)
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub mbid: String,
    pub release_group_mbid: String,
    pub title: String,
    pub artist: String,
    pub date: String,
    pub country: String,
    pub status: String,
    pub disambiguation: String,
    pub primary_type: String,
    pub tracks: Vec<TrackInfo>,
}

impl ReleaseInfo {
    pub fn year(&self) -> &str {
        self.date.get(..4).unwrap_or("")
    }

    /// Singles are stored as loose files in the artist folder instead of
    /// getting an album folder of their own.
    pub fn is_single(&self) -> bool {
        self.primary_type.trim().eq_ignore_ascii_case("single")
    }

    pub fn disc_count(&self) -> u32 {
        self.tracks.iter().map(|t| t.disc).max().unwrap_or(1)
    }

    pub fn total_duration_secs(&self) -> f64 {
        self.tracks.iter().filter_map(|t| t.length_secs()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSource {
    Itunes,
    CoverArtArchive,
}

impl CoverSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Itunes => "iTunes",
            Self::CoverArtArchive => "Cover Art Archive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoverArt {
    pub data: Vec<u8>,
    pub source: CoverSource,
}

// -------------------------------------------------------------------------
// JSON models
// -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GroupSearchResponse {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<SearchReleaseGroup>,
}

#[derive(Debug, Deserialize)]
struct SearchReleaseGroup {
    id: String,
    title: String,
    #[serde(default)]
    score: i32,
    #[serde(rename = "primary-type", default)]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
    #[serde(rename = "first-release-date", default)]
    first_release_date: Option<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(default)]
    count: u32,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Debug, Deserialize)]
struct ArtistCredit {
    #[serde(default)]
    name: String,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroup {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "primary-type", default)]
    primary_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowseReleasesResponse {
    #[serde(default)]
    releases: Vec<BrowseRelease>,
}

#[derive(Debug, Deserialize)]
struct BrowseRelease {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    media: Vec<BrowseMedium>,
}

#[derive(Debug, Deserialize)]
struct BrowseMedium {
    #[serde(rename = "track-count", default)]
    track_count: u32,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DetailRelease {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "release-group", default)]
    release_group: Option<ReleaseGroup>,
    #[serde(default)]
    media: Vec<DetailMedium>,
}

#[derive(Debug, Deserialize)]
struct DetailMedium {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tracks: Vec<DetailTrack>,
}

#[derive(Debug, Deserialize)]
struct DetailTrack {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    recording: Option<DetailRecording>,
}

#[derive(Debug, Deserialize)]
struct DetailRecording {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    video: bool,
}

#[derive(Debug, Deserialize)]
struct ItunesSearchResponse {
    #[serde(default)]
    results: Vec<ItunesAlbum>,
}

#[derive(Debug, Deserialize)]
struct ItunesAlbum {
    #[serde(rename = "collectionName", default)]
    collection_name: String,
    #[serde(rename = "artistName", default)]
    artist_name: String,
    #[serde(rename = "artworkUrl100", default)]
    artwork_url_100: Option<String>,
    #[serde(rename = "artworkUrl60", default)]
    artwork_url_60: Option<String>,
}

// -------------------------------------------------------------------------
// Public API
// -------------------------------------------------------------------------

pub fn search_release_groups(
    artist: &str,
    album: &str,
    limit: usize,
) -> Result<Vec<ReleaseGroupCandidate>> {
    let query = build_group_query(artist, album);
    let url = format!("{MB_BASE}/release-group/");
    let response: GroupSearchResponse = get_json(
        &url,
        &[
            ("query", query),
            ("fmt", "json".to_string()),
            ("limit", limit.clamp(1, 100).to_string()),
        ],
    )?;

    Ok(groups_from_search(response, artist, album))
}

fn groups_from_search(
    response: GroupSearchResponse,
    artist: &str,
    album: &str,
) -> Vec<ReleaseGroupCandidate> {
    let groups = response
        .release_groups
        .into_iter()
        .map(|group| ReleaseGroupCandidate {
            mbid: group.id,
            title: group.title,
            artist: join_artist_credit(&group.artist_credit),
            first_release_date: group.first_release_date.unwrap_or_default(),
            primary_type: group.primary_type.unwrap_or_default(),
            secondary_types: group.secondary_types,
            disambiguation: group.disambiguation.unwrap_or_default(),
            release_count: group.count,
            search_score: group.score,
            score: 0,
        })
        .collect();
    rank_groups(groups, artist, album)
}

/// Fetch every available release (pressing/edition) in a release group.
pub fn get_group_releases(
    group: &ReleaseGroupCandidate,
    limit: usize,
) -> Result<Vec<ReleaseCandidate>> {
    let url = format!("{MB_BASE}/release/");
    let response: BrowseReleasesResponse = get_json(
        &url,
        &[
            ("release-group", group.mbid.clone()),
            ("inc", "artist-credits+media".to_string()),
            ("fmt", "json".to_string()),
            ("limit", limit.clamp(1, 100).to_string()),
        ],
    )?;

    Ok(candidates_from_group(response, group))
}

fn candidates_from_group(
    response: BrowseReleasesResponse,
    group: &ReleaseGroupCandidate,
) -> Vec<ReleaseCandidate> {
    let releases = response
        .releases
        .into_iter()
        .map(|release| {
            let track_count: u32 = release.media.iter().map(|m| m.track_count).sum();
            let formats = release
                .media
                .iter()
                .filter_map(|medium| medium.format.clone())
                .collect();
            ReleaseCandidate {
                mbid: release.id,
                title: release.title,
                artist: join_artist_credit(&release.artist_credit),
                date: release.date.unwrap_or_default(),
                country: release.country.unwrap_or_default(),
                status: release.status.unwrap_or_default(),
                disambiguation: release.disambiguation.unwrap_or_default(),
                track_count,
                primary_type: group.primary_type.clone(),
                secondary_types: group.secondary_types.clone(),
                formats,
                score: 0,
            }
        })
        .collect();
    rank_group_releases(releases, group)
}

fn rank_groups(
    mut groups: Vec<ReleaseGroupCandidate>,
    artist: &str,
    album: &str,
) -> Vec<ReleaseGroupCandidate> {
    for group in &mut groups {
        group.score = score_group(group, artist, album);
    }
    groups.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| b.search_score.cmp(&a.search_score))
            .then_with(|| type_order(&a.primary_type).cmp(&type_order(&b.primary_type)))
            .then_with(|| a.first_release_date.cmp(&b.first_release_date))
            .then_with(|| a.title.cmp(&b.title))
    });
    groups
}

fn type_order(primary_type: &str) -> u8 {
    match primary_type.to_ascii_lowercase().as_str() {
        "album" => 0,
        "ep" => 1,
        "single" => 2,
        "broadcast" => 3,
        "" => 4,
        _ => 5,
    }
}

/// Lower score is more desirable. Release groups are the album/EP/single
/// entries shown before the individual pressings.
fn score_group(group: &ReleaseGroupCandidate, query_artist: &str, query_album: &str) -> i32 {
    let mut score = 0.0f64;
    let album_query = normalize_text(query_album);
    let artist_query = normalize_text(query_artist);
    let title = normalize_text(&group.title);
    let artist = normalize_text(&group.artist);

    if !album_query.is_empty() && !title.is_empty() {
        if title == album_query {
            score -= 60.0;
        } else if title.contains(&album_query) || album_query.contains(&title) {
            score -= 25.0;
        } else {
            let ratio = normalized_levenshtein(&album_query, &title);
            score += (1.0 - ratio) * 200.0;
            if ratio < 0.5 {
                score += 40.0;
            }
        }
    }

    if !artist_query.is_empty() && !artist.is_empty() {
        if artist == artist_query {
            score -= 15.0;
        } else if artist.contains(&artist_query) || artist_query.contains(&artist) {
            score -= 8.0;
        } else {
            let ratio = normalized_levenshtein(&artist_query, &artist);
            score += (1.0 - ratio) * 100.0;
            if ratio < 0.5 {
                score += 30.0;
            }
        }
    }

    // Keep studio albums first, then EPs and singles. Exact title/artist
    // matches still dominate these small type preferences.
    score += match group.primary_type.to_ascii_lowercase().as_str() {
        "album" => 0.0,
        "ep" => 8.0,
        "single" => 16.0,
        "broadcast" => 20.0,
        "" => 6.0,
        _ => 25.0,
    };

    let asked_live = contains_word(&album_query, "live");
    let asked_compilation = album_query.contains("compilation")
        || album_query.contains("greatest")
        || album_query.contains("best of");
    for secondary in &group.secondary_types {
        let lower = secondary.to_ascii_lowercase();
        if lower.contains("live") && !asked_live {
            score += 10.0;
        } else if lower.contains("compilation") && !asked_compilation {
            score += 6.0;
        } else if lower.contains("soundtrack") {
            score += 4.0;
        } else if lower.contains("remix") {
            score += 8.0;
        }
    }

    if group.first_release_date.is_empty() {
        score += 2.0;
    }

    score.round() as i32
}

/// Rank the pressings inside one release group. Remastered releases come
/// first; within that group the existing edition/format heuristics apply.
fn rank_group_releases(
    mut releases: Vec<ReleaseCandidate>,
    group: &ReleaseGroupCandidate,
) -> Vec<ReleaseCandidate> {
    for release in &mut releases {
        release.score = score_release(release, &group.artist, &group.title);
    }
    releases.sort_by(|a, b| {
        preferred_remaster(b)
            .cmp(&preferred_remaster(a))
            .then_with(|| a.score.cmp(&b.score))
            .then_with(|| a.track_count.cmp(&b.track_count))
            .then_with(|| a.date.cmp(&b.date))
            .then_with(|| a.mbid.cmp(&b.mbid))
    });
    releases
}

fn preferred_remaster(release: &ReleaseCandidate) -> bool {
    release.is_remastered()
        && !matches!(
            release.status.to_ascii_lowercase().as_str(),
            "bootleg" | "promotion" | "pseudo-release" | "withdrawn" | "cancelled"
        )
}

pub fn get_release(mbid: &str) -> Result<ReleaseInfo> {
    let url = format!("{MB_BASE}/release/{mbid}");
    let detail: DetailRelease = get_json(
        &url,
        &[
            (
                "inc",
                "recordings+artist-credits+release-groups".to_string(),
            ),
            ("fmt", "json".to_string()),
        ],
    )?;
    release_info_from_detail(detail)
}

/// Fetch an album cover, trying the iTunes Search API first and the Cover Art
/// Archive second.
///
/// `Ok(None)` means neither service had a usable cover; hard failures are
/// returned so the caller can log them without failing the album download.
pub fn fetch_cover(artist: &str, album: &str, release_mbid: &str) -> Result<Option<CoverArt>> {
    let mut problems = Vec::new();

    match fetch_itunes_cover(artist, album) {
        Ok(Some(data)) => {
            return Ok(Some(CoverArt {
                data,
                source: CoverSource::Itunes,
            }));
        }
        Ok(None) => {}
        Err(err) => problems.push(format!("iTunes: {err}")),
    }

    match fetch_cover_art_archive(release_mbid) {
        Ok(Some(data)) => {
            return Ok(Some(CoverArt {
                data,
                source: CoverSource::CoverArtArchive,
            }));
        }
        Ok(None) => {}
        Err(err) => problems.push(format!("Cover Art Archive: {err}")),
    }

    if problems.is_empty() {
        Ok(None)
    } else {
        Err(anyhow!(problems.join("; ")))
    }
}

fn fetch_itunes_cover(artist: &str, album: &str) -> Result<Option<Vec<u8>>> {
    let album_term = strip_release_noise(album);
    if album_term.is_empty() {
        return Ok(None);
    }

    let mut search_error = None;
    match itunes_search(&album_term, "album", Some("albumTerm")) {
        Ok(results) => {
            if let Some(found) = best_itunes_match(&results, artist, album) {
                return fetch_itunes_artwork(found);
            }
        }
        Err(err) => search_error = Some(err),
    }

    // Apple's album index does not contain every album (older catalogue
    // releases are a common gap), but song results still carry the album name
    // and artwork. Retry as a song search before falling back to CAA.
    let song_term = format!("{} {}", artist.trim(), album_term)
        .trim()
        .to_string();
    if !song_term.is_empty() {
        match itunes_search(&song_term, "song", None) {
            Ok(results) => {
                if let Some(found) = best_itunes_match(&results, artist, album) {
                    return fetch_itunes_artwork(found);
                }
            }
            Err(err) => {
                if search_error.is_none() {
                    search_error = Some(err);
                }
            }
        }
    }

    match search_error {
        Some(err) => Err(err),
        None => Ok(None),
    }
}

fn itunes_search(term: &str, entity: &str, attribute: Option<&str>) -> Result<Vec<ItunesAlbum>> {
    let mut query = vec![
        ("term", term),
        ("media", "music"),
        ("entity", entity),
        ("limit", "50"),
        ("country", ITUNES_COUNTRY),
    ];
    if let Some(attribute) = attribute {
        query.push(("attribute", attribute));
    }

    let mut response = http()
        .get(ITUNES_SEARCH)
        .query_pairs(query)
        .call()
        .map_err(|err| anyhow!("search failed: {err}"))?;
    let body = response
        .body_mut()
        .read_to_vec()
        .context("could not read the iTunes search response")?;
    let parsed: ItunesSearchResponse =
        serde_json::from_slice(&body).context("could not parse the iTunes search response")?;
    Ok(parsed.results)
}

fn fetch_itunes_artwork(found: &ItunesAlbum) -> Result<Option<Vec<u8>>> {
    let Some(url) = artwork_url(found) else {
        return Ok(None);
    };
    fetch_image(&upgrade_itunes_artwork(url))
}

fn fetch_cover_art_archive(release_mbid: &str) -> Result<Option<Vec<u8>>> {
    if release_mbid.trim().is_empty() {
        return Ok(None);
    }
    let url = format!("{CAA_BASE}/release/{release_mbid}/front-500");
    fetch_image(&url)
}

fn fetch_image(url: &str) -> Result<Option<Vec<u8>>> {
    let mut response = match http()
        .get(url)
        .header("Accept", "image/jpeg, image/png, image/gif")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(err) => return Err(anyhow!("image request failed: {err}")),
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.is_empty() && !content_type.starts_with("image/") {
        return Err(anyhow!(
            "cover server returned {content_type}, not an image"
        ));
    }
    let data = response
        .body_mut()
        .read_to_vec()
        .context("could not read the cover image")?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(data))
}

fn best_itunes_match<'a>(
    results: &'a [ItunesAlbum],
    artist: &str,
    album: &str,
) -> Option<&'a ItunesAlbum> {
    let wanted_artist = normalize_text(artist);
    let wanted_album = normalize_text(strip_itunes_suffix(album));
    let wanted_key = title_key(album);
    if wanted_artist.is_empty() || wanted_key.is_empty() {
        return None;
    }

    results
        .iter()
        .filter(|result| artwork_url(result).is_some())
        .filter_map(|result| {
            let artist_name = normalize_text(&result.artist_name);
            let album_display = strip_itunes_suffix(&result.collection_name);
            let album_name = normalize_text(album_display);
            let album_key = title_key(&result.collection_name);
            if artist_name.is_empty() || album_key.is_empty() {
                return None;
            }
            let artist_score = normalized_levenshtein(&wanted_artist, &artist_name);
            let album_score = normalized_levenshtein(&wanted_key, &album_key);
            let exact_album = album_name == wanted_album;
            let base_album = album_key == wanted_key;
            if artist_score < 0.7 || !(exact_album || base_album || album_score >= 0.85) {
                return None;
            }
            let mut score = artist_score + album_score;
            if exact_album {
                score += 1.5;
            } else if base_album {
                // Same album, but iTunes added an edition suffix.
                score += 0.75;
            }
            Some((score, result))
        })
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|(_, result)| result)
}

fn artwork_url(album: &ItunesAlbum) -> Option<&str> {
    album
        .artwork_url_100
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            album
                .artwork_url_60
                .as_deref()
                .filter(|url| !url.trim().is_empty())
        })
}

fn upgrade_itunes_artwork(url: &str) -> String {
    for size in ["100x100bb", "60x60bb"] {
        if url.contains(size) {
            return url.replace(size, ITUNES_ARTWORK_SIZE);
        }
    }
    url.to_string()
}

/// iTunes appends the storefront release type to collection names, e.g.
/// `Ep II - EP` or `Song - Single`. Strip it before comparing titles.
fn strip_itunes_suffix(title: &str) -> &str {
    let trimmed = title.trim();
    if let Some((base, suffix)) = trimmed.rsplit_once(" - ")
        && matches!(
            suffix.trim().to_ascii_lowercase().as_str(),
            "ep" | "single" | "album"
        )
        && !base.trim().is_empty()
    {
        return base.trim_end();
    }
    trimmed
}

fn title_key(title: &str) -> String {
    normalize_text(&strip_release_noise(strip_itunes_suffix(title)))
}

fn release_info_from_detail(detail: DetailRelease) -> Result<ReleaseInfo> {
    let DetailRelease {
        id,
        title,
        date,
        country,
        status,
        disambiguation,
        artist_credit,
        release_group,
        mut media,
    } = detail;
    let release_artist = join_artist_credit(&artist_credit);
    media.sort_by_key(|m| m.position.unwrap_or(u32::MAX));

    let mut tracks = Vec::new();
    let mut audio_disc = 0u32;
    for medium in media {
        if medium.tracks.is_empty() || is_video_medium(&medium) {
            continue;
        }
        audio_disc += 1;
        let mut medium_tracks = medium.tracks;
        medium_tracks.sort_by_key(|t| t.position.unwrap_or(u32::MAX));
        for (fallback_pos, track) in medium_tracks.into_iter().enumerate() {
            let DetailTrack {
                id: track_mbid,
                position,
                title: track_title,
                length: track_length,
                artist_credit: track_credit,
                recording,
            } = track;
            let recording = recording.unwrap_or(DetailRecording {
                id: None,
                title: None,
                length: None,
                artist_credit: Vec::new(),
                video: false,
            });
            if recording.video {
                continue;
            }
            let DetailRecording {
                id: recording_mbid,
                title: recording_title,
                length: recording_length,
                artist_credit: recording_credit,
                video: _,
            } = recording;
            let position = position.unwrap_or((fallback_pos + 1) as u32);
            let title = recording_title
                .or(track_title)
                .unwrap_or_else(|| format!("Track {position}"));
            let track_artist = join_artist_credit(&track_credit);
            let recording_artist = join_artist_credit(&recording_credit);
            let artist = if !track_artist.is_empty() {
                track_artist
            } else if !recording_artist.is_empty() {
                recording_artist
            } else {
                release_artist.clone()
            };
            tracks.push(TrackInfo {
                index: 0,
                disc: audio_disc,
                position,
                title,
                artist,
                length_ms: track_length.or(recording_length),
                recording_mbid: recording_mbid.unwrap_or_default(),
                track_mbid: track_mbid.unwrap_or_default(),
            });
        }
    }

    tracks.sort_by_key(|t| (t.disc, t.position));
    for (index, track) in tracks.iter_mut().enumerate() {
        track.index = index;
    }

    if tracks.is_empty() {
        return Err(anyhow!(
            "this MusicBrainz release has no audio tracks (it may be a video release)"
        ));
    }

    let (release_group_mbid, primary_type) = release_group
        .map(|group| {
            (
                group.id.unwrap_or_default(),
                group.primary_type.unwrap_or_default(),
            )
        })
        .unwrap_or_default();

    Ok(ReleaseInfo {
        mbid: id,
        release_group_mbid,
        title,
        artist: release_artist,
        date: date.unwrap_or_default(),
        country: country.unwrap_or_default(),
        status: status.unwrap_or_default(),
        disambiguation: disambiguation.unwrap_or_default(),
        primary_type,
        tracks,
    })
}

pub fn build_group_query(artist: &str, album: &str) -> String {
    let artist = escape_lucene(artist);
    let album = escape_lucene(album);
    match (artist.trim().is_empty(), album.trim().is_empty()) {
        (false, false) => format!("releasegroup:\"{album}\" AND artist:\"{artist}\""),
        (true, false) => format!("releasegroup:\"{album}\""),
        (false, true) => format!("artist:\"{artist}\""),
        (true, true) => String::new(),
    }
}

fn escape_lucene(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn join_artist_credit(credits: &[ArtistCredit]) -> String {
    let mut out = String::new();
    for credit in credits {
        out.push_str(&credit.name);
        out.push_str(&credit.joinphrase);
    }
    out.trim().to_string()
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| word == needle)
}

fn is_video_format(format: &str) -> bool {
    let format = format.to_ascii_lowercase();
    [
        "dvd",
        "blu-ray",
        "bluray",
        "video",
        "vhs",
        "laserdisc",
        "hd-dvd",
    ]
    .iter()
    .any(|needle| format.contains(needle))
}

fn is_video_medium(medium: &DetailMedium) -> bool {
    if medium.format.as_deref().is_some_and(is_video_format) {
        return true;
    }
    medium.title.as_deref().is_some_and(is_video_format)
}

/// Lower score is more desirable.
fn score_release(candidate: &ReleaseCandidate, query_artist: &str, query_album: &str) -> i32 {
    let mut score = 0.0f64;
    let album_query = normalize_text(query_album);
    let artist_query = normalize_text(query_artist);
    let title = normalize_text(&candidate.title);
    let artist = normalize_text(&candidate.artist);

    if !album_query.is_empty() && !title.is_empty() {
        if title == album_query {
            score -= 60.0;
        } else if title.contains(&album_query) || album_query.contains(&title) {
            score -= 25.0;
        } else {
            let ratio = normalized_levenshtein(&album_query, &title);
            score += (1.0 - ratio) * 200.0;
            if ratio < 0.5 {
                score += 40.0;
            }
        }
    }

    if !artist_query.is_empty() && !artist.is_empty() {
        if artist == artist_query {
            score -= 15.0;
        } else if artist.contains(&artist_query) || artist_query.contains(&artist) {
            score -= 8.0;
        } else {
            let ratio = normalized_levenshtein(&artist_query, &artist);
            score += (1.0 - ratio) * 100.0;
            if ratio < 0.5 {
                score += 30.0;
            }
        }
    }

    // Deluxe/live/expanded editions balloon the track count. The track count
    // term alone already separates them, but the keyword penalties make the
    // intent explicit and keep a live album from outranking the studio one.
    score += candidate.track_count as f64 * 2.0;

    let blob = format!(
        "{} {} {}",
        candidate.title,
        candidate.disambiguation,
        candidate.secondary_types.join(" ")
    )
    .to_ascii_lowercase();
    let asked_live = contains_word(&album_query, "live");
    let asked_deluxe = album_query.contains("deluxe") || album_query.contains("expanded");
    let asked_remaster = album_query.contains("remaster");
    let penalties: &[(&str, f64)] = &[
        ("super deluxe", 45.0),
        ("deluxe", 35.0),
        ("expanded", 25.0),
        ("anniversary", 25.0),
        ("bonus", 20.0),
        ("special edition", 20.0),
        ("collector", 20.0),
        ("live", 20.0),
        ("remaster", 8.0),
        ("reissue", 8.0),
        ("demo", 15.0),
        ("karaoke", 40.0),
        ("instrumental", 15.0),
        ("tribute", 40.0),
    ];
    for (keyword, penalty) in penalties {
        let matched = if *keyword == "live" {
            contains_word(&blob, "live")
        } else if *keyword == "demo" {
            contains_word(&blob, "demo") || contains_word(&blob, "demos")
        } else {
            blob.contains(keyword)
        };
        if !matched {
            continue;
        }
        if (*keyword == "live" && asked_live)
            || ((*keyword == "deluxe" || *keyword == "expanded") && asked_deluxe)
            || (*keyword == "remaster" && asked_remaster)
        {
            continue;
        }
        score += *penalty;
    }

    for secondary in &candidate.secondary_types {
        let lower = secondary.to_ascii_lowercase();
        if lower.contains("live") && !asked_live {
            score += 20.0;
        } else if lower.contains("compilation") || lower.contains("remix") {
            score += 15.0;
        } else if lower.contains("soundtrack")
            && !candidate.primary_type.eq_ignore_ascii_case("album")
        {
            score += 5.0;
        }
    }

    match candidate.status.to_ascii_lowercase().as_str() {
        "official" => score -= 15.0,
        "bootleg" => score += 50.0,
        "promotion" | "pseudo-release" => score += 30.0,
        "withdrawn" | "cancelled" => score += 40.0,
        _ => {}
    }

    match candidate.primary_type.to_ascii_lowercase().as_str() {
        "album" => score -= 10.0,
        "single" | "ep" => score += 50.0,
        "" => {}
        _ => score += 35.0,
    }

    if candidate.date.is_empty() {
        score += 3.0;
    }
    if candidate.country.is_empty() {
        score += 2.0;
    }
    let physical_formats: Vec<&String> = candidate
        .formats
        .iter()
        .filter(|format| !is_video_format(format))
        .collect();
    if !physical_formats.is_empty()
        && physical_formats
            .iter()
            .all(|format| format.to_ascii_lowercase().contains("digital"))
    {
        score += 4.0;
    } else if !physical_formats.is_empty()
        && physical_formats
            .iter()
            .all(|format| format.to_ascii_lowercase().contains("vinyl"))
    {
        score += 2.0;
    } else if !physical_formats.is_empty()
        && physical_formats
            .iter()
            .any(|format| format.to_ascii_lowercase().contains("cd"))
    {
        score -= 1.0;
    }

    if let Ok(year) = candidate.year().parse::<i32>() {
        score += ((year - 1960).max(0) as f64 * 0.05).min(6.0);
    }

    score.round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(title: &str, track_count: u32) -> ReleaseCandidate {
        ReleaseCandidate {
            mbid: "id".into(),
            title: title.into(),
            artist: "Metallica".into(),
            date: "1984-07-27".into(),
            country: "US".into(),
            status: "Official".into(),
            disambiguation: String::new(),
            track_count,
            primary_type: "Album".into(),
            secondary_types: Vec::new(),
            formats: vec!["CD".into()],
            score: 0,
        }
    }

    fn group_candidate(title: &str, primary_type: &str, date: &str) -> ReleaseGroupCandidate {
        ReleaseGroupCandidate {
            mbid: format!("{title}-{primary_type}"),
            title: title.into(),
            artist: "Metallica".into(),
            first_release_date: date.into(),
            primary_type: primary_type.into(),
            secondary_types: Vec::new(),
            disambiguation: String::new(),
            release_count: 1,
            search_score: 100,
            score: 0,
        }
    }

    #[test]
    fn transient_errors_are_retried_three_times() {
        assert!(should_retry(true, 0));
        assert!(should_retry(true, 1));
        assert!(should_retry(true, 2));
        assert!(!should_retry(true, MAX_RETRIES));
        assert!(!should_retry(false, 0));
        assert_eq!(retry_delay(0), Duration::from_secs(2));
        assert_eq!(retry_delay(1), Duration::from_secs(4));
        assert_eq!(retry_delay(2), Duration::from_secs(6));
    }

    #[test]
    fn a_standard_album_outscores_a_deluxe_edition() {
        let mut standard = candidate("Ride the Lightning", 8);
        let mut deluxe = candidate("Ride the Lightning", 16);
        deluxe.disambiguation = "deluxe edition".into();
        standard.score = score_release(&standard, "Metallica", "Ride the Lightning");
        deluxe.score = score_release(&deluxe, "Metallica", "Ride the Lightning");
        assert!(standard.score < deluxe.score);
    }

    #[test]
    fn an_explicitly_requested_live_album_is_not_penalised_twice() {
        let mut live = candidate("Live at Wembley", 12);
        live.secondary_types.push("Live".into());
        let score = score_release(&live, "Band", "Live at Wembley");
        let mut studio = candidate("Studio", 12);
        studio.title = "Studio".into();
        let studio_score = score_release(&studio, "Band", "Live at Wembley");
        assert!(score < studio_score + 100);
    }

    #[test]
    fn keyword_matching_respects_word_boundaries() {
        assert!(!contains_word("deliverance", "live"));
        assert!(contains_word("live at wembley", "live"));
        let mut c = candidate("Deliverance", 10);
        c.secondary_types.clear();
        assert!(!c.edition_tags().iter().any(|tag| tag == "Live"));
    }

    #[test]
    fn build_group_query_uses_releasegroup_field() {
        assert_eq!(
            build_group_query("AC\"DC", "Back\\Black"),
            "releasegroup:\"Back\\\\Black\" AND artist:\"AC\\\"DC\""
        );
    }

    #[test]
    fn release_group_search_json_fields_are_parsed() {
        let json = r#"{
          "release-groups": [{
            "id": "group-1",
            "title": "Jar of Flies",
            "score": 100,
            "primary-type": "EP",
            "secondary-types": [],
            "first-release-date": "1994-01-18",
            "count": 9,
            "artist-credit": [{"name": "Alice in Chains", "joinphrase": ""}]
          }]
        }"#;
        let response: GroupSearchResponse = serde_json::from_str(json).unwrap();
        let groups = groups_from_search(response, "Alice in Chains", "Jar of Flies");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].mbid, "group-1");
        assert_eq!(groups[0].artist, "Alice in Chains");
        assert_eq!(groups[0].release_count, 9);
        assert_eq!(groups[0].year(), "1994");
        assert_eq!(groups[0].type_tags(), vec!["EP".to_string()]);
        assert!(groups[0].score < 0);
    }

    #[test]
    fn albums_rank_before_eps_and_singles_for_the_same_title() {
        let album = group_candidate("Ride the Lightning", "Album", "1984");
        let ep = group_candidate("Ride the Lightning", "EP", "1984");
        let single = group_candidate("Ride the Lightning", "Single", "1984");
        let ranked = rank_groups(vec![single, ep, album], "Metallica", "Ride the Lightning");
        assert_eq!(ranked[0].primary_type, "Album");
        assert_eq!(ranked[1].primary_type, "EP");
        assert_eq!(ranked[2].primary_type, "Single");
    }

    #[test]
    fn bootleg_remasters_do_not_outrank_official_originals() {
        let group = group_candidate("Ride the Lightning", "Album", "1984");
        let mut original = candidate("Ride the Lightning", 8);
        original.mbid = "original".into();
        let mut bootleg = candidate("Ride the Lightning", 8);
        bootleg.mbid = "bootleg".into();
        bootleg.status = "Bootleg".into();
        bootleg.disambiguation = "remastered".into();

        let ranked = rank_group_releases(vec![bootleg, original], &group);
        assert_eq!(ranked[0].mbid, "original");
    }

    #[test]
    fn group_browse_json_prefers_remastered_pressings() {
        let json = r#"{
          "releases": [
            {
              "id": "original",
              "title": "Ride the Lightning",
              "date": "1984-07-27",
              "country": "US",
              "status": "Official",
              "artist-credit": [{"name": "Metallica", "joinphrase": ""}],
              "media": [{"format": "CD", "track-count": 8}]
            },
            {
              "id": "remaster",
              "title": "Ride the Lightning",
              "date": "2016-01-01",
              "country": "US",
              "status": "Official",
              "disambiguation": "2016 remaster",
              "artist-credit": [{"name": "Metallica", "joinphrase": ""}],
              "media": [{"format": "CD", "track-count": 8}]
            }
          ]
        }"#;
        let response: BrowseReleasesResponse = serde_json::from_str(json).unwrap();
        let group = group_candidate("Ride the Lightning", "Album", "1984");
        let releases = candidates_from_group(response, &group);
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].mbid, "remaster");
        assert!(releases[0].is_remastered());
        assert_eq!(releases[0].track_count, 8);
    }

    #[test]
    fn format_summary_counts_and_ignores_video() {
        let mut c = candidate("Album", 10);
        c.formats = vec!["CD".into(), "CD".into(), "DVD".into()];
        assert_eq!(c.format_summary(), "2×CD");
    }

    #[test]
    fn release_detail_parsing_skips_video_and_numbers_discs() {
        let json = r#"{
          "id": "release-id",
          "title": "Album",
          "date": "2001-02-03",
          "country": "GB",
          "status": "Official",
          "artist-credit": [{"name": "Album Artist", "joinphrase": ""}],
          "release-group": {"id": "group-id", "primary-type": "Album", "secondary-types": []},
          "media": [
            {"position": 1, "format": "CD", "tracks": [
              {"id": "track-1", "position": 1, "length": 123000, "artist-credit": [{"name": "Track Artist", "joinphrase": ""}], "recording": {"id": "rec-1", "title": "One", "length": 120000}}
            ]},
            {"position": 2, "format": "DVD", "tracks": [
              {"position": 1, "recording": {"id": "vid", "title": "Video", "video": true}}
            ]},
            {"position": 3, "format": "CD", "tracks": [
              {"id": "track-2", "position": 1, "length": 60000, "recording": {"id": "rec-2", "title": "Two"}}
            ]}
          ]
        }"#;
        let detail: DetailRelease = serde_json::from_str(json).unwrap();
        let release = release_info_from_detail(detail).unwrap();
        assert_eq!(release.artist, "Album Artist");
        assert_eq!(release.release_group_mbid, "group-id");
        assert_eq!(release.primary_type, "Album");
        assert!(!release.is_single());
        assert_eq!(release.tracks.len(), 2);
        assert_eq!(release.tracks[0].disc, 1);
        assert_eq!(release.tracks[0].position, 1);
        assert_eq!(release.tracks[0].title, "One");
        assert_eq!(release.tracks[0].artist, "Track Artist");
        assert_eq!(release.tracks[0].length_ms, Some(123000));
        assert_eq!(release.tracks[0].recording_mbid, "rec-1");
        assert_eq!(release.tracks[0].track_mbid, "track-1");
        assert_eq!(release.tracks[1].disc, 2);
        assert_eq!(release.tracks[1].title, "Two");
        assert_eq!(release.tracks[1].artist, "Album Artist");
        assert_eq!(release.tracks[1].track_mbid, "track-2");
        assert_eq!(release.tracks[1].index, 1);
    }

    fn itunes_album(artist: &str, album: &str, artwork: &str) -> ItunesAlbum {
        ItunesAlbum {
            collection_name: album.into(),
            artist_name: artist.into(),
            artwork_url_100: Some(artwork.into()),
            artwork_url_60: None,
        }
    }

    #[test]
    fn itunes_matching_prefers_the_right_artist_and_plain_album() {
        let results = vec![
            itunes_album(
                "Some Other Band",
                "Dirt",
                "https://example.com/100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Dirt (Remastered)",
                "https://example.com/remaster-100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Dirt",
                "https://example.com/dirt-100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Dirty",
                "https://example.com/dirty-100x100bb.jpg",
            ),
        ];
        let found = best_itunes_match(&results, "Alice in Chains", "Dirt").unwrap();
        assert_eq!(found.collection_name, "Dirt");
        assert_eq!(
            found.artwork_url_100.as_deref(),
            Some("https://example.com/dirt-100x100bb.jpg")
        );
    }

    #[test]
    fn itunes_matching_rejects_unrelated_albums() {
        let results = vec![
            itunes_album(
                "Other Artist",
                "Dirt",
                "https://example.com/a-100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Facelift",
                "https://example.com/b-100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Dirty",
                "https://example.com/c-100x100bb.jpg",
            ),
        ];
        assert!(best_itunes_match(&results, "Alice in Chains", "Dirt").is_none());
    }

    #[test]
    fn itunes_matching_ignores_storefront_release_type_suffix() {
        let results = vec![
            itunes_album(
                "Florence Black",
                "Ep II - EP",
                "https://example.com/ep-100x100bb.jpg",
            ),
            itunes_album(
                "Florence Black",
                "Smoke - Single",
                "https://example.com/single-100x100bb.jpg",
            ),
        ];
        let found = best_itunes_match(&results, "Florence Black", "EP II").unwrap();
        assert_eq!(found.collection_name, "Ep II - EP");
        let found = best_itunes_match(&results, "Florence Black", "Smoke").unwrap();
        assert_eq!(found.collection_name, "Smoke - Single");
    }

    #[test]
    fn itunes_matching_keeps_an_explicit_live_edition() {
        let results = vec![
            itunes_album(
                "Alice in Chains",
                "Dirt",
                "https://example.com/studio-100x100bb.jpg",
            ),
            itunes_album(
                "Alice in Chains",
                "Dirt (Live)",
                "https://example.com/live-100x100bb.jpg",
            ),
        ];
        let found = best_itunes_match(&results, "Alice in Chains", "Dirt (Live)").unwrap();
        assert_eq!(found.collection_name, "Dirt (Live)");
    }

    #[test]
    fn itunes_artwork_urls_are_upgraded() {
        assert_eq!(
            upgrade_itunes_artwork("https://example.com/art/100x100bb.jpg"),
            "https://example.com/art/600x600bb.jpg"
        );
        assert_eq!(
            upgrade_itunes_artwork("https://example.com/art/60x60bb.png"),
            "https://example.com/art/600x600bb.png"
        );
        assert_eq!(
            upgrade_itunes_artwork("https://example.com/art/original.jpg"),
            "https://example.com/art/original.jpg"
        );
    }

    #[test]
    fn itunes_search_response_is_parsed() {
        let json = r#"{
          "resultCount": 1,
          "results": [{
            "collectionName": "Dirt",
            "artistName": "Alice in Chains",
            "artworkUrl100": "https://example.com/100x100bb.jpg"
          }]
        }"#;
        let response: ItunesSearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].collection_name, "Dirt");
        assert_eq!(response.results[0].artist_name, "Alice in Chains");
    }
}
