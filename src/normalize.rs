//! Album-wide EBU R128 loudness normalization and FLAC/MP3 output.
//!
//! Every verified track is measured on its own (integrated loudness and true
//! peak). Once the whole album is in, [`album_gain`] turns those measurements
//! into one gain, and [`process_file`] applies that same linear gain to every
//! track. A single static gain keeps the balance between tracks and leaves
//! gapless transitions seamless; a per-track gain or limiter would make the
//! level jump where songs run into each other. The output is written to a
//! temporary file beside the source and only promoted after the whole encode
//! succeeds.
//!
//! FLAC output is lossless. MP3 output is 320 kbps CBR, encoded with the
//! bundled LAME library; FFmpeg still handles decoding, resampling, channel
//! conversion and the loudness filter.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg::codec::capabilities::Capabilities;
use ffmpeg::format::Sample;
use ffmpeg::{ChannelLayout, Rational, codec, filter, format, frame, media};
use ffmpeg_next as ffmpeg;
use mp3lame_encoder::{Bitrate, Builder, FlushGap, InterleavedPcm, Quality, VbrMode};

use crate::config::AudioQuality;

/// Integrated loudness target. -16 LUFS leaves headroom for car systems and
/// keeps most albums from being squashed.
pub const TARGET_LUFS: f64 = -16.0;
/// True-peak ceiling. -1.5 dBTP avoids clipping after lossy car playback.
pub const TARGET_TRUE_PEAK_DB: f64 = -1.5;
/// MP3 output bitrate.
pub const MP3_BITRATE_KBPS: u32 = 320;
/// Sanity bounds for the album gain, in dB, in case of bogus measurements.
const MAX_ALBUM_CUT_DB: f64 = -30.0;
const MAX_ALBUM_BOOST_DB: f64 = 30.0;

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let _ = ffmpeg::init();
        // The TUI owns the terminal; FFmpeg must not write diagnostics to it.
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
    });
}

/// One track's loudness, measured before any processing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// Integrated loudness, LUFS.
    pub integrated: f64,
    /// True peak, dBTP.
    pub true_peak: f64,
    /// Track length in seconds; weights the track in the album loudness.
    pub duration_secs: f64,
}

/// The single gain applied to every track of an album.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlbumGain {
    pub gain_db: f64,
    /// Album integrated loudness before the gain, LUFS.
    pub integrated: f64,
    /// Loudest true peak on the album before the gain, dBTP.
    pub true_peak: f64,
}

/// One gain for the whole album: bring the album's integrated loudness to
/// [`TARGET_LUFS`], unless that would push the loudest true peak above
/// [`TARGET_TRUE_PEAK_DB`]. No limiter is used, so a single peaky track can
/// leave the album a little below the target rather than squash it.
///
/// Album loudness is the duration-weighted energy mean of the tracks' gated
/// integrated loudness, a close approximation of measuring the album as one
/// stream. Returns `None` when no track could be measured.
pub fn album_gain(tracks: &[Loudness]) -> Option<AlbumGain> {
    let measured: Vec<&Loudness> = tracks
        .iter()
        .filter(|track| {
            track.integrated.is_finite()
                && track.integrated > -70.0
                && track.true_peak.is_finite()
                && track.duration_secs.is_finite()
                && track.duration_secs > 0.0
        })
        .collect();
    if measured.is_empty() {
        return None;
    }
    let total: f64 = measured.iter().map(|track| track.duration_secs).sum();
    let energy: f64 = measured
        .iter()
        .map(|track| track.duration_secs * 10f64.powf(track.integrated / 10.0))
        .sum::<f64>()
        / total;
    let integrated = 10.0 * energy.log10();
    let true_peak = measured
        .iter()
        .map(|track| track.true_peak)
        .fold(f64::NEG_INFINITY, f64::max);
    let gain_db = (TARGET_LUFS - integrated)
        .min(TARGET_TRUE_PEAK_DB - true_peak)
        .clamp(MAX_ALBUM_CUT_DB, MAX_ALBUM_BOOST_DB);
    Some(AlbumGain {
        gain_db,
        integrated,
        true_peak,
    })
}

/// The gain that brings one track to `target_lufs`, for tracks that do not
/// share the album gain (another master of the song). The same true-peak
/// ceiling applies, so a very peaky track may end up a little below target.
pub fn matched_gain(track: &Loudness, target_lufs: f64) -> f64 {
    (target_lufs - track.integrated)
        .min(TARGET_TRUE_PEAK_DB - track.true_peak)
        .clamp(MAX_ALBUM_CUT_DB, MAX_ALBUM_BOOST_DB)
}

