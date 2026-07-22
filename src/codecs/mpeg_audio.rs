//! MPEG-1/2/2.5 audio (MP1/MP2/MP3) frame header parsing.

use super::CodecInfo;

const RATES_V1: [u32; 3] = [44100, 48000, 32000];
// Bit rate in kbit/s by the 4-bit index (0 = free format, unresolved).
const BR_V1_L1: [u32; 15] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
];
const BR_V1_L2: [u32; 15] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
];
const BR_V1_L3: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const BR_V2_L1: [u32; 15] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
];
const BR_V2_L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// Scan for an MPEG audio frame header and parse it.
pub fn parse(buf: &[u8]) -> Option<CodecInfo> {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i] == 0xFF && (buf[i + 1] & 0xE0) == 0xE0 {
            if let Some(info) = parse_header(&buf[i..i + 4]) {
                return Some(info);
            }
        }
        i += 1;
    }
    None
}

fn parse_header(b: &[u8]) -> Option<CodecInfo> {
    let version = (b[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5
    let layer = (b[1] >> 1) & 3; // 3 = I, 2 = II, 1 = III
    if version == 1 || layer == 0 {
        return None;
    }
    let bitrate_idx = (b[2] >> 4) & 0xF;
    if bitrate_idx == 0xF {
        return None;
    }
    let sr_idx = ((b[2] >> 2) & 3) as usize;
    if sr_idx == 3 {
        return None;
    }
    let rate = match version {
        3 => RATES_V1[sr_idx],
        2 => RATES_V1[sr_idx] / 2,
        0 => RATES_V1[sr_idx] / 4,
        _ => return None,
    };
    let mode = (b[3] >> 6) & 3;
    let name = match layer {
        3 => "MP1",
        2 => "MP2",
        1 => "MP3",
        _ => unreachable!(),
    };
    let br_table: &[u32; 15] = match (version, layer) {
        (3, 3) => &BR_V1_L1,  // MPEG-1 Layer I
        (3, 2) => &BR_V1_L2,  // MPEG-1 Layer II
        (3, 1) => &BR_V1_L3,  // MPEG-1 Layer III
        (_, 3) => &BR_V2_L1,  // MPEG-2/2.5 Layer I
        (_, _) => &BR_V2_L23, // MPEG-2/2.5 Layer II/III
    };
    let bitrate = match br_table[bitrate_idx as usize] {
        0 => None, // free-format streams carry no header bit rate
        kb => Some(kb * 1000),
    };
    Some(CodecInfo {
        name: Some(name.into()),
        sample_rate: Some(rate),
        bit_depth: None,
        channels: Some(if mode == 3 { 1 } else { 2 }),
        lfe: Some(false),
        bitrate,
        ..CodecInfo::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mp3_44k_stereo() {
        // 0xFFFB: MPEG-1 Layer III; 0x90: bitrate idx 9, 44.1 kHz; 0x00: stereo
        let info = parse(&[0xFF, 0xFB, 0x90, 0x00]).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("MP3"));
        assert_eq!(info.sample_rate, Some(44100));
        assert_eq!(info.channels, Some(2));
        // MPEG-1 Layer III, bitrate index 9 -> 128 kbit/s
        assert_eq!(info.bitrate, Some(128_000));
    }

    #[test]
    fn parses_mp2_48k() {
        // 0xFFF5: MPEG-1 Layer II no CRC? -> b1 = 0xF5: version 3? (0xF5>>3)&3 = 2 -> MPEG-2
        // Use 0xFFFD: version 3 (MPEG-1), layer 2 (II)
        let info = parse(&[0xFF, 0xFD, 0x94, 0xC0]).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("MP2"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(1)); // mode 3 = mono
                                            // MPEG-1 Layer II, bitrate index 9 -> 160 kbit/s
        assert_eq!(info.bitrate, Some(160_000));
    }
}
