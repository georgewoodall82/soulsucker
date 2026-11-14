# soulsucker ♫

A terminal album downloader that is actually pleasant to use. Pick a release
from **MusicBrainz**, and soulsucker finds matching **FLAC** files on
**Soulseek**, downloads them in parallel, verifies what it got, and files
cleanly numbered tracks into an artist/album library.

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
  for multi-disc releases). An optional **disc-prefixed track numbers** setting
  writes tags like `101…113` and `201…213` for players that ignore the disc tag
  (Tesla's USB player, for example).
- **Skip or force.** Valid existing tracks are skipped by default; `--force`
  downloads them again. A stray/empty file with the right name is validated and
  replaced rather than trusted.

## Requirements

- Rust **1.91+** (edition 2024).
- A Soulseek account.
- A terminal at least **60×20** cells (the TUI asks for this if it is too small).
- Optional: [`fpcalc`](https://acoustid.org/chromaprint) from
  `chromaprint-tools` for audio fingerprinting.
- Optional: a free AcoustID API key from <https://acoustid.org/api> for
  fingerprint verification. Without it, duration checks still run.

On Debian/Ubuntu:

```bash
sudo apt install chromaprint-tools
```

## Build / install

```bash
cargo build --release
./target/release/soulsucker
# or
cargo install --path .
```

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
5. Every FLAC result is matched against every track locally. Junk (`sample`,
   `preview`, `snippet`), non-FLAC files and tiny files are ignored. A
   candidate whose reported duration is wildly different from MusicBrainz is
   dropped.
6. Tracks download in parallel. Each attempt gets its own hidden staging
   folder inside the destination folder, so partial files can never be mistaken
   for finished ones.
7. When a transfer completes, its duration is checked. With an AcoustID key
   and `fpcalc`, the Chromaprint fingerprint is also checked: the expected
   MusicBrainz recording must appear in AcoustID's **top-scoring cluster**.
8. A wrong version, a stall, a queue timeout or a rejected transfer moves on
   to the next-best source. If every source fails, the list is retried in
   last-resort passes with longer patience.
9. Accepted files are renamed, then retagged from the chosen MusicBrainz
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
| `↑` `↓` `Home` `End` | Select a field |
| `Enter` | Edit / toggle |
| `Ctrl+S` | Save and return |
| `Esc` | Discard and return |

Settings covers credentials, the AcoustID key, fingerprint on/off, music
folder, parallel downloads, search/queue/stall timeouts, length tolerance,
AcoustID score, last-resort passes and patience, sources per track, listener
port (0 = automatic), MusicBrainz result count, and disc-prefixed track numbers
for players that ignore disc tags (Tesla and similar car stereos).

## Configuration

The config is TOML. A representative file:

```toml
username = "your-soulseek-name"
password = "your-password"
acoustid_key = "your-acoustid-key"

music_dir = "/home/you/Music"
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
```

`listen_port = 0` asks the OS for a free port, which is friendlier than a
fixed port when several apps use the network. If you need port forwarding,
set a specific port and forward it on your router.

`disc_prefix_track_numbers = false` keeps standard per-disc track tags. Set it
to `true` for players that ignore the disc tag (notably Tesla's USB player):
multi-disc tracks are then tagged `101…113`, `201…213`, and so on. Album names
and filenames stay unchanged. The setting applies when tags are written, so
enable it and rerun with `--force` to retag an existing download.

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
- **No matching FLAC sources.** Soulseek is peer-to-peer: availability changes
  by the minute. Try a longer search timeout, another pressing/edition, or
  search again later.
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
