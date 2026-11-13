"""Inline live download UI (default) + fullscreen Textual UI (--tui)."""
from __future__ import annotations

import asyncio
import logging
import time
from pathlib import Path

logger = logging.getLogger(__name__)

# ------------------------------------------------------- Fullscreen Textual UI
TEXTUAL_CSS = """
Screen { align: center middle; }
#head { height: 3; border: solid green; content-align: center middle; }
#tracks { height: 1fr; border: solid blue; padding: 0 1; overflow-y: auto; }
TrackRow { height: 3; margin: 0 0 0 0; }
TrackRow .tlabel { width: 34; }
TrackRow ProgressBar { width: 1fr; height: 1; margin: 1 1 0 1; }
TrackRow .tstatus { width: 12; }
TrackRow .tmeta { width: 34; }
#log { height: 8; border: solid yellow; overflow-y: auto; }
#summary { height: 3; }
"""

STATUS_STYLE = {
    "queued": "dim",
    "downloading": "cyan",
    "verifying": "cyan",
    "stalled": "yellow",
    "retry": "magenta",
    "last-resort": "magenta",
    "done": "green",
    "failed": "red",
    "skipped": "dim",
}

STATUS_ICON = {
    "queued": "○",
    "downloading": "▼",
    "verifying": "🔍",
    "stalled": "⚠",
    "retry": "↻",
    "last-resort": "♻",
    "done": "✔",
    "failed": "✘",
    "skipped": "–",
}


def fmt_bytes(n: float) -> str:
    if n <= 0:
        return "0 B"
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


def fmt_speed(bps: float) -> str:
    if bps <= 0:
        return "--"
    return f"{fmt_bytes(bps)}/s"


