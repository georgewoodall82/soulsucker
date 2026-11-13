"""Soulseek search + resilient per-track download engine (aioslsk)."""
from __future__ import annotations

import asyncio
import logging
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Awaitable, Callable

from .fingerprint import verify_recording
from .matcher import CandidateFile, collect_candidates

logger = logging.getLogger(__name__)

try:
    from mutagen import File as _mutagen_file
except ImportError:  # pragma: no cover - mutagen ships with aioslsk
    _mutagen_file = None

TERMINAL_STATES = {"COMPLETE", "FAILED", "INCOMPLETE", "ABORTED"}
ACTIVE_STATES = {"QUEUED", "INITIALIZING", "DOWNLOADING"}


@dataclass
class TrackPlan:
    number: int
    title: str
    final_path: Path
    candidates: list[CandidateFile] = field(default_factory=list)
    expected_secs: float | None = None  # MusicBrainz track length, if known
    recording_mbid: str = ""  # MusicBrainz recording ID, for fingerprint check
    total_tracks: int | None = None  # MusicBrainz track count, for tag writing


@dataclass
class TrackProgress:
    number: int
    title: str
    status: str = "queued"          # queued|searching|downloading|stalled|retry|done|failed|skipped
    bytes_done: int = 0
    total: int = 0
    speed: float = 0.0
    source: str = ""
    attempt: int = 0
    attempts_total: int = 0
    note: str = ""

ProgressCallback = Callable[[TrackProgress], Awaitable[None] | None]


def build_search_query(artist: str, album: str) -> str:
    # One global search (fast) — per-track matching happens locally.
    return f"{artist} {album}".strip()


async def collect_search_results(client, query: str, wait_seconds: float,
                                 on_tick: Callable[[int], None] | None = None) -> list:
    """Run a single network search and gather results for `wait_seconds`."""
    request = await client.searches.search(query)
    deadline = time.monotonic() + wait_seconds
    last = -1
    while time.monotonic() < deadline:
        await asyncio.sleep(0.5)
        n = len(request.results)
        if n != last and on_tick:
            try:
                on_tick(n)
            except Exception:
                pass
            last = n
    return list(request.results)


def rank_users(results: list) -> list:
    """Sort SearchResults fastest-first (free slots, speed, queue)."""
    def key(r):
        return (
            -int(bool(getattr(r, "has_free_slots", False))),
            -(int(getattr(r, "avg_speed", 0) or 0)),
            int(getattr(r, "queue_size", 0) or 0),
        )
    return sorted(results, key=key)


def audio_duration_seconds(path: str | Path) -> float | None:
    """Measured length of an audio file in seconds, or None if unreadable."""
    if _mutagen_file is None:
        return None
    try:
        audio = _mutagen_file(str(path))
        if audio is None or getattr(audio, "info", None) is None:
            return None
        length = float(getattr(audio.info, "length", 0) or 0)
        return length if length > 0 else None
    except Exception:
        logger.debug("could not read duration of %s", path, exc_info=True)
        return None


def set_track_number(path: str | Path, number: int, total: int | None = None) -> bool:
    """Write the MusicBrainz track number into the file's metadata.

    Overwrites whatever the Soulseek source had (often wrong). Returns True
    on success, False when tagging isn't possible — callers must not fail
    the download over this.
    """
    if _mutagen_file is None:
        return False
    try:
        number = int(number)
    except (TypeError, ValueError):
        return False
    try:
        total_int: int | None = None
        if total is not None:
            try:
                total_int = int(total)
            except (TypeError, ValueError):
                total_int = None
        audio = _mutagen_file(str(path))
        if audio is None:
            return False
        cls = audio.__class__.__name__
        if cls in ("MP4", "M4A", "MP4Audio"):
            audio["trkn"] = [(number, total_int or 0)]
            audio.save()
            return True
        if cls in ("MP3",):
            from mutagen.id3 import TRCK
            text = f"{number}/{total_int}" if total_int else str(number)
            if getattr(audio, "tags", None) is None:
                try:
                    audio.add_tags()
                except Exception:
                    pass
            try:
                audio["TRCK"] = TRCK(encoding=3, text=text)
            except Exception:
                audio.tags.add(TRCK(encoding=3, text=text))
            audio.save()
            return True
        # FLAC / Ogg / Opus / WavPack / EasyMP3 — dict-style tags.
        audio["tracknumber"] = str(number)
        if total_int:
            try:
                audio["tracktotal"] = str(total_int)
            except Exception:
                pass
        audio.save()
        return True
    except Exception:
        logger.debug("could not tag track number of %s", path, exc_info=True)
        return False