/// Turn the filter's last integrated reading (LUFS) and loudest linear true
/// peak into the measurement pair, rejecting silence and missing readings.
fn measurement(integrated: Option<f64>, peak_linear: f64) -> Result<(f64, f64)> {
    let integrated = integrated.ok_or_else(|| anyhow!("the loudness filter reported nothing"))?;
    if !integrated.is_finite() || integrated <= -70.0 {
        bail!("track is too quiet to measure");
    }
    let true_peak = if peak_linear > 0.0 {
        20.0 * peak_linear.log10()
    } else {
        f64::NEG_INFINITY
    };
    Ok((integrated, true_peak))
}

fn extension(path: &Path) -> &str {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

pub fn source_is_mp3(path: &Path) -> bool {
    extension(path).eq_ignore_ascii_case("mp3")
}

pub fn source_compatible(path: &Path, quality: AudioQuality) -> bool {
    match quality {
        AudioQuality::Flac => extension(path).eq_ignore_ascii_case("flac"),
        AudioQuality::Mp3 => source_is_mp3(path) || extension(path).eq_ignore_ascii_case("flac"),
    }
}

/// Whether the source is in the wrong format for the selected quality.
pub fn needs_conversion(path: &Path, quality: AudioQuality) -> bool {
    quality.is_mp3() && !source_is_mp3(path)
}

pub fn processing_phase(path: &Path, quality: AudioQuality, normalize: bool) -> &'static str {
    match (needs_conversion(path, quality), normalize) {
        (true, true) => "applying album gain and converting to MP3 320…",
        (true, false) => "converting to MP3 320…",
        (false, true) => "applying album gain…",
        _ => "preparing audio…",
    }
}

/// Measure a track's integrated loudness and true peak (one decode pass).
pub fn measure_loudness(path: &Path) -> Result<Loudness> {
    init();
    let (integrated, true_peak) = measure(path)?;
    let duration_secs = crate::audio::duration_secs(path)?;
    Ok(Loudness {
        integrated,
        true_peak,
        duration_secs,
    })
}

fn temp_stem() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("soulsucker-audio-{}-{nonce}", std::process::id())
}