def build_app(*args, **kwargs):
    """Import textual lazily so the default inline UI works without it."""
    from textual.app import App, ComposeResult
    from textual.containers import Horizontal, Vertical
    from textual.widgets import Footer, Header, Label, ProgressBar, RichLog

    from .matcher import collect_candidates
    from .slsk import build_search_query, collect_search_results, cleanup_staging, download_all_tracks, make_track_plans

    class TrackRow(Horizontal):
        def __init__(self, number: int, title: str) -> None:
            super().__init__(id=f"track-{number}")
            self.number = number
            self.title = title

        def compose(self) -> ComposeResult:
            label = f"{self.number:02d} - {self.title}"
            if len(label) > 32:
                label = label[:31] + "…"
            yield Label(label, classes="tlabel")
            yield ProgressBar(total=100, show_eta=False, show_percentage=True)
            yield Label("queued", classes="tstatus")
            yield Label("", classes="tmeta")

        def update_progress(self, p) -> None:
            bar = self.query_one(ProgressBar)
            status = self.query_one(".tstatus", Label)
            meta = self.query_one(".tmeta", Label)
            total = p.total or 100
            bar.update(total=float(total), progress=float(min(p.bytes_done, total)))
            icon = STATUS_ICON.get(p.status, "•")
            style = STATUS_STYLE.get(p.status, "white")
            note = f" {p.note}" if p.note and p.status in ("failed", "stalled") else ""
            status.update(f"[{style}]{icon} {p.status}[/]")
            if p.status == "done":
                meta.update(f"[{style}]✔ {fmt_bytes(p.bytes_done)} ← {p.source}[/]")
            elif p.status in ("failed",):
                meta.update(f"[red]{p.note[:30]}[/]" if p.note else "")
            else:
                pct = f"{100*p.bytes_done/total:.0f}%" if total else ""
                src = f"← {p.source}" if p.source else ""
                att = f"try {p.attempt}/{p.attempts_total}" if p.attempt else ""
                meta.update(f"{fmt_bytes(p.bytes_done)}/{fmt_bytes(total)} {pct} {fmt_speed(p.speed)} {src} {att}".strip())

    class SoulsuckerApp(App):
        CSS = TEXTUAL_CSS
        TITLE = "soulsucker"

        def __init__(self, *, username, password, artist, album,
                     tracks, album_dir: Path, search_timeout=20.0,
                     stall_timeout=10.0, max_concurrent=4, port=60000,
                     length_tolerance=5.0, acoustid_key=None, fp_min_score=0.8,
                     last_resort_rounds=2, last_resort_stall_timeout=60.0):
            super().__init__()
            self._username = username
            self._password = password
            self._artist = artist
            self._album = album
            self._tracks = tracks
            self._album_dir = album_dir
            self._search_timeout = search_timeout
            self._stall_timeout = stall_timeout
            self._max_concurrent = max_concurrent
            self._port = port
            self._length_tolerance = length_tolerance
            self._acoustid_key = acoustid_key
            self._fp_min_score = fp_min_score
            self._last_resort_rounds = last_resort_rounds
            self._last_resort_stall_timeout = last_resort_stall_timeout
            self._rows: dict[int, TrackRow] = {}
            self._results: dict[int, bool] = {}
            self._log = None

        def compose(self) -> ComposeResult:
            yield Header(show_clock=True)
            yield Label(f"♪ {self._artist} — {self._album}  ({len(self._tracks)} tracks) → {self._album_dir}",
                        id="head")
            with Vertical(id="tracks"):
                for t in self._tracks:
                    row = TrackRow(t.number, t.title)
                    self._rows[t.number] = row
                    yield row
            yield RichLog(id="log", highlight=True, markup=True)
            yield Footer()

        def write_log(self, msg: str) -> None:
            try:
                self.query_one("#log", RichLog).write(msg)
            except Exception:
                pass

        async def on_mount(self) -> None:
            self.run_worker(self._run(), exclusive=True)

        async def _run(self) -> None:
            from aioslsk.client import SoulSeekClient
            from aioslsk.settings import CredentialsSettings, Settings

            staging = self._album_dir / ".staging"
            staging.mkdir(parents=True, exist_ok=True)
            settings = Settings(
                credentials=CredentialsSettings(username=self._username, password=self._password),
            )
            settings.shares.download = str(staging)
            try:
                settings.network.listening.port = int(self._port)
                settings.network.listening.obfuscated_port = int(self._port) + 1
            except Exception:
                pass

            client = SoulSeekClient(settings)
            self.write_log(f"[cyan]connecting to Soulseek as [b]{self._username}[/b]…[/]")
            try:
                await client.start()
                await client.login()
            except Exception as exc:
                self.write_log(f"[red]login failed: {exc}[/]")
                return
            self.write_log("[green]logged in ✔[/]")

            query = build_search_query(self._artist, self._album)
            self.write_log(f"[cyan]searching Soulseek for [b]{query}[/b] ({self._search_timeout:.0f}s)…[/]")
            try:
                results = await collect_search_results(client, query, self._search_timeout)
            except Exception as exc:
                self.write_log(f"[red]search failed: {exc}[/]")
                await client.stop()
                return
            flac_files = sum(len(getattr(r, "shared_items", [])) for r in results)
            self.write_log(f"[green]search done:[/] {len(results)} users, ~{flac_files} files")

            width = max(2, len(str(len(self._tracks))))
            plans = make_track_plans(
                self._tracks,
                {t.number: collect_candidates(t.title, results) for t in self._tracks},
                self._album_dir, width,
            )
            for plan in plans:
                fastest = plan.candidates[0].username if plan.candidates else "-"
                self.write_log(
                    f"[{ 'green' if plan.candidates else 'red'}]"
                    f"{plan.number:02d} {plan.title}: {len(plan.candidates)} FLAC sources"
                    f"{f' (fastest: {fastest})' if plan.candidates else ''}[/]"
                )
                if not plan.candidates:
                    try:
                        self._rows[plan.number].update_progress(_empty_failed(plan.number, plan.title))
                    except Exception:
                        pass

            async def cb(p) -> None:
                row = self._rows.get(p.number)
                if row is not None:
                    try:
                        row.update_progress(p)
                    except Exception:
                        pass
                if p.status == "done":
                    self.write_log(f"[green]✔ {p.number:02d} {p.title} ← {p.source}[/]")
                elif p.status == "failed":
                    self.write_log(f"[red]✘ {p.number:02d} {p.title}: {p.note}[/]")
                elif p.status == "verifying":
                    self.write_log(f"[cyan]🔍 {p.number:02d} {p.title}: {p.note}[/]")
                elif p.status == "last-resort":
                    self.write_log(f"[magenta]♻ {p.number:02d} {p.title}: {p.note}[/]")

            self.write_log(
                f"[cyan]downloading {len(plans)} tracks "
                f"(max {self._max_concurrent} parallel, stall timeout {self._stall_timeout:.0f}s)…[/]"
            )
            t0 = time.monotonic()
            try:
                self._results = await download_all_tracks(
                    client, plans, stall_timeout=self._stall_timeout,
                    max_concurrent=self._max_concurrent, progress_cb=cb,
                    length_tolerance=self._length_tolerance,
                    acoustid_key=self._acoustid_key,
                    fp_min_score=self._fp_min_score,
                    last_resort_rounds=self._last_resort_rounds,
                    last_resort_stall_timeout=self._last_resort_stall_timeout)
            finally:
                try:
                    await client.stop()
                except Exception:
                    pass
            cleanup_staging(staging)
            dt = time.monotonic() - t0
            ok = sum(1 for v in self._results.values() if v)
            self.write_log(f"[b]{'🎉' if ok == len(plans) else '🏁'} done: {ok}/{len(plans)} tracks in {dt:.0f}s → {self._album_dir}[/]")
            self.write_log("[dim]press q to quit[/]")

    def _empty_failed(number, title):
        from .slsk import TrackProgress
        return TrackProgress(number=number, title=title, status="failed", note="no FLAC candidates found")

    return SoulsuckerApp(*args, **kwargs)


