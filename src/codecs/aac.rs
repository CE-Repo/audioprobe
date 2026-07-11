//! AAC: ADTS frame headers, AudioSpecificConfig and LOAS/LATM StreamMuxConfig.

use super::CodecInfo;
use crate::bits::BitReader;

pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn channels_from_config(cfg: u32) -> (Option<u32>, Option<bool>) {
    match cfg {
        0 => (None, None),          // signalled in-band (PCE)
        7 => (Some(8), Some(true)), // 7.1
        6 => (Some(6), Some(true)), // 5.1
        n if n <= 5 => (Some(n), Some(false)),
        _ => (None, None),
    }
}

fn object_type_name(aot: u32) -> &'static str {
    match aot {
        1 => "AAC Main",
        2 => "AAC-LC",
        3 => "AAC SSR",
        4 => "AAC LTP",
        5 => "HE-AAC",
        23 => "AAC-LD",
        29 => "HE-AACv2",
        39 => "AAC-ELD",
        _ => "AAC",
    }
}

/// Parse an AudioSpecificConfig (e.g. Matroska CodecPrivate or esds DSI).
pub fn parse_asc(data: &[u8]) -> Option<CodecInfo> {
    let mut r = BitReader::new(data);
    parse_asc_bits(&mut r)
}

pub fn parse_asc_bits(r: &mut BitReader) -> Option<CodecInfo> {
    let mut aot = r.read_u32(5)?;
    if aot == 31 {
        aot = 32 + r.read_u32(6)?;
    }
    let sfi = r.read_u32(4)?;
    let mut rate = if sfi == 15 {
        r.read_u32(24)?
    } else {
        *SAMPLE_RATES.get(sfi as usize)?
    };
    let chan_cfg = r.read_u32(4)?;
    if aot == 5 || aot == 29 {
        // explicit SBR signalling: the extension sample rate is the output rate
        let esfi = r.read_u32(4)?;
        rate = if esfi == 15 {
            r.read_u32(24)?
        } else {
            *SAMPLE_RATES.get(esfi as usize)?
        };
    }
    let (channels, lfe) = channels_from_config(chan_cfg);
    Some(CodecInfo {
        name: Some(object_type_name(aot).into()),
        sample_rate: Some(rate),
        bit_depth: None,
        channels,
        lfe,
        note: None,
    })
}

/// Scan for an ADTS frame header and parse it.
pub fn parse_adts(buf: &[u8]) -> Option<CodecInfo> {
    let mut i = 0;
    while i + 7 <= buf.len() {
        if buf[i] == 0xFF && (buf[i + 1] & 0xF6) == 0xF0 {
            let profile = (buf[i + 2] >> 6) as u32 + 1; // MPEG-4 object type
            let sfi = ((buf[i + 2] >> 2) & 0xF) as usize;
            let chan_cfg = (((buf[i + 2] & 1) << 2) | (buf[i + 3] >> 6)) as u32;
            if let Some(&rate) = SAMPLE_RATES.get(sfi) {
                let (channels, lfe) = channels_from_config(chan_cfg);
                return Some(CodecInfo {
                    name: Some(object_type_name(profile).into()),
                    sample_rate: Some(rate),
                    bit_depth: None,
                    channels,
                    lfe,
                    note: None,
                });
            }
        }
        i += 1;
    }
    None
}

/// Scan for a LOAS AudioSyncStream and parse the LATM StreamMuxConfig.
pub fn parse_loas_latm(buf: &[u8]) -> Option<CodecInfo> {
    let mut i = 0;
    while i + 8 <= buf.len() {
        if buf[i] == 0x56 && (buf[i + 1] & 0xE0) == 0xE0 {
            if let Some(info) = parse_audio_mux_element(&buf[i + 3..]) {
                return Some(info);
            }
        }
        i += 1;
    }
    None
}

fn parse_audio_mux_element(b: &[u8]) -> Option<CodecInfo> {
    let mut r = BitReader::new(b);
    let use_same_mux = r.read_u32(1)?;
    if use_same_mux == 1 {
        return None; // config not in this element, try the next sync
    }
    // StreamMuxConfig
    let audio_mux_version = r.read_u32(1)?;
    if audio_mux_version == 1 && r.read_u32(1)? == 1 {
        return None; // audioMuxVersionA != 0: out of scope
    }
    if audio_mux_version == 1 {
        latm_get_value(&mut r)?; // taraBufferFullness
    }
    r.skip(1)?; // allStreamsSameTimeFraming
    r.skip(6)?; // numSubFrames
    let num_program = r.read_u32(4)?;
    let num_layer = r.read_u32(3)?;
    if num_program != 0 || num_layer != 0 {
        // multi-program/layer muxes are rare; the first ASC still follows
    }
    if audio_mux_version == 1 {
        latm_get_value(&mut r)?; // ascLen — ignore, parse in place
    }
    parse_asc_bits(&mut r)
}

fn latm_get_value(r: &mut BitReader) -> Option<u64> {
    let bytes = r.read_u32(2)? + 1;
    let mut v = 0u64;
    for _ in 0..bytes {
        v = (v << 8) | r.read(8)?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::tests_support::BitWriter;

    #[test]
    fn parses_asc_lc_48k_stereo() {
        let mut w = BitWriter::new();
        w.put(5, 2); // AAC-LC
        w.put(4, 3); // 48 kHz
        w.put(4, 2); // 2 channels
        w.put(3, 0); // frame length etc.
        let info = parse_asc(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("AAC-LC"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(2));
    }

    #[test]
    fn parses_asc_he_aac_output_rate() {
        let mut w = BitWriter::new();
        w.put(5, 5); // SBR
        w.put(4, 6); // core 24 kHz
        w.put(4, 2); // stereo
        w.put(4, 3); // extension rate 48 kHz
        w.put(5, 2); // implied AOT
        let info = parse_asc(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("HE-AAC"));
        assert_eq!(info.sample_rate, Some(48000));
    }

    #[test]
    fn parses_adts_lc_44k_51() {
        // 0xFFF1: MPEG-4, layer 0, no CRC
        let hdr = [0xFF, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC];
        // profile=(0x50>>6)=1 -> LC; sfi=(0x50>>2)&0xF=4 -> 44100; chancfg=2
        let info = parse_adts(&hdr).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("AAC-LC"));
        assert_eq!(info.sample_rate, Some(44100));
        assert_eq!(info.channels, Some(2));
    }
}
