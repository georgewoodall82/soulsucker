# soulsucker

A terminal album downloader: search **MusicBrainz**, pick a release, and fetch
matching **FLAC** tracks from **Soulseek**.

## Layout

- **`soulsucker/`** — the active Rust rewrite. Ratatui interface, parallel
  downloads, MusicBrainz tagging, fingerprint verification, and automatic
  cover art. See [`soulsucker/README.md`](soulsucker/README.md) for setup,
  usage, key bindings, and configuration.
- **`soulsucker-py/`** — the original Python/Textual implementation, kept as a
  reference. See [`soulsucker-py/README.md`](soulsucker-py/README.md).

## Quick start

```bash
cd soulsucker
cargo build --release
./target/release/soulsucker
```

The first run asks for your Soulseek credentials and an optional AcoustID key,
then saves them to the platform config directory.
