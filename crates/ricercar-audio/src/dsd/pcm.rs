//! DSD → 24-bit PCM, for DACs without DSD. Not bit-perfect: this is a
//! conversion, and the signal path says so.
//!
//! Filter: a cascade of linear-phase FIR lowpass filters (Kaiser-windowed
//! sinc, designed here at start-up), each followed by decimation.
//!
//! 1. 1-bit input, decimate by 8. The filter runs on whole bytes through
//!    lookup tables (one table of 256 partial sums per byte of taps).
//! 2. Decimate by 2, as many times as needed to reach 176.4 kHz (once for
//!    DSD64, twice for DSD128…).
//! 3. Decimate by 2 to 88.2 kHz, the output rate (one more halving to
//!    44.1 kHz when the DAC refuses 88.2 kHz).
//!
//! Every stage keeps 0–20 kHz flat (ripple about 10⁻⁶, far below 0.01 dB)
//! and attenuates by 120 dB whatever would fold back below the protected
//! band: 44.1 kHz at 88.2 kHz out, so the shaped DSD noise above it never
//! aliases down; 22.05 kHz at 44.1 kHz out. Between 20 kHz and the stop
//! edge (44.1 kHz at 88.2 kHz) the filters roll off, and DSD's rising
//! ultrasonic noise passes in part, as with any DSD-to-PCM converter.
//!
//! Level: DSD's full ±1 swing maps to PCM full scale. The SACD 0 dB
//! reference is 50 % modulation, so it lands at −6 dBFS: 6 dB of headroom
//! for the peaks DSD allows above its reference (up to +3 dB), which would
//! otherwise clip. The output is saturated, never wrapped, as a last
//! resort. No dither: DSD's own noise floor is far above 24-bit resolution.

use std::collections::VecDeque;

use super::source::DsdFormat;

