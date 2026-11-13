"""Download engine tests with a fake aioslsk client (no network)."""
import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest

from soulsucker import slsk
from soulsucker.matcher import remote_basename


class FakeState:
    def __init__(self, name):
        self.VALUE = SimpleNamespace(name=name)


class FakeTransfer:
    def __init__(self, username, remote, stage_dir):
        self.username = username
        self.remote_path = remote
        self.state = FakeState("DOWNLOADING")
        self.bytes_transfered = 0
        self.filesize = 5_000_000
        self.local_path = None
        self.fail_reason = None
        self.place_in_queue = 0
        self._stage = Path(stage_dir)

    def get_speed(self):
        return 0.0


class FakeMgr:
    """Behaviours keyed by username: 'stuck' never progresses,
    'fail' fails fast, anything else completes shortly after queueing."""

    def __init__(self, stage_dir):
        self.stage_dir = Path(stage_dir)
        self.stage_dir.mkdir(parents=True, exist_ok=True)
        self._flaky_seen: set[str] = set()

    async def download(self, username, remote):
        t = FakeTransfer(username, remote, self.stage_dir)
        if username == "stuck":
            return t
        if username == "fail":
            async def fail():
                await asyncio.sleep(0.1)
                t.state = FakeState("FAILED")
                t.fail_reason = "File not shared."
            asyncio.create_task(fail())
            return t
        if username == "flaky" and username not in self._flaky_seen:
            # Fails the first attempt, succeeds on any retry (last resort).
            self._flaky_seen.add(username)

            async def fail_once():
                await asyncio.sleep(0.1)
                t.state = FakeState("FAILED")
                t.fail_reason = "Remote failure."
            asyncio.create_task(fail_once())
            return t
        if username == "slow":
            # Makes no progress until 2s in: stalls out a 1s timeout but
            # completes inside a patient last-resort pass.
            async def complete_slowly():
                await asyncio.sleep(2.0)
                p = self.stage_dir / remote_basename(remote)
                p.write_bytes(b"x" * 500)
                t.local_path = str(p)
                t.bytes_transfered = 500
                t.filesize = 500
                t.state = FakeState("COMPLETE")
            asyncio.create_task(complete_slowly())
            return t

        async def complete():
            await asyncio.sleep(0.3)
            p = self.stage_dir / remote_basename(remote)
            p.write_bytes(b"x" * 500)
            t.local_path = str(p)
            t.bytes_transfered = 500
            t.filesize = 500
            t.state = FakeState("COMPLETE")
        asyncio.create_task(complete())
        return t

    async def remove(self, transfer):
        transfer.state = FakeState("ABORTED")

    async def abort(self, transfer):
        transfer.state = FakeState("ABORTED")


class FakeClient:
    def __init__(self, stage_dir):
        self.transfers = FakeMgr(stage_dir)


def _cand(user, remote="C:\\m\\01 - T.flac"):
    return slsk.CandidateFile(username=user, remote_path=remote, filesize=500,
                              avg_speed=100, queue_size=0, has_free_slots=True,
                              match_score=1.0)


def test_download_success_moves_to_final_name(tmp_path):
    async def go():
        album = tmp_path / "Album"
        album.mkdir()
        plan = slsk.TrackPlan(number=1, title="T", final_path=album / "01 - T.flac",
                              candidates=[_cand("good")])
        seen = []

        async def cb(p):
            seen.append(p.status)

        ok = await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                           stall_timeout=10, progress_cb=cb)
        return ok, plan, seen

    ok, plan, seen = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    assert "done" in seen


