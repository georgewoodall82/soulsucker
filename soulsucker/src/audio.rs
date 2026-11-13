//! Reading duration and writing clean track metadata with `lofty`.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::picture::{Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagType};

pub fn duration_secs(path: &Path) -> Result<f64> {
    let tagged = lofty::read_from_path(path)
        .with_context(|| format!("could not read audio file {}", path.display()))?;
    if tagged.file_type() != FileType::Flac {
        return Err(anyhow!("not a FLAC file"));
    }
    let duration = tagged.properties().duration().as_secs_f64();
    if duration <= 0.0 {
        return Err(anyhow!("FLAC file reports a zero duration"));
    }
    Ok(duration)
}

pub struct TrackTags<'a> {
    pub title: &'a str,
    pub artist: &'a str,
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

    save(tagged, path)
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

    #[test]
    fn musicbrainz_tags_replace_source_metadata_and_cover() {
        let path = test_path("tags");
        fs::write(&path, include_bytes!("../tests/fixtures/silence.flac")).unwrap();
        let stale = TrackTags {
            title: "Wrong Title",
            artist: "Wrong Artist",
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
        };
        write_tags(&path, &stale).unwrap();

        let cover = b"\x89PNG\r\n\x1a\nfake-image-data";
        let tags = TrackTags {
            title: "Song",
            artist: "Track Artist",
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
}
