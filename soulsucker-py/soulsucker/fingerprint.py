"""Audio fingerprint verification via Chromaprint (fpcalc) + AcoustID.

Flow: fpcalc computes a fingerprint of the downloaded file, AcoustID resolves
it to MusicBrainz recording IDs, and we compare against the recording ID from
the chosen MusicBrainz release. Needs the `fpcalc` binary (chromaprint-tools)
and a free AcoustID API key (https://acoustid.org/api).
"""
from __future__ import annotations

import asyncio
import json
import logging
import shutil
import subprocess
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Awaitable, Callable

logger = logging.getLogger(__name__)

ACOUSTID_LOOKUP_URL = "https://api.acoustid.org/v2/lookup"
ACOUSTID_TRACK_LIST_URL = "https://api.acoustid.org/v2/track/list_by_mbid"

# Cache of MBID -> "AcoustID has fingerprints for this recording". Filled
# lazily on the mismatch path only, so the common match path costs nothing.


@dataclass
class FingerprintResult:
    duration: int  # seconds, as reported by fpcalc
    fingerprint: str


@dataclass
class AcoustidMatch:
    recording_mbid: str
    score: float
    title: str = ""


@dataclass
class AcoustidResult:
    """One AcoustID fingerprint cluster: a score plus the recordings linked
    to it. Scores rank clusters; only the top cluster identifies the audio —
    lower clusters are often mislinked noise, so the expected recording must
    appear in the *top* cluster, not just anywhere in the response."""
    score: float
    recordings: list[AcoustidMatch]


def fpcalc_binary() -> str | None:
    """Path to fpcalc, or None if chromaprint-tools is not installed."""
    return shutil.which("fpcalc")


