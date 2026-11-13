"""MusicBrainz lookup with deluxe-edition deprioritisation."""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from difflib import SequenceMatcher

import musicbrainzngs

USER_AGENT_NAME = "soulsucker"
USER_AGENT_VERSION = "0.1.0"
USER_AGENT_URL = "https://github.com/example/soulsucker"

_configured = False


def ensure_configured() -> None:
    global _configured
    if not _configured:
        musicbrainzngs.set_useragent(USER_AGENT_NAME, USER_AGENT_VERSION, USER_AGENT_URL)
        _configured = True


# Keywords that scream "not the standard album".
DELUXE_PATTERNS = [
    (re.compile(r"deluxe", re.I), 30),
    (re.compile(r"expanded", re.I), 20),
    (re.compile(r"anniversary", re.I), 20),
    (re.compile(r"\bbonus\b", re.I), 18),
    (re.compile(r"super\s*deluxe", re.I), 35),
    (re.compile(r"special\s*edition", re.I), 18),
    (re.compile(r"collector'?s?\s*edition", re.I), 18),
    (re.compile(r"\blive\b", re.I), 15),
    (re.compile(r"remaster", re.I), 8),
    (re.compile(r"reissue", re.I), 6),
    (re.compile(r"demos?", re.I), 12),
    (re.compile(r" Karaoke", re.I), 25),
    (re.compile(r"instrumental", re.I), 12),
    (re.compile(r"tribute", re.I), 25),
    (re.compile(r"explicit|edited", re.I), 2),
]


@dataclass
class TrackInfo:
    number: int
    title: str
    length_ms: int | None = None
    disc: int = 1
    mbid: str = ""  # MusicBrainz recording ID, for fingerprint verification


@dataclass
class ReleaseCandidate:
    mbid: str
    title: str
    artist: str
    date: str = ""
    country: str = ""
    status: str = ""
    disambiguation: str = ""
    track_count: int = 0
    primary_type: str = ""
    secondary_types: list[str] = field(default_factory=list)
    media_formats: list[str] = field(default_factory=list)  # e.g. ["CD"], ["Digital Media"]
    score: int = 0  # lower is better
    score_reasons: list[str] = field(default_factory=list)


@dataclass
class ReleaseInfo:
    mbid: str
    title: str
    artist: str
    date: str = ""
    tracks: list[TrackInfo] = field(default_factory=list)


def _artist_phrase(release: dict) -> str:
    return release.get("artist-credit-phrase", "") or _credit_names(release.get("artist-credit", []))


def _credit_names(credit) -> str:
    if isinstance(credit, list):
        return " & ".join(c.get("name", "") for c in credit if isinstance(c, dict))
    return str(credit or "")


def _medium_position(m: dict) -> int:
    try:
        return int(m.get("position", 0) or 0)
    except (TypeError, ValueError):
        return 0


def _norm_text(s: str) -> str:
    s = (s or "").lower().replace("&", " and ")
    s = re.sub(r"[^a-z0-9]+", " ", s)
    return re.sub(r"\s+", " ", s).strip()


def score_release_candidate(
    c: ReleaseCandidate, query_artist: str = "", query_album: str = ""
) -> int:
    """Lower score = more desirable (standard edition).

    Title/artist match dominates: a wrong-title single must never outrank the
    right album just for having fewer tracks. Small provenance penalties
    (unknown date/country, Digital Media) only break ties between otherwise
    identical standard editions, favouring documented physical pressings
    whose recordings are likelier to be fingerprinted.
    """
    score = 0
    reasons: list[str] = []

    # Title match against the requested album — the dominant signal.
    if query_album:
        nq, nt = _norm_text(query_album), _norm_text(c.title)
        if nt == nq and nt:
            score -= 25
            reasons.append("-25 exact album-title match")
        elif nq and nt and (nq in nt or nt in nq):
            score -= 10
            reasons.append("-10 album-title containment match")
        elif nq and nt:
            ratio = SequenceMatcher(None, nq, nt).ratio()
            penalty = int((1.0 - ratio) * 150)
            if ratio < 0.5:
                penalty += 40
            score += penalty
            reasons.append(f"+{penalty} album-title mismatch ({ratio:.2f})")

    # Artist match — guards against tribute bands / wrong artists.
    if query_artist:
        nq, nt = _norm_text(query_artist), _norm_text(c.artist)
        if nt == nq and nt:
            score -= 10
            reasons.append("-10 exact artist match")
        elif nq and nt and (nq in nt or nt in nq):
            score -= 5
            reasons.append("-5 artist containment match")
        elif nq and nt:
            ratio = SequenceMatcher(None, nq, nt).ratio()
            penalty = int((1.0 - ratio) * 80)
            if ratio < 0.5:
                penalty += 30
            score += penalty
            reasons.append(f"+{penalty} artist mismatch ({ratio:.2f})")

    # Fewer tracks strongly preferred — deluxe/live editions balloon track counts.
    score += c.track_count * 2

    blob = f"{c.title} {c.disambiguation} {' '.join(c.secondary_types)}"
    for pattern, penalty in DELUXE_PATTERNS:
        if pattern.search(blob):
            score += penalty
            reasons.append(f"+{penalty} keyword /{pattern.pattern}/")

    for st in c.secondary_types:
        slow = st.lower()
        if "live" in slow:
            score += 15
            reasons.append("+15 secondary-type Live")
        elif "compilation" in slow:
            score += 12
            reasons.append("+12 secondary-type Compilation")
        elif "remix" in slow:
            score += 12
            reasons.append("+12 secondary-type Remix")
        elif "soundtrack" in slow and c.primary_type != "Album":
            score += 5

    if (c.status or "").lower() == "official":
        score -= 10
        reasons.append("-10 Official status")
    elif (c.status or "").lower() == "bootleg":
        score += 40
        reasons.append("+40 Bootleg")
    elif (c.status or "").lower() in ("promotion", "pseudo-release"):
        score += 25
        reasons.append(f"+25 status {c.status}")
    elif (c.status or "").lower() in ("withdrawn", "cancelled"):
        score += 30
        reasons.append(f"+30 status {c.status}")

    if (c.primary_type or "").lower() == "album":
        score -= 5
        reasons.append("-5 primary Album")
    elif (c.primary_type or "").lower() in ("single", "ep"):
        score += 30
        reasons.append(f"+30 primary {c.primary_type}")
    elif (c.primary_type or "").strip():
        score += 25
        reasons.append(f"+25 primary {c.primary_type}")

    # Provenance: entries with no date/country are low-information stubs
    # (often digital-storefront entries) — slightly worse than documented
    # pressings. Small enough to only break ties between standard editions.
    if not c.date:
        score += 2
        reasons.append("+2 unknown date")
    if not c.country:
        score += 1
        reasons.append("+1 unknown country")
    # For a download-and-verify tool, physical pressings are the canonical,
    # well-fingerprinted sources; Digital Media entries often map to
    # different masters with AcoustID-poor recording IDs.
    if c.media_formats and all("digital" in (f or "").lower() for f in c.media_formats):
        score += 3
        reasons.append("+3 Digital Media only")

    # Prefer earlier (original) releases slightly.
    if c.date:
        try:
            year = int(c.date.split("-")[0])
            score += max(0, (year - 1960)) * 0.05
        except ValueError:
            pass

    c.score_reasons = reasons
    c.score = int(score)
    return c.score


