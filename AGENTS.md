# AGENTS.md

Practical notes for agents working on **soulsucker**.

## Scope

- The Rust crate lives at the repository root.
- User-facing documentation lives in `README.md`. Update it when behavior, keys, flags, or settings change.

## Quick commands

Run from the repository root:

```bash
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

- Rust **1.91+**, edition **2024**.
- The test suite is fast and offline. Keep it that way; do not add tests that hit MusicBrainz, iTunes, AcoustID, or Soulseek.
- The release binary is `target/release/soulsucker`.
- `cargo run --release` starts the TUI and needs a real terminal. Do not run the interactive TUI in a non-interactive shell; use `--help`, unit tests, and clippy for validation.
- For manual testing without touching the user's real config/credentials, pass `--config /tmp/opencode/soulsucker-test.toml` (or set `SOULSUCKER_CONFIG`).

## Repository layout

```text
.
├── Cargo.toml
├── README.md
├── LICENSE
├── src/
│   ├── main.rs          startup, config load, logger buffering, TUI lifecycle
│   ├── app.rs           App state, screens, key handling, settings, async event routing
│   ├── ui.rs            all Ratatui rendering, popovers, footer/help text
│   ├── session.rs       Soulseek session thread, album download engine, retries
│   ├── musicbrainz.rs   release search/ranking, release details, cover lookup
│   ├── matcher.rs       local fuzzy matching and source selection
│   ├── acoustid.rs      fpcalc + AcoustID verification
│   ├── audio.rs         FLAC inspection, metadata tagging, cover embedding
│   ├── config.rs        TOML config, normalization, secure atomic saves
│   ├── cli.rs           clap arguments and persistence semantics
│   ├── input.rs         reusable text input editing
│   └── util.rs          formatting, filename/path helpers, display width
├── tests/fixtures/
│   └── silence.flac     tiny real FLAC used by the audio tagging test
└── .github/workflows/
    └── release.yml      tag-triggered multi-platform GitHub release
```

There is no library target. `src/main.rs` declares every module; add new modules there. Unit tests are inline `#[cfg(test)]` modules. Integration tests cannot import private crate internals, so put most tests next to the code they test.

## Architecture at a glance

The app uses **threads + `std::sync::mpsc`**, not async/Tokio.

```text
main
 └─ App (UI thread: crossterm + ratatui)
     ├─ MusicBrainz worker threads ──> UiEvent::MbSearch / UiEvent::MbRelease
     └─ SessionHandle ──> session thread (owns soulseek-rs Client)
           ├─ search worker ────────> SessionCommand::SearchDone
           ├─ verification workers ─> SessionCommand::Verified
           └─ cover worker ─────────> SessionCommand::CoverReady
```

Key rules:

- Never block the UI thread on network, disk-heavy work, or Soulseek calls.
- Never block the session loop for long; it polls transfers on a roughly 80 ms cadence.
- Put slow work in a named worker thread and send a `SessionCommand` or `UiEvent` back.
- Convert `anyhow` errors to `String` at thread/event boundaries; the UI displays strings.
- Use `Arc<Client>` for shared Soulseek access; the session thread owns the `Client` lifecycle.

## Module map

### `app.rs`

Owns all UI state: `Screen`, `SearchView`, `TracksView`, `DownloadView`, `SettingsForm`, connection state, logs, toasts, confirms, help, and the `SessionHandle`.

Important patterns:

- Screens: `Setup`, `Search`, `Tracks`, `Download`, `Settings`.
- `TracksView::build` owns destination paths: album releases go to `<music_dir>/<Artist>/<Album>/`, while MusicBrainz release-group type `Single` is saved as loose files in `<music_dir>/<Artist>/`. Changing this layout means changing `TracksView::build` and its path tests.
- `UiEvent` includes a `generation`; ignore stale results when the generation no longer matches.
- `App::run` drains events, renders, polls keys, then ticks. Don't do blocking work here.
- MusicBrainz search/release requests are spawned as named threads.
- `pending_start` holds a download request while waiting for Soulseek login.
- `cfg` is the runtime config (includes CLI overrides); `persisted_cfg` is what Settings edits and saves. Do not accidentally persist runtime-only CLI overrides.
- Settings are edited through `SettingsForm`; saving validates and calls `config::save`, then may respawn the session if credentials/listener settings changed.
- `DownloadRequest` snapshots download options at start. Changing settings mid-download does not affect the active album.

### `ui.rs`

All Ratatui rendering lives here. `ui::render` dispatches by `app.screen`.

- Keep business logic out of rendering.
- When adding a key or screen, update footer hints and help text here, plus `README.md`.
- Soulseek peer strings are untrusted. Use `util::display_width`, `util::truncate`, and `util::fit` for layout; use `util::sanitize_filename` for filenames.
- Use `app.push_log` / `SlskEvent::Log` for activity messages. Never print raw logs to stdout/stderr while the TUI owns the terminal.
- `main.rs` buffers the Soulseek library's stderr logs and only flushes them after the TUI restores, and only with `--verbose`.

