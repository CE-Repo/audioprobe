//! FLAC STREAMINFO parsing.

use super::CodecInfo;

/// Parse FLAC stream info from `data`, which may start with the "fLaC"
/// marker (native files, MP4 dfLa payload) or directly with a metadata
/// block header (Matroska CodecPrivate always starts with "fLaC").
pub fn parse(data: &[u8]) -> Option<CodecInfo> {
    let mut p = 0;
    if data.len() >= 4 && &data[0..4] == b"fLaC" {
        p = 4;
    }
    // Iterate metadata blocks looking for STREAMINFO (type 0).
    loop {
        if p + 4 > data.len() {
            return None;
        }
        let block_type = data[p] & 0x7F;
        let last = data[p] & 0x80 != 0;
        let len =
            ((data[p + 1] as usize) << 16) | ((data[p + 2] as usize) << 8) | data[p + 3] as usize;
        p += 4;
        if block_type == 0 {
            return parse_streaminfo(data.get(p..p + len.max(18))?);
        }
        if last {
            return None;
        }
        p += len;
    }
}

fn parse_streaminfo(b: &[u8]) -> Option<CodecInfo> {
    if b.len() < 18 {
        return None;
    }
    // bytes 10..13: sample rate (20 bits), channels-1 (3 bits), bps-1 (5 bits)
    let rate = ((b[10] as u32) << 12) | ((b[11] as u32) << 4) | ((b[12] as u32) >> 4);
    let channels = (((b[12] >> 1) & 0x7) as u32) + 1;
    let bps = ((((b[12] & 1) as u32) << 4) | ((b[13] as u32) >> 4)) + 1;
    if rate == 0 {
        return None;
    }
    Some(CodecInfo {
        name: Some("FLAC".into()),
        sample_rate: Some(rate),
        bit_depth: Some(bps),
        channels: Some(channels),
        lfe: None,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a STREAMINFO block for 96 kHz / 24-bit / 2 channels.
    pub fn streaminfo_96k_24_stereo() -> Vec<u8> {
        let mut si = vec![0u8; 34];
        // min/max block size, min/max frame size: irrelevant
        let rate: u32 = 96000;
        si[10] = (rate >> 12) as u8;
        si[11] = (rate >> 4) as u8;
        si[12] = (((rate & 0xF) as u8) << 4) | ((2u8 - 1) << 1) | ((24 - 1) >> 4);
        si[13] = ((24u8 - 1) & 0xF) << 4;
        let mut data = b"fLaC".to_vec();
        data.push(0x80); // last block, type 0
        data.extend_from_slice(&[0, 0, 34]);
        data.extend_from_slice(&si);
        data
    }

    #[test]
    fn parses_streaminfo() {
        let info = parse(&streaminfo_96k_24_stereo()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("FLAC"));
        assert_eq!(info.sample_rate, Some(96000));
        assert_eq!(info.bit_depth, Some(24));
        assert_eq!(info.channels, Some(2));
    }
}
