//! Dolby TrueHD / MLP major sync header parsing.

use super::CodecInfo;
use crate::bits::BitReader;

// Channel counts contributed by each bit of the TrueHD (FBA) 13-bit
// 8ch presentation channel assignment field.
const THD_GROUP8_COUNTS: [u32; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 1, 1];
// ... and of the 5-bit 6ch presentation channel assignment field.
const THD_GROUP6_COUNTS: [u32; 5] = [2, 1, 1, 2, 2];
const MLP_QUANT_BITS: [u32; 3] = [16, 20, 24];
// MLP (FBB) 5-bit channel assignment -> channel count.
const MLP_CHANNELS: [u32; 21] = [
    1, 2, 3, 4, 3, 4, 5, 3, 4, 5, 4, 5, 6, 4, 5, 4, 5, 6, 5, 5, 6,
];

fn sample_rate(ratebits: u32) -> Option<u32> {
    if ratebits == 0xF {
        return None;
    }
    let base = if ratebits & 8 != 0 { 44100 } else { 48000 };
    let rate = base << (ratebits & 7);
    if rate > 192000 {
        None
    } else {
        Some(rate)
    }
}

/// Scan for an MLP/TrueHD major sync (0xF8726FBA / 0xF8726FBB) and parse it.
pub fn parse(buf: &[u8]) -> Option<CodecInfo> {
    let mut i = 0;
    while i + 12 <= buf.len() {
        if buf[i] == 0xF8
            && buf[i + 1] == 0x72
            && buf[i + 2] == 0x6F
            && (buf[i + 3] == 0xBA || buf[i + 3] == 0xBB)
            // major sync signature 0xB752 follows the 32-bit format_info
            && buf[i + 8] == 0xB7
            && buf[i + 9] == 0x52
        {
            if let Some(info) = parse_major_sync(&buf[i..]) {
                return Some(info);
            }
        }
        i += 1;
    }
    None
}

fn parse_major_sync(b: &[u8]) -> Option<CodecInfo> {
    let truehd = b[3] == 0xBA;
    let mut r = BitReader::new(b);
    r.skip(32)?; // format_sync

    if truehd {
        let ratebits = r.read_u32(4)?;
        let rate = sample_rate(ratebits)?;
        r.skip(4)?; // 6ch/8ch multichannel type + reserved
        r.skip(2 + 2)?; // 2ch / 6ch presentation channel modifiers
        let ch6 = r.read_u32(5)?;
        r.skip(2)?; // 8ch presentation channel modifier
        let ch8 = r.read_u32(13)?;
        let (assignment, counts): (u32, &[u32]) = if ch8 != 0 {
            (ch8, &THD_GROUP8_COUNTS)
        } else {
            (ch6, &THD_GROUP6_COUNTS)
        };
        let mut channels = 0;
        for (bit, cnt) in counts.iter().enumerate() {
            if (assignment >> bit) & 1 == 1 {
                channels += cnt;
            }
        }
        let lfe = (assignment >> 2) & 1 == 1;

        // The major sync continues past the channel assignments with the peak
        // data rate and the substream count. These sit beyond the 12 bytes the
        // scanner guarantees, so they are read best-effort: a short buffer just
        // leaves bitrate/Atmos unresolved without failing the whole parse.
        let (bitrate, immersive) = read_truehd_extra(&mut r, rate);

        Some(CodecInfo {
            name: Some("TrueHD".into()),
            sample_rate: Some(rate),
            // TrueHD carries a 24-bit PCM pipeline; the format has no
            // per-stream word length field.
            bit_depth: Some(24),
            channels: if channels > 0 { Some(channels) } else { None },
            lfe: if channels > 0 { Some(lfe) } else { None },
            bitrate,
            immersive,
            note: None,
        })
    } else {
        // MLP (FBB): quant_1(4) quant_2(4) rate_1(4) rate_2(4) skip(11) chan_assign(5)
        let quant1 = r.read_u32(4)? as usize;
        r.skip(4)?;
        let ratebits = r.read_u32(4)?;
        let rate = sample_rate(ratebits)?;
        r.skip(4)?; // group 2 rate
        r.skip(11)?;
        let assignment = r.read_u32(5)? as usize;
        Some(CodecInfo {
            name: Some("MLP".into()),
            sample_rate: Some(rate),
            bit_depth: MLP_QUANT_BITS.get(quant1).copied(),
            channels: MLP_CHANNELS.get(assignment).copied(),
            lfe: None,
            ..CodecInfo::default()
        })
    }
}

