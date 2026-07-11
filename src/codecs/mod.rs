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
    pub note: Option<String>,
}
