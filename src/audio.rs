//! Reading duration and writing clean track metadata with `lofty`.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::ogg::OpusFile;
use lofty::picture::{Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagType};

pub fn duration_secs(path: &Path) -> Result<f64> {
    let tagged = lofty::read_from_path(path)
        .with_context(|| format!("could not read audio file {}", path.display()))?;
    ensure_supported(path, tagged.file_type())?;
    let duration = tagged.properties().duration().as_secs_f64();
    if duration <= 0.0 {
        return Err(anyhow!("audio file reports a zero duration"));
    }
    Ok(duration)
}

/// Audio bitrate in kbps for a supported file.
pub fn bitrate_kbps(path: &Path) -> Result<Option<u32>> {
    let tagged = lofty::read_from_path(path)
        .with_context(|| format!("could not read audio file {}", path.display()))?;
    ensure_supported(path, tagged.file_type())?;
    Ok(tagged.properties().audio_bitrate())
}

fn ensure_supported(path: &Path, file_type: FileType) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let is_flac = extension.eq_ignore_ascii_case("flac");
    let is_mp3 = extension.eq_ignore_ascii_case("mp3");
    let is_opus = extension.eq_ignore_ascii_case("opus");
    match file_type {
        FileType::Flac if is_flac => Ok(()),
        FileType::Mpeg if is_mp3 => Ok(()),
        FileType::Opus if is_opus => Ok(()),
        FileType::Flac => Err(anyhow!("FLAC file does not use a .flac extension")),
        FileType::Mpeg => Err(anyhow!("MP3 file does not use a .mp3 extension")),
        FileType::Opus => Err(anyhow!("Opus file does not use a .opus extension")),
        _ => Err(anyhow!("not a FLAC, MP3 or Opus file")),
    }
}

/// Loudness values written as tags, for players to apply on playback. Gains
/// are in dB relative to the loudness target the album was measured against;
/// peaks are linear (1.0 is full scale). Missing values are left out.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ReplayGain {
    pub track_gain_db: Option<f64>,
    pub track_peak: Option<f64>,
    pub album_gain_db: Option<f64>,
    pub album_peak: Option<f64>,
}

/// ReplayGain 2.0 references -18 LUFS; the Opus `R128_*_GAIN` tags reference
/// -23 LUFS. Players that read both (Rockbox, foobar2000, ...) add this to an
/// R128 gain to make it comparable to a ReplayGain one.
const R128_TO_REPLAYGAIN_DB: f64 = 5.0;

const REPLAYGAIN_KEYS: [ItemKey; 4] = [
    ItemKey::ReplayGainTrackGain,
    ItemKey::ReplayGainTrackPeak,
    ItemKey::ReplayGainAlbumGain,
    ItemKey::ReplayGainAlbumPeak,
];

fn format_gain(gain_db: f64) -> String {
    format!("{gain_db:.2} dB")
}

fn format_peak(peak: f64) -> String {
    format!("{peak:.6}")
}

/// An Opus `R128_*_GAIN` value: a Q7.8 fixed-point integer in dB.
///
/// `gain_db` is the gain that brings the audio to the loudness target (as for
/// ReplayGain). The R128 tags reference -23 LUFS and players add
/// [`R128_TO_REPLAYGAIN_DB`], so that offset is taken off here: the player
/// then lands on the target.
fn r128_gain_value(gain_db: f64) -> i16 {
    ((gain_db - R128_TO_REPLAYGAIN_DB) * 256.0)
        .round()
        .clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

pub struct TrackTags<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    /// Original per-track credit, kept when `artist` is forced to the album
    /// artist for car-stereo grouping.
    pub track_artists: Option<&'a str>,
    pub album_artist: &'a str,
    pub album: &'a str,
    pub date: Option<&'a str>,
    pub track: u32,
    pub track_total: u32,
    pub disc: u32,
    pub disc_total: u32,
    pub release_mbid: &'a str,
    pub release_group_mbid: &'a str,
    pub recording_mbid: &'a str,
    pub track_mbid: &'a str,
    pub cover: Option<&'a [u8]>,
    /// ReplayGain tags to write. `None` leaves any existing ones alone;
    /// `Some` replaces them (stale values from the source are dropped).
    pub replay_gain: Option<ReplayGain>,
}

