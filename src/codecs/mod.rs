//! Native codec bitstream header parsers.
//!
//! Every parser takes a byte slice (typically the first frames of the
//! elementary stream) and extracts sample rate, bit depth (where the codec
//! defines one), channel count and a display name from the sync headers.

pub mod aac;
pub mod ac3;
pub mod dts;
pub mod flac;
pub mod mpeg_audio;
pub mod pcm;
pub mod truehd;
pub mod xiph;

#[derive(Debug, Default, Clone)]
pub struct CodecInfo {
    pub name: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub lfe: Option<bool>,
    /// Nominal (constant-bitrate codecs) or peak (TrueHD) bit rate, in bits
    /// per second. `None` for formats whose header carries no rate the parser
    /// can trust (variable-bitrate AAC, lossless FLAC, …).
    pub bitrate: Option<u32>,
    /// Object/immersive audio layered on top of the base codec: `"Atmos"`
    /// (TrueHD or E-AC-3/JOC) or `"DTS:X"`. `None` for channel-based audio.
    pub immersive: Option<String>,
    pub note: Option<String>,
}
