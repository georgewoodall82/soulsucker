# Vendored crates

## `ffmpeg-sys-next` 8.1.0

`ffmpeg-next` 8.1 pulls `ffmpeg-sys-next` 8.1, whose build script always passes
GCC-style `-march=native -mtune=native` when the target equals the host. MSVC
rejects those flags, so Windows release builds fail in FFmpeg's compiler test.
The native flags also make release binaries depend on the build runner's CPU:
an AVX-512 runner produces code that crashes with SIGILL on an AVX2 machine.

The vendored copy removes the native CPU flags for every target and pins
x86_64 builds to `-march=x86-64 -mtune=generic`, leaving MSVC untouched. The
explicit baseline matters because some distributions (Ubuntu 26.04, for
example) configure GCC to default to x86-64-v3. FFmpeg's hand-written SIMD is
runtime-dispatched, so the compiler baseline does not lose the important
optimizations. The script always reruns configure when the FFmpeg source tree
exists, compares `ffbuild/config.mak` before and after, and runs `make clean`
when the flags change, so a cached build cannot keep objects from an older
configuration.
Remove the `[patch.crates-io]` entry in `Cargo.toml` and this directory once a
compatible `ffmpeg-sys-next` release stops forcing host-specific code
generation.
