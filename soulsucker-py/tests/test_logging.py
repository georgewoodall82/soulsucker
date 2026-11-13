"""The aioslsk GetPeerAddress/InvalidStateError race is benign — filter it."""
import asyncio
import logging

from soulsucker.cli import BenignAioslskErrorFilter


def _rec(msg, exc=None):
    return logging.LogRecord(
        "aioslsk.network.connection", logging.ERROR, __file__, 1, msg, None, exc)


def _invalid_state_exc_info():
    try:
        raise asyncio.InvalidStateError("invalid state")
    except asyncio.InvalidStateError:
        import sys
        return sys.exc_info()


def test_benign_callback_race_is_dropped():
    f = BenignAioslskErrorFilter()
    rec = _rec(
        "[server.slsknet.org:2416] error during callback : "
        "GetPeerAddress.Response(username='c23', ip='107.223.21.235', port=2234)",
        _invalid_state_exc_info(),
    )
    assert f.filter(rec) is False


def test_other_errors_are_kept():
    f = BenignAioslskErrorFilter()
    assert f.filter(_rec("error during callback : boom", None)) is True
    assert f.filter(_rec("failed to fulfill ConnectToPeer request", None)) is True
    assert f.filter(_rec("login failed", None)) is True
