//! Two-pass EBU R128 loudness normalization and FLAC/MP3 output.
//!
//! Pass one measures the input and writes JSON stats. Pass two applies those
//! measurements in linear mode when possible, with a true-peak ceiling. The
//! output is written to a temporary file beside the source and only promoted
//! after the whole encode succeeds.
//!
//! FLAC output is lossless. MP3 output is 320 kbps CBR, encoded with the
//! bundled LAME library; FFmpeg still handles decoding, resampling, channel
//! conversion and the loudness filter.

use std::fmt::Write as _;
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
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality, VbrMode};
use serde::Deserialize;

use crate::config::AudioQuality;

/// Integrated loudness target. -16 LUFS leaves headroom for car systems and
/// keeps most albums from being squashed.
pub const TARGET_LUFS: f64 = -16.0;
/// True-peak ceiling. -1.5 dBTP avoids clipping after lossy car playback.
pub const TARGET_TRUE_PEAK_DB: f64 = -1.5;
/// Loudness range target, in LU.
pub const TARGET_LRA: f64 = 11.0;
/// MP3 output bitrate.
pub const MP3_BITRATE_KBPS: u32 = 320;

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let _ = ffmpeg::init();
        // The TUI owns the terminal; FFmpeg must not write diagnostics to it.
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
    });
}

#[derive(Debug, Deserialize)]
struct LoudnormStats {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

#[derive(Debug, Clone, Copy)]
struct Measured {
    input_i: f64,
    input_tp: f64,
    input_lra: f64,
    input_thresh: f64,
    target_offset: f64,
}

impl LoudnormStats {
    fn measured(self) -> Result<Measured> {
        let measured = Measured {
            input_i: parse_measurement("input_i", &self.input_i)?,
            input_tp: parse_measurement("input_tp", &self.input_tp)?,
            input_lra: parse_measurement("input_lra", &self.input_lra)?,
            input_thresh: parse_measurement("input_thresh", &self.input_thresh)?,
            target_offset: parse_measurement("target_offset", &self.target_offset)?,
        };
        if measured.input_i <= -70.0 {
            bail!("track is too quiet to normalize");
        }
        Ok(measured)
    }
}

fn parse_measurement(name: &str, value: &str) -> Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("loudnorm returned an invalid {name}: {value}"))
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

/// Whether the verified source needs a second pass before it can be saved.
pub fn needs_processing(path: &Path, quality: AudioQuality, normalize: bool) -> bool {
    source_compatible(path, quality) && (normalize || (quality.is_mp3() && !source_is_mp3(path)))
}

pub fn processing_phase(path: &Path, quality: AudioQuality, normalize: bool) -> &'static str {
    match (quality.is_mp3(), source_is_mp3(path), normalize) {
        (true, false, true) => "normalizing and converting to MP3 320 (two-pass)…",
        (true, false, false) => "converting to MP3 320…",
        (_, _, true) => "normalizing loudness (two-pass)…",
        _ => "preparing audio…",
    }
}

/// Normalize and/or convert a verified file, returning the temporary output.
///
/// The source is left untouched. The caller owns the returned file and should
/// either promote it or delete it.
pub fn process_file(path: &Path, quality: AudioQuality, normalize: bool) -> Result<PathBuf> {
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
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let stem = format!("soulsucker-audio-{}-{nonce}", std::process::id());
    let stats_path = parent.join(format!(".{stem}.json"));
    let output = parent.join(format!(".{stem}.{}", quality.extension()));

    let result = (|| {
        let measured = if normalize {
            Some(measure(path, &stats_path)?)
        } else {
            None
        };
        match quality {
            AudioQuality::Flac => apply_flac(path, &output, measured)?,
            AudioQuality::Mp3 => apply_mp3(path, &output, measured)?,
        }
        crate::audio::copy_pictures(path, &output)
            .context("could not preserve embedded cover art")?;
        Ok(output.clone())
    })();
    let _ = fs::remove_file(&stats_path);
    if result.is_err() {
        let _ = fs::remove_file(&output);
    }
    result
}

fn measure(input: &Path, stats_path: &Path) -> Result<Measured> {
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

    let spec =
        format!("I={TARGET_LUFS}:TP={TARGET_TRUE_PEAK_DB}:LRA={TARGET_LRA}:print_format=json");
    let mut graph = build_graph(
        &decoder,
        decoder.format(),
        decoder.rate(),
        decoder.channel_layout(),
        Some(&spec),
        Some(stats_path),
    )?;

    run_decode(
        &mut ictx,
        stream_index,
        &mut decoder,
        &mut graph,
        |_| Ok(()),
    )?;
    drop(graph);

    let json = fs::read_to_string(stats_path)
        .with_context(|| format!("loudnorm did not write {}", stats_path.display()))?;
    let stats: LoudnormStats =
        serde_json::from_str(&json).context("could not parse loudnorm measurements")?;
    stats.measured()
}

fn loudnorm_apply_args(measured: Measured) -> String {
    format!(
        "I={TARGET_LUFS}:TP={TARGET_TRUE_PEAK_DB}:LRA={TARGET_LRA}:\
         measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true",
        measured.input_i,
        measured.input_tp,
        measured.input_lra,
        measured.input_thresh,
        measured.target_offset,
    )
}

