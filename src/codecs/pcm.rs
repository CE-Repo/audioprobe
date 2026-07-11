//! PCM variants: Blu-ray (HDMV) LPCM headers and WAVEFORMATEX structures.

use super::CodecInfo;

/// Parse the 4-byte HDMV LPCM audio data header that starts every PES packet
/// of a Blu-ray LPCM stream (stream_type 0x80).
pub fn parse_hdmv_lpcm(payload: &[u8]) -> Option<CodecInfo> {
    if payload.len() < 4 {
        return None;
    }
    let channel_assignment = payload[2] >> 4;
    let sampling_frequency = payload[2] & 0xF;
    let bits_per_sample = payload[3] >> 6;
    let rate = match sampling_frequency {
        1 => 48000,
        4 => 96000,
        5 => 192000,
        _ => return None,
    };
    let depth = match bits_per_sample {
        1 => 16,
        2 => 20,
        3 => 24,
        _ => return None,
    };
    let channels: u32 = match channel_assignment {
        1 => 1,
        3 => 2,
        4 | 5 => 3,
        6 | 7 => 4,
        8 => 5,
        9 => 6, // 5.1
        10 => 7,
        11 => 8, // 7.1
        _ => return None,
    };
    let lfe = matches!(channel_assignment, 9 | 11);
    Some(CodecInfo {
        name: Some("LPCM (Blu-ray)".into()),
        sample_rate: Some(rate),
        bit_depth: Some(depth),
        channels: Some(channels),
        lfe: Some(lfe),
        note: None,
    })
}

/// Parse a WAVEFORMATEX structure (WAV `fmt ` chunk, Matroska A_MS/ACM
/// CodecPrivate). Handles WAVE_FORMAT_EXTENSIBLE.
pub fn parse_waveformatex(b: &[u8]) -> Option<CodecInfo> {
    if b.len() < 16 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u32;
    let u32le = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut tag = u16le(0);
    let channels = u16le(2);
    let rate = u32le(4);
    let mut bits = u16le(14);
    if tag == 0xFFFE && b.len() >= 26 {
        // WAVE_FORMAT_EXTENSIBLE: wValidBitsPerSample + SubFormat GUID
        let valid_bits = u16le(20);
        if valid_bits > 0 {
            bits = valid_bits;
        }
        tag = u16le(24); // first two bytes of the SubFormat GUID
    }
    let (name, is_pcm) = match tag {
        0x0001 => ("PCM", true),
        0x0003 => ("PCM (float)", true),
        0x0006 => ("A-law", false),
        0x0007 => ("µ-law", false),
        0x0050 => ("MP2", false),
        0x0055 => ("MP3", false),
        0x00FF => ("AAC", false),
        0x2000 => ("AC-3", false),
        0x2001 => ("DTS", false),
        0x0161 => ("WMA", false),
        0x0162 => ("WMA Pro", false),
        0x0164 => ("WMA Lossless", false),
        _ => ("PCM/ACM", false),
    };
    if rate == 0 || channels == 0 {
        return None;
    }
    Some(CodecInfo {
        name: Some(name.into()),
        sample_rate: Some(rate),
        bit_depth: if is_pcm && bits > 0 { Some(bits) } else { None },
        channels: Some(channels),
        lfe: None,
        note: None,
    })
}

/// Parse an ALACSpecificConfig (Matroska CodecPrivate / MP4 `alac` box payload).
pub fn parse_alac_config(b: &[u8]) -> Option<CodecInfo> {
    if b.len() < 24 {
        return None;
    }
    let depth = b[5] as u32;
    let channels = b[9] as u32;
    let rate = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    if !matches!(depth, 16 | 20 | 24 | 32) || rate == 0 || channels == 0 || channels > 8 {
        return None;
    }
    Some(CodecInfo {
        name: Some("ALAC".into()),
        sample_rate: Some(rate),
        bit_depth: Some(depth),
        channels: Some(channels),
        lfe: None,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hdmv_lpcm() {
        // channel_assignment=9 (5.1), sampling=4 (96 kHz), bits=3 (24)
        let payload = [0x03, 0xC0, 0x94, 0xC0];
        let info = parse_hdmv_lpcm(&payload).expect("should parse");
        assert_eq!(info.sample_rate, Some(96000));
        assert_eq!(info.bit_depth, Some(24));
        assert_eq!(info.channels, Some(6));
        assert_eq!(info.lfe, Some(true));
    }

    #[test]
    fn parses_waveformatex_pcm() {
        let mut b = vec![0u8; 16];
        b[0..2].copy_from_slice(&1u16.to_le_bytes()); // PCM
        b[2..4].copy_from_slice(&2u16.to_le_bytes()); // stereo
        b[4..8].copy_from_slice(&44100u32.to_le_bytes());
        b[14..16].copy_from_slice(&16u16.to_le_bytes());
        let info = parse_waveformatex(&b).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("PCM"));
        assert_eq!(info.sample_rate, Some(44100));
        assert_eq!(info.bit_depth, Some(16));
        assert_eq!(info.channels, Some(2));
    }
}
