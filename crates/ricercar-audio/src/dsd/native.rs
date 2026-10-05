//! Native DSD: ALSA's DSD sample formats. The pipeline carries DSD as one
//! 32-bit word per channel (4 bytes, earliest byte on top); the sink splits
//! the words into whatever the driver takes.

use std::collections::VecDeque;

use super::SILENCE;

/// ALSA DSD formats, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativeFormat {
    U32Be,
    U32Le,
    U16Le,
    U16Be,
    U8,
}

impl NativeFormat {
    pub const ALL: [NativeFormat; 5] = [
        NativeFormat::U32Be,
        NativeFormat::U32Le,
        NativeFormat::U16Le,
        NativeFormat::U16Be,
        NativeFormat::U8,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NativeFormat::U8 => "DSD_U8",
            NativeFormat::U16Le => "DSD_U16_LE",
            NativeFormat::U16Be => "DSD_U16_BE",
            NativeFormat::U32Le => "DSD_U32_LE",
            NativeFormat::U32Be => "DSD_U32_BE",
        }
    }

    pub fn from_label(label: &str) -> Option<NativeFormat> {
        NativeFormat::ALL.into_iter().find(|f| f.label() == label)
    }

    /// Bytes of DSD per channel in one ALSA sample.
    pub fn bytes(self) -> usize {
        match self {
            NativeFormat::U8 => 1,
            NativeFormat::U16Le | NativeFormat::U16Be => 2,
            NativeFormat::U32Le | NativeFormat::U32Be => 4,
        }
    }

    /// ALSA frames per pipeline word.
    pub fn frames_per_word(self) -> usize {
        4 / self.bytes()
    }

    pub(crate) fn alsa(self) -> alsa::pcm::Format {
        use alsa::pcm::Format;
        match self {
            NativeFormat::U8 => Format::DSDU8,
            NativeFormat::U16Le => Format::DSDU16LE,
            NativeFormat::U16Be => Format::DSDU16BE,
            NativeFormat::U32Le => Format::DSDU32LE,
            NativeFormat::U32Be => Format::DSDU32BE,
        }
    }
}

/// Word rate carrying `dsd_rate`: 88.2 kHz for DSD64.
pub fn word_rate(dsd_rate: u32) -> u32 {
    dsd_rate / 32
}

/// Interleaved DSD bytes → one 32-bit word per channel and frame (4 bytes
/// per channel, earliest on top). A partial last frame is padded with
/// silence.
pub fn words(bytes: &[u8], channels: usize, out: &mut VecDeque<i32>) {
    let ch = channels.max(1);
    for frame in bytes.chunks(4 * ch) {
        for c in 0..ch {
            let mut w = 0u32;
            for k in 0..4 {
                w = (w << 8) | frame.get(k * ch + c).copied().unwrap_or(SILENCE) as u32;
            }
            out.push_back(w as i32);
        }
    }
}

/// Words → the bytes ALSA expects for `f`, interleaved.
pub fn append(out: &mut Vec<u8>, words: &[i32], channels: usize, f: NativeFormat) {
    let ch = channels.max(1);
    out.reserve(words.len() * 4);
    for frame in words.chunks(ch) {
        match f {
            NativeFormat::U32Be => frame
                .iter()
                .for_each(|&w| out.extend_from_slice(&(w as u32).to_be_bytes())),
            NativeFormat::U32Le => frame
                .iter()
                .for_each(|&w| out.extend_from_slice(&(w as u32).to_le_bytes())),
            NativeFormat::U16Le | NativeFormat::U16Be => {
                for half in [16, 0] {
                    for &w in frame {
                        let v = ((w as u32) >> half) as u16;
                        out.extend_from_slice(&if f == NativeFormat::U16Le {
                            v.to_le_bytes()
                        } else {
                            v.to_be_bytes()
                        });
                    }
                }
            }
            NativeFormat::U8 => {
                for shift in [24, 16, 8, 0] {
                    for &w in frame {
                        out.push(((w as u32) >> shift) as u8);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_per_format() {
        // Stereo, 4 bytes per channel: L = 01 02 03 04, R = A1 A2 A3 A4.
        let bytes = [1, 0xA1, 2, 0xA2, 3, 0xA3, 4, 0xA4];
        let mut q = VecDeque::new();
        words(&bytes, 2, &mut q);
        let w: Vec<i32> = q.into_iter().collect();
        assert_eq!(w, [0x0102_0304, 0xA1A2_A3A4u32 as i32]);
        let pack = |f| {
            let mut out = Vec::new();
            append(&mut out, &w, 2, f);
            out
        };
        assert_eq!(pack(NativeFormat::U8), [1, 0xA1, 2, 0xA2, 3, 0xA3, 4, 0xA4]);
        assert_eq!(
            pack(NativeFormat::U16Be),
            [1, 2, 0xA1, 0xA2, 3, 4, 0xA3, 0xA4]
        );
        assert_eq!(
            pack(NativeFormat::U16Le),
            [2, 1, 0xA2, 0xA1, 4, 3, 0xA4, 0xA3]
        );
        assert_eq!(
            pack(NativeFormat::U32Be),
            [1, 2, 3, 4, 0xA1, 0xA2, 0xA3, 0xA4]
        );
        assert_eq!(
            pack(NativeFormat::U32Le),
            [4, 3, 2, 1, 0xA4, 0xA3, 0xA2, 0xA1]
        );
        for f in NativeFormat::ALL {
            assert_eq!(NativeFormat::from_label(f.label()), Some(f));
            assert_eq!(f.bytes() * f.frames_per_word(), 4);
        }
        assert_eq!(word_rate(2_822_400), 88_200);
    }

    #[test]
    fn partial_frame_is_padded() {
        let mut q = VecDeque::new();
        words(&[0x10], 1, &mut q);
        assert_eq!(
            q[0] as u32,
            u32::from_be_bytes([0x10, SILENCE, SILENCE, SILENCE])
        );
    }
}
