use std::collections::VecDeque;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

use crate::error::{AudioError, Result};
use crate::fmt::PcmFormat;
use crate::http;
use crate::redact::redact_url;

/// A decoded track ready (or nearly ready) to be pumped into a sink.
pub struct TrackSource {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub uri: String,
    pub duration_ms: Option<u64>,
    /// Known once the first packet has been decoded.
    pub format: Option<PcmFormat>,
    /// Decoded, not-yet-consumed interleaved i32 samples.
    pub pending: VecDeque<i32>,
    pub end_of_stream: bool,
    scratch: Vec<i32>,
    pub seekable: bool,
    time_base: Option<TimeBase>,
    titles: Option<crossbeam_channel::Receiver<String>>,
    /// Sample rate announced by the container (before any decode).
    rate: Option<u32>,
    /// Range of the file played as this track, in frames (cue sheets).
    span: Option<Span>,
    /// Decoded frames before this one are dropped (span start, seeks).
    skip_to: u64,
    /// Samples decoded past the span end: the start of whatever follows it
    /// in the file.
    overflow: VecDeque<i32>,
    /// Local file being decoded.
    path: Option<String>,
}

/// Part of a file played as a track, in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u64,
    end: Option<u64>,
}

/// A time range carried by a `file://` URI fragment, in seconds:
/// `#t=start[,end]` as in W3C media fragments (`npt:` prefix allowed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub start: f64,
    pub end: Option<f64>,
}

impl Clip {
    fn frames(&self, rate: u32) -> Span {
        let at = |s: f64| (s * rate as f64).round().max(0.0) as u64;
        Span {
            start: at(self.start),
            end: self.end.map(at),
        }
    }
}

/// Split a `file://` URI into the URI of the file and its clip, if any.
/// Paths in `file://` URIs have `#` percent-encoded, so a bare `#` always
/// starts a fragment.
pub fn split_clip(uri: &str) -> (&str, Option<Clip>) {
    if !uri.starts_with("file://") {
        return (uri, None);
    }
    let Some((base, frag)) = uri.split_once('#') else {
        return (uri, None);
    };
    let clip = frag
        .split('&')
        .find_map(|kv| kv.strip_prefix("t="))
        .and_then(|v| {
            let v = v.strip_prefix("npt:").unwrap_or(v);
            let (a, b) = v.split_once(',').unwrap_or((v, ""));
            let num = |x: &str| x.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0);
            let start = if a.is_empty() { 0.0 } else { num(a)? };
            let end = if b.is_empty() { None } else { Some(num(b)?) };
            (end.is_none_or(|e| e > start)).then_some(Clip { start, end })
        });
    (base, clip)
}

#[cfg(test)]
#[test]
fn clip_fragments() {
    assert_eq!(split_clip("file:///a.flac"), ("file:///a.flac", None));
    assert_eq!(
        split_clip("file:///a.flac#t=1.5,3"),
        (
            "file:///a.flac",
            Some(Clip {
                start: 1.5,
                end: Some(3.0)
            })
        )
    );
    assert_eq!(
        split_clip("file:///a.flac#t=npt:2"),
        (
            "file:///a.flac",
            Some(Clip {
                start: 2.0,
                end: None
            })
        )
    );
    assert_eq!(split_clip("file:///a.flac#t=3,1").1, None);
    assert_eq!(split_clip("file:///a.flac#x").1, None);
    assert_eq!(split_clip("/x/#1.flac"), ("/x/#1.flac", None));
    assert_eq!(split_clip("http://h/a#t=1").1, None);
    // A cue frame (1/75 s) written with 9 decimals lands on its sample.
    let c = split_clip("file:///a#t=12.346666667").1.unwrap();
    assert_eq!(c.frames(44100).start, 926 * 588);
}

/// Integer width of the decoded buffer; `None` for float output (lossy
/// codecs), whose full-scale content we keep 24 bits of, like every other
/// lossy player.
fn native_bits(b: &GenericAudioBufferRef) -> Option<u8> {
    use GenericAudioBufferRef as G;
    match b {
        G::U8(_) | G::S8(_) => Some(8),
        G::U16(_) | G::S16(_) => Some(16),
        G::U24(_) | G::S24(_) => Some(24),
        G::U32(_) | G::S32(_) => Some(32),
        G::F32(_) | G::F64(_) => None,
    }
}