def test_stall_switches_to_next_source(tmp_path):
    async def go():
        album = tmp_path / "Album"
        album.mkdir()
        plan = slsk.TrackPlan(
            number=1, title="T", final_path=album / "01 - T.flac",
            candidates=[_cand("stuck", "a\\01 - T.flac"), _cand("good", "b\\01 - T.flac")])
        notes = []

        async def cb(p):
            notes.append((p.status, p.source))

        ok = await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                           stall_timeout=1, progress_cb=cb)
        return ok, plan, notes

    ok, plan, notes = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    sources = [s for _, s in notes]
    assert "stuck" in sources and "good" in sources
    assert any(s == "stalled" for s, _ in notes)


def test_failed_source_tries_next(tmp_path):
    async def go():
        album = tmp_path / "Album"
        album.mkdir()
        plan = slsk.TrackPlan(
            number=1, title="T", final_path=album / "01 - T.flac",
            candidates=[_cand("fail"), _cand("good")])
        ok = await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                           stall_timeout=10)
        return ok, plan

    ok, plan = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()


def test_cleanup_staging_removes_dir_with_leftovers(tmp_path):
    staging = tmp_path / "Album" / ".staging"
    staging.mkdir(parents=True)
    (staging / "partial.flac").write_bytes(b"junk")
    assert slsk.cleanup_staging(staging) is True
    assert not staging.exists()


def test_cleanup_staging_missing_dir_is_noop(tmp_path):
    assert slsk.cleanup_staging(tmp_path / "Album" / ".staging") is False


def _length_plan(tmp_path, expected_secs, *users):
    album = tmp_path / "Album"
    album.mkdir(exist_ok=True)
    return slsk.TrackPlan(
        number=1, title="T", final_path=album / "01 - T.flac",
        candidates=[_cand(u, f"{u}\\01 - T.flac") for u in users],
        expected_secs=expected_secs)


def test_wrong_length_tries_next_source(tmp_path, monkeypatch):
    durations = iter([999.0, 200.0])
    monkeypatch.setattr(slsk, "audio_duration_seconds", lambda p: next(durations))

    async def go():
        plan = _length_plan(tmp_path, 200.0, "wrong", "good")
        notes = []

        async def cb(p):
            notes.append((p.status, p.source, p.note))

        ok = await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                           stall_timeout=10, progress_cb=cb,
                                           length_tolerance=5.0)
        return ok, plan, notes

    ok, plan, notes = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    assert any("wrong version" in n for _, _, n in notes)


def test_length_within_tolerance_accepted(tmp_path, monkeypatch):
    monkeypatch.setattr(slsk, "audio_duration_seconds", lambda p: 203.0)

    async def go():
        plan = _length_plan(tmp_path, 200.0, "only")
        return await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, length_tolerance=5.0)

    assert asyncio.run(go()) is True


def test_length_check_disabled_with_zero_tolerance(tmp_path, monkeypatch):
    monkeypatch.setattr(slsk, "audio_duration_seconds", lambda p: 999.0)

    async def go():
        plan = _length_plan(tmp_path, 200.0, "only")
        return await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, length_tolerance=0)

    assert asyncio.run(go()) is True


def test_make_track_plans_carries_expected_secs(tmp_path):
    tracks = [SimpleNamespace(number=1, title="A", length_ms=208000),
              SimpleNamespace(number=2, title="B", length_ms=None)]
    plans = slsk.make_track_plans(tracks, {}, tmp_path, width=2)
    assert plans[0].expected_secs == pytest.approx(208.0)
    assert plans[1].expected_secs is None


def test_make_track_plans_carries_recording_mbid(tmp_path):
    tracks = [SimpleNamespace(number=1, title="A", length_ms=1000, mbid="MID-1")]
    plans = slsk.make_track_plans(tracks, {}, tmp_path, width=2)
    assert plans[0].recording_mbid == "MID-1"


def test_make_track_plans_carries_total_tracks(tmp_path):
    tracks = [SimpleNamespace(number=1, title="A"), SimpleNamespace(number=2, title="B")]
    plans = slsk.make_track_plans(tracks, {}, tmp_path, width=2)
    assert [p.total_tracks for p in plans] == [2, 2]
    assert slsk.make_track_plans([], {}, tmp_path, width=2) == []