# ------------------------------------------------- Inline live UI (default)
# Rich renders every task row and scrolls with the terminal, so unlike the
# fullscreen app no track can ever be clipped off-screen.
TRACK_LABEL_WIDTH = 42


def track_label(number: int, title: str, max_len: int = TRACK_LABEL_WIDTH) -> str:
    label = f"{number:02d} - {title}"
    if len(label) > max_len:
        label = label[: max_len - 1] + "…"
    return label


def format_track_info(p) -> str:
    """Plain-text status/source snippet for a track progress row."""
    if p.status == "done":
        return f"✔ ← {p.source}" if p.source else "✔ done"
    if p.status == "failed":
        return f"✘ {p.note[:48]}" if p.note else "✘ failed"
    if p.status == "stalled":
        return f"⚠ stalled ← {p.source}" if p.source else "⚠ stalled"
    if p.status == "verifying":
        return f"🔍 {p.note[:52]}" if p.note else "🔍 verifying…"
    if p.status == "retry":
        return f"↻ {p.note[:48]}" if p.note else "↻ retry"
    if p.status == "last-resort":
        return f"♻ {p.note[:48]}" if p.note else "♻ last-resort retry"
    src = f"← {p.source} " if p.source else ""
    att = f"· try {p.attempt}/{p.attempts_total}" if p.attempt else ""
    return f"{src}{att}".strip() or "queued"


def build_download_progress(console, tracks, cands_per_track):
    """Create a Rich Progress with one always-visible row per track."""
    from rich.progress import (
        BarColumn,
        DownloadColumn,
        Progress,
        SpinnerColumn,
        TextColumn,
        TimeRemainingColumn,
        TransferSpeedColumn,
    )
    from rich.table import Column

    progress = Progress(
        SpinnerColumn(finished_text="[green]✔[/]"),
        TextColumn("{task.fields[name]}", markup=False,
                   table_column=Column(max_width=TRACK_LABEL_WIDTH + 2, no_wrap=True, overflow="ellipsis")),
        BarColumn(bar_width=None, complete_style="magenta", finished_style="green", pulse_style="cyan"),
        TextColumn("{task.percentage:>3.0f}%", table_column=Column(width=5, no_wrap=True)),
        DownloadColumn(),
        TransferSpeedColumn(),
        TimeRemainingColumn(),
        TextColumn("{task.fields[info]}", markup=False,
                   table_column=Column(max_width=52, no_wrap=True, overflow="ellipsis")),
        console=console,
        transient=False,
    )
    task_ids: dict[int, int] = {}
    for t in tracks:
        cands = cands_per_track.get(t.number, []) or []
        total = cands[0].filesize if cands else 100
        task_ids[t.number] = progress.add_task(
            f"{t.number:02d}",
            total=(total or 100),
            name=track_label(t.number, t.title),
            info="queued",
        )
    return progress, task_ids