fn path_from_uri(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("file://") {
        Some(percent_decode(rest))
    } else if uri.contains("://") {
        None
    } else {
        Some(uri.to_string())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[test]
fn percent_decode_edges() {
    assert_eq!(percent_decode("%aé"), "%aé");
    assert_eq!(percent_decode("a%41"), "aA");
    assert_eq!(percent_decode("%"), "%");
    assert_eq!(percent_decode("%4"), "%4");
}

fn extension_of(uri: &str) -> Option<String> {
    let path = uri.rsplit_once(['?', '#']).map_or(uri, |a| a.0);
    let ext = path.rsplit_once('.')?.1.to_lowercase();
    if ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext)
    } else {
        None
    }
}

impl TrackSource {
    pub fn open(uri: &str) -> Result<TrackSource> {
        let (file_uri, clip) = split_clip(uri);
        let local = path_from_uri(file_uri);
        let (src, seekable, titles): (Box<dyn MediaSource>, bool, _) =
            if let Some(path) = local.clone() {
                let file = std::fs::File::open(&path)
                    .map_err(|e| AudioError::UnsupportedSource(format!("open {path}: {e}")))?;
                (Box::new(file), true, None)
            } else if uri.starts_with("http://") || uri.starts_with("https://") {
                let h = http::open(uri)?;
                (h.source, h.seekable, h.titles)
            } else {
                return Err(AudioError::UnsupportedSource(redact_url(uri)));
            };
        let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());