/// Overwrite the source's track metadata with the MusicBrainz values.
///
/// This is best-effort: a download is never failed because tagging failed.
/// When a cover is supplied, every embedded picture is replaced with it so
/// art from a different edition or source cannot survive.
pub fn write_tags(path: &Path, tags: &TrackTags<'_>) -> Result<()> {
    let mut tagged = tagged_file(path)?;
    let tag = editable_tag(&mut tagged)?;

    let artist = non_empty(tags.artist).unwrap_or(tags.album_artist);
    let album_artist = non_empty(tags.album_artist).unwrap_or(artist);

    tag.set_track(tags.track);
    tag.set_track_total(tags.track_total.max(tags.track));
    tag.set_disk(tags.disc.max(1));
    tag.set_disk_total(tags.disc_total.max(tags.disc));
    tag.set_title(tags.title.to_string());
    tag.set_artist(artist.to_string());
    if let Some(track_artists) = tags.track_artists.and_then(non_empty) {
        tag.insert_text(ItemKey::TrackArtists, track_artists.to_string());
    } else {
        tag.remove_key(ItemKey::TrackArtists);
    }
    tag.set_album(tags.album.to_string());
    if let Some(album_artist) = non_empty(album_artist) {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.to_string());
    }
    if let Some(date) = tags.date.and_then(non_empty) {
        tag.insert_text(ItemKey::RecordingDate, date.to_string());
    }
    insert_id(tag, ItemKey::MusicBrainzReleaseId, tags.release_mbid);
    insert_id(
        tag,
        ItemKey::MusicBrainzReleaseGroupId,
        tags.release_group_mbid,
    );
    insert_id(tag, ItemKey::MusicBrainzRecordingId, tags.recording_mbid);
    insert_id(tag, ItemKey::MusicBrainzTrackId, tags.track_mbid);

    if let Some(cover) = tags.cover.and_then(picture_from_bytes) {
        replace_pictures(tag, cover);
    }

    let is_opus = tagged.file_type() == FileType::Opus;
    if let Some(gain) = tags.replay_gain.as_ref()
        && !is_opus
    {
        set_replay_gain(editable_tag(&mut tagged)?, gain);
    }
    save(tagged, path)?;
    if let Some(gain) = tags.replay_gain.as_ref()
        && is_opus
    {
        write_opus_gain(path, gain)?;
    }
    Ok(())
}

/// FLAC (Vorbis comments) and MP3 (ID3v2 TXXX frames) carry the standard
/// `REPLAYGAIN_*` fields; lofty maps them to the right frame per format.
fn set_replay_gain(tag: &mut Tag, gain: &ReplayGain) {
    for key in REPLAYGAIN_KEYS {
        tag.remove_key(key);
    }
    let values = [
        gain.track_gain_db.map(format_gain),
        gain.track_peak.map(format_peak),
        gain.album_gain_db.map(format_gain),
        gain.album_peak.map(format_peak),
    ];
    for (key, value) in REPLAYGAIN_KEYS.into_iter().zip(values) {
        if let Some(value) = value {
            tag.insert_text(key, value);
        }
    }
}

/// Opus stores loudness as `R128_TRACK_GAIN` / `R128_ALBUM_GAIN` (RFC 7845);
/// `REPLAYGAIN_*_GAIN` must not be used there. The peaks are not part of the
/// spec but harmless, and Rockbox uses them to prevent clipping.
///
/// lofty's generic tag has no field for the R128 names, so this edits the
/// file's Vorbis comments directly.
fn write_opus_gain(path: &Path, gain: &ReplayGain) -> Result<()> {
    let mut file = File::options()
        .read(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let mut opus = OpusFile::read_from(&mut file, ParseOptions::new())
        .with_context(|| format!("could not read {}", path.display()))?;
    drop(file);
    let comments = opus.vorbis_comments_mut();
    for name in [
        "R128_TRACK_GAIN",
        "R128_ALBUM_GAIN",
        "REPLAYGAIN_TRACK_GAIN",
        "REPLAYGAIN_ALBUM_GAIN",
        "REPLAYGAIN_TRACK_PEAK",
        "REPLAYGAIN_ALBUM_PEAK",
    ] {
        comments.remove(name).for_each(drop);
    }
    let entries = [
        (
            "R128_TRACK_GAIN",
            gain.track_gain_db.map(|db| r128_gain_value(db).to_string()),
        ),
        (
            "R128_ALBUM_GAIN",
            gain.album_gain_db.map(|db| r128_gain_value(db).to_string()),
        ),
        ("REPLAYGAIN_TRACK_PEAK", gain.track_peak.map(format_peak)),
        ("REPLAYGAIN_ALBUM_PEAK", gain.album_peak.map(format_peak)),
    ];
    for (name, value) in entries {
        if let Some(value) = value {
            comments.insert(name.to_string(), value);
        }
    }
    opus.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("could not write loudness tags to {}", path.display()))
}

