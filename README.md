# soulsucker ♫

A terminal album downloader that is actually pleasant to use. Pick a release
from **MusicBrainz**, and soulsucker finds matching **FLAC** or **320 kbps MP3**
files on **Soulseek**, downloads them in parallel, verifies what it got, and
files cleanly numbered tracks into an artist/album library.

Built in Rust with [`ratatui`](https://ratatui.rs/) for the TUI,
[`clap`](https://clap.rs/) for the command line, and
[`soulseek-rs-lib`](https://crates.io/crates/soulseek-rs-lib) for Soulseek.

```text
 ♫ soulsucker  MusicBrainz → Soulseek                    ● youruser · port 51234
╭ Search MusicBrainz ────────────────────────────────────────────────────────╮
│ ▸ Artist:           Radiohead                                              │
│   Album:            In Rainbows                                            │
│ Enter searches · Tab moves between fields                                   │
╰────────────────────────────────────────────────────────────────────────────╯
╭ Releases · 25 ─────────────────────────────────────────────────────────────╮
│ ▌  1. ★ In Rainbows  ·  Radiohead  recommended                             │
│      2007 · GB · Official · CD · 10 tracks                                 │
│    2.   In Rainbows  ·  Radiohead                                          │
│      2007 · JP · Official · CD · 10 tracks                                 │
│    …                                                                       │
╰────────────────────────────────────────────────────────────────────────────╯
```

## Features

soulsucker uses one pooled Soulseek search, local track matching,
duration/fingerprint verification, and automatic retries:

- **New TUI** with a clear screen flow, live per-track progress, an activity
  feed, help everywhere, and adaptive columns.
- **One-time setup.** Soulseek credentials are asked for once, written with
  mode `0600`, and no longer requested at every launch.
- **No per-track search spam.** One `artist album` search feeds every track;
  matching happens locally.
- **Better source selection.** Each remote file is assigned to the track it
  matches best, so similarly named tracks cannot steal each other's sources.
  Matching score comes first, then free slots, peer speed, duration closeness
  and file size. Reported durations are used to reject cross-track matches
  before bytes are spent.
- **Resilient transfers.** Per-attempt staging folders, queue/stall
  watchdogs, next-source fallback, and last-resort passes with more patience.
- **Actual verification.** Finished files are checked against the
  MusicBrainz duration, and optionally against Chromaprint/AcoustID.
- **Clean output.** Metadata is overwritten from the chosen MusicBrainz
  release — canonical album/artist/title/date, track/disc numbers, MusicBrainz
  IDs, and cover art from the iTunes Search API (with the Cover Art Archive as
  a fallback) — and files are named `01 - Title.flac` (or `1-01 - Title.flac`
  for multi-disc releases; `.mp3` in MP3 mode). An optional **disc-prefixed
  track numbers** setting writes tags like `101…113` and `201…213` for players
  that ignore the disc tag (Tesla's USB player, for example).
- **FLAC or MP3 320.** Choose lossless FLAC or 320 kbps MP3 in Settings. MP3
  mode prefers a verified 320 kbps MP3 source; when none is available it takes
  a FLAC source and converts it after verification. The conversion and loudness
  normalization share the same second pass.
- **Loudness normalization.** An optional two-pass EBU R128 pass brings each
  download to -16 LUFS with a -1.5 dBTP true-peak ceiling, so quiet and loud
  albums sit at a similar level in a car or on a portable player.
- **Skip or force.** Valid existing tracks are skipped by default; `--force`
  downloads them again. A stray/empty file with the right name is validated and
  replaced rather than trusted.
- **In-app updates.** Settings → **Check for updates** asks GitHub for the
  latest release, and when it is newer downloads the matching binary and
  replaces the running executable. Restart soulsucker to use it.

## Requirements

- Rust **1.91+** (edition 2024).
- A Soulseek account.
- FFmpeg **8.x** development libraries to build from source. Prebuilt release
  binaries link FFmpeg and the bundled LAME MP3 encoder statically and need no
  runtime libraries.
- A terminal at least **60×20** cells (the TUI asks for this if it is too small).
- Optional: [`fpcalc`](https://acoustid.org/chromaprint) from
  `chromaprint-tools` for audio fingerprinting.
- Optional: a free AcoustID API key from <https://acoustid.org/api> for
  fingerprint verification. Without it, duration checks still run.

On Fedora:

```bash
sudo dnf install ffmpeg ffmpeg-devel
```

On Debian/Ubuntu:

```bash
sudo apt install ffmpeg libavcodec-dev libavformat-dev libavfilter-dev \
  libavutil-dev libswresample-dev chromaprint-tools
```

## Build / install

```bash
cargo build --release
./target/release/soulsucker
# or
cargo install --path .
```

The normal build links the system FFmpeg shared libraries. For a
self-contained binary that needs no FFmpeg runtime libraries, build FFmpeg
statically:

```bash
cargo build --release --features ffmpeg-static
```

That path needs `git`, `make`, `nasm`, and a C compiler, and takes several
minutes the first time. Release binaries are built this way.

## First run

Run the app with no arguments:

```bash
soulsucker
```

If no credentials are configured, a setup screen asks for:

1. Soulseek username
2. Soulseek password
3. AcoustID API key (optional; leave blank to skip)

They are saved to the config file shown on screen, normally:

| Platform | Path |
| --- | --- |
| Linux | `~/.config/soulsucker/config.toml` |
| macOS | `~/Library/Application Support/soulsucker/config.toml` |
| Windows | `%APPDATA%\soulsucker\config.toml` |

`music_dir` defaults to the platform music folder (`~/Music` on most Linux and
macOS setups). Each artist gets a folder there, with album folders inside it:
`<music folder>/<Artist>/<Album>/`.

On Unix the file and its directory are created with owner-only permissions.
The password is stored in plain text, so treat the file like any other secret. Set `SOULSUCKER_CONFIG` or use `--config FILE` to
put it somewhere else.

You can change everything later in **Settings (`F2`)**. Credential-related CLI
flags (`--username`, `--password`, `--acoustid-key`) are also saved, so they
only have to be supplied once.

## Usage

```bash
# Open the TUI on the search screen
soulsucker

# Prefill the search
soulsucker "Metallica" "Ride the Lightning"
soulsucker -a "Metallica" -A "Ride the Lightning"

# Save a different source folder for this run
soulsucker -o ~/Music

# Pick the recommended release and start immediately
soulsucker -a "Radiohead" -A "In Rainbows" --yes

# Re-download tracks that already exist
soulsucker -a "Radiohead" -A "In Rainbows" --force

# Re-open the credential screen
soulsucker --setup

# Disable fingerprinting for this run
soulsucker --no-fingerprint
```

### What happens when you press Download

1. MusicBrainz returns releases sorted **standard edition first**, preferring
   the **original release and country** inferred from the earliest official
   release. Later reissues get a small tie-break penalty. Deluxe, anniversary,
   live, expanded, bonus-heavy and bootleg releases are penalised.
2. You pick a release; soulsucker fetches its full audio tracklist, including
   multi-disc releases and skipping DVD/Blu-ray mediums.
3. The destination is created at `<music folder>/<Artist>/<Album>/`.
   Releases MusicBrainz marks as **Single** skip the album folder and are saved
   as loose files directly in `<music folder>/<Artist>/`, alongside the album
   folders.
4. One Soulseek search for `"artist album"` runs for the configured timeout.
5. In FLAC mode, every FLAC result is matched against every track locally. In
   MP3 320 mode, MP3 results identified as 320 kbps are preferred and FLAC is
   kept as a fallback. Junk (`sample`, `preview`, `snippet`), the wrong format,
   tiny files and wildly wrong durations are ignored.
6. Tracks download in parallel. Each attempt gets its own hidden staging
   folder inside the destination folder, so partial files can never be mistaken
   for finished ones.
7. When a transfer completes, its duration is checked. With an AcoustID key
   and `fpcalc`, the Chromaprint fingerprint is also checked: the expected
   MusicBrainz recording must appear in AcoustID's **top-scoring cluster**.
   MP3 sources are also checked for a genuine 320 kbps bitrate.
8. A wrong version, a stall, a queue timeout or a rejected transfer moves on
   to the next-best source. If every source fails, the list is retried in
   last-resort passes with longer patience.
9. Only after verification, an accepted file is normalized and/or converted.
   In MP3 mode a FLAC source becomes a 320 kbps MP3 here; with loudness
   normalization enabled, that conversion shares the second normalization
   pass.
10. Accepted files are renamed, then retagged from the chosen MusicBrainz
    release: title, track artist, album artist, album, release date,
    track/disc numbers, and MusicBrainz IDs. Cover art is fetched from the
    iTunes Search API first, with the Cover Art Archive as a fallback.
    Embedded art from the source is replaced, so a file cannot keep a cover
    from a different edition. If **disc-prefixed track numbers** is enabled,
    multi-disc track tags become `101…113`, `201…213`, and so on.

## Keys

Global keys work everywhere (while a text field is focused, printable keys
edit the field; `F1` still opens help):

| Key | Action |
| --- | --- |
| `F1` / `?` | Help for the current screen |
| `F2` | Settings |
| `F5` | Reconnect to Soulseek |
| `Ctrl+Q` / `Ctrl+C` | Quit (asks first if downloads are running) |
| `Esc` | Back / cancel the current edit |

Settings is unavailable while a download is running or a MusicBrainz request
is in flight; `Esc` cancels the request so you can open it.

### Search

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Move between artist, album and results |
| `Enter` | Search (from album) / open release (from results) |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | Choose a release |
| `/` or `e` | Edit the album field |
| `r` | Run the search again |
| `s` | Settings |

### Tracklist

| Key | Action |
| --- | --- |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | Scroll |
| `d` / `Enter` | Start downloading |
| `f` | Toggle fingerprint verification for this run |
| `b` / `Esc` | Back to the search results |
| `s` | Settings |

### Download

| Key | Action |
| --- | --- |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | Select a track |
| `Tab` | Switch focus between tracks and the activity log |
| `x` | Cancel the selected track |
| `r` | Retry failed tracks |
| `n` | Cancel and start a new search |
| `o` | Open the album folder in the file manager |
| `Esc` | Back to search once finished (or cancel a pending login) |
| `q` | Quit |

### Settings

| Key | Action |
| --- | --- |
| `←` `→` / `Tab` | Switch tabs |
| `↑` `↓` `Home` `End` | Select a field in the current tab |
| `Enter` | Edit / toggle / cycle / run action |
| `Ctrl+S` | Save and return |
| `Esc` | Discard and return |

Settings is split into four tabs:

- **Account** — Soulseek credentials, AcoustID key, fingerprint checking and
  score.
- **Library** — music folder, FLAC/MP3 320 output, Tesla-friendly tags and
  loudness normalization.
- **Downloading** — parallel downloads, search/queue/stall timeouts, length
  tolerance, retry passes and sources per track.
- **Advanced** — listener port, MusicBrainz result count and the update check.

**Check for updates** compares the running version with the latest public
GitHub release. If a newer release exists, it downloads the matching raw
binary, checks the executable header, and replaces the running file atomically.
The current process keeps running the old version until you restart. Automatic
updates work with the prebuilt Linux x86_64, macOS ARM64/Intel, and Windows
x86_64 releases.

## Configuration

The config is TOML. A representative file:

```toml
username = "your-soulseek-name"
password = "your-password"
acoustid_key = "your-acoustid-key"

music_dir = "/home/you/Music"
audio_quality = "flac"
max_concurrent = 4
search_timeout_secs = 20
queue_timeout_secs = 30
stall_timeout_secs = 15
length_tolerance_secs = 5.0
fingerprint = true
fingerprint_min_score = 0.8
last_resort_rounds = 2
last_resort_stall_timeout_secs = 60
candidates_per_track = 12
listen_port = 0
mb_limit = 25
disc_prefix_track_numbers = false
album_artist_for_tracks = false
normalize_loudness = false
```

`listen_port = 0` asks the OS for a free port, which is friendlier than a
fixed port when several apps use the network. If you need port forwarding,
set a specific port and forward it on your router.

`audio_quality = "flac"` keeps lossless output. Set it to `"mp3_320"` to write
320 kbps MP3 files instead. MP3 mode prefers a Soulseek source whose reported
bitrate or size/duration identifies a 320 kbps MP3; every downloaded MP3 is
also checked after transfer so a mislabeled 128 kbps file is rejected. If no
320 kbps MP3 source is available, soulsucker downloads a FLAC and converts it
after the duration and AcoustID checks. MP3 sources are kept untouched when
normalization is off; with normalization on they are decoded and re-encoded at
320 kbps as part of the two-pass normalization. If conversion fails for a
FLAC-to-MP3 track, that source is rejected and the next source is tried.

`disc_prefix_track_numbers = false` keeps standard per-disc track tags. Set it
to `true` for players that ignore the disc tag (notably Tesla's USB player):
multi-disc tracks are then tagged `101…113`, `201…213`, and so on. Album names
and filenames stay unchanged. The setting applies when tags are written, so
enable it and rerun with `--force` to retag an existing download.

`album_artist_for_tracks = false` keeps each MusicBrainz track credit in
`ARTIST`. Set it to `true` for players that group by track artist (notably
Tesla's USB player): every track gets the album artist in `ARTIST`, while the
original credit is preserved in `ARTISTS`. Guest and live tracks then stay
inside the album instead of creating a second artist entry. This also applies
when tags are written, so enable it and rerun with `--force` to retag an
existing download.

`normalize_loudness = true` runs a two-pass EBU R128 normalization on each
downloaded track before it is tagged. The first pass measures the track with
FFmpeg's `loudnorm` filter; the second pass applies **-16 LUFS** integrated
loudness, a **-1.5 dBTP** true-peak ceiling, and an **11 LU** loudness range.
FLAC output is re-encoded losslessly. In MP3 320 mode the same second pass also
performs the FLAC-to-MP3 conversion, so there is no separate conversion pass.
The processed file replaces the original only after the entire encode succeeds.
If normalization fails, an already-correct source is kept and tagged
unprocessed; a FLAC source that still needs MP3 conversion is rejected so
another source can be tried. Existing files are skipped unless you rerun with
`--force`.

## CLI options

Run `soulsucker --help` for the full list. Useful ones:

| Flag | Meaning |
| --- | --- |
| `-a`, `-A` | Artist / album |
| `-o`, `--output` | Root music folder for this run |
| `--yes` | Auto-pick the recommended release and download |
| `--force` | Re-download existing files |
| `--no-fingerprint` | Duration checks only for this run |
| `--search-timeout` | Seconds to collect Soulseek results |
| `--queue-timeout` | Seconds to wait in a peer queue |
| `--stall-timeout` | Seconds without progress before switching source |
| `--max-concurrent` | Parallel tracks |
| `--length-tolerance` | Allowed MusicBrainz duration difference |
| `--fingerprint-min-score` | Minimum AcoustID match score |
| `--last-resort-rounds` | Extra passes over exhausted sources |
| `--last-resort-stall-timeout` | Patience during those passes |
| `--candidates` | Sources tried per track |
| `--port` | Soulseek listener port |
| `--limit` | MusicBrainz releases to fetch |
| `--config` | Alternate config file |
| `--setup` | Force the credential screen |

Credential flags (`--username`, `--password`, `--acoustid-key`) are written to
the config file. Other flags override the stored settings for the current run
only; use Settings (`F2`) to make a value the new saved default. `--verbose`
prints the Soulseek library's raw logs after the TUI exits; those lines can
contain peer-supplied text, so treat them as untrusted.

## Troubleshooting

- **Login rejected.** Open Settings (`F2`) and check the username/password.
  If another client is logged in with the same account, the server will
  displace one of them.
- **MusicBrainz is busy.** The app retries `429/502/503` responses with a
  backoff. If it still fails, wait a moment and press `r`.
- **No matching sources.** Soulseek is peer-to-peer: availability changes by
  the minute. Try a longer search timeout, another pressing/edition, set
  **Audio quality** to FLAC, or search again later.
- **A track keeps failing.** Open the activity log (`Tab`) to see whether it
  was a queue timeout, a stall, a duration mismatch or an AcoustID mismatch.
- **Fingerprinting does not run.** Install `fpcalc` and/or add an AcoustID
  key. Without them, duration checks still reject wrong-length versions.
- **Stale partial files.** They live in the destination folder's hidden
  `.soulsucker-staging` directory and are cleaned up at the start of the next
  download for that release. Quitting also tries to remove them.
- **Two track 1s on a multi-disc album (Tesla).** Tesla's USB player ignores
  the disc tag and sorts only by track number. Enable Settings →
  **Disc-prefixed tracks** (or `disc_prefix_track_numbers = true`) and rerun
  with `--force` to retag; tracks then appear as `101…`, `201…`.
- **The same album appears under a second artist (Tesla).** A guest or live
  track carries a different MusicBrainz `ARTIST` credit. Enable Settings →
  **Album artist on tracks** (or `album_artist_for_tracks = true`) and rerun
  with `--force`; every track is then grouped under the album artist, with the
  original credit kept in `ARTISTS`.
- **Update failed with a permission error.** soulsucker cannot replace a binary
  in a read-only or system-owned directory. Move it somewhere writable, install
  it under your home directory, or download the release asset manually.
- **Firewalled peer.** Soulseek can broker connections, but two peers both
  behind strict NAT may not be able to transfer. The app simply tries another
  source.

## Notes

Soulseek is peer-to-peer. Downloads depend on who is online and sharing, and
peers set their own queues and speeds. The Soulseek server allows one session
per account, so do not run two instances with the same login. This client only
downloads; it does not share folders. Be excellent: respect artists and other
users, and only download material you have the rights to.

This project is not affiliated with Soulseek, MusicBrainz or AcoustID.

MP3 encoding uses the bundled [LAME](https://lame.sourceforge.io/) library,
which is licensed under the LGPL. The project source is available in this
repository.