        let mut hint = Hint::new();
        if let Some(ext) = extension_of(file_uri) {
            hint.with_extension(&ext);
        }
        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| AudioError::Decode(format!("probe {}: {e}", redact_url(uri))))?;

        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| AudioError::Decode(format!("no audio track in {}", redact_url(uri))))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let num_frames = track.num_frames;

        let params = match track.codec_params.as_ref() {
            Some(CodecParameters::Audio(p)) => p,
            _ => return Err(AudioError::Decode("no decodable audio codec".into())),
        };
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| AudioError::Decode(format!("decoder: {e}")))?;

        let tb = time_base.or_else(|| params.sample_rate.and_then(TimeBase::try_from_recip));
        let mut duration_ms = match (track.duration, tb) {
            (Some(dur), Some(tb)) => tb.calc_duration(dur).map(|t| t.as_millis() as u64),
            _ => None,
        };
        if duration_ms.is_none()
            && let (Some(nf), Some(sr)) = (num_frames, params.sample_rate)
        {
            duration_ms = Some(nf * 1000 / sr as u64);
        }
        let rate = params.sample_rate;
        let span = match (clip, rate) {
            (None, _) => None,
            (Some(c), Some(r)) => Some(c.frames(r)),
            (Some(_), None) => {
                return Err(AudioError::Decode(format!(
                    "no sample rate to cut {}",
                    redact_url(uri)
                )));
            }
        };
        if let (Some(sp), Some(r)) = (span, rate) {
            let file_ms = duration_ms.unwrap_or(0);
            duration_ms = Some(match sp.end {
                Some(e) => (e - sp.start) * 1000 / r as u64,
                None => file_ms.saturating_sub(sp.start * 1000 / r as u64),
            });
        }

        let mut src = TrackSource {
            reader,
            decoder,
            track_id,
            uri: uri.into(),
            duration_ms,
            format: None,
            pending: VecDeque::new(),
            end_of_stream: false,
            scratch: Vec::new(),
            seekable,
            time_base: tb,
            titles,
            rate,
            span,
            skip_to: 0,
            overflow: VecDeque::new(),
            path: local,
        };
        if let Some(sp) = span.filter(|s| s.start > 0) {
            src.seek_frame(sp.start)?;
        }
        Ok(src)
    }

    /// Whether `uri` is the part of the same file that directly follows
    /// this one, so decoding can just go on (gapless cue tracks).
    pub fn continues_into(&self, uri: &str) -> bool {
        let (file_uri, clip) = split_clip(uri);
        let (Some(span), Some(clip), Some(rate)) = (self.span, clip, self.rate) else {
            return false;
        };
        self.path.is_some()
            && path_from_uri(file_uri) == self.path
            && span
                .end
                .is_some_and(|e| e == clip.frames(rate).start && self.skip_to <= e)
    }

    /// The span end was reached and everything before it was handed out.
    pub fn span_done(&self) -> bool {
        self.span.is_some_and(|s| s.end.is_some()) && self.end_of_stream && self.pending.is_empty()
    }

    /// Carry on with the next part of the file (see `continues_into`): the
    /// samples decoded past the old end are the first ones of the new part.
    pub fn continue_into(&mut self, uri: &str) {
        let (_, Some(clip)) = split_clip(uri) else {
            return;
        };
        let Some(rate) = self.rate else { return };
        let span = clip.frames(rate);
        let ch = self.format.map_or(2, |f| f.channels.max(1)) as usize;
        self.uri = uri.into();
        self.span = Some(span);
        self.skip_to = span.start;
        self.end_of_stream = false;
        self.pending = std::mem::take(&mut self.overflow);
        if let Some(end) = span.end {
            // A part shorter than one packet: what lies beyond it waits.
            let keep = ((end - span.start) as usize * ch).min(self.pending.len());
            self.overflow = self.pending.split_off(keep);
            if !self.overflow.is_empty() {
                self.end_of_stream = true;
            }
        }
        self.duration_ms = span.end.map(|e| (e - span.start) * 1000 / rate as u64);
        if self.duration_ms.is_none()
            && let Some(file_ms) = self.file_duration_ms()
        {
            self.duration_ms = Some(file_ms.saturating_sub(span.start * 1000 / rate as u64));
        }
    }

    /// Exact length of a span in frames (`None` without a span, or when the
    /// container does not say how long the file is).
    pub fn span_frames(&self) -> Option<u64> {
        let span = self.span?;
        let end = match span.end {
            Some(e) => e,
            None => {
                let track = self
                    .reader
                    .tracks()
                    .iter()
                    .find(|t| t.id == self.track_id)?;
                track.num_frames.or_else(|| {
                    let rate = self.rate?;
                    let tb = self.time_base?;
                    let dur = track.duration?;
                    if TimeBase::try_from_recip(rate) == Some(tb) {
                        Some(dur.get())
                    } else {
                        None
                    }
                })?
            }
        };
        Some(end.saturating_sub(span.start))
    }

    fn file_duration_ms(&self) -> Option<u64> {
        let track = self
            .reader
            .tracks()
            .iter()
            .find(|t| t.id == self.track_id)?;
        let (dur, tb) = (track.duration?, self.time_base?);
        tb.calc_duration(dur).map(|t| t.as_millis() as u64)
    }

    /// Frame index of a timestamp of the decoded track.
    fn frame_of(&self, ts: Timestamp) -> u64 {
        let (Some(rate), Some(tb)) = (self.rate, self.time_base) else {
            return ts.get().max(0) as u64;
        };
        if TimeBase::try_from_recip(rate) == Some(tb) {
            return ts.get().max(0) as u64;
        }
        tb.calc_time(ts).map_or(0, |t| {
            (t.as_secs_f64() * rate as f64).round().max(0.0) as u64
        })
    }

    /// Seek to an exact frame: the demuxer lands at or before it, and what
    /// comes before it is dropped as it is decoded.
    fn seek_frame(&mut self, frame: u64) -> Result<()> {
        let rate = self.rate.unwrap_or(44100);
        let to = match self.time_base {
            Some(tb) if TimeBase::try_from_recip(rate) == Some(tb) => SeekTo::Timestamp {
                ts: Timestamp::new(frame as i64),
                track_id: self.track_id,
            },
            _ => SeekTo::Time {
                time: symphonia::core::units::Time::try_from_secs_f64(frame as f64 / rate as f64)
                    .ok_or_else(|| AudioError::Decode("seek out of range".into()))?,
                track_id: Some(self.track_id),
            },
        };
        self.reader
            .seek(SeekMode::Accurate, to)
            .map_err(|e| AudioError::Decode(format!("seek: {e}")))?;
        self.decoder.reset();
        self.pending.clear();
        self.overflow.clear();
        self.end_of_stream = false;
        self.skip_to = frame;
        Ok(())
    }

    /// Keep the decoded samples that fall inside the span; true when the
    /// span end was reached.
    fn cut(&mut self, first: u64, span: Span, ch: usize) -> bool {
        let n = (self.scratch.len() / ch) as u64;
        let lo = self.skip_to.saturating_sub(first).min(n);
        let hi = span
            .end
            .map_or(n, |e| e.saturating_sub(first).min(n))
            .max(lo);
        let done = hi < n;
        if done {
            self.overflow.extend(self.scratch.drain(hi as usize * ch..));
        }
        self.pending
            .extend(self.scratch.drain(..).skip(lo as usize * ch));
        done
    }

    /// Decode one packet (or until some samples are available). Returns
    /// `Ok(true)` while samples remain somewhere (pending or stream),
    /// `Ok(false)` at end of stream with an empty pending queue.
    pub fn pump(&mut self) -> Result<bool> {
        if !self.pending.is_empty() {
            return Ok(true);
        }
        loop {
            if self.end_of_stream {
                return Ok(false);
            }
            match self.reader.next_packet() {
                Ok(None) => {
                    self.end_of_stream = true;
                    let _ = self.decoder.finalize();
                    return Ok(false);
                }
                Ok(Some(packet)) => {
                    if packet.track_id != self.track_id {
                        continue;
                    }
                    let first = self.frame_of(packet.pts);
                    match self.decoder.decode(&packet) {
                        Ok(buf) => {
                            let spec = buf.spec();
                            let rate = spec.rate();
                            let channels = spec.channels().count() as u16;
                            let native = native_bits(&buf);
                            self.scratch.clear();
                            buf.copy_to_vec_interleaved::<i32>(&mut self.scratch);
                            let bits = self
                                .decoder
                                .codec_params()
                                .bits_per_sample
                                .map(|b| b as u8)
                                .or(native)
                                .unwrap_or(24)
                                .clamp(1, 32);
                            // Conversion to i32 scales every sample type to
                            // full range (content MSB-aligned); shift back to
                            // LSB alignment so the container stage can
                            // re-align losslessly.
                            let rshift = 32 - bits as u32;
                            if rshift > 0 {
                                for v in self.scratch.iter_mut() {
                                    *v >>= rshift;
                                }
                            }
                            self.format = Some(PcmFormat {
                                sample_rate: rate,
                                channels,
                                bits,
                            });
                            if let Some(span) = self.span {
                                if self.cut(first, span, channels.max(1) as usize) {
                                    self.end_of_stream = true;
                                    return Ok(!self.pending.is_empty());
                                }
                                if self.pending.is_empty() {
                                    continue;
                                }
                                return Ok(true);
                            }
                            self.pending.extend(self.scratch.drain(..));
                            return Ok(true);
                        }
                        Err(symphonia::core::errors::Error::DecodeError(_))
                        | Err(symphonia::core::errors::Error::IoError(_)) => {
                            // undecodable packet: skip
                            continue;
                        }
                        Err(e) => {
                            return Err(AudioError::Decode(format!("decode: {e}")));
                        }
                    }
                }
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                // A truncated final frame surfaces as an unexpected EOF, not
                // as `Ok(None)` — treat it as end of stream.
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.end_of_stream = true;
                    let _ = self.decoder.finalize();
                    return Ok(!self.pending.is_empty());
                }
                // Anything else is the transport failing (network, disk):
                // not an end of track.
                Err(symphonia::core::errors::Error::IoError(e)) => {
                    return Err(AudioError::Io(e));
                }
                Err(e) => return Err(AudioError::Decode(format!("packet: {e}"))),
            }
        }
    }

    /// Latest stream title announced by the source (internet radio), if a
    /// new one arrived since the last call.
    pub fn take_stream_title(&mut self) -> Option<String> {
        self.titles.as_ref().and_then(|rx| rx.try_iter().last())
    }

    /// Seek (seekable sources only). Returns the position actually reached:
    /// the demuxer lands on a packet boundary at or before the request, and
    /// that is where playback resumes.
    pub fn seek_ms(&mut self, ms: u64) -> Result<std::time::Duration> {
        if let (Some(span), Some(rate)) = (self.span, self.rate) {
            // Inside a span: relative to its start, and to the frame.
            let mut frame = span.start + ms * rate as u64 / 1000;
            if let Some(end) = span.end {
                frame = frame.min(end);
            }
            self.seek_frame(frame)?;
            let rel = frame - span.start;
            return Ok(std::time::Duration::from_secs_f64(rel as f64 / rate as f64));
        }
        let time = symphonia::core::units::Time::try_from_secs_f64(ms as f64 / 1000.0)
            .ok_or_else(|| AudioError::Decode("seek out of range".into()))?;
        let seeked = self
            .reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| AudioError::Decode(format!("seek: {e}")))?;
        self.decoder.reset();
        self.pending.clear();
        self.end_of_stream = false;
        let actual = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map_or(ms as f64 / 1000.0, |t| t.as_secs_f64().max(0.0));
        Ok(std::time::Duration::from_secs_f64(actual))
    }
}
