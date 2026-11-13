"""Match MusicBrainz track titles against Soulseek filenames."""
from __future__ import annotations

import os
import re
from dataclasses import dataclass
from difflib import SequenceMatcher

AUDIO_EXTS = {"flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif", "wma", "ape"}
LOSSLESS_EXTS = {"flac", "wav", "aiff", "aif", "ape"}

# Junk that should never match an album track.
JUNK_PATTERNS = [
    re.compile(r"\bsample\b", re.I),
    re.compile(r"\bpreview\b", re.I),
    re.compile(r"\bsnippet\b", re.I),
]


def remote_basename(remote_path: str) -> str:
    """Soulseek paths use \\ (windows) or / — handle both."""
    return remote_path.replace("\\", "/").rstrip("/").split("/")[-1]


def extension_of(filename: str) -> str:
    _, dot, ext = filename.rpartition(".")
    return ext.lower() if dot else ""


def is_flac_file(filename: str) -> bool:
    return extension_of(filename) == "flac"


def stem(filename: str) -> str:
    base = filename
    if "." in base:
        base = base.rsplit(".", 1)[0]
    return base


_STRIP_PATTERNS = [
    re.compile(r"^\d{1,3}\s*[-._\s]+"),          # leading track numbers "01 - "
    re.compile(r"^\(?\s*disc\s*\d+[^a-z0-9]*", re.I),
    re.compile(r"\s*\[[^\]]*\]\s*"),             # [FLAC], [2024] etc.
    re.compile(r"\s*\([^)]*(flac|mp3|320|vinyl|web|cd|remaster)[^)]*\)\s*", re.I),
    re.compile(r"\s*\{[^}]*\}\s*"),
]


def normalize(s: str) -> str:
    s = s.lower()
    for pat in _STRIP_PATTERNS:
        s = pat.sub(" ", s)
    # '&' and 'and' equivalence, punctuation -> space
    s = s.replace("&", " and ")
    s = re.sub(r"[^a-z0-9]+", " ", s)
    s = re.sub(r"\s+", " ", s).strip()
    return s


def title_match_score(track_title: str, remote_path: str) -> float:
    """0.0..1.0 similarity between an MB track title and a Soulseek filename."""
    base = remote_basename(remote_path)
    for pat in JUNK_PATTERNS:
        if pat.search(base):
            return 0.0
    n_track = normalize(track_title)
    n_file = normalize(stem(base))
    if not n_track or not n_file:
        return 0.0
    if n_track == n_file:
        return 1.0
    # Filename often "artist - album - 01 - title" or "01 title".
    # If the normalised track is a whole word-substring, strong score.
    if re.search(rf"(^|\s){re.escape(n_track)}(\s|$)", n_file):
        # Longer filenames (artist + album prefix) dilute ratio; reward containment.
        return max(0.85, SequenceMatcher(None, n_track, n_file).ratio())
    return SequenceMatcher(None, n_track, n_file).ratio()


@dataclass
class CandidateFile:
    username: str
    remote_path: str
    filesize: int
    avg_speed: int
    queue_size: int
    has_free_slots: bool
    match_score: float = 0.0

    @property
    def basename(self) -> str:
        return remote_basename(self.remote_path)


def collect_candidates(
    track_title: str,
    search_results: list,
    min_score: float = 0.45,
    flac_only: bool = True,
) -> list[CandidateFile]:
    """Flatten Soulseek search results into per-track candidates, fastest first.

    `search_results` is a list of aioslsk SearchResult objects (duck-typed so
    this is unit-testable without aioslsk installed).
    """
    cands: list[CandidateFile] = []
    for res in search_results:
        user = getattr(res, "username", "")
        speed = int(getattr(res, "avg_speed", 0) or 0)
        queue = int(getattr(res, "queue_size", 0) or 0)
        free = bool(getattr(res, "has_free_slots", False))
        items = list(getattr(res, "shared_items", []) or []) + list(
            getattr(res, "locked_results", []) or []
        )
        for item in items:
            remote = getattr(item, "filename", "")
            if not remote:
                continue
            base = remote_basename(remote)
            if flac_only and not is_flac_file(base):
                continue
            if not flac_only and extension_of(base) not in AUDIO_EXTS:
                continue
            score = title_match_score(track_title, remote)
            if score < min_score:
                continue
            try:
                size = int(getattr(item, "filesize", 0) or 0)
            except (TypeError, ValueError):
                size = 0
            # Skip tiny files (likely snippets) for FLAC: < 1MB.
            if flac_only and 0 < size < 1_000_000:
                continue
            cands.append(CandidateFile(
                username=user,
                remote_path=remote,
                filesize=size,
                avg_speed=speed,
                queue_size=queue,
                has_free_slots=free,
                match_score=score,
            ))
    # Fastest first, but never prefer a weak title match over a strong one:
    # primary = match quality bucket, secondary = free slots, then speed.
    def sort_key(c: CandidateFile):
        return (
            -round(c.match_score, 1),
            -int(c.has_free_slots),
            -c.avg_speed,
            c.queue_size,
            -c.filesize,
        )
    cands.sort(key=sort_key)
    return cands


def sanitize_filename(name: str) -> str:
    name = re.sub(r'[<>:"/\\|?*\x00-\x1f]', "_", name).strip()
    name = re.sub(r"\s+", " ", name).strip(" .")
    return name or "untitled"


def track_filename(track_number: int, track_title: str, width: int = 2, ext: str = "flac") -> str:
    return f"{track_number:0{width}d} - {sanitize_filename(track_title)}.{ext}"