async def _remove_transfer(client, transfer) -> None:
    try:
        await client.transfers.remove(transfer)
    except Exception:
        try:
            await client.transfers.abort(transfer)
        except Exception:
            pass


async def download_one_track(
    client,
    plan: TrackPlan,
    stall_timeout: float = 10.0,
    per_candidate_timeout: float = 300.0,
    progress_cb: ProgressCallback | None = None,
    length_tolerance: float = 5.0,
    acoustid_key: str | None = None,
    fp_min_score: float = 0.8,
    last_resort_rounds: int = 2,
    last_resort_stall_timeout: float = 60.0,
) -> bool:
    """Try each candidate in order until one completes. Returns True on success.

    Completed files whose measured length differs from the MusicBrainz length
    by more than `length_tolerance` seconds are rejected as wrong versions
    and the next source is tried. `length_tolerance <= 0` disables the check.

    When `acoustid_key` and the plan's recording MBID are known, the audio
    fingerprint is additionally verified against AcoustID; mismatches move on
    to the next source.

    Last resort: when every source is exhausted (stalled/failed/rejected),
    the whole candidate list is retried `last_resort_rounds` more times with
    `last_resort_stall_timeout` patience — slow niche-album sources get a
    second chance instead of failing outright. `last_resort_rounds <= 0`
    disables this.
    """

    async def emit(status: str, bytes_done: int = 0, total: int = 0,
                   speed: float = 0.0, source: str = "", attempt: int = 0,
                   note: str = "") -> None:
        if progress_cb is None:
            return
        p = TrackProgress(
            number=plan.number, title=plan.title, status=status,
            bytes_done=bytes_done, total=total, speed=speed,
            source=source, attempt=attempt, attempts_total=len(plan.candidates),
            note=note,
        )
        r = progress_cb(p)
        if asyncio.iscoroutine(r):
            await r

    if not plan.candidates:
        await emit("failed", note="no FLAC candidates found")
        return False

    for idx, cand in enumerate(plan.candidates, 1):
        await emit("downloading", source=cand.username, attempt=idx,
                   total=cand.filesize, note=f"try {idx}/{len(plan.candidates)}")
        try:
            transfer = await client.transfers.download(cand.username, cand.remote_path)
        except Exception as exc:  # queueing failed — try next source
            logger.warning("queue failed %s from %s: %r", cand.remote_path, cand.username, exc)
            await emit("retry", source=cand.username, attempt=idx, note=f"queue error: {exc}")
            continue

        last_bytes = -1
        last_change = time.monotonic()
        start = time.monotonic()
        stalled_notified = False
        outcome: str | None = None

        while True:
            await asyncio.sleep(0.25)
            try:
                state_name = transfer.state.VALUE.name
            except Exception:
                state_name = "QUEUED"
            b = int(getattr(transfer, "bytes_transfered", 0) or 0)
            total = int(getattr(transfer, "filesize", 0) or cand.filesize or 0)
            try:
                speed = float(transfer.get_speed())
            except Exception:
                speed = 0.0

            if b != last_bytes:
                last_bytes = b
                last_change = time.monotonic()
                stalled_notified = False
            now = time.monotonic()
            stalled_for = now - last_change

            if state_name == "COMPLETE":
                outcome = "done"
                await emit("downloading", bytes_done=b, total=total or b,
                           speed=speed, source=cand.username, attempt=idx)
                break
            if state_name == "FAILED":
                outcome = f"failed: {getattr(transfer, 'fail_reason', '') or 'rejected'}"
                break
            if state_name in ("ABORTED",):
                outcome = "aborted"
                break
            if state_name == "INCOMPLETE":
                # Uploader vanished without reason; manager may retry internally,
                # but we move on after a short grace period.
                if stalled_for > 5:
                    outcome = "incomplete"
                    break
            if state_name == "PAUSED":
                outcome = "paused"
                break

            # Stall watchdog: no byte movement for `stall_timeout`.
            if state_name in ACTIVE_STATES and stalled_for >= stall_timeout:
                if not stalled_notified:
                    await emit("stalled", bytes_done=b, total=total, speed=speed,
                               source=cand.username, attempt=idx,
                               note=f"stalled {stalled_for:.0f}s — switching source")
                    stalled_notified = True
                outcome = f"stalled {stalled_for:.0f}s"
                break

            if now - start > per_candidate_timeout:
                outcome = "timeout"
                break

            # Live progress ping (throttled by the 0.25s loop).
            status = "downloading" if state_name in ("DOWNLOADING", "INITIALIZING") else "downloading"
            if state_name == "QUEUED":
                note = f"queued behind {getattr(transfer, 'place_in_queue', '?')} — try {idx}/{len(plan.candidates)}"
            else:
                note = f"try {idx}/{len(plan.candidates)}"
            await emit(status, bytes_done=b, total=total, speed=speed,
                       source=cand.username, attempt=idx, note=note)

        if outcome == "done":
            src = getattr(transfer, "local_path", None)
            if (
                src
                and Path(src).is_file()
                and plan.expected_secs
                and length_tolerance > 0
            ):
                actual = audio_duration_seconds(src)
                if actual is not None and abs(actual - plan.expected_secs) > length_tolerance:
                    note = (f"length {actual:.0f}s vs expected {plan.expected_secs:.0f}s "
                            f"(±{length_tolerance:.0f}s) — wrong version, trying next")
                    logger.info("track %02d source %s wrong length (%s), trying next",
                                plan.number, cand.username, note)
                    await emit("retry", source=cand.username, attempt=idx, note=note)
                    try:
                        Path(src).unlink()
                    except OSError:
                        pass
                    await _remove_transfer(client, transfer)
                    await asyncio.sleep(0.3)
                    continue
            if plan.recording_mbid and acoustid_key and fp_min_score > 0:
                async def fp_phase(note: str) -> None:
                    await emit("verifying", bytes_done=b, total=total or b,
                               source=cand.username, attempt=idx, note=note)
                await fp_phase("verifying audio fingerprint…")
                verdict, detail = await verify_recording(
                    src, plan.recording_mbid, acoustid_key, fp_min_score,
                    on_phase=fp_phase)
                if verdict == "mismatch":
                    note = f"{detail} — trying next source"
                    logger.info("track %02d source %s fingerprint mismatch (%s), trying next",
                                plan.number, cand.username, detail)
                    await emit("retry", source=cand.username, attempt=idx, note=note)
                    try:
                        Path(src).unlink()
                    except OSError:
                        pass
                    await _remove_transfer(client, transfer)
                    await asyncio.sleep(0.3)
                    continue
                await emit("verifying", bytes_done=b, total=total or b,
                           source=cand.username, attempt=idx, note=detail)
            # Move staged file to its pretty "01 - Title.flac" name.
            src = getattr(transfer, "local_path", None)
            try:
                if src and Path(src).is_file():
                    plan.final_path.parent.mkdir(parents=True, exist_ok=True)
                    if Path(src).resolve() != plan.final_path.resolve():
                        shutil.move(src, plan.final_path)
                    # Stamp the MusicBrainz track number — source tags are
                    # often wrong. Tag failure never fails the download.
                    if not set_track_number(plan.final_path, plan.number,
                                            plan.total_tracks):
                        logger.debug("track %02d: could not write tracknumber tag",
                                     plan.number)
                    await emit("done", bytes_done=plan.final_path.stat().st_size,
                               total=plan.final_path.stat().st_size,
                               source=cand.username, attempt=idx)
                else:
                    await emit("done", source=cand.username, attempt=idx,
                               note="complete (staged file not found, kept in place)")
            except Exception as exc:
                await emit("done", source=cand.username, attempt=idx,
                           note=f"complete, rename failed: {exc}")
            return True

        # Failed this source — clean up partial file + transfer, try next.
        logger.info("track %02d source %s failed (%s), trying next", plan.number, cand.username, outcome)
        await emit("retry", source=cand.username, attempt=idx, note=str(outcome))
        await _remove_transfer(client, transfer)
        await asyncio.sleep(0.3)

    if last_resort_rounds > 0:
        patience = (last_resort_stall_timeout
                    if last_resort_stall_timeout > 0 else stall_timeout)
        noun = "pass" if last_resort_rounds == 1 else "passes"
        await emit("last-resort",
                   note=(f"all {len(plan.candidates)} sources exhausted — "
                         f"last-resort retry with {patience:.0f}s patience "
                         f"({last_resort_rounds} {noun} left)"))
        await asyncio.sleep(5)  # brief backoff: queued slots may free up
        return await download_one_track(
            client, plan, stall_timeout=patience,
            per_candidate_timeout=per_candidate_timeout,
            progress_cb=progress_cb, length_tolerance=length_tolerance,
            acoustid_key=acoustid_key, fp_min_score=fp_min_score,
            last_resort_rounds=last_resort_rounds - 1,
            last_resort_stall_timeout=last_resort_stall_timeout)

    await emit("failed", note=f"all {len(plan.candidates)} sources exhausted")
    return False