/// Copy embedded pictures from one audio file to another.
///
/// Normalization or conversion re-encodes audio into a fresh file, so this
/// keeps the source cover until canonical tags (and a better cover, when
/// available) are written.
pub fn copy_pictures(from: &Path, to: &Path) -> Result<()> {
    let source = tagged_file(from)?;
    let Some(source_tag) = source.primary_tag() else {
        return Ok(());
    };
    if source_tag.pictures().is_empty() {
        return Ok(());
    }
    let pictures = source_tag.pictures().to_vec();
    let mut target = tagged_file(to)?;
    let tag = editable_tag(&mut target)?;
    for picture in pictures {
        tag.push_picture(picture);
    }
    save(target, to)
}

/// Add or replace the front cover on an already tagged file.
///
/// Used when the Cover Art Archive request finishes after a track has already
/// been saved. Other tags are left untouched.
pub fn write_cover(path: &Path, cover: &[u8]) -> Result<()> {
    let picture = picture_from_bytes(cover)
        .ok_or_else(|| anyhow!("the cover art is not a supported image"))?;
    let mut tagged = tagged_file(path)?;
    let tag = editable_tag(&mut tagged)?;
    replace_pictures(tag, picture);
    save(tagged, path)
}

fn tagged_file(path: &Path) -> Result<TaggedFile> {
    lofty::read_from_path(path)
        .with_context(|| format!("could not read audio file {}", path.display()))
}

fn editable_tag(tagged: &mut TaggedFile) -> Result<&mut Tag> {
    // FLACs from Soulseek sometimes carry a stray ID3v2/APE tag alongside the
    // standard Vorbis comments. Drop those so players cannot keep showing an
    // old album title or cover from the source.
    let primary = tagged.primary_tag_type();
    let stale: Vec<TagType> = tagged
        .tags()
        .iter()
        .map(|tag| tag.tag_type())
        .filter(|tag_type| *tag_type != primary)
        .collect();
    for tag_type in stale {
        tagged.remove(tag_type);
    }
    if tagged.primary_tag_mut().is_none() {
        tagged.insert_tag(Tag::new(primary));
    }
    tagged
        .primary_tag_mut()
        .ok_or_else(|| anyhow!("the file has no writable tag"))
}

fn save(tagged: TaggedFile, path: &Path) -> Result<()> {
    tagged
        .save_to_path(path, WriteOptions::default())
        .with_context(|| format!("could not write tags to {}", path.display()))
}

