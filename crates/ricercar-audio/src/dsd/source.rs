//! The shape every DSD reader hands to the player: DSF and DFF files,
//! tracks of a SACD image. Shared contract, keep it stable.

use std::io;

/// A DSD stream's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsdFormat {
    /// Bits per second per channel: 2 822 400 for DSD64, 5 644 800 for
    /// DSD128, and so on.
    pub rate: u32,
    pub channels: u16,
}

impl DsdFormat {
    /// The DSD multiple of 44.1 kHz (64 for DSD64).
    pub fn multiple(&self) -> u32 {
        self.rate / 44_100
    }
}

/// A readable DSD stream.
///
/// Bytes are interleaved one byte per channel at a time (L, R, L, R…), and
/// in each byte the most significant bit is the earliest sample (the
/// DSDIFF order). Positions are counted in bytes per channel, so one unit
/// is 8 DSD samples of every channel.
pub trait DsdSource: Send {
    fn format(&self) -> DsdFormat;
    /// Fills `buf` with interleaved bytes; the count returned is a multiple
    /// of the channel count. `Ok(0)` at the end of the stream.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Moves to byte `pos` of each channel.
    fn seek(&mut self, pos: u64) -> io::Result<()>;
    /// Length in bytes per channel, when known.
    fn len(&self) -> Option<u64>;
    /// Tags a reader found (title, artist, album…), if any.
    fn tags(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}