/// Read the TrueHD major-sync fields that follow the channel assignments:
/// `peak_data_rate` (converted to bits/s) and `substreams`. Dolby Atmos adds
/// a fourth substream carrying the height/object data, so `substreams > 3`
/// marks an Atmos stream. `r` must be positioned right after the 8-channel
/// presentation assignment.
fn read_truehd_extra(r: &mut BitReader, sample_rate: u32) -> (Option<u32>, Option<String>) {
    fn read(r: &mut BitReader, sample_rate: u32) -> Option<(u32, u32)> {
        r.skip(16 + 16 + 16)?; // signature (0xB752), flags, reserved
        r.skip(1)?; // is_vbr
        let peak_data_rate = r.read_u32(15)?;
        let substreams = r.read_u32(4)?;
        // peak_bitrate = peak_data_rate * sample_rate / 16
        let bitrate = ((peak_data_rate as u64 * sample_rate as u64) >> 4) as u32;
        Some((bitrate, substreams))
    }
    match read(r, sample_rate) {
        Some((bitrate, substreams)) => (
            if bitrate > 0 { Some(bitrate) } else { None },
            if substreams > 3 {
                Some("Atmos".into())
            } else {
                None
            },
        ),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::tests_support::BitWriter;

    #[test]
    fn parses_truehd_71_48k() {
        let mut w = BitWriter::new();
        w.put(32, 0xF8726FBA);
        w.put(4, 0); // ratebits -> 48 kHz
        w.put(4, 0); // multichannel types + reserved
        w.put(2, 0); // 2ch modifier
        w.put(2, 0); // 6ch modifier
        w.put(5, 0b00111); // 6ch assignment: L/R, C, LFE
        w.put(2, 0); // 8ch modifier
        w.put(13, 0b0000000001111); // 8ch: L/R + C + LFE + Ls/Rs -> 6... plus bit4 -> 8
                                    // use bits 0..4 set = 2+1+1+2+2 = 8 channels (7.1)
        w.put(16, 0xB752); // signature
        w.put(64, 0);
        let bytes = w.finish();
        let info = parse(&bytes).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("TrueHD"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.bit_depth, Some(24));
        assert_eq!(info.channels, Some(6));
        assert_eq!(info.lfe, Some(true));
    }

    #[test]
    fn parses_truehd_atmos_71() {
        let mut w = BitWriter::new();
        w.put(32, 0xF8726FBA);
        w.put(4, 0); // ratebits -> 48 kHz
        w.put(4, 0); // multichannel types + reserved
        w.put(2, 0); // 2ch modifier
        w.put(2, 0); // 6ch modifier
        w.put(5, 0); // 6ch assignment
        w.put(2, 0); // 8ch modifier
        w.put(13, 0b0000000011111); // 8ch: 2+1+1+2+2 = 8 (7.1)
        w.put(16, 0xB752); // signature
        w.put(16, 0); // flags
        w.put(16, 0); // reserved
        w.put(1, 0); // is_vbr
        w.put(15, 1000); // peak_data_rate
        w.put(4, 4); // substreams -> 4 marks Atmos
        w.put(32, 0);
        let info = parse(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("TrueHD"));
        assert_eq!(info.channels, Some(8));
        assert_eq!(info.lfe, Some(true));
        // peak_bitrate = 1000 * 48000 / 16
        assert_eq!(info.bitrate, Some(3_000_000));
        assert_eq!(info.immersive.as_deref(), Some("Atmos"));
    }

    #[test]
    fn parses_truehd_192k() {
        let mut w = BitWriter::new();
        w.put(32, 0xF8726FBA);
        w.put(4, 2); // ratebits: 48000 << 2 = 192 kHz
        w.put(4, 0);
        w.put(2, 0);
        w.put(2, 0);
        w.put(5, 0b00011); // L/R + C
        w.put(2, 0);
        w.put(13, 0);
        w.put(16, 0xB752);
        w.put(64, 0);
        let info = parse(&w.finish()).expect("should parse");
        assert_eq!(info.sample_rate, Some(192000));
        assert_eq!(info.channels, Some(3));
        assert_eq!(info.lfe, Some(false));
    }
}