def test_fingerprint_mismatch_tries_next_source(tmp_path, monkeypatch):
    calls = iter([
        ("mismatch", "AcoustID says “Wrong Song” 0.95 — different recording"),
        ("match", "AcoustID match 0.94"),
    ])

    async def fake_verify(path, mbid, key, min_score=0.8, on_phase=None):
        if on_phase:
            await on_phase("checking…")
        return next(calls)

    monkeypatch.setattr(slsk, "verify_recording", fake_verify)

    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(
            number=1, title="T", final_path=album / "01 - T.flac",
            candidates=[_cand("bad", "b\\01 - T.flac"), _cand("good", "g\\01 - T.flac")],
            expected_secs=None, recording_mbid="MID-EXPECTED")
        notes = []

        async def cb(p):
            notes.append((p.status, p.source, p.note))

        ok = await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, stall_timeout=10,
            progress_cb=cb, acoustid_key="KEY")
        return ok, plan, notes

    ok, plan, notes = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    assert any("AcoustID" in n for _, _, n in notes)
    assert any(s == "verifying" for s, _, _ in notes)


def test_fingerprint_match_accepts_first_source(tmp_path, monkeypatch):
    async def fake_verify(path, mbid, key, min_score=0.8, on_phase=None):
        return ("match", "AcoustID match 0.94")

    monkeypatch.setattr(slsk, "verify_recording", fake_verify)

    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(
            number=1, title="T", final_path=album / "01 - T.flac",
            candidates=[_cand("good")],
            expected_secs=None, recording_mbid="MID-EXPECTED")
        return await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, acoustid_key="KEY")

    assert asyncio.run(go()) is True


def test_no_fingerprint_check_without_key(tmp_path, monkeypatch):
    called = []

    async def fake_verify(*a, **k):
        called.append(True)
        return ("mismatch", "should never be consulted")

    monkeypatch.setattr(slsk, "verify_recording", fake_verify)

    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(
            number=1, title="T", final_path=album / "01 - T.flac",
            candidates=[_cand("good")],
            expected_secs=None, recording_mbid="MID-EXPECTED")
        return await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, acoustid_key=None)

    assert asyncio.run(go()) is True
    assert called == []


def test_audio_duration_reads_real_wav(tmp_path):
    import wave
    p = tmp_path / "t.wav"
    with wave.open(str(p), "w") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(44100)
        w.writeframes(b"\x00" * 44100 * 2 * 2 * 2)  # 2 seconds
    assert slsk.audio_duration_seconds(p) == pytest.approx(2.0, abs=0.05)


def test_audio_duration_garbage_returns_none(tmp_path):
    p = tmp_path / "junk.flac"
    p.write_bytes(b"not audio at all")
    assert slsk.audio_duration_seconds(p) is None


def test_no_candidates_fails_fast(tmp_path):
    async def go():
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=tmp_path / "01 - T.flac", candidates=[])
        return await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan)

    assert asyncio.run(go()) is False


def test_all_sources_exhausted_returns_false(tmp_path):
    async def go():
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=tmp_path / "01 - T.flac",
                              candidates=[_cand("stuck")])
        return await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                             stall_timeout=1,
                                             last_resort_rounds=0)

    assert asyncio.run(go()) is False


def test_last_resort_retries_failed_source_and_succeeds(tmp_path):
    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=album / "01 - T.flac",
                              candidates=[_cand("flaky")])
        statuses = []

        async def cb(p):
            statuses.append(p.status)

        ok = await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, stall_timeout=10,
            progress_cb=cb, last_resort_rounds=1, last_resort_stall_timeout=10)
        return ok, plan, statuses

    ok, plan, statuses = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    assert "last-resort" in statuses
    assert "done" in statuses