/// Apply a gain and/or convert a verified file, returning the temporary
/// output.
///
/// The source is left untouched. The caller owns the returned file and should
/// either promote it or delete it.
pub fn process_file(path: &Path, quality: AudioQuality, gain_db: Option<f64>) -> Result<PathBuf> {
    init();
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    if !source_compatible(path, quality) {
        bail!(
            "{} cannot be converted to {}",
            path.display(),
            quality.label()
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", path.display()))?;
    let output = parent.join(format!(".{}.{}", temp_stem(), quality.extension()));
    let filter = gain_filter(gain_db);

    let result = (|| {
        match quality {
            AudioQuality::Flac => apply_flac(path, &output, filter.as_deref())?,
            AudioQuality::Mp3 => apply_mp3(path, &output, filter.as_deref())?,
        }
        crate::audio::copy_pictures(path, &output)
            .context("could not preserve embedded cover art")?;
        Ok(output.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&output);
    }
    result
}

/// A static linear gain. `volume` keeps no state across samples, so tracks
/// that run into each other still join seamlessly.
fn gain_filter(gain_db: Option<f64>) -> Option<String> {
    gain_db
        .filter(|gain| gain.is_finite() && gain.abs() >= 0.005)
        .map(|gain| format!("volume={gain:.2}dB:precision=double"))
}

fn measure(input: &Path) -> Result<(f64, f64)> {
    let mut ictx =
        format::input(input).with_context(|| format!("could not open {}", input.display()))?;
    let stream = ictx
        .streams()
        .best(media::Type::Audio)
        .ok_or_else(|| anyhow!("{} has no audio stream", input.display()))?;
    let stream_index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    decoder.set_parameters(stream.parameters())?;

    // The `ebur128` filter measures BS.1770 integrated loudness and true peak
    // several times faster than `loudnorm`'s analysis pass (which also
    // computes things the album gain does not use). It passes audio through
    // and publishes its running readings as frame metadata; the last
    // integrated reading covers the whole track, and the true peak is the
    // loudest frame's. Its output is packed doubles, so ask the sink for that
    // to avoid a pointless conversion.
    let mut graph = build_graph(
        &decoder,
        Sample::F64(format::sample::Type::Packed),
        decoder.rate(),
        decoder.channel_layout(),
        Some("ebur128=peak=true:metadata=1:framelog=quiet"),
    )?;

    let mut integrated = None;
    let mut peak_linear = 0.0_f64;
    run_decode(&mut ictx, stream_index, &mut decoder, &mut graph, |frame| {
        let metadata = frame.metadata();
        if let Some(value) = metadata
            .get("lavfi.r128.I")
            .and_then(|value| value.trim().parse::<f64>().ok())
        {
            integrated = Some(value);
        }
        if let Some(value) = metadata
            .get("lavfi.r128.true_peak")
            .and_then(|value| value.trim().parse::<f64>().ok())
        {
            peak_linear = peak_linear.max(value);
        }
        Ok(())
    })?;
    drop(graph);
    measurement(integrated, peak_linear)
}

fn apply_flac(input: &Path, output: &Path, filter: Option<&str>) -> Result<()> {
    let mut ictx =
        format::input(input).with_context(|| format!("could not reopen {}", input.display()))?;
    let stream = ictx
        .streams()
        .best(media::Type::Audio)
        .ok_or_else(|| anyhow!("{} has no audio stream", input.display()))?;
    let stream_index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    decoder.set_parameters(stream.parameters())?;

    let mut octx =
        format::output(output).with_context(|| format!("could not create {}", output.display()))?;
    let codec = ffmpeg::encoder::find(codec::Id::FLAC)
        .ok_or_else(|| anyhow!("this FFmpeg build has no FLAC encoder"))?
        .audio()?;
    let sample_rate = decoder.rate();
    let layout = decoder.channel_layout();
    let sample_format = preferred_sample_format(&codec, decoder.format())?;
    let mut encoder;
    let out_time_base;
    {
        let mut out_stream = octx
            .add_stream(codec)
            .context("could not add a FLAC output stream")?;
        let context = codec::context::Context::from_parameters(out_stream.parameters())?;
        let mut builder = context
            .encoder()
            .audio()
            .context("could not create a FLAC encoder")?;
        builder.set_rate(sample_rate as i32);
        builder.set_channel_layout(layout);
        builder.set_format(sample_format);
        builder.set_time_base(Rational::new(1, sample_rate as i32));
        out_stream.set_time_base(Rational::new(1, sample_rate as i32));
        let bits = unsafe { (*decoder.as_ptr()).bits_per_raw_sample };
        if bits > 0 {
            unsafe {
                (*builder.as_mut_ptr()).bits_per_raw_sample = bits;
            }
        }
        encoder = builder
            .open_as(codec)
            .context("could not open the FLAC encoder")?;
        out_stream.set_parameters(&encoder);
        out_time_base = out_stream.time_base();
    }

    let mut graph = build_graph(&decoder, sample_format, sample_rate, layout, filter)?;
    if let Some(codec) = encoder.codec()
        && !codec
            .capabilities()
            .contains(Capabilities::VARIABLE_FRAME_SIZE)
        && encoder.frame_size() > 0
    {
        graph
            .get("out")
            .ok_or_else(|| anyhow!("the filter sink is missing"))?
            .sink()
            .set_frame_size(encoder.frame_size());
    }

    octx.write_header()
        .context("could not write the FLAC header")?;
    let in_time_base = decoder.time_base();
    let total_samples = encode_stream(
        &mut ictx,
        stream_index,
        &mut decoder,
        &mut graph,
        &mut encoder,
        &mut octx,
        in_time_base,
        out_time_base,
    )?;
    encoder.send_eof()?;
    drain_encoder(&mut encoder, &mut octx, in_time_base, out_time_base)?;
    octx.write_trailer()?;
    drop(octx);
    update_flac_total_samples(output, total_samples)?;
    Ok(())
}

fn apply_mp3(input: &Path, output: &Path, filter: Option<&str>) -> Result<()> {
    let mut ictx =
        format::input(input).with_context(|| format!("could not reopen {}", input.display()))?;
    let stream = ictx
        .streams()
        .best(media::Type::Audio)
        .ok_or_else(|| anyhow!("{} has no audio stream", input.display()))?;
    let stream_index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    decoder.set_parameters(stream.parameters())?;

    let sample_rate = mp3_sample_rate(decoder.rate());
    let channels = if decoder.channel_layout().channels() == 1 {
        1
    } else {
        2
    };
    let layout = if channels == 1 {
        ChannelLayout::MONO
    } else {
        ChannelLayout::STEREO
    };
    let mut graph = build_graph(
        &decoder,
        Sample::I16(format::sample::Type::Packed),
        sample_rate,
        layout,
        filter,
    )?;
    let mut sink = Mp3Sink::new(sample_rate, channels)?;
    run_decode(
        &mut ictx,
        stream_index,
        &mut decoder,
        &mut graph,
        |filtered| sink.push(filtered),
    )?;
    sink.finish(output)
}

fn mp3_sample_rate(rate: u32) -> u32 {
    match rate {
        32_000 | 44_100 | 48_000 => rate,
        88_200 | 176_400 => 44_100,
        96_000 | 192_000 => 48_000,
        // 320 kbps is only valid for MPEG-1 sample rates.
        _ => 44_100,
    }
}

struct Mp3Sink {
    encoder: mp3lame_encoder::Encoder,
    frames: Vec<u8>,
    channels: usize,
}

impl Mp3Sink {
    fn new(sample_rate: u32, channels: usize) -> Result<Self> {
        debug_assert_eq!(MP3_BITRATE_KBPS, 320);
        let mut builder =
            Builder::new().ok_or_else(|| anyhow!("could not create a LAME encoder"))?;
        builder
            .set_num_channels(channels as u8)
            .map_err(|err| anyhow!("could not configure the MP3 channels: {err}"))?;
        builder
            .set_sample_rate(sample_rate)
            .map_err(|err| anyhow!("could not configure the MP3 sample rate: {err}"))?;
        builder
            .set_brate(Bitrate::Kbps320)
            .map_err(|err| anyhow!("could not configure the MP3 bitrate: {err}"))?;
        builder
            .set_quality(Quality::NearBest)
            .map_err(|err| anyhow!("could not configure the MP3 quality: {err}"))?;
        builder
            .set_vbr_mode(VbrMode::Off)
            .map_err(|err| anyhow!("could not configure CBR MP3 output: {err}"))?;
        builder
            .set_to_write_vbr_tag(true)
            .map_err(|err| anyhow!("could not configure the MP3 header: {err}"))?;
        let encoder = builder
            .build()
            .map_err(|err| anyhow!("could not open the MP3 encoder: {err}"))?;
        Ok(Self {
            encoder,
            frames: Vec::new(),
            channels,
        })
    }

    fn push(&mut self, filtered: &frame::Audio) -> Result<()> {
        if filtered.format() != Sample::I16(format::sample::Type::Packed) {
            bail!("the MP3 filter produced an unexpected sample format");
        }
        if filtered.channels() as usize != self.channels {
            bail!("the MP3 filter produced an unexpected channel layout");
        }
        let samples = filtered.samples();
        let values = samples
            .checked_mul(self.channels)
            .ok_or_else(|| anyhow!("the MP3 frame is too large"))?;
        let needed = values
            .checked_mul(2)
            .ok_or_else(|| anyhow!("the MP3 frame is too large"))?;
        let data = filtered.data(0);
        if data.len() < needed {
            bail!("the MP3 filter produced a truncated frame");
        }
        let pcm = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<i16>(), values) };
        self.frames
            .reserve(mp3lame_encoder::max_required_buffer_size(values));
        self.encoder
            .encode_to_vec(InterleavedPcm(pcm), &mut self.frames)
            .map_err(|err| anyhow!("MP3 encoding failed: {err}"))?;
        Ok(())
    }

    fn finish(mut self, output: &Path) -> Result<()> {
        // A full flush encodes LAME's buffered tail and records the end
        // padding in the LAME tag. `FlushNoGap` is for chaining streams: it
        // leaves the last PCM samples unencoded and writes zero padding.
        self.frames.reserve(
            mp3lame_encoder::max_required_buffer_size(1152 * self.channels).max(MP3_FLUSH_BYTES),
        );
        self.encoder
            .flush_to_vec::<FlushGap>(&mut self.frames)
            .map_err(|err| anyhow!("could not finish the MP3 stream: {err}"))?;

        let tag_size = self.encoder.lame_tag_size();
        if tag_size > 0 {
            let mut tag = vec![MaybeUninit::uninit(); tag_size];
            if let Some(written) = self.encoder.lame_tag_encode(&mut tag) {
                let tag =
                    unsafe { std::slice::from_raw_parts(tag.as_ptr() as *const u8, written.get()) };
                install_lame_tag(&mut self.frames, tag)?;
            }
        }

        let mut file = File::create(output)
            .with_context(|| format!("could not create {}", output.display()))?;
        file.write_all(&self.frames)
            .context("could not write the MP3 audio")?;
        file.sync_all().context("could not sync the MP3 file")?;
        Ok(())
    }
}

/// LAME's documented minimum buffer for a final flush.
const MP3_FLUSH_BYTES: usize = 7200;

/// LAME reserves the first frame of the stream as an all-zero placeholder for
/// its Xing/LAME tag. The finished tag (encoder delay, padding, frame count)
/// must overwrite that frame. Prepending it instead leaves the blank frame in
/// the audio, which players decode as an extra frame of silence and which
/// throws off gapless trimming.
fn install_lame_tag(frames: &mut [u8], tag: &[u8]) -> Result<()> {
    let placeholder = frames
        .get(..tag.len())
        .ok_or_else(|| anyhow!("the MP3 stream is shorter than its LAME tag"))?;
    if tag.len() < 4 || placeholder[0] != 0xFF || placeholder[1] & 0xE0 != 0xE0 {
        bail!("the MP3 stream does not start with a LAME tag placeholder");
    }
    if placeholder[4..].iter().any(|&byte| byte != 0) {
        bail!("the first MP3 frame is not an empty LAME tag placeholder");
    }
    frames[..tag.len()].copy_from_slice(tag);
    Ok(())
}

fn preferred_sample_format(codec: &codec::audio::Audio, preferred: Sample) -> Result<Sample> {
    let formats: Vec<Sample> = codec
        .formats()
        .ok_or_else(|| anyhow!("the FLAC encoder reports no sample formats"))?
        .collect();
    if formats.contains(&preferred) {
        Ok(preferred)
    } else {
        formats
            .first()
            .copied()
            .ok_or_else(|| anyhow!("the FLAC encoder reports no sample formats"))
    }
}

fn build_graph(
    decoder: &codec::decoder::Audio,
    sample_format: Sample,
    sample_rate: u32,
    layout: ChannelLayout,
    pre_filter: Option<&str>,
) -> Result<filter::Graph> {
    let mut graph = filter::Graph::new();
    let args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.time_base(),
        decoder.rate(),
        decoder.format().name(),
        decoder.channel_layout().bits()
    );
    graph.add(
        &filter::find("abuffer").ok_or_else(|| anyhow!("FFmpeg has no abuffer filter"))?,
        "in",
        &args,
    )?;
    {
        let mut sink = graph.add(
            &filter::find("abuffersink")
                .ok_or_else(|| anyhow!("FFmpeg has no abuffersink filter"))?,
            "out",
            "",
        )?;
        sink.set_sample_format(sample_format);
        sink.set_sample_rate(sample_rate);
        sink.set_channel_layout(layout);
    }
    let audio_format = format!(
        "aformat=sample_fmts={}:sample_rates={}:channel_layouts=0x{:x}",
        sample_format.name(),
        sample_rate,
        layout.bits()
    );
    let chain = match pre_filter {
        Some(filter) => format!("{filter},{audio_format}"),
        None => audio_format,
    };
    graph.output("in", 0)?.input("out", 0)?.parse(&chain)?;
    graph.validate()?;
    Ok(graph)
}