/// Kept flat.
pub const PASS_HZ: f64 = 20_000.0;
/// Stopband attenuation of every stage.
pub const STOP_DB: f64 = 120.0;
/// Output rates, preferred first.
pub const RATES: [u32; 2] = [88_200, 44_100];

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > sum * 1e-17 {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

fn kaiser_beta(atten: f64) -> f64 {
    if atten > 50.0 {
        0.1102 * (atten - 8.7)
    } else if atten > 21.0 {
        0.5842 * (atten - 21.0).powf(0.4) + 0.07886 * (atten - 21.0)
    } else {
        0.0
    }
}

/// Kaiser's estimate of the length reaching `atten` dB over a transition
/// band of `width` Hz at `fs`.
fn kaiser_taps(atten: f64, width: f64, fs: f64) -> usize {
    let dw = 2.0 * std::f64::consts::PI * width / fs;
    ((atten - 7.95) / (2.285 * dw)).ceil() as usize + 1
}

/// Windowed-sinc lowpass of `taps` coefficients, -6 dB at `cutoff` Hz,
/// normalized to unity gain at DC.
pub fn lowpass(taps: usize, cutoff: f64, fs: f64, beta: f64) -> Vec<f64> {
    let fc = cutoff / fs;
    let m = (taps - 1) as f64 / 2.0;
    let i0b = bessel_i0(beta);
    let mut h: Vec<f64> = (0..taps)
        .map(|n| {
            let x = n as f64 - m;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let r = x / m;
            sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h
}

/// One decimation stage: input rate, factor and coefficients.
#[derive(Debug, Clone)]
pub struct Stage {
    pub fs_in: f64,
    pub factor: usize,
    pub stop: f64,
    pub taps: Vec<f64>,
}

impl Stage {
    fn design(fs_in: f64, factor: usize, stop: f64, multiple: usize) -> Stage {
        // Kaiser's estimate runs short on short filters: 10 dB of margin.
        let n = kaiser_taps(STOP_DB + 10.0, stop - PASS_HZ, fs_in).next_multiple_of(multiple);
        let taps = lowpass(
            n,
            (PASS_HZ + stop) / 2.0,
            fs_in,
            kaiser_beta(STOP_DB + 10.0),
        );
        Stage {
            fs_in,
            factor,
            stop,
            taps,
        }
    }

    /// Magnitude response at `f` Hz.
    pub fn response(&self, f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f / self.fs_in;
        let (re, im) = self
            .taps
            .iter()
            .enumerate()
            .fold((0.0, 0.0), |(re, im), (n, &h)| {
                (re + h * (w * n as f64).cos(), im - h * (w * n as f64).sin())
            });
        (re * re + im * im).sqrt()
    }
}

/// The stages from `dsd_rate` down to `out_rate`.
pub fn plan(dsd_rate: u32, out_rate: u32) -> Vec<Stage> {
    let protect = (out_rate as f64 / 2.0).min(44_100.0);
    let mut fs = dsd_rate as f64;
    let mut stages = vec![Stage::design(fs, 8, fs / 8.0 - protect, 8)];
    fs /= 8.0;
    while fs > out_rate as f64 * 1.5 {
        stages.push(Stage::design(fs, 2, fs / 2.0 - protect, 1));
        fs /= 2.0;
    }
    stages
}

/// FIR state of one later stage, for one channel. The history is stored
/// twice so the last `n` samples are always one contiguous slice.
#[derive(Clone)]
struct Fir {
    buf: Vec<f64>,
    at: usize,
    phase: usize,
}

impl Fir {
    fn new(n: usize) -> Fir {
        Fir {
            buf: vec![0.0; 2 * n],
            at: 0,
            phase: 0,
        }
    }

    fn push(&mut self, x: f64, taps: &[f64], factor: usize) -> Option<f64> {
        let n = taps.len();
        self.buf[self.at] = x;
        self.buf[self.at + n] = x;
        self.at = (self.at + 1) % n;
        self.phase += 1;
        if self.phase < factor {
            return None;
        }
        self.phase = 0;
        // Oldest to newest; the taps are symmetric.
        let w = &self.buf[self.at..self.at + n];
        Some(w.iter().zip(taps).map(|(a, b)| a * b).sum())
    }
}

#[derive(Clone)]
struct Channel {
    /// Last bytes of stage 1, newest at `at`.
    bytes: Vec<u8>,
    at: usize,
    firs: Vec<Fir>,
    out: Vec<f64>,
}

/// DSD → PCM converter for one stream.
pub struct Decimator {
    format: DsdFormat,
    out_rate: u32,
    /// Stage 1: `tables[k][b]` is the contribution of byte value `b` when
    /// it is the k-th newest byte.
    tables: Vec<[f64; 256]>,
    later: Vec<Stage>,
    chans: Vec<Channel>,
}

impl Decimator {
    pub fn new(format: DsdFormat, out_rate: u32) -> Decimator {
        let mut stages = plan(format.rate, out_rate);
        let first = stages.remove(0);
        let tables = first
            .taps
            .chunks(8)
            .map(|c| {
                let mut t = [0.0; 256];
                for (b, v) in t.iter_mut().enumerate() {
                    // MSB first: bit j (from the LSB) is the j-th newest.
                    *v = c
                        .iter()
                        .enumerate()
                        .map(|(j, h)| if b >> j & 1 == 1 { *h } else { -*h })
                        .sum();
                }
                t
            })
            .collect::<Vec<_>>();
        let chan = Channel {
            bytes: vec![super::SILENCE; tables.len()],
            at: 0,
            firs: stages.iter().map(|s| Fir::new(s.taps.len())).collect(),
            out: Vec::new(),
        };
        Decimator {
            format,
            out_rate,
            tables,
            later: stages,
            chans: vec![chan; format.channels.max(1) as usize],
        }
    }

    pub fn format(&self) -> DsdFormat {
        self.format
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Forget the past (after a seek).
    pub fn reset(&mut self) {
        *self = Decimator::new(self.format, self.out_rate);
    }

    /// Convert interleaved DSD bytes; appends interleaved 24-bit samples.
    pub fn process(&mut self, bytes: &[u8], out: &mut VecDeque<i32>) {
        let ch = self.chans.len();
        let k = self.tables.len();
        for (c, chan) in self.chans.iter_mut().enumerate() {
            chan.out.clear();
            for &b in bytes.iter().skip(c).step_by(ch) {
                chan.at = (chan.at + 1) % k;
                chan.bytes[chan.at] = b;
                let mut y = 0.0;
                for (i, t) in self.tables.iter().enumerate() {
                    y += t[chan.bytes[(chan.at + k - i) % k] as usize];
                }
                let mut v = Some(y);
                for (fir, st) in chan.firs.iter_mut().zip(&self.later) {
                    v = v.and_then(|x| fir.push(x, &st.taps, st.factor));
                }
                if let Some(v) = v {
                    chan.out.push(v);
                }
            }
        }
        let n = self.chans[0].out.len();
        const FULL: f64 = (1 << 23) as f64;
        for i in 0..n {
            for chan in &self.chans {
                let v = (chan.out[i] * FULL).round().clamp(-FULL, FULL - 1.0);
                out.push_back(v as i32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    /// The cascade's response at `f` (each stage at its own rate).
    fn cascade(stages: &[Stage], f: f64) -> f64 {
        stages.iter().map(|s| s.response(f)).product()
    }

    #[test]
    fn passband_is_flat_and_aliases_are_stopped() {
        for (rate, out) in [
            (2_822_400, 88_200),
            (5_644_800, 88_200),
            (11_289_600, 88_200),
            (2_822_400, 44_100),
        ] {
            let stages = plan(rate, out);
            let fs_out = stages
                .iter()
                .fold(rate as f64, |fs, s| fs / s.factor as f64);
            assert_eq!(fs_out, out as f64);
            for i in 0..=200 {
                let f = PASS_HZ * i as f64 / 200.0;
                let g = db(cascade(&stages, f));
                assert!(g.abs() < 0.01, "{rate}->{out}: {f} Hz at {g} dB");
            }
            // Whatever a stage folds onto the protected band is stopped.
            let protect = (out as f64 / 2.0).min(44_100.0);
            for s in &stages {
                let fs_next = s.fs_in / s.factor as f64;
                for k in 1..=s.factor / 2 {
                    for i in 0..=50 {
                        let f = k as f64 * fs_next - protect + 2.0 * protect * i as f64 / 50.0;
                        if f >= s.stop && f <= s.fs_in / 2.0 {
                            let g = db(s.response(f));
                            assert!(
                                g < -STOP_DB,
                                "{rate}: stage at {} Hz: {f} Hz at {g} dB",
                                s.fs_in
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn steady_ones_and_zeros_are_full_scale() {
        let fmt = DsdFormat {
            rate: 2_822_400,
            channels: 2,
        };
        let mut d = Decimator::new(fmt, 88_200);
        let mut out = VecDeque::new();
        // L all ones (+1), R all zeros (−1).
        let bytes: Vec<u8> = (0..20_000).flat_map(|_| [0xFF, 0x00]).collect();
        d.process(&bytes, &mut out);
        assert_eq!(out.len(), 2 * 20_000 * 8 / 32);
        let (l, r) = (out[out.len() - 2], out[out.len() - 1]);
        assert_eq!(l, (1 << 23) - 1);
        assert_eq!(r, -(1 << 23));
        // Silence pattern: zero.
        let mut d = Decimator::new(fmt, 88_200);
        let mut out = VecDeque::new();
        d.process(&vec![super::super::SILENCE; 40_000], &mut out);
        assert!(
            out.iter().skip(200).all(|&v| v.abs() < 2),
            "{:?}",
            &out.as_slices().0[200..210]
        );
    }
}
