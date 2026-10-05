//! DSD over PCM (DoP 1.1): each PCM frame carries 16 DSD bits per channel
//! in a 24-bit sample, earliest byte in bits 15..8, with a marker byte on
//! top that alternates 0x05 / 0xFA from frame to frame. The DAC sees the
//! markers and plays DSD; anything that changes a bit (volume, resampling,
//! mixing) breaks it, so the stream must reach the device untouched.

use std::collections::VecDeque;

use super::SILENCE;

pub const MARKERS: [u8; 2] = [0x05, 0xFA];

/// PCM frame rate carrying `dsd_rate`: 176.4 kHz for DSD64.
pub fn pcm_rate(dsd_rate: u32) -> u32 {
    dsd_rate / 16
}

/// Interleaved DSD bytes (MSB first) → one 16-bit payload per channel and
/// frame, marker not set yet. `bytes` holds a whole number of frames
/// (2 bytes per channel), except at the very end of a stream, where the
/// missing byte is DSD silence.
pub fn payloads(bytes: &[u8], channels: usize, out: &mut VecDeque<i32>) {
    let ch = channels.max(1);
    for frame in bytes.chunks(2 * ch) {
        for c in 0..ch {
            let hi = frame.get(c).copied().unwrap_or(SILENCE);
            let lo = frame.get(ch + c).copied().unwrap_or(SILENCE);
            out.push_back(((hi as i32) << 8) | lo as i32);
        }
    }
}

/// Set the marker of every frame of `samples`, alternating; `phase` carries
/// the alternation over from one call (and one track) to the next. The
/// 24-bit result is sign-extended like any LSB-aligned 24-bit sample, so
/// every container carries it untouched (S32_LE: in the top three bytes).
pub fn mark(samples: &mut [i32], channels: usize, phase: &mut bool) {
    for frame in samples.chunks_mut(channels.max(1)) {
        let marker = MARKERS[*phase as usize] as i32;
        for s in frame.iter_mut() {
            let word = (marker << 16) | (*s & 0xFFFF);
            *s = (word << 8) >> 8;
        }
        *phase = !*phase;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmt::{Container, append_container};

    #[test]
    fn packs_bits_and_alternates_markers() {
        // L0 R0 L1 R1 | L2 R2 L3 R3
        let bytes = [0x11, 0xA1, 0x22, 0xA2, 0x33, 0xA3, 0x44, 0xA4];
        let mut q = VecDeque::new();
        payloads(&bytes, 2, &mut q);
        let mut s: Vec<i32> = q.into_iter().collect();
        assert_eq!(s, [0x1122, 0xA1A2, 0x3344, 0xA3A4]);
        let mut phase = false;
        mark(&mut s, 2, &mut phase);
        let u: Vec<u32> = s.iter().map(|&v| v as u32 & 0xFF_FFFF).collect();
        assert_eq!(u, [0x05_1122, 0x05_A1A2, 0xFA_3344, 0xFA_A3A4]);
        assert!(!phase, "two frames: back to the first marker");
        // The phase carries over.
        let mut t = vec![0x5555, 0x5555];
        mark(&mut t, 2, &mut true);
        assert_eq!(t[0] as u32 & 0xFF_FFFF, 0xFA_5555);
    }

    #[test]
    fn odd_tail_is_padded_with_silence() {
        let mut q = VecDeque::new();
        payloads(&[0x12, 0x34], 2, &mut q);
        assert_eq!(
            q.into_iter().collect::<Vec<_>>(),
            [0x1200 | SILENCE as i32, 0x3400 | SILENCE as i32]
        );
    }

    /// Each container carries marker and payload bit for bit.
    #[test]
    fn containers_carry_dop_untouched() {
        let mut s = vec![0xBEEF, 0x0102];
        mark(&mut s, 1, &mut false);
        for (c, want) in [
            (Container::S24_3, vec![0xEF, 0xBE, 0x05, 0x02, 0x01, 0xFA]),
            (
                Container::S24,
                vec![0xEF, 0xBE, 0x05, 0x00, 0x02, 0x01, 0xFA, 0xFF],
            ),
            (
                Container::S32,
                vec![0x00, 0xEF, 0xBE, 0x05, 0x00, 0x02, 0x01, 0xFA],
            ),
        ] {
            let mut out = Vec::new();
            append_container(&mut out, &s, c, 24);
            assert_eq!(out, want, "{c:?}");
        }
        assert_eq!(pcm_rate(2_822_400), 176_400);
        assert_eq!(pcm_rate(5_644_800), 352_800);
    }
}