def test_last_resort_disabled_stays_failed(tmp_path):
    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=album / "01 - T.flac",
                              candidates=[_cand("flaky")])
        statuses = []

        async def cb(p):
            statuses.append(p.status)

        return await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, stall_timeout=10,
            progress_cb=cb, last_resort_rounds=0)

    assert asyncio.run(go()) is False


def test_last_resort_uses_longer_stall_patience(tmp_path):
    # A source that stalls out the normal timeout can still complete when
    # the last-resort pass gives it more patience.
    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=album / "01 - T.flac",
                              candidates=[_cand("slow")])
        statuses = []

        async def cb(p):
            statuses.append(p.status)

        ok = await slsk.download_one_track(
            FakeClient(tmp_path / "stage"), plan, stall_timeout=1,
            progress_cb=cb, last_resort_rounds=1, last_resort_stall_timeout=5)
        return ok, plan, statuses

    ok, plan, statuses = asyncio.run(go())
    assert ok is True
    assert plan.final_path.is_file()
    assert "stalled" in statuses  # normal pass gave up fast...
    assert "last-resort" in statuses  # ...patient pass finished it


def _make_flac(path, tracknumber="99"):
    import shutil
    import subprocess
    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg not available")
    subprocess.run(
        ["ffmpeg", "-y", "-v", "error", "-f", "lavfi",
         "-i", "anullsrc=r=44100:cl=stereo", "-t", "1", str(path)],
        check=True)
    from mutagen.flac import FLAC
    a = FLAC(str(path))
    a["tracknumber"] = tracknumber
    a["title"] = "Kept Title"
    a.save()


def test_set_track_number_overwrites_flac_tag(tmp_path):
    p = tmp_path / "09 - Of Wolf and Man.flac"
    _make_flac(p, tracknumber="99")
    assert slsk.set_track_number(p, 9, 12) is True
    from mutagen.flac import FLAC
    a = FLAC(str(p))
    assert list(a["tracknumber"]) == ["9"]
    assert list(a["tracktotal"]) == ["12"]
    assert list(a["title"]) == ["Kept Title"]  # other tags preserved


def test_set_track_number_without_total(tmp_path):
    p = tmp_path / "01 - T.flac"
    _make_flac(p)
    assert slsk.set_track_number(p, 1) is True
    from mutagen.flac import FLAC
    assert list(FLAC(str(p))["tracknumber"]) == ["1"]


def test_set_track_number_graceful_on_garbage_and_missing(tmp_path):
    junk = tmp_path / "junk.flac"
    junk.write_bytes(b"not audio at all")
    assert slsk.set_track_number(junk, 5, 12) is False
    assert slsk.set_track_number(tmp_path / "nope.flac", 5, 12) is False
    assert slsk.set_track_number(junk, "not-a-number", 12) is False


def test_download_tags_final_file_with_mb_number(tmp_path, monkeypatch):
    seen = []
    monkeypatch.setattr(slsk, "set_track_number",
                        lambda path, n, total=None: seen.append((Path(path).name, n, total)) or True)

    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(
            number=9, title="Of Wolf and Man",
            final_path=album / "09 - Of Wolf and Man.flac",
            candidates=[_cand("good")], total_tracks=12)
        ok = await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                           stall_timeout=10)
        return ok, plan

    ok, plan = asyncio.run(go())
    assert ok is True
    assert seen == [("09 - Of Wolf and Man.flac", 9, 12)]


def test_download_succeeds_when_tagging_fails(tmp_path, monkeypatch):
    monkeypatch.setattr(slsk, "set_track_number", lambda *a, **k: False)

    async def go():
        album = tmp_path / "Album"
        album.mkdir(exist_ok=True)
        plan = slsk.TrackPlan(number=1, title="T",
                              final_path=album / "01 - T.flac",
                              candidates=[_cand("good")])
        return await slsk.download_one_track(FakeClient(tmp_path / "stage"), plan,
                                             stall_timeout=10)

    assert asyncio.run(go()) is True
