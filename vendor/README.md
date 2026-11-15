# Vendored crates

## `ffmpeg-sys-next` 8.1.0

`ffmpeg-next` 8.1 pulls `ffmpeg-sys-next` 8.1, whose build script always passes
GCC-style `-march=native -mtune=native` when the target equals the host. MSVC
rejects those flags, so Windows release builds fail in FFmpeg's compiler test.

The vendored copy adds one guard in `build.rs`: skip the CPU flags for
`target_env = "msvc"`. Remove the `[patch.crates-io]` entry in `Cargo.toml` and
this directory once a compatible `ffmpeg-sys-next` release includes the fix.