fn apply_flac(input: &Path, output: &Path, measured: Option<Measured>) -> Result<()> {
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

    let spec = measured.map(loudnorm_apply_args);
    let mut graph = build_graph(
        &decoder,
        sample_format,
        sample_rate,
        layout,
        spec.as_deref(),
        None,
    )?;
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

fn apply_mp3(input: &Path, output: &Path, measured: Option<Measured>) -> Result<()> {
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
    let spec = measured.map(loudnorm_apply_args);
    let mut graph = build_graph(
        &decoder,
        Sample::I16(format::sample::Type::Packed),
        sample_rate,
        layout,
        spec.as_deref(),
        None,
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
        self.frames
            .reserve(mp3lame_encoder::max_required_buffer_size(
                1152 * self.channels,
            ));
        let written = self
            .encoder
            .flush::<FlushNoGap>(self.frames.spare_capacity_mut())
            .map_err(|err| anyhow!("could not finish the MP3 stream: {err}"))?;
        let length = self.frames.len();
        unsafe {
            self.frames.set_len(length + written);
        }

        let mut file = File::create(output)
            .with_context(|| format!("could not create {}", output.display()))?;
        let tag_size = self.encoder.lame_tag_size();
        if tag_size > 0 {
            let mut tag = vec![MaybeUninit::uninit(); tag_size];
            if let Some(written) = self.encoder.lame_tag_encode(&mut tag) {
                let bytes =
                    unsafe { std::slice::from_raw_parts(tag.as_ptr() as *const u8, written.get()) };
                file.write_all(bytes)
                    .context("could not write the MP3 header")?;
            }
        }
        file.write_all(&self.frames)
            .context("could not write the MP3 audio")?;
        file.sync_all().context("could not sync the MP3 file")?;
        Ok(())
    }
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
    loudnorm_args: Option<&str>,
    stats_path: Option<&Path>,
) -> Result<filter::Graph> {
    let mut graph = filter::Graph::new();
    let args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.time_base(),
        decoder.rate(),
        decoder.format().name(),
        decoder.channel_layout().bits()
    );
    let loudnorm = match (loudnorm_args, stats_path) {
        (Some(args), Some(stats_path)) => Some(format!(
            "{args}:stats_file={}",
            escape_filter_value(
                stats_path
                    .to_str()
                    .ok_or_else(|| anyhow!("the stats path is not valid UTF-8"))?
            )
        )),
        (Some(args), None) => Some(args.to_string()),
        (None, Some(_)) => bail!("loudnorm stats requested without a loudnorm filter"),
        (None, None) => None,
    };
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
    let chain = match loudnorm {
        Some(args) => format!("loudnorm={args},{audio_format}"),
        None => audio_format,
    };
    graph.output("in", 0)?.input("out", 0)?.parse(&chain)?;
    graph.validate()?;
    Ok(graph)
}

fn escape_filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '\'' | ':' | ',' | '[' | ']' | ';') {
            let _ = escaped.write_char('\\');
        }
        let _ = escaped.write_char(ch);
    }
    escaped
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
    fn loudnorm_stats_become_measurements() {
        let stats: LoudnormStats = serde_json::from_str(
            r#"{
                "input_i" : "-27.61",
                "input_tp" : "-2.32",
                "input_lra" : "0.70",
                "input_thresh" : "-38.83",
                "output_i" : "-16.01",
                "output_tp" : "-1.50",
                "output_lra" : "0.70",
                "output_thresh" : "-27.23",
                "normalization_type" : "linear",
                "target_offset" : "0.01"
            }"#,
        )
        .unwrap();
        let measured = stats.measured().unwrap();
        assert_eq!(measured.input_i, -27.61);
        assert_eq!(measured.input_tp, -2.32);
        assert_eq!(measured.input_lra, 0.70);
        assert_eq!(measured.input_thresh, -38.83);
        assert_eq!(measured.target_offset, 0.01);
    }

    #[test]
    fn invalid_or_silent_stats_are_rejected() {
        let stats: LoudnormStats = serde_json::from_str(
            r#"{
                "input_i" : "-inf",
                "input_tp" : "-inf",
                "input_lra" : "0.00",
                "input_thresh" : "-70.00",
                "target_offset" : "0.00"
            }"#,
        )
        .unwrap();
        assert!(stats.measured().is_err());
    }

    #[test]
    fn targets_are_safe_defaults() {
        assert_eq!(TARGET_LUFS, -16.0);
        assert_eq!(TARGET_TRUE_PEAK_DB, -1.5);
        assert_eq!(TARGET_LRA, 11.0);
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

        assert!(needs_processing(flac, AudioQuality::Mp3, false));
        assert!(!needs_processing(mp3, AudioQuality::Mp3, false));
        assert!(needs_processing(mp3, AudioQuality::Mp3, true));
        assert!(!needs_processing(flac, AudioQuality::Flac, false));
        assert!(needs_processing(flac, AudioQuality::Flac, true));
        assert!(!needs_processing(mp3, AudioQuality::Flac, false));
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
}
