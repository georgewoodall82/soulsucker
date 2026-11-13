"""CLI: interactive args → MusicBrainz pick → Soulseek FLAC download."""
from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from pathlib import Path

from .matcher import sanitize_filename
from .mb import format_candidate_line, get_release, search_releases


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="soulsucker",
        description="Download an album as FLAC from Soulseek using a MusicBrainz tracklist.",
    )
    p.add_argument("artist", nargs="?", help="Artist name (prompted if omitted)")
    p.add_argument("album", nargs="?", help="Album name (prompted if omitted)")
    p.add_argument("-a", "--artist", dest="artist_opt", default=None, help="Artist name")
    p.add_argument("-A", "--album", dest="album_opt", default=None, help="Album name")
    p.add_argument("-u", "--username", default=None, help="Soulseek username (else env/config/prompt)")
    p.add_argument("-p", "--password", default=None, help="Soulseek password (else env/config/prompt)")
    p.add_argument("-o", "--output", default=".", help="Base directory for the new album folder (default: cwd)")
    p.add_argument("--search-timeout", type=float, default=20.0, help="Seconds to gather Soulseek results (default: 20)")
    p.add_argument("--stall-timeout", type=float, default=10.0, help="Seconds without progress before switching source (default: 10)")
    p.add_argument("--max-concurrent", type=int, default=4, help="Parallel track downloads (default: 4)")
    p.add_argument("--port", type=int, default=60000, help="Listening port for Soulseek (default: 60000)")
    p.add_argument("--limit", type=int, default=25, help="MusicBrainz candidates to show (default: 25)")
    p.add_argument("--length-tolerance", type=float, default=5.0,
                   help="Reject downloads whose length differs from MusicBrainz by more than this (seconds, default: 5; 0 disables)")
    p.add_argument("--acoustid-key", default=None,
                   help="AcoustID API key for fingerprint verification (else ACOUSTID_API_KEY env / config file; free at https://acoustid.org/api)")
    p.add_argument("--fingerprint-min-score", type=float, default=0.8,
                   help="Minimum AcoustID score to accept a fingerprint match (default: 0.8)")
    p.add_argument("--no-fingerprint", action="store_true",
                   help="Disable Chromaprint/AcoustID verification (duration check only)")
    p.add_argument("--last-resort-rounds", type=int, default=2,
                   help="Extra passes over a track's sources after all are exhausted (default: 2; 0 disables)")
    p.add_argument("--last-resort-stall-timeout", type=float, default=60.0,
                   help="Stall patience in seconds during last-resort passes (default: 60)")
    p.add_argument("--yes", action="store_true", help="Auto-pick the recommended (standard, fewest-tracks) release")
    p.add_argument("--tui", action="store_true", help="Use the fullscreen Textual UI instead of the default inline view")
    p.add_argument("--no-tui", action="store_true", help="Deprecated: inline progress is now the default")
    p.add_argument("-v", "--verbose", action="store_true", help="Verbose (debug) logging")
    p.add_argument("--log-file", default=None, help="Write debug logs here instead of stderr (e.g. slsk.log)")
    return p


def prompt_missing(artist: str | None, album: str | None) -> tuple[str, str]:
    if not artist:
        try:
            artist = input("Artist: ").strip()
        except EOFError:
            artist = ""
    if not album:
        try:
            album = input("Album: ").strip()
        except EOFError:
            album = ""
    if not artist or not album:
        raise SystemExit("error: artist and album are both required")
    return artist, album


def pick_release(artist: str, album: str, limit: int, auto: bool):
    print(f"Searching MusicBrainz for '{artist} — {album}'…")
    try:
        candidates = search_releases(artist, album, limit=limit)
    except Exception as exc:
        raise SystemExit(f"error: MusicBrainz search failed: {exc}")
    if not candidates:
        raise SystemExit("error: no MusicBrainz releases found — try a different spelling")
    print(f"\nFound {len(candidates)} releases (sorted: standard editions first):\n")
    for i, c in enumerate(candidates, 1):
        print(format_candidate_line(i, c, recommended=(i == 1)))
    print("\n★ = recommended (official, fewest bonus/live tracks). Deluxe editions sink to the bottom.")
    if auto:
        print("\n--yes: auto-picked #1")
        return candidates[0]
    while True:
        try:
            raw = input(f"\nPick a release [1-{len(candidates)}] (default 1): ").strip()
        except EOFError:
            raw = ""
        if raw == "":
            return candidates[0]
        try:
            n = int(raw)
            if 1 <= n <= len(candidates):
                return candidates[n - 1]
        except ValueError:
            pass
        print(f"Enter a number 1-{len(candidates)}.")