def candidate_sort_key(c: ReleaseCandidate):
    # Unknown dates sort last: missing metadata must never win a tie.
    return (c.score, c.track_count, not c.date, c.date)


def search_releases(artist: str, album: str, limit: int = 25) -> list[ReleaseCandidate]:
    """Search MusicBrainz for releases, sorted best (standard) first."""
    ensure_configured()
    result = musicbrainzngs.search_releases(artist=artist, release=album, limit=limit)
    out: list[ReleaseCandidate] = []
    for rel in result.get("release-list", []):
        rg = rel.get("release-group", {}) or {}
        formats = [m.get("format", "") for m in (rel.get("medium-list", []) or [])
                   if m.get("format")]
        c = ReleaseCandidate(
            mbid=rel["id"],
            title=rel.get("title", ""),
            artist=_artist_phrase(rel),
            date=rel.get("date", ""),
            country=rel.get("country", ""),
            status=rel.get("status", ""),
            disambiguation=rel.get("disambiguation", ""),
            track_count=int(rel.get("medium-track-count", 0) or 0),
            primary_type=rg.get("primary-type", "") or rg.get("type", ""),
            secondary_types=list((rg.get("secondary-type-list") or [])),
            media_formats=formats,
        )
        score_release_candidate(c, query_artist=artist, query_album=album)
        out.append(c)
    out.sort(key=candidate_sort_key)
    return out


def get_release(mbid: str) -> ReleaseInfo:
    """Fetch full tracklist for a release MBID.

    Every audio medium is included — standard albums often span several discs
    (Hardwired… to Self-Destruct is 2×CD). Only video mediums (DVD/Blu-ray)
    are skipped. Deluxe bonus *audio* is kept: the user explicitly chose this
    release; avoiding deluxe editions happens at ranking time, not by
    truncating the tracklist.
    """
    ensure_configured()
    data = musicbrainzngs.get_release_by_id(mbid, includes=["recordings", "artists"])
    rel = data["release"]
    title = rel.get("title", "")
    artist = _artist_phrase(rel)
    date = rel.get("date", "")
    tracks: list[TrackInfo] = []
    mediums = sorted(rel.get("medium-list", []) or [], key=_medium_position)
    for index, m in enumerate(mediums, 1):
        fmt = (m.get("format") or "").lower()
        if any(video in fmt for video in ("dvd", "video", "blu-ray", "hd-dvd", "vhs", "laserdisc")):
            continue
        try:
            disc_no = int(m.get("position", index) or index)
        except (TypeError, ValueError):
            disc_no = index
        for t in m.get("track-list", []) or []:
            try:
                pos = int(t.get("position", len(tracks) + 1))
            except (TypeError, ValueError):
                pos = len(tracks) + 1
            rec = t.get("recording", {}) or {}
            tracks.append(TrackInfo(
                number=pos,
                title=rec.get("title", "") or t.get("title", ""),
                length_ms=int(t.get("length", 0) or 0) or None,
                disc=disc_no,
                mbid=str(rec.get("id", "") or ""),
            ))
    tracks.sort(key=lambda t: (t.disc, t.number))
    # Renumber sequentially (some releases have weird positions).
    for i, t in enumerate(tracks, 1):
        t.number = i
    return ReleaseInfo(mbid=mbid, title=title, artist=artist, date=date, tracks=tracks)


def format_candidate_line(i: int, c: ReleaseCandidate, recommended: bool = False) -> str:
    star = " ★ recommended" if recommended else ""
    dis = f" [{c.disambiguation}]" if c.disambiguation else ""
    sec = f" ({', '.join(c.secondary_types)})" if c.secondary_types else ""
    return (
        f"{i:2d}. {c.artist} - {c.title}{dis}{sec} "
        f"| {c.date or '????'} {c.country} | {c.status} | "
        f"{c.track_count} tracks{star}"
    )
