"""Fingerprint verification tests (fpcalc/AcoustID mocked — no binaries needed)."""
import asyncio
import json
from types import SimpleNamespace

from soulsucker import fingerprint as fp


SAMPLE_RESPONSE = {
    "status": "ok",
    "results": [
        {"id": "res1", "score": 0.94,
         "recordings": [{"id": "MBID-EXPECTED", "title": "Nutshell",
                         "artists": [{"name": "Alice in Chains"}]}]},
        {"id": "res2", "score": 0.51, "recordings": []},
        {"id": "res3", "score": "bad", "recordings": [{"id": "MBID-X"}]},
    ],
}


def _result(score, *recs):
    return fp.AcoustidResult(
        score=score,
        recordings=[fp.AcoustidMatch(mbid, score, title) for mbid, title in recs])


def _mocked(monkeypatch, tmp_path, results, known: bool = True):
    monkeypatch.setattr(fp, "fpcalc_binary", lambda: "/usr/bin/fpcalc")
    monkeypatch.setattr(fp, "compute_fingerprint",
                        lambda p, timeout=120.0: fp.FingerprintResult(256, "FP"))
    monkeypatch.setattr(fp, "acoustid_lookup", lambda *a, **k: results)
    # Mismatch path consults AcoustID for the expected MBID; default to known
    # (individual tests override). Fresh cache per test.
    monkeypatch.setattr(fp, "_mbid_known_cache", {})
    monkeypatch.setattr(fp, "acoustid_mbid_track_count",
                        lambda mbid, key, timeout=20.0: 3 if known else 0)
    return tmp_path / "x.flac"


def test_parse_lookup_results_keeps_clusters():
    results = fp.parse_lookup_results(SAMPLE_RESPONSE)
    assert [(r.score, [(m.recording_mbid, m.title) for m in r.recordings])
            for r in results] == [
        (0.94, [("MBID-EXPECTED", "Nutshell")]),
        (0.51, []),
    ]
    assert fp.parse_lookup_results({"status": "ok", "results": []}) == []
    assert fp.parse_lookup_results({}) == []


def test_parse_lookup_response_flat_compat():
    matches = fp.parse_lookup_response(SAMPLE_RESPONSE)
    assert [(m.recording_mbid, m.score) for m in matches] == [("MBID-EXPECTED", 0.94)]


def test_compute_fingerprint_parses_fpcalc_json(tmp_path, monkeypatch):
    monkeypatch.setattr(fp, "fpcalc_binary", lambda: "/usr/bin/fpcalc")
    out = json.dumps({"duration": 256, "fingerprint": "AQAAAA..."})
    completed = SimpleNamespace(returncode=0, stdout=out, stderr="")
    monkeypatch.setattr(fp.subprocess, "run", lambda *a, **k: completed)
    res = fp.compute_fingerprint(tmp_path / "x.flac")
    assert res == fp.FingerprintResult(duration=256, fingerprint="AQAAAA...")


def test_compute_fingerprint_no_binary_returns_none(tmp_path, monkeypatch):
    monkeypatch.setattr(fp, "fpcalc_binary", lambda: None)
    assert fp.compute_fingerprint(tmp_path / "x.flac") is None


def test_compute_fingerprint_bad_output_returns_none(tmp_path, monkeypatch):
    monkeypatch.setattr(fp, "fpcalc_binary", lambda: "/usr/bin/fpcalc")
    completed = SimpleNamespace(returncode=0, stdout="not json", stderr="")
    monkeypatch.setattr(fp.subprocess, "run", lambda *a, **k: completed)
    assert fp.compute_fingerprint(tmp_path / "x.flac") is None


def test_verify_match(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path, [_result(0.94, ("MBID-EXPECTED", "Nutshell"))])
    verdict, detail = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "match"
    assert "0.94" in detail


def test_verify_expected_in_noisy_top_cluster_matches(monkeypatch, tmp_path):
    # Enter Sandman shape: top cluster links the expected ID plus noise.
    f = _mocked(monkeypatch, tmp_path, [_result(
        0.987, ("MBID-NOISE", "Wild Angels"), ("MBID-EXPECTED", "Enter Sandman"))])
    verdict, _ = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "match"


