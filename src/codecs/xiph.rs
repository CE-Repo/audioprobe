//! Opus and Vorbis identification headers.

use super::CodecInfo;
use crate::bits::find_pattern;

/// Parse an OpusHead structure (Matroska CodecPrivate, Ogg first packet,
/// MP4 dOps has a slightly different layout handled by the caller).
pub fn parse_opus_head(data: &[u8]) -> Option<CodecInfo> {
    let pos = find_pattern(data, b"OpusHead")?;
    let b = &data[pos..];
    if b.len() < 19 {
        return None;
    }
    let channels = b[9] as u32;
    let input_rate = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
    let mut note = None;
    if input_rate != 0 && input_rate != 48000 {
        note = Some(format!("original input rate {} Hz", input_rate));
    }
    Some(CodecInfo {
        name: Some("Opus".into()),
        // Opus always decodes at 48 kHz.
        sample_rate: Some(48000),
        bit_depth: None,
        channels: if channels > 0 { Some(channels) } else { None },
        lfe: None,
        note,
    })
}

/// Parse a Vorbis identification header (packet type 1).
pub fn parse_vorbis(data: &[u8]) -> Option<CodecInfo> {
    let pos = find_pattern(data, b"\x01vorbis")?;
    let b = &data[pos..];
    if b.len() < 16 {
        return None;
    }
    let channels = b[11] as u32;
    let rate = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
    if rate == 0 {
        return None;
    }
    Some(CodecInfo {
        name: Some("Vorbis".into()),
        sample_rate: Some(rate),
        bit_depth: None,
        channels: if channels > 0 { Some(channels) } else { None },
        lfe: None,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_opus_head() {
        let mut b = b"OpusHead".to_vec();
        b.push(1); // version
        b.push(6); // channels
        b.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        b.extend_from_slice(&48000u32.to_le_bytes()); // input rate
        b.extend_from_slice(&[0, 0, 0]); // gain + mapping family
        let info = parse_opus_head(&b).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("Opus"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(6));
    }

    #[test]
    fn parses_vorbis_id_header() {
        let mut b = b"\x01vorbis".to_vec();
        b.extend_from_slice(&0u32.to_le_bytes()); // version
        b.push(2); // channels
        b.extend_from_slice(&44100u32.to_le_bytes());
        b.extend_from_slice(&[0; 12]);
        let info = parse_vorbis(&b).expect("should parse");
        assert_eq!(info.sample_rate, Some(44100));
        assert_eq!(info.channels, Some(2));
    }
}