async def download_all_tracks(
    client,
    plans: list[TrackPlan],
    stall_timeout: float = 10.0,
    max_concurrent: int = 4,
    progress_cb: ProgressCallback | None = None,
    length_tolerance: float = 5.0,
    acoustid_key: str | None = None,
    fp_min_score: float = 0.8,
    last_resort_rounds: int = 2,
    last_resort_stall_timeout: float = 60.0,
) -> dict[int, bool]:
    sem = asyncio.Semaphore(max(1, max_concurrent))
    results: dict[int, bool] = {}

    async def _one(plan: TrackPlan):
        async with sem:
            ok = await download_one_track(
                client, plan, stall_timeout=stall_timeout,
                progress_cb=progress_cb, length_tolerance=length_tolerance,
                acoustid_key=acoustid_key, fp_min_score=fp_min_score,
                last_resort_rounds=last_resort_rounds,
                last_resort_stall_timeout=last_resort_stall_timeout)
            results[plan.number] = ok

    await asyncio.gather(*(_one(p) for p in plans))
    return results


def cleanup_staging(staging: Path) -> bool:
    """Remove the staging dir (partial/failed downloads). Returns True if removed."""
    try:
        if staging.is_dir():
            shutil.rmtree(staging)
            return True
    except OSError:
        logger.warning("could not remove staging dir %s", staging, exc_info=True)
    return False


def make_track_plans(tracks, candidates_per_track: dict[int, list[CandidateFile]],
                     album_dir: Path, width: int, ext: str = "flac") -> list[TrackPlan]:
    from .matcher import track_filename
    tracks = list(tracks)
    total = len(tracks) or None
    plans = []
    for t in tracks:
        number = getattr(t, "number", len(plans) + 1)
        title = getattr(t, "title", f"Track {number}")
        length_ms = getattr(t, "length_ms", None)
        try:
            expected = float(length_ms) / 1000.0 if length_ms else None
        except (TypeError, ValueError):
            expected = None
        plans.append(TrackPlan(
            number=number, title=title,
            final_path=album_dir / track_filename(number, title, width=width, ext=ext),
            candidates=candidates_per_track.get(number, []),
            expected_secs=expected,
            recording_mbid=str(getattr(t, "mbid", "") or ""),
            total_tracks=total,
        ))
    return plans