def test_verify_expected_only_in_lower_cluster_mismatches(monkeypatch, tmp_path):
    # S&M shape: top cluster is solely the live recording; the studio ID
    # only appears lower down among mislinked noise.
    f = _mocked(monkeypatch, tmp_path, [
        _result(0.996, ("MBID-SM", "Of Wolf and Man")),
        _result(0.984, ("MBID-TRIBUTE", "Of Wolf and Man"),
                ("MBID-EXPECTED", "Of Wolf and Man")),
    ])
    verdict, detail = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "mismatch"
    assert "Of Wolf and Man" in detail


def test_verify_mismatch_with_unknown_expected_is_kept(monkeypatch, tmp_path):
    # Welcome to the Jungle shape: picked pressing's recording has zero
    # AcoustID fingerprints, so the top hit can't prove anything — keep it.
    f = _mocked(monkeypatch, tmp_path, [
        _result(0.99, ("MBID-COMMON", "Welcome to the Jungle")),
    ], known=False)
    verdict, detail = asyncio.run(fp.verify_recording(f, "MBID-OBSCURE", "KEY"))
    assert verdict == "unknown"
    assert "kept on duration" in detail


def test_verify_mismatch_kept_when_lookup_of_expected_fails(monkeypatch, tmp_path):
    # A blip in list_by_mbid must not silently weaken verification.
    f = _mocked(monkeypatch, tmp_path, [
        _result(0.99, ("MBID-OTHER", "Welcome to the Jungle")),
    ])

    def boom(mbid, key, timeout=20.0):
        raise OSError("network down")

    monkeypatch.setattr(fp, "acoustid_mbid_track_count", boom)
    verdict, _ = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "mismatch"


def test_mbid_known_caches_result(monkeypatch):
    monkeypatch.setattr(fp, "_mbid_known_cache", {})
    calls = []

    def count(mbid, key, timeout=20.0):
        calls.append(mbid)
        return 0

    monkeypatch.setattr(fp, "acoustid_mbid_track_count", count)
    assert fp.acoustid_mbid_known("MBID-X", "KEY") is False
    assert fp.acoustid_mbid_known("mbid-x", "KEY") is False  # case-insensitive hit
    assert calls == ["MBID-X"]


def test_verify_tied_top_clusters_expected_in_one_matches(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path, [
        _result(0.99, ("MBID-OTHER", "Same Title")),
        _result(0.99, ("MBID-EXPECTED", "Same Title")),
    ])
    verdict, _ = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "match"


def test_verify_weak_top_evidence_is_unknown(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path,
                [_result(0.4, ("MBID-EXPECTED", "Nutshell"))])
    verdict, detail = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "unknown"
    assert "kept on duration" in detail


def test_verify_unlinked_top_clusters_ignored(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path,
                [fp.AcoustidResult(0.99, []),
                 _result(0.90, ("MBID-EXPECTED", "Nutshell"))])
    verdict, _ = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "match"


def test_verify_unknown_when_nothing_linked(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path, [fp.AcoustidResult(0.9, [])])
    verdict, detail = asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY"))
    assert verdict == "unknown"
    assert "kept on duration" in detail


def test_verify_skipped_without_key_or_mbid_or_binary(monkeypatch, tmp_path):
    f = tmp_path / "x.flac"
    f.write_bytes(b"x")
    assert asyncio.run(fp.verify_recording(f, "MBID", ""))[0] == "skipped"
    assert asyncio.run(fp.verify_recording(f, "", "KEY"))[0] == "skipped"
    monkeypatch.setattr(fp, "fpcalc_binary", lambda: None)
    assert asyncio.run(fp.verify_recording(f, "MBID", "KEY"))[0] == "skipped"


def test_verify_phases_reported_in_order(monkeypatch, tmp_path):
    f = _mocked(monkeypatch, tmp_path, [_result(0.9, ("MBID-EXPECTED", "N"))])
    phases = []

    async def on_phase(note):
        phases.append(note)

    asyncio.run(fp.verify_recording(f, "MBID-EXPECTED", "KEY", on_phase=on_phase))
    assert phases == ["computing Chromaprint fingerprint…", "querying AcoustID…"]