def compute_fingerprint(path: str | Path, timeout: float = 120.0) -> FingerprintResult | None:
    """Run fpcalc on a file. Returns None if the binary is missing or the
    file can't be fingerprinted."""
    binary = fpcalc_binary()
    if not binary:
        return None
    try:
        proc = subprocess.run(
            [binary, "-json", str(path)],
            capture_output=True, text=True, timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        logger.debug("fpcalc failed for %s: %r", path, exc)
        return None
    if proc.returncode != 0:
        logger.debug("fpcalc exit %s for %s: %s", proc.returncode, path,
                     proc.stderr.strip()[:200])
        return None
    try:
        data = json.loads(proc.stdout)
        return FingerprintResult(duration=int(data["duration"]),
                                 fingerprint=str(data["fingerprint"]))
    except (KeyError, TypeError, ValueError) as exc:
        logger.debug("could not parse fpcalc output for %s: %r", path, exc)
        return None


def parse_lookup_response(data: dict) -> list[AcoustidMatch]:
    """Flatten an AcoustID lookup response into recording matches."""
    matches: list[AcoustidMatch] = []
    for result in parse_lookup_results(data):
        matches.extend(result.recordings)
    return matches


def parse_lookup_results(data: dict) -> list[AcoustidResult]:
    """Parse an AcoustID lookup response, preserving score clusters."""
    results: list[AcoustidResult] = []
    for result in data.get("results") or []:
        try:
            score = float(result.get("score", 0) or 0)
        except (TypeError, ValueError):
            continue
        recordings: list[AcoustidMatch] = []
        for rec in result.get("recordings") or []:
            mbid = rec.get("id") or ""
            if mbid:
                recordings.append(AcoustidMatch(
                    recording_mbid=str(mbid),
                    score=score,
                    title=str(rec.get("title") or ""),
                ))
        results.append(AcoustidResult(score=score, recordings=recordings))
    return results


def acoustid_lookup(fingerprint: str, duration: int, api_key: str,
                    timeout: float = 20.0) -> list[AcoustidResult]:
    """Query AcoustID. Raises on network/API errors (caller falls back)."""
    params = urllib.parse.urlencode({
        "client": api_key,
        "meta": "recordings",
        "duration": str(int(duration)),
        "fingerprint": fingerprint,
    })
    req = urllib.request.Request(
        f"{ACOUSTID_LOOKUP_URL}?{params}",
        headers={"User-Agent": "soulsucker/0.1.0"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.load(resp)
    if data.get("status") != "ok":
        err = data.get("error", {})
        raise ValueError(f"AcoustID error: {err.get('message', data) if isinstance(err, dict) else err}")
    return parse_lookup_results(data)


_mbid_known_cache: dict[str, bool] = {}


def acoustid_mbid_track_count(mbid: str, api_key: str, timeout: float = 20.0) -> int:
    """How many AcoustID fingerprint tracks link to a recording MBID.

    Raises on network/API errors.
    """
    params = urllib.parse.urlencode({"client": api_key, "mbid": mbid})
    req = urllib.request.Request(
        f"{ACOUSTID_TRACK_LIST_URL}?{params}",
        headers={"User-Agent": "soulsucker/0.1.0"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.load(resp)
    if data.get("status") != "ok":
        err = data.get("error", {})
        raise ValueError(f"AcoustID error: {err.get('message', data) if isinstance(err, dict) else err}")
    return len(data.get("tracks") or [])


def acoustid_mbid_known(mbid: str, api_key: str) -> bool:
    """Does AcoustID hold any fingerprint for this recording MBID?

    A "different recording" verdict is only meaningful when the database
    knows the expected recording — obscure pressings can have zero linked
    fingerprints, in which case the top hit says nothing about our file.
    Errors assume known (keeps the mismatch) so transient API blips don't
    silently weaken verification.
    """
    key = (mbid or "").lower()
    if key in _mbid_known_cache:
        return _mbid_known_cache[key]
    try:
        known = acoustid_mbid_track_count(mbid, api_key) > 0
    except Exception as exc:
        logger.debug("AcoustID list_by_mbid failed for %s: %r", mbid, exc)
        return True
    _mbid_known_cache[key] = known
    return known


PhaseCallback = Callable[[str], Awaitable[None] | None]


async def verify_recording(path: str | Path, expected_mbid: str, api_key: str,
                           min_score: float = 0.8,
                           on_phase: PhaseCallback | None = None) -> tuple[str, str]:
    """Check a file against an expected MusicBrainz recording ID.

    Returns (verdict, detail) with verdict in:
      match    — the expected recording is linked to AcoustID's top cluster
      mismatch — the top cluster identifies a different recording *and*
                 AcoustID knows the expected recording (so the absence is real)
      unknown  — couldn't tell (weak evidence, not in DB, expected recording
                 unknown to AcoustID, fpcalc/lookup failed); keep on duration
      skipped  — check not possible (no key/binary/MBID); keep on duration
    """
    async def phase(note: str) -> None:
        if on_phase is None:
            return
        r = on_phase(note)
        if asyncio.iscoroutine(r):
            await r

    if not expected_mbid:
        return "skipped", "no MusicBrainz recording ID — duration check only"
    if not api_key:
        return "skipped", "no AcoustID key — duration check only"
    if fpcalc_binary() is None:
        return "skipped", "fpcalc not found — duration check only"

    await phase("computing Chromaprint fingerprint…")
    fp = await asyncio.to_thread(compute_fingerprint, path)
    if fp is None:
        return "unknown", "could not fingerprint file — kept on duration"

    await phase("querying AcoustID…")
    try:
        results = await asyncio.to_thread(acoustid_lookup, fp.fingerprint, fp.duration, api_key)
    except Exception as exc:
        logger.debug("AcoustID lookup failed: %r", exc)
        return "unknown", f"AcoustID lookup failed ({exc}) — kept on duration"

    # Only clusters with linked recordings carry identity information.
    identified = [r for r in results if r.recordings]
    if not identified:
        return "unknown", "fingerprint unknown to AcoustID — kept on duration"

    top = max(r.score for r in identified)
    if top < min_score:
        return "unknown", f"weak AcoustID evidence (best {top:.2f}) — kept on duration"

    # A result ties for the top cluster if its score is effectively equal.
    tier = [r for r in identified if r.score >= top - 1e-9]
    expected = expected_mbid.lower()
    if any(m.recording_mbid.lower() == expected for r in tier for m in r.recordings):
        return "match", f"AcoustID match {top:.2f}"
    first = tier[0].recordings[0]
    label = f"“{first.title}” [{first.recording_mbid[:8]}] " if first.title else ""
    # "Different recording" is only meaningful if AcoustID actually knows the
    # expected one — obscure pressings can have zero linked fingerprints, in
    # which case any top hit is just the DB not knowing our version.
    known = await asyncio.to_thread(acoustid_mbid_known, expected_mbid, api_key)
    if not known:
        return ("unknown",
                "expected recording unknown to AcoustID (no fingerprints) — "
                "kept on duration")
    return ("mismatch",
            f"AcoustID top hit {label}{top:.2f} is a different recording")