class BenignAioslskErrorFilter(logging.Filter):
    """Drops a known-harmless aioslsk race: a late/duplicate server response
    (e.g. GetPeerAddress) hits an already-resolved future, so the library logs
    "error during callback" with asyncio.InvalidStateError. Downloads continue
    fine — the traceback only scares users and smears the TUI."""

    def filter(self, record: logging.LogRecord) -> bool:
        try:
            msg = record.getMessage()
        except Exception:
            return True
        if "error during callback" not in msg:
            return True
        exc = record.exc_info
        if isinstance(exc, tuple) and len(exc) == 3 and exc[0] is not None:
            try:
                if issubclass(exc[0], asyncio.InvalidStateError):
                    return False
            except TypeError:
                pass
            if "invalid state" in str(exc[1] or "").lower():
                return False
        return True


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)
    # aioslsk logs routine peer-connection failures (offline peers, NAT) as
    # WARNING, which spams stderr and corrupts the Textual UI. Silence it by
    # default; -v restores everything, optionally into --log-file.
    handlers: list[logging.Handler] = []
    if args.log_file:
        handlers.append(logging.FileHandler(args.log_file))
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.ERROR,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        handlers=handlers or None,
    )
    if not args.verbose:
        benign_filter = BenignAioslskErrorFilter()
        for handler in logging.root.handlers:
            handler.addFilter(benign_filter)
        for name in (
            "aioslsk",
            "aioslsk.network",
            "aioslsk.network.network",
            "aioslsk.network.connection",
        ):
            logger = logging.getLogger(name)
            logger.setLevel(logging.ERROR)
            logger.addFilter(benign_filter)

    artist = args.artist_opt or args.artist
    album = args.album_opt or args.album
    artist, album = prompt_missing(artist, album)

    from .config import resolve_acoustid_key, resolve_credentials
    username, password = resolve_credentials(args.username, args.password)
    acoustid_key = "" if args.no_fingerprint else resolve_acoustid_key(args.acoustid_key)

    choice = pick_release(artist, album, args.limit, args.yes)
    print(f"\nFetching tracklist for '{choice.artist} — {choice.title}' ({choice.date})…")
    try:
        release = get_release(choice.mbid)
    except Exception as exc:
        raise SystemExit(f"error: could not fetch release details: {exc}")
    if not release.tracks:
        raise SystemExit("error: release has no tracks")
    if choice.track_count and len(release.tracks) != choice.track_count:
        print(f"Note: fetched {len(release.tracks)} audio tracks but the search listing "
              f"showed {choice.track_count} — continuing with the fetched tracklist.")
    print(f"\n{release.artist} — {release.title} ({release.date}) — {len(release.tracks)} tracks:")
    for t in release.tracks:
        mins, secs = divmod(int((t.length_ms or 0) / 1000), 60)
        dur = f"{mins}:{secs:02d}" if t.length_ms else "-:--"
        print(f"  {t.number:02d}. {t.title} [{dur}]")

    base = Path(args.output).expanduser().resolve()
    folder_name = sanitize_filename(f"{release.artist} - {release.title}")
    album_dir = base / folder_name
    album_dir.mkdir(parents=True, exist_ok=True)
    print(f"\nDownloading into: {album_dir}")
    if acoustid_key:
        print("Fingerprint verification: Chromaprint + AcoustID on")
    else:
        print("Fingerprint verification: off (duration check only) — get a free key at "
              "https://acoustid.org/api, then --acoustid-key / ACOUSTID_API_KEY")

    if args.tui:
        try:
            from .ui import build_app
        except ImportError:
            raise SystemExit("error: --tui needs the 'textual' package (pip install textual)")
        try:
            app = build_app(
                username=username, password=password,
                artist=release.artist or artist, album=release.title or album,
                tracks=release.tracks, album_dir=album_dir,
                search_timeout=args.search_timeout, stall_timeout=args.stall_timeout,
                max_concurrent=args.max_concurrent, port=args.port,
                length_tolerance=args.length_tolerance,
                acoustid_key=acoustid_key or None,
                fp_min_score=args.fingerprint_min_score,
                last_resort_rounds=args.last_resort_rounds,
                last_resort_stall_timeout=args.last_resort_stall_timeout,
            )
        except ImportError:
            raise SystemExit("error: --tui needs the 'textual' package (pip install textual)")
        app.run()
    else:
        from .ui import run_inline
        asyncio.run(run_inline(
            username=username, password=password,
            artist=release.artist or artist, album=release.title or album,
            tracks=release.tracks, album_dir=album_dir,
            search_timeout=args.search_timeout, stall_timeout=args.stall_timeout,
            max_concurrent=args.max_concurrent, port=args.port,
            length_tolerance=args.length_tolerance,
            acoustid_key=acoustid_key or None,
            fp_min_score=args.fingerprint_min_score,
            last_resort_rounds=args.last_resort_rounds,
            last_resort_stall_timeout=args.last_resort_stall_timeout,
        ))


if __name__ == "__main__":
    main(sys.argv[1:])
