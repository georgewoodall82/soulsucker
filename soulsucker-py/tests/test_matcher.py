"""Unit tests for filename matching + release scoring (no network)."""
from types import SimpleNamespace

from soulsucker.matcher import (
    CandidateFile,
    collect_candidates,
    normalize,
    remote_basename,
    title_match_score,
    track_filename,
)
from soulsucker.mb import ReleaseCandidate, score_release_candidate


def _res(user, speed, queue, free, *files):
    return SimpleNamespace(
        username=user, avg_speed=speed, queue_size=queue, has_free_slots=free,
        shared_items=[SimpleNamespace(filename=f, filesize=30_000_000) for f in files],
        locked_results=[],
    )


def test_basename_handles_windows_paths():
    assert remote_basename(r"@@abc\Mუსიკ\Metallica\Ride\01 - Fight Fire with Fire.flac") == "01 - Fight Fire with Fire.flac"
    assert remote_basename("/music/x.flac") == "x.flac"


def test_normalize_strips_track_numbers():
    assert normalize("01 - Fight Fire with Fire") == "fight fire with fire"
    assert normalize("Metallica - Ride The Lightning - 01 - Fight Fire With Fire [FLAC]") == "metallica ride the lightning 01 fight fire with fire"


def test_exact_match_scores_one():
    assert title_match_score("Fight Fire with Fire", r"C:\share\01 - Fight Fire with Fire.flac") == 1.0


def test_contained_match_scores_high():
    s = title_match_score("Fight Fire with Fire", r"\\host\Metallica - Ride the Lightning - 01 - Fight Fire with Fire.flac")
    assert s >= 0.85, s


def test_wrong_track_scores_low():
    s = title_match_score("Fight Fire with Fire", r"D:\music\02 - Ride the Lightning.flac")
    assert s < 0.6, s


def test_collect_prefers_flac_and_fastest():
    results = [
        _res("slow", 100, 5, False, r"C:\m\01 - Fight Fire with Fire.flac"),
        _res("fast", 9000, 0, True, r"C:\m\01 - Fight Fire with Fire.flac"),
        _res("mp3guy", 50000, 0, True, r"C:\m\01 - Fight Fire with Fire.mp3"),
        _res("wrong", 99999, 0, True, r"C:\m\02 - Ride the Lightning.flac"),
    ]
    cands = collect_candidates("Fight Fire with Fire", results)
    assert cands, "expected candidates"
    assert all(c.basename.endswith(".flac") for c in cands)
    assert cands[0].username == "fast", [c.username for c in cands]
    assert "wrong" not in [c.username for c in cands]


def test_deluxe_release_scores_worse():
    std = ReleaseCandidate(mbid="a", title="Ride the Lightning", artist="Metallica",
                           date="1984-07-27", status="Official", track_count=8, primary_type="Album")
    dlx = ReleaseCandidate(mbid="b", title="Ride the Lightning", artist="Metallica",
                           date="2016-04-15", status="Official", disambiguation="deluxe edition",
                           track_count=97, primary_type="Album")
    assert score_release_candidate(std) < score_release_candidate(dlx)


def test_wrong_title_promo_loses_to_correct_album():
    # Regression: "Sea of Sorrow" 2-track promo must not outrank "Jar of Flies".
    wrong = ReleaseCandidate(mbid="x", title="Sea of Sorrow", artist="Alice in Chains",
                             date="1991", status="Promotion", track_count=2,
                             primary_type="Single")
    right = ReleaseCandidate(mbid="y", title="Jar of Flies", artist="Alice in Chains",
                             date="1994-01-25", status="Official", track_count=7,
                             primary_type="Album")
    assert score_release_candidate(right, "Alice in Chains", "Jar of Flies") < \
        score_release_candidate(wrong, "Alice in Chains", "Jar of Flies")


def test_track_filename_format():
    assert track_filename(1, "Fight Fire with Fire") == "01 - Fight Fire with Fire.flac"
    assert track_filename(12, "A/B: C?") == "12 - A_B_ C_.flac"
