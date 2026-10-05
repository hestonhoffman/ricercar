//! A DSD source made into the engine's samples for one output mode:
//! DoP payloads, native words, or converted PCM.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use super::pcm::{Decimator, RATES};
use super::source::{DsdFormat, DsdSource};
use super::{DsdMode, check_format, dop, native};
use crate::fmt::{PcmFormat, Wire};

/// Bytes per channel read per pump (about 23 ms of DSD64).
const CHUNK: usize = 8192;

pub struct DsdPlayback {
    src: Box<dyn DsdSource>,
    format: DsdFormat,
    mode: DsdMode,
    conv: Option<Decimator>,
    buf: Vec<u8>,
    /// Bytes waiting for the rest of their word (DoP: 2 per channel,
    /// native: 4).
    carry: Vec<u8>,
    /// Played range, in bytes per channel.
    start: u64,
    end: Option<u64>,
    /// Next byte per channel to read.
    pos: u64,
    done: bool,
}

impl DsdPlayback {
    /// `clip` plays part of the stream only: (start, end) in seconds.
    pub fn new(
        mut src: Box<dyn DsdSource>,
        mode: DsdMode,
        clip: Option<(f64, Option<f64>)>,
    ) -> io::Result<DsdPlayback> {
        let format = src.format();
        check_format(format)?;
        let unit = unit(mode) as u64;
        let bytes =
            |secs: f64| (secs * format.rate as f64 / 8.0).round().max(0.0) as u64 / unit * unit;
        let len = src.len();
        let (start, end) = match clip {
            Some((a, b)) => (bytes(a), b.map(bytes)),
            None => (0, None),
        };
        let end = match (end, len) {
            (Some(e), Some(l)) => Some(e.min(l)),
            (e, l) => e.or(l),
        };
        if start > 0 {
            src.seek(start)?;
        }
        Ok(DsdPlayback {
            conv: (mode == DsdMode::Pcm).then(|| Decimator::new(format, RATES[0])),
            src,
            format,
            mode,
            buf: Vec::new(),
            carry: Vec::new(),
            start,
            end,
            pos: start,
            done: false,
        })
    }

    pub fn mode(&self) -> DsdMode {
        self.mode
    }

    pub fn source_format(&self) -> DsdFormat {
        self.format
    }

    pub fn tags(&self) -> Vec<(String, String)> {
        self.src.tags()
    }

    /// What the engine hands to the sink.
    pub fn pcm_format(&self) -> PcmFormat {
        let channels = self.format.channels;
        match self.mode {
            DsdMode::Dop => PcmFormat {
                sample_rate: dop::pcm_rate(self.format.rate),
                channels,
                bits: 24,
            },
            DsdMode::Native => PcmFormat {
                sample_rate: native::word_rate(self.format.rate),
                channels,
                bits: 32,
            },
            DsdMode::Pcm => PcmFormat {
                sample_rate: self.conv.as_ref().map_or(RATES[0], |c| c.out_rate()),
                channels,
                bits: 24,
            },
        }
    }

    pub fn wire(&self) -> Wire {
        match self.mode {
            DsdMode::Pcm => Wire::Pcm,
            DsdMode::Dop => Wire::Dop,
            DsdMode::Native => Wire::NativeDsd,
        }
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.end
            .map(|e| (e - self.start.min(e)) * 8000 / self.format.rate as u64)
    }

    /// Frames of `pcm_format` the played range makes.
    pub fn out_frames(&self) -> Option<u64> {
        let rate = self.pcm_format().sample_rate as u64;
        self.end
            .map(|e| (e - self.start.min(e)) * 8 * rate / self.format.rate as u64)
    }

    /// Read and convert one chunk into `out`; false once the stream is
    /// over (what was left has been flushed into `out`).
    pub fn pump(&mut self, out: &mut VecDeque<i32>) -> io::Result<bool> {
        if self.done {
            return Ok(false);
        }
        let ch = self.format.channels.max(1) as usize;
        let left = self
            .end
            .map_or(CHUNK as u64, |e| e.saturating_sub(self.pos));
        let want = (CHUNK as u64).min(left) as usize;
        let n = if want == 0 {
            0
        } else {
            self.buf.resize(want * ch, 0);
            self.src.read(&mut self.buf)? / ch
        };
        if n == 0 {
            // End: a partial word goes out padded with silence.
            let tail = std::mem::take(&mut self.carry);
            self.emit(&tail, out);
            self.done = true;
            return Ok(false);
        }
        self.pos += n as u64;
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(&self.buf[..n * ch]);
        let step = unit(self.mode) * ch;
        let whole = data.len() / step * step;
        self.emit(&data[..whole], out);
        self.carry = data.split_off(whole);
        Ok(true)
    }

    fn emit(&mut self, bytes: &[u8], out: &mut VecDeque<i32>) {
        if bytes.is_empty() {
            return;
        }
        let ch = self.format.channels.max(1) as usize;
        match (self.mode, self.conv.as_mut()) {
            (DsdMode::Dop, _) => dop::payloads(bytes, ch, out),
            (DsdMode::Native, _) => native::words(bytes, ch, out),
            (DsdMode::Pcm, Some(c)) => c.process(bytes, out),
            (DsdMode::Pcm, None) => {}
        }
    }

    /// Seek to `ms` from the start of the played range; returns where it
    /// landed (on a word boundary).
    pub fn seek_ms(&mut self, ms: u64) -> io::Result<Duration> {
        let unit = unit(self.mode) as u64;
        let mut rel = ms * self.format.rate as u64 / 8000 / unit * unit;
        if let Some(e) = self.end {
            rel = rel.min(e.saturating_sub(self.start));
        }
        self.src.seek(self.start + rel)?;
        self.pos = self.start + rel;
        self.carry.clear();
        self.done = false;
        if let Some(c) = &mut self.conv {
            c.reset();
        }
        Ok(Duration::from_secs_f64(
            rel as f64 * 8.0 / self.format.rate as f64,
        ))
    }

    /// Converted PCM refused by the DAC at its rate: go one rate down, from
    /// the start of the played range. False when there is none left.
    pub fn lower_rate(&mut self) -> bool {
        let Some(c) = &self.conv else { return false };
        let Some(&next) = RATES.iter().find(|&&r| r < c.out_rate()) else {
            return false;
        };
        self.conv = Some(Decimator::new(self.format, next));
        self.seek_ms(0).is_ok()
    }

    /// Hand the converter's state over to the next track, so a gapless
    /// transition is a continuous filter, not a restart from silence.
    pub fn take_converter(&mut self) -> Option<Decimator> {
        self.conv.take()
    }

    pub fn adopt_converter(&mut self, prev: Option<Decimator>) {
        if let (Some(p), Some(c)) = (prev, &self.conv)
            && p.format() == c.format()
            && p.out_rate() == c.out_rate()
            && self.pos == self.start
        {
            self.conv = Some(p);
        }
    }
}

fn unit(mode: DsdMode) -> usize {
    match mode {
        DsdMode::Pcm => 1,
        DsdMode::Dop => 2,
        DsdMode::Native => 4,
    }
}
