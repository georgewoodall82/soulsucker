# soulsucker 🧛🎶

Awesome terminal music downloader: give it **artist + album**, pick the right
[MusicBrainz](https://musicbrainz.org/) release, and it pulls every track as
**FLAC** off [Soulseek](https://www.slsknet.org/) via
[`aioslsk`](https://github.com/JurgenR/aioslsk) — with a live inline
progress view (fullscreen [Textual](https://textual.textualize.io/) UI
available via `--tui`).

## How it works

1. `soulsucker "Metallica" "Ride the Lightning"` (or run bare for prompts).
2. Searches MusicBrainz, lists releases **sorted standard-first** — deluxe /
   anniversary / live / expanded editions are penalised so the plain album
   floats to the top as ★ recommended.
3. Fetches the release tracklist, creates `./Artist - Album/`.
4. **One** Soulseek search for `"artist album"` (fast — no per-track search
   spam), then matches each MusicBrainz track against the pooled results
   locally (fuzzy title match, FLAC-only).
5. Downloads tracks **individually** (never blind whole-folder grabs, so no
   junk/missing files), fastest users first (`free slots → speed → queue`),
   `N` tracks in parallel.
6. Per-track watchdog: failure / **10 s stall** → abort partial file, try the
   next-fastest source automatically.
   Last resort: if a track burns through every source (common with niche
   albums), its sources are retried up to 2 more passes with **60 s** stall
   patience (`♻` in the event feed) before giving up — tune with
   `--last-resort-rounds` / `--last-resort-stall-timeout`.
7. Files land as `01 - Fight Fire with Fire.flac` (zero-padded, sanitised),
   with the MusicBrainz track number (and total) stamped into each file's
   metadata — source tags are often wrong, so this is overwritten.

## Install

```bash
pip install -e .
# or: pip install aioslsk musicbrainzngs textual rich
```

You need a Soulseek account (username + password). You can pass
`-u/-p`, use env vars (`SLSK_USERNAME`/`SLSK_PASSWORD`), or let it prompt you
(it can remember them in `~/.config/soulsucker/config.json`).

## Use

```bash
# interactive
soulsucker
# args
soulsucker "Metallica" "Ride the Lightning"
soulsucker --artist "Metallica" --album "Ride the Lightning" -o ~/Music
# non-interactive release pick (great for ssh/logs)
soulsucker -a "Metallica" -A "Ride the Lightning" --yes
# tuning
soulsucker -a X -A Y --search-timeout 30 --stall-timeout 10 --max-concurrent 4 --port 60000
python -m soulsucker -a X -A Y
# fullscreen Textual UI instead of the default inline view
soulsucker -a X -A Y --tui
```

During download you get an inline live view (normal terminal scrollback,
nothing hidden): one row per track, in album order, each with a colourful
progress bar, speed, source user and retry status, plus an event feed:

- spinner/downloading · `⚠ stalled` → auto-switches source ·
  `↻ retry` · `✔ done` green · `✘ failed` red.
- Every finished track is length-checked against MusicBrainz (±5 s by
  default, `--length-tolerance`): wrong versions (live cuts, mislabels)
  are discarded and the next-fastest source is tried.
- Optional Chromaprint/AcoustID fingerprint check per finished track.

## Fingerprint verification

Each finished track can be verified by audio content, not just title and
length: `fpcalc` (install `chromaprint-tools`) fingerprints the file and
AcoustID resolves it to candidate MusicBrainz recording IDs. The expected
recording must be linked to AcoustID's top-scoring cluster — a mere
lower-ranked appearance doesn't count, since those are often mislinked noise
(an S&M live cut once hid behind its studio ID that way). One exception:
if AcoustID holds no fingerprints at all for the expected recording
(obscure pressings happen), a "different recording" verdict would be baseless,
so the file is kept on duration instead of rejected. Mismatches are discarded and the next-fastest source tried;
while checking, rows show `🔍 computing Chromaprint fingerprint…` /
`🔍 querying AcoustID…` plus result lines like `AcoustID match 0.94`.

```bash
soulsucker -a "Alice in Chains" -A "Jar of Flies" --acoustid-key YOUR_KEY
export ACOUSTID_API_KEY=YOUR_KEY   # or "acoustid_key" in ~/.config/soulsucker/config.json
```

Get a free key at https://acoustid.org/api. Without a key (or with
`--no-fingerprint`) only the duration check runs. `--fingerprint-min-score`
(default 0.8) sets how confident the match must be; files AcoustID has never
seen are kept on duration rather than rejected.

## Notes

- Soulseek results vary by who's online — if a track has zero FLAC sources,
  try a longer `--search-timeout` or a different MusicBrainz pressing.
- First login can take a few seconds; the server rate-limits reconnects, so
  don't hammer it.
- Be excellent: share, respect artists, only grab what you own the rights to.