fn run_decode<F>(
    ictx: &mut format::context::Input,
    stream_index: usize,
    decoder: &mut codec::decoder::Audio,
    graph: &mut filter::Graph,
    mut on_frame: F,
) -> Result<()>
where
    F: FnMut(&frame::Audio) -> Result<()>,
{
    let mut decoded = frame::Audio::empty();
    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            graph
                .get("in")
                .ok_or_else(|| anyhow!("the filter source is missing"))?
                .source()
                .add(&decoded)?;
            drain_sink(graph, &mut on_frame)?;
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        let timestamp = decoded.timestamp();
        decoded.set_pts(timestamp);
        graph
            .get("in")
            .ok_or_else(|| anyhow!("the filter source is missing"))?
            .source()
            .add(&decoded)?;
        drain_sink(graph, &mut on_frame)?;
    }
    graph
        .get("in")
        .ok_or_else(|| anyhow!("the filter source is missing"))?
        .source()
        .flush()?;
    drain_sink(graph, &mut on_frame)?;
    Ok(())
}

fn drain_sink<F>(graph: &mut filter::Graph, on_frame: &mut F) -> Result<()>
where
    F: FnMut(&frame::Audio) -> Result<()>,
{
    let mut filtered = frame::Audio::empty();
    while graph
        .get("out")
        .ok_or_else(|| anyhow!("the filter sink is missing"))?
        .sink()
        .frame(&mut filtered)
        .is_ok()
    {
        on_frame(&filtered)?;
    }
    Ok(())
}

