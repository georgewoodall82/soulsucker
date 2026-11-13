"""Inline progress helpers: every track gets a visible row."""
from types import SimpleNamespace

from rich.console import Console

from soulsucker.slsk import TrackProgress
from soulsucker.ui import build_download_progress, format_track_info, track_label


def _tracks(n=12):
    titles = ["We Die Young", "Man in the Box", "Sea of Sorrow", "Bleed the Freak",
              "I Can't Remember", "Love, Hate, Love", "It Ain't Like That",
              "Sunshine", "Put You Down", "Confusion", "I Know Somethin (Bout You)",
              "Real Thing"]
    return [SimpleNamespace(number=i + 1, title=titles[i]) for i in range(n)]


def _cands(tracks):
    return {t.number: [SimpleNamespace(filesize=25_000_000)] for t in tracks}


def test_track_label_truncates():
    assert track_label(1, "We Die Young") == "01 - We Die Young"
    long_label = track_label(1, "X" * 100)
    assert len(long_label) <= 43
    assert long_label.endswith("…")


def test_format_track_info_covers_all_states():
    assert "✔" in format_track_info(TrackProgress(1, "T", status="done", source="u"))
    assert "✘" in format_track_info(TrackProgress(1, "T", status="failed", note="boom"))
    assert "stalled" in format_track_info(TrackProgress(1, "T", status="stalled", source="u"))
    assert "try 1/3" in format_track_info(
        TrackProgress(1, "T", status="downloading", source="u", attempt=1, attempts_total=3))


def test_all_twelve_tracks_rendered():
    # Regression: the fullscreen app clipped rows on short terminals.
    # The inline view must render one row per track — all 12, not 7.
    tracks = _tracks(12)
    console = Console(record=True, width=120)
    progress, task_ids = build_download_progress(console, tracks, _cands(tracks))
    assert len(task_ids) == 12
    for t in tracks:
        progress.update(task_ids[t.number], completed=10, total=100, info="downloading")
    console.print(progress)
    text = console.export_text()
    for t in tracks:
        assert track_label(t.number, t.title) in text