async def run_inline(*, username, password, artist, album, tracks,
                     album_dir: Path, search_timeout=20.0, stall_timeout=10.0,
                     max_concurrent=4, port=60000, length_tolerance=5.0,
                     acoustid_key=None, fp_min_score=0.8,
                     last_resort_rounds=2, last_resort_stall_timeout=60.0) -> dict[int, bool]:
    """Spotdl-style inline UI: scrollback-friendly log + live progress rows."""
    from collections import deque

    from rich.console import Console, Group
    from rich.live import Live
    from rich.markup import escape
    from rich.panel import Panel
    from rich.table import Table
    from rich.text import Text

    from .matcher import collect_candidates
    from .slsk import build_search_query, collect_search_results, cleanup_staging, download_all_tracks, make_track_plans

    console = Console()
    console.print(Panel(
        f"[bold magenta]♪ {escape(artist)} — {escape(album)}[/]  [dim]({len(tracks)} tracks)[/]\n"
        f"[dim]→ {escape(str(album_dir))}[/]",
        title="soulsucker", border_style="magenta", expand=False,
    ))

    staging = album_dir / ".staging"
    staging.mkdir(parents=True, exist_ok=True)

    from aioslsk.client import SoulSeekClient
    from aioslsk.settings import CredentialsSettings, Settings
    settings = Settings(credentials=CredentialsSettings(username=username, password=password))
    settings.shares.download = str(staging)
    try:
        settings.network.listening.port = int(port)
        settings.network.listening.obfuscated_port = int(port) + 1
    except Exception:
        pass

    client = SoulSeekClient(settings)
    console.print(f"[cyan]connecting as [bold]{escape(username)}[/]…[/]")
    await client.start()
    await client.login()
    console.print("[green]✔ logged in[/]")

    query = build_search_query(artist, album)
    with console.status(f"[cyan]searching Soulseek for '{query}' ({search_timeout:.0f}s)…[/]") as status:
        def tick(n: int) -> None:
            status.update(f"[cyan]searching Soulseek for '{query}' — {n} users so far…[/]")
        results = await collect_search_results(client, query, search_timeout, on_tick=tick)
    flac_files = sum(len(getattr(r, "shared_items", [])) for r in results)
    console.print(f"[green]✔ search done:[/] {len(results)} users, ~{flac_files} files")

    width = max(2, len(str(len(tracks))))
    cands = {t.number: collect_candidates(t.title, results) for t in tracks}
    table = Table(show_header=True, header_style="bold dim", box=None, pad_edge=False)
    table.add_column("#", style="dim", width=4)
    table.add_column("Track", overflow="ellipsis", max_width=48)
    table.add_column("FLAC", justify="right")
    table.add_column("Fastest peer", style="cyan", overflow="ellipsis", max_width=24)
    for t in tracks:
        n = len(cands[t.number])
        fastest = cands[t.number][0].username if n else "—"
        table.add_row(f"{t.number:02d}", t.title,
                      f"[green]{n}[/]" if n else "[red]0[/]", fastest)
    console.print(table)

    plans = make_track_plans(tracks, cands, album_dir, width)
    progress, task_ids = build_download_progress(console, tracks, cands)

    events: deque[str] = deque(maxlen=6)
    ok_count = 0
    fail_count = 0

    def render() -> Group:
        log = Text.from_markup("\n".join(events)) if events else Text("waiting for downloads…", style="dim")
        return Group(
            progress,
            Panel(log, title=f"events · {ok_count} done · {fail_count} failed",
                  border_style="dim", expand=False),
        )

    console.print(f"[cyan]downloading {len(plans)} tracks "
                  f"(max {max_concurrent} parallel, stall timeout {stall_timeout:.0f}s)…[/]")
    t0 = time.monotonic()
    with Live(render(), console=console, refresh_per_second=10) as live:
        async def cb(p) -> None:
            nonlocal ok_count, fail_count
            tid = task_ids.get(p.number)
            if tid is None:
                return
            total = p.total or (progress.tasks[tid].total or 100)
            if p.status == "done":
                progress.update(tid, completed=total, total=total, info=format_track_info(p))
                events.append(f"[green]✔ {p.number:02d} {escape(p.title)} ← {escape(p.source)}[/]")
                ok_count += 1
            elif p.status == "failed":
                progress.update(tid, info=format_track_info(p))
                events.append(f"[red]✘ {p.number:02d} {escape(p.title)}: {escape(p.note)}[/]")
                fail_count += 1
            elif p.status == "verifying":
                progress.update(tid, completed=min(p.bytes_done, total), total=total,
                                info=format_track_info(p))
                events.append(f"[cyan]🔍 {p.number:02d} {escape(p.title)}: {escape(p.note)}[/]")
            elif p.status == "last-resort":
                progress.update(tid, completed=min(p.bytes_done, total), total=total,
                                info=format_track_info(p))
                events.append(f"[magenta]♻ {p.number:02d} {escape(p.title)}: {escape(p.note)}[/]")
            else:
                progress.update(tid, completed=min(p.bytes_done, total), total=total,
                                info=format_track_info(p))
                if p.status == "stalled":
                    events.append(f"[yellow]⚠ {p.number:02d} {escape(p.title)} stalled — switching source[/]")
            live.update(render())

        try:
            res = await download_all_tracks(
                client, plans, stall_timeout=stall_timeout,
                max_concurrent=max_concurrent, progress_cb=cb,
                length_tolerance=length_tolerance,
                acoustid_key=acoustid_key, fp_min_score=fp_min_score,
                last_resort_rounds=last_resort_rounds,
                last_resort_stall_timeout=last_resort_stall_timeout)
        finally:
            try:
                await client.stop()
            except Exception:
                pass
    cleanup_staging(staging)
    dt = time.monotonic() - t0
    ok = sum(1 for v in res.values() if v)
    summary = Table(show_header=False, box=None, pad_edge=False)
    for t in tracks:
        summary.add_row("[green]✔[/]" if res.get(t.number) else "[red]✘[/]",
                        f"{t.number:02d} {t.title}")
    console.print(Panel(
        summary,
        title=f"{'🎉' if ok == len(plans) else '🏁'} {ok}/{len(plans)} in {dt:.0f}s → {album_dir}",
        border_style="green" if ok == len(plans) else "yellow",
        expand=False,
    ))
    return res


async def run_headless(*args, **kwargs) -> dict[int, bool]:
    """Deprecated alias: inline progress is now the default."""
    return await run_inline(*args, **kwargs)