### `session.rs`

The background album engine. This is the most state-heavy file.

Main flow:

1. `start_album` checks existing files, starts cover lookup, and runs one pooled Soulseek search for the whole album.
2. `search_done` converts hits into candidates with `matcher::build_candidates`.
3. `schedule` starts tracks up to `max_concurrent`.
4. `start_candidate` downloads into a unique per-attempt staging directory.
5. `poll_downloads` watches queue/stall/progress, rejects bad attempts, and queues completed files for verification.
6. `begin_verification` removes the library download record, finds the staged non-`.part` file, and verifies it in a worker.
7. `verified` promotes the file to its final name, writes MusicBrainz tags and cover art, and marks the track done.
8. Failures go through `fail_attempt` and then `advance_after_failure`: next candidate, last-resort rounds, or `Failed`.
9. `check_completion` fires when every track is `Done`, `Failed`, or `Cancelled`.

Track states: `Queued`, `Downloading`, `Verifying`, `Retrying`, `Done`, `Failed`, `Cancelled`.
Album phases: `Searching`, `Downloading`, `Complete`.

### `musicbrainz.rs`

- `search_releases` builds a Lucene query, fetches candidates, and ranks them.
- **`ReleaseCandidate.score` is a penalty: lower is better.** The list sorts ascending. Tests assert a standard album scores lower than a deluxe edition.
- `rank_candidates` infers the original country from the earliest official release with a country, then adds `FOREIGN_COUNTRY_PENALTY` to other pressings. The penalty is deliberately smaller than the edition/status penalties, so it breaks pressing ties without promoting deluxe or bootleg releases.
- `get_release` requests `recordings+artist-credits+release-groups`, skips video media, numbers audio discs, and captures track/recording MBIDs and per-track artist credits.
- `ReleaseInfo.primary_type` comes from the release group. `is_single()` makes `TracksView::build` save singles as loose files in the artist folder instead of creating an album folder.
- `fetch_cover` order:
  1. iTunes album search (`entity=album`, `attribute=albumTerm`)
  2. iTunes song search fallback (some albums, including Dirt, are absent from Apple's album index but song results still carry the album cover)
  3. Cover Art Archive `front-500` by MusicBrainz release MBID
- iTunes matching uses `matcher::normalize_text` and `matcher::strip_release_noise`. Changes to those functions affect cover matching too.
- iTunes artwork URLs are upgraded from `100x100bb` to `600x600bb`.
- iTunes collection names append storefront types such as `Ep II - EP`; `strip_itunes_suffix` removes ` - EP`, ` - Single`, and ` - Album` before matching. Keep explicit edition markers like `(Live)`, which are meaningful.
- MusicBrainz requests are throttled to roughly one per 1.1 s with a proper User-Agent and retries. Keep it that way.

### `matcher.rs`

Turns Soulseek search hits into ranked per-track candidates.

- Filters non-FLAC, tiny/huge files, samples/previews/snippets, and bad durations.
- Scores title similarity, duration closeness, free slots, peer speed, and file size.
- Deduplicates sources across the album so two tracks don't grab the same remote file.
- `normalize_text` is shared by MusicBrainz cover matching.
- `strip_release_noise` removes format/edition noise and is also used by iTunes matching. It is `pub(crate)` for that reason.
- Constraints such as `MIN_MATCH_SCORE`, size bounds, and per-user-per-track limits are intentional.

### `acoustid.rs`

- Duration checking always runs, even without an AcoustID key or `fpcalc`.
- Fingerprint verification is best-effort: missing key, missing `fpcalc`, timeout, or AcoustID outage falls back to accepting on duration.
- A clear top-cluster mismatch rejects the file.
- `fpcalc` is run with a timeout and killed if it hangs.
- AcoustID requests are throttled and cached where appropriate.

### `audio.rs`

- `duration_secs` requires a real FLAC container and a positive duration.
- `write_tags` overwrites canonical MusicBrainz fields and strips non-primary tag blocks from FLACs (e.g. stray ID3v2/APE) so old source metadata cannot survive.
- When a cover is supplied, all embedded pictures are replaced with it.
- `write_cover` is used for late-arriving covers and leaves the other tags untouched.
- Tagging is best-effort: a tag/cover failure must not fail the download.

### `config.rs`

- TOML config contains the Soulseek password and AcoustID key in plaintext. Use `config::save`; it validates, writes atomically, tightens permissions to `0600` on Unix, and handles replacement safely. Do not add a plain `fs::write` path.
- `Config::normalize` clamps hand-edited/CLI values into usable ranges. Any new numeric setting needs a sensible normalize rule.
- `Config`'s `Debug` implementation redacts secrets. Keep it that way.
- Config path: `SOULSUCKER_CONFIG` if set, otherwise the platform config dir from `directories::ProjectDirs` (normally `~/.config/soulsucker/config.toml`). `--config FILE` overrides for the run.

### `cli.rs`

- Credential flags (`--username`, `--password`, `--acoustid-key`) are persisted to the config file.
- All other flags are runtime-only overrides; they do not change stored settings.
- `--setup` forces the first-run credential screen.
- `--force` redownloads existing tracks, which is also required to retag already-valid files.
- `--yes` auto-picks the recommended release and starts downloading; it requires artist and album.

## Critical Soulseek/library invariants

These are easy to break and hard to debug.

1. **Download tokens are not stable.** `soulseek-rs-lib` replaces the token when a peer sends its `TransferRequest`. Always locate a live transfer with `find_live_download(downloads, username, path)`, never by the original token.
2. **Keep cancelled records when the library asks.** If `cancel_download` returns true, leave the library's `Cancelled` entry in place. It is what lets the library deny a late peer transfer offer. Only call `remove_download` when cancellation returned false.
3. **The library flattens downloads by basename.** Two tracks or attempts with the same filename can collide. Every attempt gets its own staging directory under `.soulsucker-staging`; do not download directly to a final path.
4. **Ignore stale async results.** Before mutating state, check `generation` (UI events), `job_id`, `search_attempt`, or `attempt_no` as appropriate. Late results from a previous album/attempt must be discarded.
5. **Existing files are only trusted after validation.** `existing_file_is_valid` requires a readable FLAC and a duration within tolerance. Size alone is not enough. Existing valid tracks are skipped and are not retagged; use `--force` for that.
6. **Promotion must preserve an existing file on failure.** `promote_file` uses a backup/rename fallback; keep that behavior.
7. **Ownership of the Soulseek client matters.** `Arc<Client>` is shared with workers, but the session thread owns connect/reconnect/shutdown. Do not call blocking client operations from the UI thread.

## Config/CLI change checklist

When adding or changing a setting:

1. Add the field to `Config` in `src/config.rs`.
2. Update `Config::default`.
3. Update `Config::normalize` if it is numeric/bounded.
4. Add it to `SettingsForm::from_config` and `SettingsForm::to_config` in `src/app.rs` with a `SettingKind`.
5. Add a CLI flag in `src/cli.rs` if it should be a runtime override.
6. If it affects downloads, decide whether it belongs in `DownloadOptions` (snapshotted per album) or is read directly from `Config`.
7. Update `README.md` (Settings and/or CLI sections).
8. Add tests for parsing/normalization when the rule is non-trivial.

## Where to make common changes

- **New screen or key binding:** `app.rs` (`Screen`, `handle_key`), `ui.rs` (render dispatch, footer hints, help), `README.md`.
- **New MusicBrainz field:** `musicbrainz.rs` models + `ReleaseInfo`/`TrackInfo`, then `app.rs` (`TrackMeta`) and `session.rs` (tags), plus tests.
- **Matching/scoring changes:** `matcher.rs`; update its tests. Remember cover matching depends on `normalize_text`/`strip_release_noise`.
- **Retry/queue/stall behavior:** `session.rs` (`advance_after_failure`, `start_candidate`, `poll_downloads`, `schedule`).
- **Verification behavior:** `acoustid.rs` and `session.rs` (`begin_verification`/`verified`).
- **Tag fields or cover handling:** `audio.rs`, `session.rs` (`TrackTags` construction), then the audio unit test.
- **UI formatting/truncation:** `util.rs` and `ui.rs`.

## Testing notes

- Run `cargo test`; currently all tests are unit tests and run offline.
- Audio tests use `tests/fixtures/silence.flac`; the tag test writes a real FLAC, rewrites tags/cover, and reads them back.
- Include regression tests for state-machine bugs. Existing examples cover stale search results, stale verification results, promotion rollback, token changes, and cover replacement.
- If you need a live API check, do it manually (e.g. a temporary ignored test) and remove it before finishing. Do not leave network-dependent tests enabled.
- Before finishing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and usually `cargo build --release`.

## Style and dependencies

- Use `anyhow::Result` internally; convert to `String` for events/UI.
- Name spawned threads (`soulseek-connect`, `soulsucker-verify`, `cover-art`, etc.) to make logs diagnosable.
- Prefer existing dependencies (`ureq`, `serde`, `ratatui`, `lofty`, `strsim`) over adding new crates.
- Respect external rate limits and use the existing throttles/User-Agents.
- Keep TUI text width-safe and sanitize untrusted peer/source names.
- Don't log credentials, API keys, or raw config contents.
- Keep user-visible messages concise; detailed diagnostics belong in the activity log.