/// FLAC's STREAMINFO block stores the total sample count in 36 bits. The
/// muxer only learns it from a side-data packet this FFmpeg build rejects, so
/// write it directly so players can seek and show the duration.
fn update_flac_total_samples(path: &Path, total_samples: u64) -> Result<()> {
    let total = total_samples & 0x0f_ffff_ffff;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("could not reopen {}", path.display()))?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)
        .context("could not read the FLAC header")?;
    if &header[..4] != b"fLaC" || header[4] & 0x7f != 0 {
        bail!("the processed file does not start with a FLAC STREAMINFO block");
    }
    let mut streaminfo = [0u8; 34];
    file.read_exact(&mut streaminfo)
        .context("could not read the FLAC STREAMINFO block")?;
    streaminfo[13] = (streaminfo[13] & 0xf0) | ((total >> 32) as u8 & 0x0f);
    streaminfo[14] = (total >> 24) as u8;
    streaminfo[15] = (total >> 16) as u8;
    streaminfo[16] = (total >> 8) as u8;
    streaminfo[17] = total as u8;
    file.seek(SeekFrom::Start(8))
        .context("could not seek to the FLAC STREAMINFO block")?;
    file.write_all(&streaminfo)
        .context("could not update the FLAC duration")?;
    file.sync_all().context("could not sync the FLAC file")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode_stream(
    ictx: &mut format::context::Input,
    stream_index: usize,
    decoder: &mut codec::decoder::Audio,
    graph: &mut filter::Graph,
    encoder: &mut codec::encoder::Audio,
    octx: &mut format::context::Output,
    in_time_base: Rational,
    out_time_base: Rational,
) -> Result<u64> {
    let mut total_samples = 0u64;
    run_decode(ictx, stream_index, decoder, graph, |filtered| {
        total_samples += filtered.samples() as u64;
        encoder
            .send_frame(filtered)
            .context("could not send a frame to the FLAC encoder")?;
        drain_encoder(encoder, octx, in_time_base, out_time_base)
    })?;
    Ok(total_samples)
}

