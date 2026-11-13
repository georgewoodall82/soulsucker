"""MusicBrainz release fetching tests (network mocked)."""
import musicbrainzngs

from soulsucker import mb


def _medium(pos, fmt, titles):
    return {
        "position": str(pos),
        "format": fmt,
        "track-list": [
            {"position": str(i + 1), "length": "200000", "title": t,
             "recording": {"id": f"MID-{t}", "title": t}}
            for i, t in enumerate(titles)
        ],
    }


def _payload(mediums):
    return {"release": {
        "title": "Hardwired… to Self-Destruct",
        "artist-credit-phrase": "Metallica",
        "date": "2016-11-18",
        "medium-list": mediums,
    }}


def test_get_release_spans_all_audio_discs(monkeypatch):
    # Regression: Hardwired (2xCD) fetched only disc 1's 6 tracks.
    payload = _payload([
        _medium(1, "CD", ["Hardwired", "Atlas, Rise!", "Now That We’re Dead",
                          "Moth Into Flame", "Dream No More", "Halo on Fire"]),
        _medium(2, "CD", ["Confusion", "ManUNkind", "Here Comes Revenge",
                          "Am I Savage?", "Murder One", "Spit Out the Bone"]),
    ])
    monkeypatch.setattr(musicbrainzngs, "get_release_by_id", lambda *a, **k: payload)
    rel = mb.get_release("dummy")
    assert len(rel.tracks) == 12
    assert [t.number for t in rel.tracks] == list(range(1, 13))
    assert [t.disc for t in rel.tracks] == [1] * 6 + [2] * 6
    assert rel.tracks[6].title == "Confusion"
    assert rel.tracks[0].mbid == "MID-Hardwired"
    assert rel.tracks[0].length_ms == 200000


def test_get_release_skips_video_discs(monkeypatch):
    payload = _payload([
        _medium(1, "CD", ["A", "B"]),
        _medium(2, "DVD-Video", ["Video Thing"]),
        _medium(3, "Blu-ray", ["More Video"]),
    ])
    monkeypatch.setattr(musicbrainzngs, "get_release_by_id", lambda *a, **k: payload)
    rel = mb.get_release("dummy")
    assert [t.title for t in rel.tracks] == ["A", "B"]
    assert [t.number for t in rel.tracks] == [1, 2]


def test_get_release_single_disc_unchanged(monkeypatch):
    payload = _payload([_medium(1, "CD", ["A", "B", "C"])])
    monkeypatch.setattr(musicbrainzngs, "get_release_by_id", lambda *a, **k: payload)
    rel = mb.get_release("dummy")
    assert [(t.number, t.title, t.disc) for t in rel.tracks] == [
        (1, "A", 1), (2, "B", 1), (3, "C", 1)]


def test_get_release_empty_mediums(monkeypatch):
    payload = _payload([])
    monkeypatch.setattr(musicbrainzngs, "get_release_by_id", lambda *a, **k: payload)
    assert mb.get_release("dummy").tracks == []


def _candidate(**kw):
    base = dict(mbid="m", title="Appetite for Destruction", artist="Guns N' Roses",
                status="Official", track_count=12, primary_type="Album")
    base.update(kw)
    return mb.ReleaseCandidate(**base)


def _score(c):
    return mb.score_release_candidate(c, query_artist="Guns N' Roses",
                                      query_album="Appetite for Destruction")


def test_documented_cd_beats_bare_digital_stub():
    # Regression: a dateless/countryless Digital Media entry outranked the
    # canonical CD pressing and its recordings had zero AcoustID presence.
    digital = _candidate(date="", country="", media_formats=["Digital Media"])
    cd = _candidate(date="1987", country="US", media_formats=["CD"])
    assert _score(cd) < _score(digital)


def test_provenance_penalties_are_tiebreak_scale():
    # Unknown date/country + Digital Media must not outweigh real signals
    # like deluxe keywords or single status.
    stub = _candidate(date="", country="", media_formats=["Digital Media"])
    single = _candidate(date="1987", country="US", media_formats=["CD"],
                        primary_type="Single", track_count=2)
    assert _score(stub) < _score(single)


def test_unknown_date_sorts_last_on_tie():
    a = _candidate(date="", country="US")
    b = _candidate(date="1987", country="US")
    a.score = b.score = -24
    ordered = sorted([a, b], key=mb.candidate_sort_key)
    assert [c.date for c in ordered] == ["1987", ""]


def test_search_releases_parses_media_formats(monkeypatch):
    payload = {"release-list": [
        {"id": "d", "title": "Appetite for Destruction",
         "artist-credit-phrase": "Guns N' Roses", "status": "Official",
         "medium-track-count": 12,
         "release-group": {"primary-type": "Album"},
         "medium-list": [{"format": "Digital Media"}]},
        {"id": "c", "title": "Appetite for Destruction",
         "artist-credit-phrase": "Guns N' Roses", "status": "Official",
         "date": "1987", "country": "US", "medium-track-count": 12,
         "release-group": {"primary-type": "Album"},
         "medium-list": [{"format": "CD"}]},
    ]}
    monkeypatch.setattr(musicbrainzngs, "search_releases", lambda **k: payload)
    out = mb.search_releases("Guns N' Roses", "Appetite for Destruction")
    assert [(c.mbid, c.media_formats) for c in out] == [
        ("c", ["CD"]), ("d", ["Digital Media"])]