fn insert_id(tag: &mut Tag, key: ItemKey, value: &str) {
    if let Some(value) = non_empty(value) {
        tag.insert_text(key, value.to_string());
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn replace_pictures(tag: &mut Tag, picture: Picture) {
    for index in (0..tag.pictures().len()).rev() {
        tag.remove_picture(index);
    }
    tag.push_picture(picture);
}

fn picture_from_bytes(data: &[u8]) -> Option<Picture> {
    let mut reader = Cursor::new(data);
    let mut picture = Picture::from_reader(&mut reader).ok()?;
    picture.set_pic_type(PictureType::CoverFront);
    picture.set_description(Some("Front cover".to_string()));
    Some(picture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "soulsucker-audio-{}-{name}.flac",
            std::process::id()
        ))
    }

    fn mp3_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "soulsucker-audio-{}-{name}.mp3",
            std::process::id()
        ))
    }

    #[test]
    fn mp3_duration_and_bitrate_are_read() {
        let path = mp3_test_path("inspect");
        fs::write(&path, include_bytes!("../tests/fixtures/silence.mp3")).unwrap();
        let duration = duration_secs(&path).unwrap();
        assert!((duration - 1.0).abs() < 0.1, "{duration}");
        let bitrate = bitrate_kbps(&path).unwrap().unwrap();
        assert!(bitrate >= 300, "{bitrate}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn musicbrainz_tags_replace_source_metadata_and_cover() {
        let path = test_path("tags");
        fs::write(&path, include_bytes!("../tests/fixtures/silence.flac")).unwrap();
        let stale = TrackTags {
            title: "Wrong Title",
            artist: "Wrong Artist",
            track_artists: Some("Wrong Track Artists"),
            album_artist: "Wrong Album Artist",
            album: "Wrong Album (Remastered)",
            date: Some("1999"),
            track: 9,
            track_total: 9,
            disc: 9,
            disc_total: 9,
            release_mbid: "wrong-release",
            release_group_mbid: "wrong-group",
            recording_mbid: "wrong-recording",
            track_mbid: "wrong-track",
            cover: Some(b"\x89PNG\r\n\x1a\nold-art"),
            replay_gain: None,
        };
        write_tags(&path, &stale).unwrap();

        let cover = b"\x89PNG\r\n\x1a\nfake-image-data";
        let tags = TrackTags {
            title: "Song",
            artist: "Track Artist",
            track_artists: Some("Original Track Credit"),
            album_artist: "Album Artist",
            album: "Album",
            date: Some("2001-02-03"),
            track: 3,
            track_total: 10,
            disc: 2,
            disc_total: 2,
            release_mbid: "release-id",
            release_group_mbid: "group-id",
            recording_mbid: "recording-id",
            track_mbid: "track-id",
            cover: Some(cover),
            replay_gain: None,
        };
        write_tags(&path, &tags).unwrap();

        let tagged = lofty::read_from_path(&path).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.artist().as_deref(), Some("Track Artist"));
        assert_eq!(tag.album().as_deref(), Some("Album"));
        assert_eq!(tag.track(), Some(3));
        assert_eq!(tag.track_total(), Some(10));
        assert_eq!(tag.disk(), Some(2));
        assert_eq!(tag.disk_total(), Some(2));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Album Artist"));
        assert_eq!(
            tag.get_string(ItemKey::TrackArtists),
            Some("Original Track Credit")
        );
        assert_eq!(tag.get_string(ItemKey::RecordingDate), Some("2001-02-03"));
        assert_eq!(
            tag.get_string(ItemKey::MusicBrainzReleaseId),
            Some("release-id")
        );
        assert_eq!(
            tag.get_string(ItemKey::MusicBrainzReleaseGroupId),
            Some("group-id")
        );
        assert_eq!(
            tag.get_string(ItemKey::MusicBrainzRecordingId),
            Some("recording-id")
        );
        assert_eq!(
            tag.get_string(ItemKey::MusicBrainzTrackId),
            Some("track-id")
        );
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].pic_type(), PictureType::CoverFront);
        assert_eq!(tag.pictures()[0].data(), cover);

        write_cover(&path, b"\xff\xd8\xff\xe0new-cover").unwrap();
        let tagged = lofty::read_from_path(&path).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].data(), b"\xff\xd8\xff\xe0new-cover");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn mp3_tags_replace_source_metadata_and_cover() {
        let path = mp3_test_path("tags");
        fs::write(&path, include_bytes!("../tests/fixtures/silence.mp3")).unwrap();
        let cover = b"\x89PNG\r\n\x1a\nfake-image-data";
        let tags = TrackTags {
            title: "Song",
            artist: "Track Artist",
            track_artists: Some("Original Track Credit"),
            album_artist: "Album Artist",
            album: "Album",
            date: Some("2001-02-03"),
            track: 3,
            track_total: 10,
            disc: 2,
            disc_total: 2,
            release_mbid: "release-id",
            release_group_mbid: "group-id",
            recording_mbid: "recording-id",
            track_mbid: "track-id",
            cover: Some(cover),
            replay_gain: None,
        };
        write_tags(&path, &tags).unwrap();

        let tagged = lofty::read_from_path(&path).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.artist().as_deref(), Some("Track Artist"));
        assert_eq!(tag.album().as_deref(), Some("Album"));
        assert_eq!(tag.track(), Some(3));
        assert_eq!(tag.track_total(), Some(10));
        assert_eq!(tag.disk(), Some(2));
        assert_eq!(tag.disk_total(), Some(2));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Album Artist"));
        assert_eq!(
            tag.get_string(ItemKey::TrackArtists),
            Some("Original Track Credit")
        );
        assert_eq!(
            tag.get_string(ItemKey::MusicBrainzRecordingId),
            Some("recording-id")
        );
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].data(), cover);
        assert!(duration_secs(&path).unwrap() > 0.5);
        let _ = fs::remove_file(path);
    }
}