fn drain_encoder(
    encoder: &mut codec::encoder::Audio,
    octx: &mut format::context::Output,
    in_time_base: Rational,
    out_time_base: Rational,
) -> Result<()> {
    let mut packet = ffmpeg::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        // We patch STREAMINFO's total-sample count after muxing, so the
        // encoder's trailing side-data-only packet is not written.
        if packet.size() == 0 {
            continue;
        }
        packet.set_stream(0);
        packet.rescale_ts(in_time_base, out_time_base);
        packet
            .write(octx)
            .context("could not write an encoded FLAC packet")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_readings_become_measurements() {
        // A linear true peak of 0.13 is about -17.7 dBTP.
        let (integrated, true_peak) = measurement(Some(-29.02), 0.130).unwrap();
        assert_eq!(integrated, -29.02);
        assert!((true_peak - -17.72).abs() < 0.01, "{true_peak}");
    }

    #[test]
    fn missing_or_silent_readings_are_rejected() {
        assert!(measurement(None, 0.5).is_err());
        assert!(measurement(Some(f64::NEG_INFINITY), 0.0).is_err());
        assert!(measurement(Some(-70.0), 0.0).is_err());
    }

    #[test]
    fn an_outlier_track_is_matched_to_the_album_level_without_moving_the_album() {
        let own = [track(-23.0, -8.0, 200.0), track(-21.0, -5.0, 200.0)];
        let album = album_gain(&own).unwrap();
        let level = album.integrated + album.gain_db;
        // A much louder master of another song on the album.
        let loud = track(-11.8, -1.6, 130.0);
        let gain = matched_gain(&loud, level);
        assert!(gain < album.gain_db, "{gain} vs {}", album.gain_db);
        assert!((loud.integrated + gain - level).abs() < 0.01);
        // Including it in the album gain instead would cap the album: its
        // peak holds everything else down.
        let mixed = album_gain(&[own[0], own[1], loud]).unwrap();
        assert!(mixed.gain_db < album.gain_db - 1.0);
        // The ceiling still applies to a very peaky track.
        let peaky = track(-30.0, -0.5, 100.0);
        assert!(peaky.true_peak + matched_gain(&peaky, level) <= TARGET_TRUE_PEAK_DB + 1e-9);
    }

    #[test]
    fn targets_are_safe_defaults() {
        assert_eq!(TARGET_LUFS, -16.0);
        assert_eq!(TARGET_TRUE_PEAK_DB, -1.5);
        assert_eq!(MP3_BITRATE_KBPS, 320);
    }

    #[test]
    fn flac_and_mp3_conversion_rules_match_the_selected_quality() {
        let flac = Path::new("track.flac");
        let mp3 = Path::new("track.mp3");

        assert!(source_compatible(flac, AudioQuality::Flac));
        assert!(!source_compatible(mp3, AudioQuality::Flac));
        assert!(source_compatible(flac, AudioQuality::Mp3));
        assert!(source_compatible(mp3, AudioQuality::Mp3));

        assert!(needs_conversion(flac, AudioQuality::Mp3));
        assert!(!needs_conversion(mp3, AudioQuality::Mp3));
        assert!(!needs_conversion(flac, AudioQuality::Flac));
    }

    fn track(integrated: f64, true_peak: f64, duration_secs: f64) -> Loudness {
        Loudness {
            integrated,
            true_peak,
            duration_secs,
        }
    }

    #[test]
    fn album_gain_is_one_gain_from_the_duration_weighted_album_loudness() {
        // Two equal-length tracks 10 dB apart: the album is ~2.6 dB below the
        // louder one (energy mean), not the arithmetic mean of -25.
        let gain = album_gain(&[track(-20.0, -10.0, 100.0), track(-30.0, -20.0, 100.0)]).unwrap();
        assert!((gain.integrated - -22.596).abs() < 0.01, "{gain:?}");
        assert!((gain.gain_db - (TARGET_LUFS - gain.integrated)).abs() < 1e-9);
        assert_eq!(gain.true_peak, -10.0);

        // A long quiet track counts for more than a short loud one.
        let weighted =
            album_gain(&[track(-20.0, -10.0, 10.0), track(-30.0, -20.0, 300.0)]).unwrap();
        assert!(weighted.integrated < -28.0, "{weighted:?}");
    }

    #[test]
    fn album_gain_never_pushes_the_loudest_peak_over_the_ceiling() {
        // A quiet album with one peaky track: the boost stops at the ceiling.
        let gain = album_gain(&[track(-24.0, -3.0, 200.0), track(-24.0, -9.0, 200.0)]).unwrap();
        assert!(
            (gain.gain_db - (TARGET_TRUE_PEAK_DB - -3.0)).abs() < 1e-9,
            "{gain:?}"
        );
        // A loud album is turned down, whatever its peaks.
        let gain = album_gain(&[track(-8.0, 0.5, 200.0)]).unwrap();
        assert!((gain.gain_db - -8.0).abs() < 1e-9, "{gain:?}");
    }

    #[test]
    fn album_gain_ignores_unmeasurable_tracks() {
        assert!(album_gain(&[]).is_none());
        assert!(album_gain(&[track(f64::NEG_INFINITY, -70.0, 30.0)]).is_none());
        let gain = album_gain(&[
            track(f64::NEG_INFINITY, -70.0, 30.0),
            track(-16.0, -6.0, 0.0),
            track(-20.0, -6.0, 120.0),
        ])
        .unwrap();
        assert!((gain.integrated - -20.0).abs() < 1e-9);
    }

    #[test]
    fn gain_filter_is_a_static_volume_change() {
        assert_eq!(gain_filter(None), None);
        assert_eq!(gain_filter(Some(0.0)), None);
        assert_eq!(gain_filter(Some(f64::NAN)), None);
        assert_eq!(
            gain_filter(Some(-3.456)).as_deref(),
            Some("volume=-3.46dB:precision=double")
        );
    }

    #[test]
    fn high_resolution_sources_are_resampled_for_mp3() {
        assert_eq!(mp3_sample_rate(44_100), 44_100);
        assert_eq!(mp3_sample_rate(48_000), 48_000);
        assert_eq!(mp3_sample_rate(32_000), 32_000);
        assert_eq!(mp3_sample_rate(22_050), 44_100);
        assert_eq!(mp3_sample_rate(88_200), 44_100);
        assert_eq!(mp3_sample_rate(96_000), 48_000);
        assert_eq!(mp3_sample_rate(192_000), 48_000);
    }

    fn write_tone_wav(path: &Path, rate: u32, frames: u32) {
        let channels = 2u16;
        let data_len = frames * u32::from(channels) * 2;
        // WAVE_FORMAT_EXTENSIBLE with a front-left/right mask, so FFmpeg
        // reports a stereo layout like it does for real FLAC/MP3 sources.
        let mut bytes = Vec::with_capacity(68 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(60 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&22u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&0x3u32.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM
        bytes.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let t = frame as f32 / rate as f32;
            // Start at full amplitude so leading silence is easy to spot.
            let sample =
                ((t * 440.0 * std::f32::consts::TAU).cos() * 0.25 * i16::MAX as f32) as i16;
            for _ in 0..channels {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        fs::write(path, bytes).unwrap();
    }

    /// Decode a file the way a gapless player would (honoring the LAME
    /// delay/padding) and return its sample count and first samples.
    fn decode_samples(path: &Path) -> (u64, Vec<i16>) {
        init();
        let mut ictx = format::input(path).unwrap();
        let stream = ictx.streams().best(media::Type::Audio).unwrap();
        let index = stream.index();
        let mut decoder = codec::context::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .audio()
            .unwrap();
        let mut total = 0u64;
        let mut head = Vec::new();
        let mut decoded = frame::Audio::empty();
        let take = |decoded: &frame::Audio, total: &mut u64, head: &mut Vec<i16>| {
            *total += decoded.samples() as u64;
            if head.len() < 64 && decoded.samples() > 0 {
                match decoded.format() {
                    Sample::F32(format::sample::Type::Planar) => {
                        head.extend(
                            decoded
                                .plane::<f32>(0)
                                .iter()
                                .take(64)
                                .map(|v| (v * 32767.0) as i16),
                        );
                    }
                    Sample::I16(format::sample::Type::Planar) => {
                        head.extend(decoded.plane::<i16>(0).iter().take(64));
                    }
                    other => panic!("unexpected decoder format {other:?}"),
                }
            }
        };
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            decoder.send_packet(&packet).unwrap();
            while decoder.receive_frame(&mut decoded).is_ok() {
                take(&decoded, &mut total, &mut head);
            }
        }
        decoder.send_eof().unwrap();
        while decoder.receive_frame(&mut decoded).is_ok() {
            take(&decoded, &mut total, &mut head);
        }
        (total, head)
    }

    #[test]
    fn mp3_output_is_sample_exact_for_gapless_playback() {
        init();
        let dir = std::env::temp_dir().join(format!("soulsucker-gapless-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let mp3 = dir.join("tone.mp3");
        // Deliberately not a multiple of the 1152-sample MP3 frame.
        let frames = 44_100 + 777;
        write_tone_wav(&wav, 44_100, frames);

        apply_mp3(&wav, &mp3, None).unwrap();

        let bytes = fs::read(&mp3).unwrap();
        let first_frame = 144 * 320_000 / 44_100;
        assert!(
            bytes[..first_frame + 1].windows(4).any(|w| w == b"Info"),
            "the first frame must carry the LAME tag"
        );
        assert!(
            !bytes[first_frame..first_frame + 8]
                .windows(1)
                .all(|w| w[0] == 0)
                && bytes[first_frame] == 0xFF,
            "the LAME placeholder frame must be replaced, not left behind the tag"
        );

        let (decoded, head) = decode_samples(&mp3);
        assert_eq!(
            decoded,
            u64::from(frames),
            "encoder delay and padding must trim back to the source length"
        );
        assert!(
            head.iter().take(8).any(|&sample| sample.abs() > 1000),
            "audio must start immediately, without a frame of silence"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn album_gain_changes_measured_loudness_by_exactly_the_gain() {
        init();
        let dir = std::env::temp_dir().join(format!("soulsucker-gain-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let flac = dir.join("tone.flac");
        write_tone_wav(&wav, 44_100, 44_100 * 3);
        apply_flac(&wav, &flac, None).unwrap();

        let before = measure_loudness(&flac).unwrap();
        assert!((before.duration_secs - 3.0).abs() < 0.01, "{before:?}");
        let processed = process_file(&flac, AudioQuality::Flac, Some(-6.0)).unwrap();
        let renamed = dir.join("quieter.flac");
        fs::rename(&processed, &renamed).unwrap();
        let after = measure_loudness(&renamed).unwrap();

        assert!(
            (after.integrated - (before.integrated - 6.0)).abs() < 0.1,
            "{before:?} -> {after:?}"
        );
        assert!(
            (after.true_peak - (before.true_peak - 6.0)).abs() < 0.2,
            "{before:?} -> {after:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
